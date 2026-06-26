//! In-process Rust SDK for driving botobot sessions through `bot-api::Hub`.
//!
//! `bot-api` owns the transport-facing Hub and protocol types. This crate keeps
//! that boundary intact and adds a small ergonomic facade for Rust binaries that
//! want to open a session, submit a turn, and collect the events for that turn.

use std::sync::Arc;

use agent_loop::Agent;
use base_types::{ContentPart, LlmOpts};
use bot_api::Hub;
use bot_api::protocol::{Event, EventMsg, Op, OpId, SessionId, Submission};
use tokio::sync::broadcast;

pub use bot_api::protocol;

#[derive(Debug, thiserror::Error)]
pub enum SdkError {
    #[error("hub rejected operation: {0}")]
    Hub(String),
    #[error("session not found: {0}")]
    SessionNotFound(SessionId),
    #[error("event stream lagged; dropped {0} events")]
    Lagged(u64),
    #[error("event stream closed before turn completed")]
    Closed,
}

#[derive(Debug, Clone)]
pub struct TurnOutput {
    pub session_id: SessionId,
    pub op_id: OpId,
    pub events: Vec<Event>,
}

#[derive(Clone)]
pub struct BotSdk {
    hub: Hub,
}

impl BotSdk {
    pub fn new(agent: Arc<Agent>) -> Self {
        Self {
            hub: Hub::new(agent),
        }
    }

    pub fn from_hub(hub: Hub) -> Self {
        Self { hub }
    }

    pub fn hub(&self) -> &Hub {
        &self.hub
    }

    pub fn open_session(&self, session_id: impl Into<SessionId>) -> Result<SessionId, SdkError> {
        self.hub.open_session(session_id).map_err(SdkError::Hub)
    }

    pub fn subscribe(
        &self,
        session_id: impl AsRef<str>,
    ) -> Result<broadcast::Receiver<Event>, SdkError> {
        let id = session_id.as_ref();
        self.hub
            .subscribe(id)
            .ok_or_else(|| SdkError::SessionNotFound(id.to_string()))
    }

    pub async fn submit(&self, submission: Submission) -> Result<(), SdkError> {
        self.hub.submit(submission).await.map_err(SdkError::Hub)
    }

    pub async fn user_message(
        &self,
        session_id: impl Into<SessionId>,
        text: impl Into<String>,
    ) -> Result<OpId, SdkError> {
        let sub = Submission::new(
            session_id.into(),
            Op::UserMessage {
                text: text.into(),
                images: Vec::new(),
                thinking: None,
                web_search: None,
                code_execution: None,
                force_recall: false,
            },
        );
        let op_id = sub.id.clone();
        self.submit(sub).await?;
        Ok(op_id)
    }

    pub async fn run_user_message(
        &self,
        session_id: impl Into<SessionId>,
        text: impl Into<String>,
    ) -> Result<TurnOutput, SdkError> {
        self.run_parts(session_id, vec![ContentPart::Text(text.into())], None)
            .await
    }

    pub async fn run_parts(
        &self,
        session_id: impl Into<SessionId>,
        parts: Vec<ContentPart>,
        thinking: Option<bool>,
    ) -> Result<TurnOutput, SdkError> {
        self.run_parts_with_opts(
            session_id,
            parts,
            LlmOpts {
                thinking,
                ..LlmOpts::default()
            },
        )
        .await
    }

