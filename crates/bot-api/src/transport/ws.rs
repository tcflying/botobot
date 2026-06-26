//! WebSocket transport：只负责 wire 编解码、订阅 Hub 事件并写回客户端。

use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use tokio::sync::mpsc;

use crate::Hub;
use crate::logs;
use crate::protocol::{Event, EventMsg, Op, SessionId, Submission};

/// §1.8.3b 召回默认开：Submit/Steer 未带 `force_recall` 时默认开（人面向请求缺省即强制召回）。
fn default_force_recall() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMsg {
    #[serde(alias = "turn_start")]
    UserMessage {
        #[serde(default, alias = "thread_id")]
        session_id: Option<String>,
        #[serde(default)]
        bot_id: Option<String>,
        #[serde(default)]
        text: String,
        #[serde(default)]
        images: Vec<String>,
        #[serde(default)]
        thinking: Option<bool>,
        #[serde(default)]
        web_search: Option<bool>,
        #[serde(default)]
        code_execution: Option<bool>,
        #[serde(default = "default_force_recall")]
        force_recall: bool,
    },
    #[serde(alias = "turn_steer")]
    Steer {
        #[serde(default, alias = "thread_id")]
        session_id: Option<String>,
        #[serde(default)]
        bot_id: Option<String>,
        #[serde(default)]
        text: String,
        #[serde(default)]
        images: Vec<String>,
        #[serde(default)]
        thinking: Option<bool>,
        #[serde(default)]
        web_search: Option<bool>,
        #[serde(default)]
        code_execution: Option<bool>,
        #[serde(default = "default_force_recall")]
        force_recall: bool,
    },
    #[serde(alias = "approval_respond")]
    Approval {
        #[serde(default, alias = "thread_id")]
        session_id: Option<String>,
        approval_id: String,
        approved: bool,
        /// §2.11 四档（once/session/always/deny）；缺省回退 `approved`。
        #[serde(default)]
        decision: Option<base_types::ApprovalDecision>,
        #[serde(default)]
        reason: Option<String>,
    },
    #[serde(alias = "turn_interrupt")]
    Cancel {
        #[serde(default, alias = "thread_id")]
        session_id: Option<String>,
    },
    Subscribe {
        session_id: String,
        #[serde(default)]
        bot_id: Option<String>,
    },
    Ping,
    SubscribeLogs,
}

