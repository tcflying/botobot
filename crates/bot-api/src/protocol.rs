//! Hub/transport 之间的线协议类型。

use base_types::{AgentEvent, ApprovalDecision, ApprovalResponse, ContentPart, LlmOpts, Message};
use serde::{Deserialize, Serialize};

pub type SessionId = String;
pub type OpId = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Submission {
    pub id: OpId,
    pub session_id: SessionId,
    pub op: Op,
}

impl Submission {
    pub fn new(session_id: impl Into<SessionId>, op: Op) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: session_id.into(),
            op,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Op {
    #[serde(alias = "turn_start")]
    UserMessage {
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
        /// §1.8.8 composer 开关：harness 强制按本轮 query 检索记忆并增广 user 消息。
        #[serde(default)]
        force_recall: bool,
    },
    #[serde(alias = "turn_steer")]
    Steer {
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
        #[serde(default)]
        force_recall: bool,
    },
    #[serde(alias = "approval_respond")]
    Approval {
        approval_id: String,
        approved: bool,
        /// §2.11 四档（once/session/always/deny）。新客户端发此字段；缺省时回退 `approved` 布尔
        /// （true→once、false→deny），兼容旧客户端。
        #[serde(default)]
        decision: Option<ApprovalDecision>,
        #[serde(default)]
        reason: Option<String>,
    },
    #[serde(alias = "turn_interrupt")]
    Cancel,
    #[serde(alias = "thread_shutdown")]
    Shutdown,
}

impl Op {
    pub fn parts(&self) -> Vec<ContentPart> {
        match self {
            Op::UserMessage { text, images, .. } | Op::Steer { text, images, .. } => {
                parts_of(text, images)
            }
            Op::Approval { .. } | Op::Cancel | Op::Shutdown => Vec::new(),
        }
    }

    pub fn llm_opts(&self) -> LlmOpts {
        match self {
            Op::UserMessage {
                thinking,
                web_search,
                code_execution,
                force_recall,
                ..
            }
            | Op::Steer {
                thinking,
                web_search,
                code_execution,
                force_recall,
                ..
            } => LlmOpts {
                thinking: *thinking,
                web_search: *web_search,
                code_execution: *code_execution,
                force_recall: *force_recall,
            },
            Op::Approval { .. } | Op::Cancel | Op::Shutdown => LlmOpts::default(),
        }
    }

    pub fn approval_response(&self) -> Option<ApprovalResponse> {
        let Op::Approval {
            approval_id,
            approved,
            decision,
            reason,
        } = self
        else {
            return None;
        };
        // 优先用显式四档；缺省回退布尔（true→Once、false→Deny），兼容旧客户端。
        let decision = decision.unwrap_or(if *approved {
            ApprovalDecision::Once
        } else {
            ApprovalDecision::Deny
        });
        Some(ApprovalResponse {
            approval_id: approval_id.clone(),
            decision,
            reason: reason.clone(),
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub id: OpId,
    pub session_id: SessionId,
    pub msg: EventMsg,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum EventMsg {
    Agent(AgentEvent),
    TurnComplete,
    CancelComplete,
    ShutdownComplete,
    History { messages: Vec<Message> },
    Error { message: String },
}

fn parts_of(text: &str, images: &[String]) -> Vec<ContentPart> {
    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(ContentPart::Text(text.to_string()));
    }
    for image in images {
        parts.push(ContentPart::ImageUrl(image.clone()));
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submission_json_shape_roundtrips() {
        let json = r#"{
            "id":"op-1",
            "session_id":"s-1",
            "op":{"type":"user_message","text":"hi","images":[],"thinking":true,"web_search":true}
        }"#;
        let sub: Submission = serde_json::from_str(json).unwrap();
        assert_eq!(sub.id, "op-1");
        assert_eq!(sub.session_id, "s-1");
        assert_eq!(sub.op.parts().len(), 1);
        assert_eq!(sub.op.llm_opts().thinking, Some(true));
        assert_eq!(sub.op.llm_opts().web_search, Some(true));
        assert_eq!(sub.op.llm_opts().code_execution, None);
        let encoded = serde_json::to_value(&sub).unwrap();
        assert_eq!(encoded["op"]["type"], "user_message");
    }

    #[test]
    fn protocol_aliases_accept_thread_turn_names() {
        let sub: Submission = serde_json::from_str(
            r#"{
                "id":"op-2",
                "session_id":"thread-1",
                "op":{"type":"turn_start","text":"hi"}
            }"#,
        )
        .unwrap();
        assert!(matches!(sub.op, Op::UserMessage { .. }));

        let sub: Submission = serde_json::from_str(
            r#"{
                "id":"op-3",
                "session_id":"thread-1",
                "op":{"type":"approval_respond","approval_id":"a","approved":true}
            }"#,
        )
        .unwrap();
        assert!(matches!(sub.op, Op::Approval { .. }));
    }

    #[test]
    fn approval_response_prefers_decision_else_falls_back_to_bool() {
        use base_types::ApprovalDecision;
        let parse = |json: &str| -> ApprovalDecision {
            serde_json::from_str::<Submission>(json)
                .unwrap()
                .op
                .approval_response()
                .unwrap()
                .decision
        };
        // §2.11 显式四档优先。
        assert_eq!(
            parse(
                r#"{"id":"o","session_id":"s","op":{"type":"approval_respond","approval_id":"a","approved":true,"decision":"session"}}"#
            ),
            ApprovalDecision::Session
        );
        assert_eq!(
            parse(
                r#"{"id":"o","session_id":"s","op":{"type":"approval_respond","approval_id":"a","approved":true,"decision":"always"}}"#
            ),
            ApprovalDecision::Always
        );
        // 旧客户端无 decision：true→Once、false→Deny（向后兼容）。
        assert_eq!(
            parse(
                r#"{"id":"o","session_id":"s","op":{"type":"approval_respond","approval_id":"a","approved":true}}"#
            ),
            ApprovalDecision::Once
        );
        assert_eq!(
            parse(
                r#"{"id":"o","session_id":"s","op":{"type":"approval_respond","approval_id":"a","approved":false}}"#
            ),
            ApprovalDecision::Deny
        );
    }

    #[test]
    fn event_json_contains_session_and_msg_type() {
        let ev = Event {
            id: "op-1".into(),
            session_id: "s-1".into(),
            msg: EventMsg::TurnComplete,
        };
        let v = serde_json::to_value(ev).unwrap();
        assert_eq!(v["id"], "op-1");
        assert_eq!(v["session_id"], "s-1");
        assert_eq!(v["msg"]["type"], "turn_complete");
    }
}
