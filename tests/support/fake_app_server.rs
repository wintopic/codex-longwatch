#![allow(clippy::needless_pass_by_value)]

use std::{
    env, fs,
    io::{self, BufRead, Write},
    path::PathBuf,
    process,
};

use serde_json::{Value, json};

fn main() {
    let scenario = env::var("LONGWATCH_FAKE_SCENARIO").unwrap_or_else(|_| "success".into());
    let marker = env::var_os("LONGWATCH_FAKE_MARKER").map(PathBuf::from);
    let log = env::var_os("LONGWATCH_FAKE_LOG").map(PathBuf::from);
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    let mut turns = 0_u32;

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let id = message.get("id").cloned();
        match method {
            "initialize" => {
                respond(
                    &mut stdout,
                    id,
                    json!({
                        "codexHome": "C:/fake-codex-home",
                        "platformFamily": "windows",
                        "platformOs": "windows",
                        "userAgent": "fake-codex/1"
                    }),
                );
                if scenario == "server_request_string_id" {
                    send(
                        &mut stdout,
                        json!({
                            "id": "approval-request",
                            "method": "item/commandExecution/requestApproval",
                            "params": {}
                        }),
                    );
                }
            }
            "initialized" => {}
            "thread/start" => respond(
                &mut stdout,
                id,
                json!({"thread": {"id": "thread-1", "turns": []}}),
            ),
            "thread/resume" => {
                let restored = if scenario == "crash_then_resume"
                    && marker.as_ref().is_some_and(|path| path.exists())
                {
                    vec![completed_turn("turn-crashed", "recovered successfully")]
                } else {
                    Vec::new()
                };
                respond(
                    &mut stdout,
                    id,
                    json!({"thread": {"id": "thread-1", "turns": restored}}),
                );
            }
            "turn/start" => {
                turns += 1;
                let turn_id = format!("turn-{turns}");
                respond(
                    &mut stdout,
                    id,
                    json!({"turn": {"id": turn_id, "status": "inProgress", "items": []}}),
                );
                send(
                    &mut stdout,
                    json!({
                        "method": "turn/started",
                        "params": {
                            "threadId": "thread-1",
                            "turn": {"id": turn_id, "status": "inProgress", "items": []}
                        }
                    }),
                );
                match scenario.as_str() {
                    "two_busy_then_success" if turns <= 2 => complete(
                        &mut stdout,
                        &turn_id,
                        "We're experiencing high demand. Please try again later.",
                    ),
                    "internal_retry" => {
                        send(
                            &mut stdout,
                            json!({
                                "method": "error",
                                "params": {
                                    "threadId": "thread-1",
                                    "turnId": turn_id,
                                    "willRetry": true,
                                    "error": {
                                        "message": "temporary overload",
                                        "codexErrorInfo": "serverOverloaded"
                                    }
                                }
                            }),
                        );
                        complete(&mut stdout, &turn_id, "completed after internal retry");
                    }
                    "fatal" => send(
                        &mut stdout,
                        json!({
                            "method": "turn/completed",
                            "params": {
                                "threadId": "thread-1",
                                "turn": {
                                    "id": turn_id,
                                    "status": "failed",
                                    "items": [],
                                    "error": {
                                        "message": "authentication required",
                                        "codexErrorInfo": "unauthorized"
                                    }
                                }
                            }
                        }),
                    ),
                    "crash_then_resume" if marker.as_ref().is_some_and(|path| !path.exists()) => {
                        if let Some(path) = &marker {
                            fs::write(path, b"accepted").expect("write crash marker");
                        }
                        stdout.flush().expect("flush before crash");
                        process::exit(17);
                    }
                    "timeout" => {}
                    "late_event" if turns == 1 => {
                        complete(&mut stdout, &turn_id, "first response");
                    }
                    "late_event" => {
                        complete(&mut stdout, "turn-1", "late duplicate");
                        complete(&mut stdout, &turn_id, "current response");
                    }
                    "completed_item_only" => {
                        complete_with_authoritative_item(&mut stdout, &turn_id, "final from item");
                    }
                    _ => complete(&mut stdout, &turn_id, "normal successful response"),
                }
            }
            "turn/interrupt" => {
                if let Some(path) = &log {
                    fs::write(path, b"interrupt").expect("write interrupt log");
                }
                respond(&mut stdout, id, json!({}));
            }
            _ if id.is_some() && message.get("error").is_some() => {
                if let Some(path) = &log {
                    fs::write(path, serde_json::to_vec(&message).unwrap())
                        .expect("write client response log");
                }
            }
            _ if id.is_some() => {
                send(
                    &mut stdout,
                    json!({
                        "id": id,
                        "error": {"code": -32601, "message": "unknown fake method"}
                    }),
                );
            }
            _ => {}
        }
    }
}

fn completed_turn(id: &str, text: &str) -> Value {
    json!({
        "id": id,
        "status": "completed",
        "items": [{"id": format!("item-{id}"), "type": "agentMessage", "text": text}]
    })
}

fn complete(stdout: &mut impl Write, turn_id: &str, text: &str) {
    send(
        stdout,
        json!({
            "method": "item/agentMessage/delta",
            "params": {
                "threadId": "thread-1",
                "turnId": turn_id,
                "itemId": format!("item-{turn_id}"),
                "delta": text
            }
        }),
    );
    send_completed_item(stdout, turn_id, text);
    send(
        stdout,
        json!({
            "method": "turn/completed",
            "params": {"threadId": "thread-1", "turn": completed_turn(turn_id, text)}
        }),
    );
}

fn complete_with_authoritative_item(stdout: &mut impl Write, turn_id: &str, text: &str) {
    send_completed_item(stdout, turn_id, text);
    send(
        stdout,
        json!({
            "method": "turn/completed",
            "params": {
                "threadId": "thread-1",
                "turn": {
                    "id": turn_id,
                    "status": "completed",
                    "items": [],
                    "error": null
                }
            }
        }),
    );
}

fn send_completed_item(stdout: &mut impl Write, turn_id: &str, text: &str) {
    send(
        stdout,
        json!({
            "method": "item/completed",
            "params": {
                "threadId": "thread-1",
                "turnId": turn_id,
                "completedAtMs": 0,
                "item": {
                    "id": format!("item-{turn_id}"),
                    "type": "agentMessage",
                    "text": text,
                    "phase": "final_answer"
                }
            }
        }),
    );
}

fn respond(stdout: &mut impl Write, id: Option<Value>, result: Value) {
    if let Some(id) = id {
        send(stdout, json!({"id": id, "result": result}));
    }
}

fn send(stdout: &mut impl Write, value: Value) {
    serde_json::to_writer(&mut *stdout, &value).expect("serialize fake event");
    stdout.write_all(b"\n").expect("write fake newline");
    stdout.flush().expect("flush fake event");
}