    pub async fn run_parts_with_opts(
        &self,
        session_id: impl Into<SessionId>,
        parts: Vec<ContentPart>,
        opts: LlmOpts,
    ) -> Result<TurnOutput, SdkError> {
        let session_id = self.open_session(session_id)?;
        let mut rx = self.subscribe(&session_id)?;
        let sub = Submission::new(
            session_id.clone(),
            user_message_op_from_parts_with_opts(parts, opts),
        );
        let op_id = sub.id.clone();
        self.submit(sub).await?;

        let mut events = Vec::new();
        loop {
            match rx.recv().await {
                Ok(ev) if ev.id != op_id => continue,
                Ok(ev) => {
                    let done = matches!(
                        ev.msg,
                        EventMsg::TurnComplete
                            | EventMsg::CancelComplete
                            | EventMsg::ShutdownComplete
                            | EventMsg::Error { .. }
                    );
                    events.push(ev);
                    if done {
                        return Ok(TurnOutput {
                            session_id,
                            op_id,
                            events,
                        });
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    return Err(SdkError::Lagged(n));
                }
                Err(broadcast::error::RecvError::Closed) => return Err(SdkError::Closed),
            }
        }
    }
}

pub fn user_message_op_from_parts(parts: Vec<ContentPart>, thinking: Option<bool>) -> Op {
    user_message_op_from_parts_with_opts(
        parts,
        LlmOpts {
            thinking,
            ..LlmOpts::default()
        },
    )
}

pub fn user_message_op_from_parts_with_opts(parts: Vec<ContentPart>, opts: LlmOpts) -> Op {
    let mut text = Vec::new();
    let mut images = Vec::new();
    for part in parts {
        match part {
            ContentPart::Text(t) => text.push(t),
            ContentPart::ImageUrl(url) => images.push(url),
        }
    }
    Op::UserMessage {
        text: text.join("\n"),
        images,
        thinking: opts.thinking,
        web_search: opts.web_search,
        code_execution: opts.code_execution,
        force_recall: opts.force_recall,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use base_types::{AgentEvent, Decision, Llm, LlmError, LlmEvent, LlmOpts, Message, ToolSpec};

    struct OneShotLlm;

    #[async_trait]
    impl Llm for OneShotLlm {
        async fn infer(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _opts: &LlmOpts,
        ) -> Result<base_types::LlmStream, LlmError> {
            let decision = Decision {
                text: "sdk ok".into(),
                finish_reason: Some("stop".into()),
                ..Default::default()
            };
            let events = vec![
                Ok(LlmEvent::TextDelta(decision.text.clone())),
                Ok(LlmEvent::Done(decision)),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[test]
    fn parts_map_to_user_message_op() {
        let op = user_message_op_from_parts(
            vec![
                ContentPart::Text("hello".into()),
                ContentPart::ImageUrl("data:image/png;base64,abc".into()),
                ContentPart::Text("world".into()),
            ],
            Some(true),
        );

        match op {
            Op::UserMessage {
                text,
                images,
                thinking,
                web_search,
                code_execution,
                force_recall: _,
            } => {
                assert_eq!(text, "hello\nworld");
                assert_eq!(images, vec!["data:image/png;base64,abc".to_string()]);
                assert_eq!(thinking, Some(true));
                assert_eq!(web_search, None);
                assert_eq!(code_execution, None);
            }
            _ => panic!("SDK should build user_message ops"),
        }
    }

    #[test]
    fn parts_map_to_user_message_op_with_opts() {
        let op = user_message_op_from_parts_with_opts(
            vec![ContentPart::Text("hello".into())],
            LlmOpts {
                thinking: Some(true),
                web_search: Some(true),
                code_execution: Some(false),
                ..Default::default()
            },
        );

        match op {
            Op::UserMessage {
                text,
                images,
                thinking,
                web_search,
                code_execution,
                force_recall: _,
            } => {
                assert_eq!(text, "hello");
                assert!(images.is_empty());
                assert_eq!(thinking, Some(true));
                assert_eq!(web_search, Some(true));
                assert_eq!(code_execution, Some(false));
            }
            _ => panic!("SDK should build user_message ops"),
        }
    }

    #[tokio::test]
    async fn run_user_message_collects_turn_events() {
        let agent = Agent::builder().llm(Arc::new(OneShotLlm)).build();
        let sdk = BotSdk::new(agent);

        let out = sdk.run_user_message("s-sdk", "hello").await.unwrap();

        assert_eq!(out.session_id, "s-sdk");
        assert!(
            out.events
                .iter()
                .any(|ev| matches!(ev.msg, EventMsg::TurnComplete))
        );
        assert!(out.events.iter().any(|ev| matches!(
            &ev.msg,
            EventMsg::Agent(AgentEvent::Token { text, .. }) if text == "sdk ok"
        )));
    }

    #[test]
    fn subscribe_missing_session_is_clear() {
        let agent = Agent::builder().llm(Arc::new(OneShotLlm)).build();
        let sdk = BotSdk::new(agent);

        let err = sdk.subscribe("missing").unwrap_err();
        assert!(matches!(err, SdkError::SessionNotFound(id) if id == "missing"));
    }
}