pub async fn handle_socket(socket: WebSocket, hub: Hub) {
    let (tx, mut rx) = socket.split();
    // §5.5 D4：默认会话 id **懒创建**——不在连接时 `open_session`（前端默认 bot 用自生成 id、从不
    // 走空 session_id，故每连接 eager open 只会在 hub 留一个永不使用的孤儿 session，长跑累积）。
    // 仅当某条消息缺 session_id 退回此 id 时，对应 handler 才 open_session（见下）。
    let default_session_id = uuid::Uuid::new_v4().to_string();
    tracing::info!(target: "botobot::ws", default_session_id = %default_session_id, "open");

    let (outbox_tx, mut outbox_rx) = mpsc::channel::<Message>(256);
    let writer = tokio::spawn(async move {
        let mut tx = tx;
        while let Some(msg) = outbox_rx.recv().await {
            if tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    spawn_log_forwarder(outbox_tx.clone());
    // §5.5 D4：不再为默认会话预建转发器（懒创建）——各消息 handler 在 open_session 后自行 ensure。
    let mut subscribed = HashSet::<SessionId>::new();

    while let Some(Ok(msg)) = rx.next().await {
        match msg {
            Message::Text(text) => {
                if let Ok(sub) = serde_json::from_str::<Submission>(&text) {
                    let session_id = sub.session_id.clone();
                    if matches!(&sub.op, Op::UserMessage { .. } | Op::Steer { .. }) {
                        match hub.open_session(session_id.clone()) {
                            Ok(session_id) => ensure_session_forwarder(
                                &hub,
                                session_id,
                                &outbox_tx,
                                &mut subscribed,
                            ),
                            Err(e) => {
                                let _ = send_json(&outbox_tx, error_json(&e)).await;
                                continue;
                            }
                        }
                    } else {
                        ensure_session_forwarder(
                            &hub,
                            session_id.clone(),
                            &outbox_tx,
                            &mut subscribed,
                        );
                    }
                    if let Err(e) = hub.submit(sub).await {
                        let _ = send_json(&outbox_tx, error_json(&e)).await;
                    }
                    continue;
                }

                let Ok(parsed) = serde_json::from_str::<ClientMsg>(&text) else {
                    tracing::warn!(target: "botobot::ws", "bad client message: {text}");
                    continue;
                };
                match parsed {
                    ClientMsg::Ping => {
                        tracing::trace!(target: "botobot::ws", dir = "←", kind = "ping", "");
                        let _ = send_text(&outbox_tx, r#"{"type":"pong"}"#).await;
                    }
                    ClientMsg::SubscribeLogs => {
                        tracing::debug!(target: "botobot::ws", dir = "←", kind = "subscribe_logs", "already subscribed");
                    }
                    ClientMsg::Subscribe { session_id, bot_id } => match bot_id
                        .map(|bot_id| hub.open_session_for_bot(session_id.clone(), bot_id))
                        .unwrap_or_else(|| hub.open_session(session_id))
                    {
                        Ok(session_id) => {
                            ensure_session_forwarder(&hub, session_id, &outbox_tx, &mut subscribed);
                        }
                        Err(e) => {
                            let _ = send_json(&outbox_tx, error_json(&e)).await;
                        }
                    },
                    ClientMsg::UserMessage {
                        session_id,
                        bot_id,
                        text,
                        images,
                        thinking,
                        web_search,
                        code_execution,
                        force_recall,
                    } => {
                        let session_id = session_id.unwrap_or_else(|| default_session_id.clone());
                        let sub = Submission::new(
                            session_id.clone(),
                            Op::UserMessage {
                                text,
                                images,
                                thinking,
                                web_search,
                                code_execution,
                                force_recall,
                            },
                        );
                        let result = if let Some(bot_id) = bot_id {
                            hub.open_session_for_bot(session_id, bot_id)
                        } else {
                            hub.open_session(session_id)
                        };
                        if let Err(e) = result {
                            let _ = send_json(&outbox_tx, error_json(&e)).await;
                            continue;
                        }
                        ensure_session_forwarder(
                            &hub,
                            sub.session_id.clone(),
                            &outbox_tx,
                            &mut subscribed,
                        );
                        if let Err(e) = hub.submit(sub).await {
                            let _ = send_json(&outbox_tx, error_json(&e)).await;
                        }
                    }
                    ClientMsg::Steer {
                        session_id,
                        bot_id,
                        text,
                        images,
                        thinking,
                        web_search,
                        code_execution,
                        force_recall,
                    } => {
                        let session_id = session_id.unwrap_or_else(|| default_session_id.clone());
                        let sub = Submission::new(
                            session_id.clone(),
                            Op::Steer {
                                text,
                                images,
                                thinking,
                                web_search,
                                code_execution,
                                force_recall,
                            },
                        );
                        if let Some(bot_id) = bot_id {
                            if let Err(e) = hub.open_session_for_bot(sub.session_id.clone(), bot_id)
                            {
                                let _ = send_json(&outbox_tx, error_json(&e)).await;
                                continue;
                            }
                        } else if let Err(e) = hub.open_session(session_id) {
                            let _ = send_json(&outbox_tx, error_json(&e)).await;
                            continue;
                        }
                        ensure_session_forwarder(
                            &hub,
                            sub.session_id.clone(),
                            &outbox_tx,
                            &mut subscribed,
                        );
                        if let Err(e) = hub.submit(sub).await {
                            let _ = send_json(&outbox_tx, error_json(&e)).await;
                        }
                    }
                    ClientMsg::Approval {
                        session_id,
                        approval_id,
                        approved,
                        decision,
                        reason,
                    } => {
                        let session_id = session_id.unwrap_or_else(|| default_session_id.clone());
                        ensure_session_forwarder(
                            &hub,
                            session_id.clone(),
                            &outbox_tx,
                            &mut subscribed,
                        );
                        let sub = Submission::new(
                            session_id,
                            Op::Approval {
                                approval_id,
                                approved,
                                decision,
                                reason,
                            },
                        );
                        if let Err(e) = hub.submit(sub).await {
                            let _ = send_json(&outbox_tx, error_json(&e)).await;
                        }
                    }
                    ClientMsg::Cancel { session_id } => {
                        let session_id = session_id.unwrap_or_else(|| default_session_id.clone());
                        ensure_session_forwarder(
                            &hub,
                            session_id.clone(),
                            &outbox_tx,
                            &mut subscribed,
                        );
                        match hub.cancel_session(&session_id).await {
                            Ok(Some(_)) => {}
                            Ok(None) => {
                                let _ =
                                    send_json(&outbox_tx, error_json("session not found")).await;
                            }
                            Err(e) => {
                                let _ = send_json(&outbox_tx, error_json(&e)).await;
                            }
                        }
                    }
                }
            }
            Message::Ping(p) => {
                let _ = outbox_tx.send(Message::Pong(p)).await;
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    drop(outbox_tx);
    let _ = writer.await;
    tracing::info!(target: "botobot::ws", default_session_id = %default_session_id, "closed");
}

fn ensure_session_forwarder(
    hub: &Hub,
    session_id: SessionId,
    outbox: &mpsc::Sender<Message>,
    subscribed: &mut HashSet<SessionId>,
) {
    if subscribed.insert(session_id.clone()) {
        spawn_session_forwarder(hub.clone(), session_id, outbox.clone());
    }
}

fn spawn_session_forwarder(hub: Hub, session_id: SessionId, outbox: mpsc::Sender<Message>) {
    let Some(mut rx) = hub.subscribe(&session_id) else {
        return;
    };
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    if let Some(value) = event_to_client_json(&ev) {
                        if send_json(&outbox, value).await.is_err() {
                            break;
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    let msg = serde_json::json!({
                        "type": "error",
                        "session_id": session_id,
                        "message": format!("session event stream lagged, dropped {n} events")
                    });
                    if send_json(&outbox, msg).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

fn spawn_log_forwarder(outbox: mpsc::Sender<Message>) {
    let mut log_bcast_rx = logs::subscribe();
    tokio::spawn(async move {
        for ev in logs::snapshot() {
            if let Ok(s) = serde_json::to_string(&ev) {
                if outbox.send(Message::Text(s)).await.is_err() {
                    return;
                }
            }
        }
        let _ = outbox
            .send(Message::Text(r#"{"type":"log_snapshot_done"}"#.into()))
            .await;
        loop {
            match log_bcast_rx.recv().await {
                Ok(ev) => {
                    if let Ok(s) = serde_json::to_string(&ev) {
                        if outbox.send(Message::Text(s)).await.is_err() {
                            break;
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    let msg = format!(
                        r#"{{"type":"log","seq":0,"time":"","level":"warn","target":"botobot::logs","message":"log panel lagged, dropped {n} events"}}"#
                    );
                    if outbox.send(Message::Text(msg)).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

fn event_to_client_json(ev: &Event) -> Option<Value> {
    match &ev.msg {
        EventMsg::Agent(agent_event) => {
            trace_out(agent_event);
            let mut value = serde_json::to_value(agent_event).ok()?;
            if let Value::Object(ref mut map) = value {
                map.insert("level".into(), serde_json::json!(agent_event.level()));
                map.insert("op_id".into(), Value::String(ev.id.clone()));
            }
            Some(value)
        }
        EventMsg::TurnComplete => Some(serde_json::json!({
            "type": "turn_complete",
            "id": ev.id,
            "session_id": ev.session_id,
        })),
        EventMsg::CancelComplete => Some(serde_json::json!({
            "type": "cancel_complete",
            "id": ev.id,
            "session_id": ev.session_id,
        })),
        EventMsg::ShutdownComplete => Some(serde_json::json!({
            "type": "shutdown_complete",
            "id": ev.id,
            "session_id": ev.session_id,
        })),
        EventMsg::History { messages } => Some(serde_json::json!({
            "type": "history",
            "id": ev.id,
            "session_id": ev.session_id,
            "messages": messages,
        })),
        EventMsg::Error { message } => Some(serde_json::json!({
            "type": "error",
            "id": ev.id,
            "session_id": ev.session_id,
            "message": message,
        })),
    }
}

fn trace_out(ev: &base_types::AgentEvent) {
    use base_types::AgentEvent::{Debug, Error, Reasoning, Token, ToolEnd};
    let (k, r) = (ev.kind(), ev.run_id());
    match ev {
        Token { .. } | Reasoning { .. } | Debug { .. } => {
            tracing::debug!(target: "botobot::ws", dir = "→", kind = k, run = r, "")
        }
        Error { .. } => tracing::error!(target: "botobot::ws", dir = "→", kind = k, run = r, ""),
        ToolEnd { ok: false, .. } => {
            tracing::warn!(target: "botobot::ws", dir = "→", kind = k, run = r, "")
        }
        _ => tracing::info!(target: "botobot::ws", dir = "→", kind = k, run = r, ""),
    }
}

async fn send_text(
    outbox: &mpsc::Sender<Message>,
    text: &str,
) -> Result<(), mpsc::error::SendError<Message>> {
    outbox.send(Message::Text(text.to_string())).await
}

async fn send_json(
    outbox: &mpsc::Sender<Message>,
    value: Value,
) -> Result<(), mpsc::error::SendError<Message>> {
    outbox.send(Message::Text(value.to_string())).await
}

fn error_json(message: &str) -> Value {
    serde_json::json!({
        "type": "error",
        "message": message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base_types::AgentEvent;

    // §1.8.3b 钉死「召回默认开」：Submit/Steer 省略 force_recall → true；显式 false 仍可关。
    #[test]
    fn submit_and_steer_force_recall_default_on() {
        let submit: ClientMsg =
            serde_json::from_str(r#"{"type":"user_message","text":"hi"}"#).unwrap();
        match submit {
            ClientMsg::UserMessage { force_recall, .. } => {
                assert!(force_recall, "submit 省略应默认开")
            }
            _ => panic!("应解析为 UserMessage"),
        }
        let steer: ClientMsg =
            serde_json::from_str(r#"{"type":"steer","text":"hi","force_recall":false}"#).unwrap();
        match steer {
            ClientMsg::Steer { force_recall, .. } => {
                assert!(!force_recall, "steer 显式 false 仍可关")
            }
            _ => panic!("应解析为 Steer"),
        }
    }

    #[test]
    fn agent_event_json_keeps_legacy_shape_with_session_and_op() {
        let ev = Event {
            id: "op-1".into(),
            session_id: "s-1".into(),
            msg: EventMsg::Agent(AgentEvent::Token {
                session_id: "s-1".into(),
                run_id: "r-1".into(),
                text: "hi".into(),
            }),
        };

        let value = event_to_client_json(&ev).unwrap();

        assert_eq!(value["type"], "token");
        assert_eq!(value["session_id"], "s-1");
        assert_eq!(value["run_id"], "r-1");
        assert_eq!(value["text"], "hi");
        assert_eq!(value["op_id"], "op-1");
        assert_eq!(value["level"], "info");
    }

    #[test]
    fn hub_event_json_reports_turn_completion_for_session() {
        let ev = Event {
            id: "op-2".into(),
            session_id: "s-2".into(),
            msg: EventMsg::TurnComplete,
        };

        let value = event_to_client_json(&ev).unwrap();

        assert_eq!(value["type"], "turn_complete");
        assert_eq!(value["id"], "op-2");
        assert_eq!(value["session_id"], "s-2");
    }

    #[test]
    fn client_message_accepts_thread_turn_aliases() {
        let msg: ClientMsg =
            serde_json::from_str(r#"{"type":"turn_start","thread_id":"t-1","text":"hi"}"#).unwrap();
        assert!(matches!(
            msg,
            ClientMsg::UserMessage {
                session_id: Some(ref id),
                ..
            } if id == "t-1"
        ));

        let msg: ClientMsg = serde_json::from_str(
            r#"{"type":"approval_respond","thread_id":"t-1","approval_id":"a","approved":false}"#,
        )
        .unwrap();
        assert!(matches!(
            msg,
            ClientMsg::Approval {
                session_id: Some(ref id),
                approved: false,
                ..
            } if id == "t-1"
        ));
    }
}
