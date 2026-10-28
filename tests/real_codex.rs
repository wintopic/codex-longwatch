//! Opt-in round trip against a real `codex app-server`.
//!
//! Run with a throwaway `CODEX_HOME` whose provider points at a local fake
//! Responses endpoint (never a real account):
//!
//! ```text
//! LONGWATCH_REAL_CODEX_HOME=<dir> cargo test --features test-support \
//!     --test real_codex -- --ignored
//! ```
#![cfg(feature = "test-support")]

use std::{path::PathBuf, time::Duration};

use codex_longwatch::{
    app_server::AppServerTransport,
    config::ClientIdentity,
    transport::{CodexTransport, TransportEvent, TurnStatus},
};

fn real_transport(home: &str) -> AppServerTransport {
    let codex = std::env::var_os("LONGWATCH_REAL_CODEX")
        .map_or_else(|| PathBuf::from("codex"), PathBuf::from);
    AppServerTransport::new(codex)
        .with_client_identity(ClientIdentity::OfficialDesktop)
        .with_env("CODEX_HOME", home)
}

#[tokio::test]
#[ignore = "needs a real Codex CLI and a disposable CODEX_HOME"]
async fn real_app_server_round_trip_with_reconciliation() {
    let Ok(home) = std::env::var("LONGWATCH_REAL_CODEX_HOME") else {
        eprintln!("LONGWATCH_REAL_CODEX_HOME is not set; skipping");
        return;
    };
    let mut transport = real_transport(&home);
    transport.connect().await.unwrap();
    let agent = transport.status().server_agent.unwrap_or_default();
    assert!(agent.starts_with("Codex Desktop/"), "user agent {agent}");

    let thread = transport.start_thread(None).await.unwrap();
    let turn = transport
        .start_turn(
            &thread.id,
            "Reply with READY",
            None,
            "longwatch-real-client-id",
        )
        .await
        .unwrap();
    let completed = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            match transport.next_event().await {
                Some(TransportEvent::TurnCompleted {
                    turn: completed, ..
                }) if completed.id == turn.id => {
                    break completed;
                }
                Some(_) => {}
                None => panic!("app-server closed before the turn completed"),
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(completed.status, TurnStatus::Completed);
    assert!(!completed.final_message.trim().is_empty());
    assert_eq!(
        transport
            .fetch_turn_message(&thread.id, &turn.id)
            .await
            .as_deref(),
        Some(completed.final_message.as_str())
    );
    transport.shutdown().await;

    // A fresh process sees the same turn and recognises it as ours.
    let mut transport = real_transport(&home);
    transport.connect().await.unwrap();
    let resumed = transport.resume_thread(&thread.id, None).await.unwrap();
    let latest = resumed.latest_turn.expect("latest turn is returned");
    assert_eq!(latest.id, turn.id);
    assert!(latest.client_ids_reported);
    assert_eq!(latest.client_message_ids, ["longwatch-real-client-id"]);
    assert_eq!(latest.final_message, completed.final_message);
    transport.shutdown().await;
}
