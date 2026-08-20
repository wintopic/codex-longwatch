#![cfg(feature = "test-support")]

use std::path::PathBuf;

use codex_longwatch::{
    app_server::AppServerTransport,
    config::{PersistedQueueState, QueueConfig},
    queue::QueuePhase,
    runtime::{RuntimeCommand, spawn_runtime},
    transport::{CodexTransport, TransportEvent, TransportKind, TurnStatus},
};

fn fake_transport(scenario: &str) -> AppServerTransport {
    AppServerTransport::new(PathBuf::from(env!("CARGO_BIN_EXE_fake-codex-app-server")))
        .with_env("LONGWATCH_FAKE_SCENARIO", scenario)
}

async fn completed(transport: &mut AppServerTransport, expected_turn: &str) -> String {
    loop {
        match transport.next_event().await {
            Some(TransportEvent::TurnCompleted { turn, .. }) if turn.id == expected_turn => {
                return turn.final_message;
            }
            Some(_) => {}
            None => panic!("fake app-server disconnected before completion"),
        }
    }
}

#[tokio::test]
async fn completed_item_supplies_the_final_reply_when_turn_items_are_empty() {
    let mut transport = fake_transport("completed_item_only");
    transport.connect().await.unwrap();
    let thread = transport.start_thread(None).await.unwrap();
    let turn = transport
        .start_turn(&thread.id, "real task", None)
        .await
        .unwrap();

    assert_eq!(completed(&mut transport, &turn.id).await, "final from item");
    transport.shutdown().await;
}

#[tokio::test]
async fn initialize_exposes_the_real_app_server_identity() {
    let mut transport = fake_transport("success");
    transport.connect().await.unwrap();

    let status = transport.status();
    assert!(status.connected);
    assert_eq!(status.kind, TransportKind::AppServer);
    assert_eq!(status.server_agent.as_deref(), Some("fake-codex/1"));

    transport.shutdown().await;
    assert!(!transport.status().connected);
}

#[tokio::test]
async fn the_same_transport_can_reconnect_with_fresh_rpc_state() {
    let mut transport = fake_transport("completed_item_only");
    transport.connect().await.unwrap();
    let first_thread = transport.start_thread(None).await.unwrap();
    transport.shutdown().await;

    transport.connect().await.unwrap();
    let second_thread = transport.start_thread(None).await.unwrap();
    let turn = transport
        .start_turn(&second_thread.id, "real task", None)
        .await
        .unwrap();

    assert_eq!(first_thread.id, second_thread.id);
    assert_eq!(completed(&mut transport, &turn.id).await, "final from item");
    transport.shutdown().await;
}

#[tokio::test]
async fn completed_item_stops_the_runtime_without_scheduling_an_empty_reply_retry() {
    let mut config = QueueConfig {
        prompt: "real task".into(),
        ..QueueConfig::default()
    };
    config.retry_policy.base_delay_secs = 60;
    let runtime = spawn_runtime(
        fake_transport("completed_item_only"),
        config,
        PersistedQueueState::default(),
        None,
    );
    let mut snapshots = runtime.snapshot();
    runtime.send(RuntimeCommand::Start).await.unwrap();

    let snapshot = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            snapshots.changed().await.unwrap();
            let snapshot = snapshots.borrow().clone();
            if matches!(
                snapshot.phase,
                QueuePhase::Success | QueuePhase::Backoff | QueuePhase::Paused
            ) {
                break snapshot;
            }
        }
    })
    .await
    .unwrap();

    assert_eq!(snapshot.phase, QueuePhase::Success);
    assert_eq!(snapshot.attempt_count, 1);
    assert_eq!(snapshot.reply_preview, "final from item");
    assert!(snapshot.transport_status.connected);
    assert_eq!(snapshot.transport_status.kind, TransportKind::AppServer);
    assert_eq!(
        snapshot.transport_status.server_agent.as_deref(),
        Some("fake-codex/1")
    );
    runtime.send(RuntimeCommand::Shutdown).await.unwrap();
    runtime.join().await.unwrap();
}

#[tokio::test]
async fn two_high_demand_turns_then_success() {
    let mut transport = fake_transport("two_busy_then_success");
    transport.connect().await.unwrap();
    let thread = transport.start_thread(None).await.unwrap();
    for attempt in 1..=3 {
        let turn = transport
            .start_turn(&thread.id, "real task", None)
            .await
            .unwrap();
        let final_message = completed(&mut transport, &turn.id).await;
        if attempt <= 2 {
            assert!(final_message.starts_with("We're experiencing high demand"));
        } else {
            assert_eq!(final_message, "normal successful response");
        }
    }
    transport.shutdown().await;
}

#[tokio::test]
async fn internal_retry_stays_on_the_same_turn() {
    let mut transport = fake_transport("internal_retry");
    transport.connect().await.unwrap();
    let thread = transport.start_thread(None).await.unwrap();
    let turn = transport
        .start_turn(&thread.id, "real task", None)
        .await
        .unwrap();
    let mut saw_internal_retry = false;
    loop {
        match transport.next_event().await {
            Some(TransportEvent::Error {
                turn_id,
                will_retry,
                ..
            }) => {
                assert_eq!(turn_id, turn.id);
                assert!(will_retry);
                saw_internal_retry = true;
            }
            Some(TransportEvent::TurnCompleted {
                turn: completed, ..
            }) => {
                assert_eq!(completed.id, turn.id);
                assert_eq!(completed.final_message, "completed after internal retry");
                break;
            }
            Some(_) => {}
            None => panic!("fake app-server disconnected"),
        }
    }
    assert!(saw_internal_retry);
    transport.shutdown().await;
}

