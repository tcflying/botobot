//! Per-session actor：串行驱动一个 [`agent_loop::Session`]。

use std::sync::Arc;
use std::sync::Mutex;

use agent_loop::{Agent, Session};
use futures::StreamExt;
use tokio::sync::{broadcast, mpsc};

use crate::protocol::{Event, EventMsg, Op, SessionId, Submission};
use crate::session_store::SessionStore;

pub type EventLog = Arc<Mutex<Vec<Event>>>;

#[derive(Clone)]
pub struct SessionHandle {
    tx_sub: mpsc::Sender<Submission>,
}

impl SessionHandle {
    pub async fn submit(&self, sub: Submission) -> Result<(), mpsc::error::SendError<Submission>> {
        self.tx_sub.send(sub).await
    }

    pub fn try_submit(&self, sub: Submission) -> Result<(), mpsc::error::TrySendError<Submission>> {
        self.tx_sub.try_send(sub)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_session_driver(
    session_id: SessionId,
    agent: Arc<Agent>,
    event_tx: broadcast::Sender<Event>,
    event_log: EventLog,
    event_log_capacity: usize,
    history: Vec<base_types::Message>,
    store: Option<SessionStore>,
    bot_id: String,
) -> SessionHandle {
    let (tx_sub, rx_sub) = mpsc::channel(512);
    // 初始 history 已落盘（来自 load_messages），后续只追加本 turn 新增尾部。
    let persisted_len = history.len();
    let session = Session::with_id_and_history(session_id, agent, history);
    tokio::spawn(run_session_driver(
        session,
        rx_sub,
        event_tx,
        event_log,
        event_log_capacity,
        store,
        persisted_len,
        bot_id,
    ));
    SessionHandle { tx_sub }
}

#[allow(clippy::too_many_arguments)]
async fn run_session_driver(
    mut session: Session,
    mut rx_sub: mpsc::Receiver<Submission>,
    event_tx: broadcast::Sender<Event>,
    event_log: EventLog,
    event_log_capacity: usize,
    store: Option<SessionStore>,
    mut persisted_len: usize,
    bot_id: String,
) {
    while let Some(sub) = rx_sub.recv().await {
        match sub.op {
            Op::UserMessage { .. } => {
                run_turn(
                    &mut session,
                    sub,
                    &mut rx_sub,
                    &event_tx,
                    &event_log,
                    event_log_capacity,
                    store.as_ref(),
                    &mut persisted_len,
                    &bot_id,
                )
                .await;
            }
            Op::Steer { .. } => {
                if !session.steer(sub.op.parts()) {
                    publish(
                        &event_tx,
                        &event_log,
                        event_log_capacity,
                        &sub.id,
                        session.id(),
                        EventMsg::Error {
                            message: "no active turn to steer".into(),
                        },
                    );
                }
            }
            Op::Approval { .. } => {
                publish(
                    &event_tx,
                    &event_log,
                    event_log_capacity,
                    &sub.id,
                    session.id(),
                    EventMsg::Error {
                        message: "no active turn to approve".into(),
                    },
                );
            }
            Op::Cancel => {
                publish(
                    &event_tx,
                    &event_log,
                    event_log_capacity,
                    &sub.id,
                    session.id(),
                    EventMsg::Error {
                        message: "no active turn to cancel".into(),
                    },
                );
            }
            Op::Shutdown => {
                publish(
                    &event_tx,
                    &event_log,
                    event_log_capacity,
                    &sub.id,
                    session.id(),
                    EventMsg::ShutdownComplete,
                );
                break;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_turn(
    session: &mut Session,
    sub: Submission,
    rx_sub: &mut mpsc::Receiver<Submission>,
    event_tx: &broadcast::Sender<Event>,
    event_log: &EventLog,
    event_log_capacity: usize,
    store: Option<&SessionStore>,
    persisted_len: &mut usize,
    bot_id: &str,
) {
    let parts = sub.op.parts();
    if parts.is_empty() {
        publish(
            event_tx,
            event_log,
            event_log_capacity,
            &sub.id,
            session.id(),
            EventMsg::Error {
                message: "empty user message".into(),
            },
        );
        return;
    }

    let opts = sub.op.llm_opts();
    let op_id = sub.id.clone();
    let (mut events, hrx, delta_rx) = session.turn(parts, opts);
    // §2.6 缺陷3 阶0：本轮 finalized message 增量写 turn-scratch（崩溃恢复 rollout）。
    // 通道关闭（run_loop 结束）后置 None 停止轮询，避免 closed-recv 忙转。
    let mut delta_rx = Some(delta_rx);
    let mut interrupted = false;

    loop {
        tokio::select! {
            ev = events.next() => match ev {
                Some(ev) => {
                    publish(
                        event_tx,
                        event_log,
                        event_log_capacity,
                        &op_id,
                        session.id(),
                        EventMsg::Agent(ev),
                    );
                }
                None => break,
            },
            incoming = rx_sub.recv() => match incoming {
                Some(next) => match next.op {
                    Op::Approval { .. } => {
                        let response = next
                            .op
                            .approval_response()
                            .expect("approval op should convert");
                        if !session.approve(response) {
                            publish(
                                event_tx,
                                event_log,
                                event_log_capacity,
                                &next.id,
                                session.id(),
                                EventMsg::Error {
                                    message: "no active approval request".into(),
                                },
                            );
                        }
                    }
                    Op::Steer { .. } | Op::UserMessage { .. } => {
                        let parts = next.op.parts();
                        if !parts.is_empty() {
                            session.steer(parts);
                        }
                    }
                    Op::Cancel => {
                        if session.cancel() {
                            publish(
                                event_tx,
                                event_log,
                                event_log_capacity,
                                &next.id,
                                session.id(),
                                EventMsg::CancelComplete,
                            );
                        } else {
                            publish(
                                event_tx,
                                event_log,
                                event_log_capacity,
                                &next.id,
                                session.id(),
                                EventMsg::Error {
                                    message: "no active turn to cancel".into(),
                                },
                            );
                        }
                        interrupted = true;
                        break;
                    }
                    Op::Shutdown => {
                        session.cancel();
                        publish(
                            event_tx,
                            event_log,
                            event_log_capacity,
                            &next.id,
                            session.id(),
                            EventMsg::ShutdownComplete,
                        );
                        interrupted = true;
                        break;
                    }
                },
                None => break,
            },
            // §2.6 缺陷3 阶0：finalized message 增量落 turn-scratch。通道关闭后置 None 停止轮询。
            dm = async { delta_rx.as_mut().unwrap().recv().await }, if delta_rx.is_some() => {
                match dm {
                    Some(m) => {
                        if let Some(store) = store {
                            if let Err(err) = store.append_scratch(session.id(), &m) {
                                tracing::warn!("append turn-scratch failed: {err}");
                            }
                        }
                    }
                    None => delta_rx = None,
                }
            },
        }
    }

    if interrupted {
        let _ = hrx.await;
        session.discard_turn();
        // 取消/关停：本轮被丢弃，scratch 不应在下次启动被恢复 → 清空。
        if let Some(store) = store {
            let _ = store.clear_scratch(session.id());
        }
        return;
    }

    if let Ok(history) = hrx.await {
        session.commit(history);
        if let Some(store) = store {
            // 只追加本 turn 新增的尾部消息（增量 append-only），再更新 meta。
            let full = session.history();
            let mut committed = true;
            if *persisted_len < full.len() {
                let tail = &full[*persisted_len..];
                if let Err(err) = store.append_messages(session.id(), tail) {
                    committed = false;
                    publish(
                        event_tx,
                        event_log,
                        event_log_capacity,
                        &op_id,
                        session.id(),
                        EventMsg::Error { message: err },
                    );
                } else {
                    *persisted_len = full.len();
                }
            }
            // 懒持久化：首条消息提交时才 upsert meta（空会话不落盘，避免空壳累积）。
            if let Err(err) = store.upsert_meta_after_turn(session.id(), bot_id, full.len()) {
                tracing::warn!("upsert session meta failed: {err}");
            }
            // §2.6 缺陷3 阶0：messages.jsonl 已收下本轮尾部 → 清 scratch（无需崩溃恢复）。
            // append 失败则保留 scratch，下次启动 recover_scratch 补救。
            if committed {
                if let Err(err) = store.clear_scratch(session.id()) {
                    tracing::warn!("clear turn-scratch failed: {err}");
                }
            }
        }
        publish(
            event_tx,
            event_log,
            event_log_capacity,
            &op_id,
            session.id(),
            EventMsg::TurnComplete,
        );
    }
}

fn publish(
    event_tx: &broadcast::Sender<Event>,
    event_log: &EventLog,
    event_log_capacity: usize,
    op_id: &str,
    session_id: &str,
    msg: EventMsg,
) {
    let ev = Event {
        id: op_id.to_string(),
        session_id: session_id.to_string(),
        msg,
    };
    if let Ok(mut log) = event_log.lock() {
        if event_log_capacity > 0 {
            log.push(ev.clone());
        }
        if log.len() > event_log_capacity {
            let excess = log.len() - event_log_capacity;
            log.drain(0..excess);
        }
    }
    let _ = event_tx.send(ev);
}
