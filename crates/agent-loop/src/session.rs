//! Session: one live conversation for an agent.
//!
//! A session owns its conversation history and runs turns serially. During a
//! running turn, callers may steer the next reasoning step or cancel the turn.

use std::sync::Arc;

use base_types::{AgentEvent, ApprovalResponse, ContentPart, Message};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::Agent;
use crate::agent::new_id;

pub struct Session {
    pub id: String,
    agent: Arc<Agent>,
    history: Vec<Message>,
    steer_tx: Option<mpsc::UnboundedSender<Vec<ContentPart>>>,
    approval_tx: Option<mpsc::UnboundedSender<ApprovalResponse>>,
    cancel: Option<CancellationToken>,
}

impl Session {
    pub fn new(agent: Arc<Agent>) -> Self {
        Self {
            id: new_id(),
            agent,
            history: Vec::new(),
            steer_tx: None,
            approval_tx: None,
            cancel: None,
        }
    }

    pub fn with_id(id: impl Into<String>, agent: Arc<Agent>) -> Self {
        Self {
            id: id.into(),
            agent,
            history: Vec::new(),
            steer_tx: None,
            approval_tx: None,
            cancel: None,
        }
    }

    pub fn with_id_and_history(
        id: impl Into<String>,
        agent: Arc<Agent>,
        history: Vec<Message>,
    ) -> Self {
        Self {
            id: id.into(),
            agent,
            history,
            steer_tx: None,
            approval_tx: None,
            cancel: None,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn history(&self) -> &[Message] {
        &self.history
    }

    pub fn turn(
        &mut self,
        parts: Vec<ContentPart>,
        llm_opts: base_types::LlmOpts,
    ) -> (
        UnboundedReceiverStream<AgentEvent>,
        oneshot::Receiver<Vec<Message>>,
        // §2.6 缺陷3 阶0：本轮 finalized message 增量流（驱动器写 turn-scratch 供崩溃恢复）。
        mpsc::UnboundedReceiver<Message>,
    ) {
        let (stream, hrx, steer_tx, approval_tx, cancel, delta_rx) = self
            .agent
            .clone()
            .run_session_steerable(self.id.clone(), self.history.clone(), parts, llm_opts);
        self.steer_tx = Some(steer_tx);
        self.approval_tx = Some(approval_tx);
        self.cancel = Some(cancel);
        (stream, hrx, delta_rx)
    }

    pub fn steer(&self, parts: Vec<ContentPart>) -> bool {
        self.steer_tx
            .as_ref()
            .map(|tx| tx.send(parts).is_ok())
            .unwrap_or(false)
    }

    pub fn approve(&self, response: ApprovalResponse) -> bool {
        self.approval_tx
            .as_ref()
            .map(|tx| tx.send(response).is_ok())
            .unwrap_or(false)
    }

    pub fn cancel(&self) -> bool {
        let Some(cancel) = &self.cancel else {
            return false;
        };
        cancel.cancel();
        true
    }

    pub fn commit(&mut self, history: Vec<Message>) {
        self.history = history;
        self.steer_tx = None;
        self.approval_tx = None;
        self.cancel = None;
    }

    pub fn discard_turn(&mut self) {
        self.steer_tx = None;
        self.approval_tx = None;
        self.cancel = None;
    }
}