#[tokio::test]
async fn fatal_error_is_returned_on_completion() {
    let mut transport = fake_transport("fatal");
    transport.connect().await.unwrap();
    let thread = transport.start_thread(None).await.unwrap();
    let turn = transport
        .start_turn(&thread.id, "real task", None)
        .await
        .unwrap();
    loop {
        if let Some(TransportEvent::TurnCompleted {
            turn: completed, ..
        }) = transport.next_event().await
        {
            assert_eq!(completed.id, turn.id);
            assert_eq!(completed.status, TurnStatus::Failed);
            assert_eq!(completed.error.unwrap().message, "authentication required");
            break;
        }
    }
    transport.shutdown().await;
}

#[tokio::test]
async fn process_crash_can_be_reconciled_by_thread_resume() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("crashed.marker");
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake-codex-app-server"));
    let mut first = AppServerTransport::new(fake.clone())
        .with_env("LONGWATCH_FAKE_SCENARIO", "crash_then_resume")
        .with_env("LONGWATCH_FAKE_MARKER", marker.as_os_str());
    first.connect().await.unwrap();
    let thread = first.start_thread(None).await.unwrap();
    first
        .start_turn(&thread.id, "real task", None)
        .await
        .unwrap();
    loop {
        if matches!(
            first.next_event().await,
            Some(TransportEvent::Disconnected { .. }) | None
        ) {
            break;
        }
    }
    first.shutdown().await;

    let mut second = AppServerTransport::new(fake)
        .with_env("LONGWATCH_FAKE_SCENARIO", "crash_then_resume")
        .with_env("LONGWATCH_FAKE_MARKER", marker.as_os_str());
    second.connect().await.unwrap();
    let restored = second.resume_thread(&thread.id, None).await.unwrap();
    let latest = restored.latest_turn.expect("accepted turn is restored");
    assert_eq!(latest.id, "turn-crashed");
    assert_eq!(latest.final_message, "recovered successfully");
    second.shutdown().await;
}

#[tokio::test]
async fn timeout_is_followed_by_an_interrupt_request() {
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("interrupt.log");
    let mut transport = fake_transport("timeout").with_env("LONGWATCH_FAKE_LOG", log.as_os_str());
    transport.connect().await.unwrap();
    let thread = transport.start_thread(None).await.unwrap();
    let turn = transport
        .start_turn(&thread.id, "real task", None)
        .await
        .unwrap();
    let completion = tokio::time::timeout(std::time::Duration::from_millis(100), async {
        loop {
            if matches!(
                transport.next_event().await,
                Some(TransportEvent::TurnCompleted { .. }) | None
            ) {
                break;
            }
        }
    })
    .await;
    assert!(
        completion.is_err(),
        "the fake turn should remain in progress"
    );
    transport
        .interrupt_turn(&thread.id, &turn.id)
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(log).unwrap(), "interrupt");
    transport.shutdown().await;
}

#[tokio::test]
async fn late_completion_keeps_the_old_turn_id() {
    let mut transport = fake_transport("late_event");
    transport.connect().await.unwrap();
    let thread = transport.start_thread(None).await.unwrap();
    let first = transport
        .start_turn(&thread.id, "real task", None)
        .await
        .unwrap();
    assert_eq!(completed(&mut transport, &first.id).await, "first response");

    let second = transport
        .start_turn(&thread.id, "real task", None)
        .await
        .unwrap();
    let mut late = None;
    let mut current = None;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while late.is_none() || current.is_none() {
            match transport.next_event().await {
                Some(TransportEvent::TurnCompleted { turn, .. }) if turn.id == first.id => {
                    late = Some(turn.final_message);
                }
                Some(TransportEvent::TurnCompleted { turn, .. }) if turn.id == second.id => {
                    current = Some(turn.final_message);
                }
                Some(_) => {}
                None => panic!("fake app-server disconnected before both completions"),
            }
        }
    })
    .await
    .unwrap();

    assert_eq!(late.as_deref(), Some("late duplicate"));
    assert_eq!(current.as_deref(), Some("current response"));
    transport.shutdown().await;
}

#[tokio::test]
async fn server_request_with_string_id_is_explicitly_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("server-request-response.json");
    let mut transport =
        fake_transport("server_request_string_id").with_env("LONGWATCH_FAKE_LOG", log.as_os_str());
    transport.connect().await.unwrap();

    let response: serde_json::Value =
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Ok(bytes) = std::fs::read(&log)
                    && let Ok(response) = serde_json::from_slice(&bytes)
                {
                    break response;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    assert_eq!(response["id"], "approval-request");
    assert_eq!(response["error"]["code"], -32601);
    transport.shutdown().await;
}
