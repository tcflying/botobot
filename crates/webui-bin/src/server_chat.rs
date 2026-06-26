//! §1.6 助手半边：server.exe 的**无状态**对话端点（远端只读助手站的「助手」帽子）。
//!
//! 设计（§1.6）：远端无 server 侧 session——浏览器每次携带全量历史，server 用 scratch 临时
//! 上下文跑**一轮** turn，SSE 流式回吐。无记忆 / 无 write·exec·file 工具（只读助手）。
//!
//! 门控在 `chat` feature：`server` bin 经 `--no-default-features --features chat` 即带对话、
//! 仍不含 candle/team/mcp/browser。`build_chat_agent` 从 config 端点装一个轻 agent（仅 LLM）。

use std::convert::Infallible;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::response::sse::{Event, Sse};
use axum::routing::post;
use futures::StreamExt;
use serde::Deserialize;

use agent_loop::{Agent, AgentEvent};
use base_types::{ContentPart, LlmOpts, Message};

/// 无状态对话请求：`history`=浏览器侧（IndexedDB）携带的既往消息；`text`=本轮新用户输入。
#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    #[serde(default)]
    pub history: Vec<Message>,
    pub text: String,
}

/// 从 config 端点装一个**轻量只读对话 agent**（仅 LLM，无工具/记忆）。
pub fn build_chat_agent() -> Arc<Agent> {
    let ep = crate::config::Endpoint::resolve();
    let mut provider = agent_infer::OpenAiCompat::new(ep.base_url, ep.api_key, ep.model)
        .with_temperature(ep.temperature);
    if let Some(b) = ep.thinking {
        provider = provider.with_thinking(b);
    }
    Agent::builder()
        .llm(Arc::new(provider))
        .system(
            "You are botobot's remote read-only assistant. You have no write, exec, or file \
             tools — answer from your own knowledge and the conversation so far. Be concise.",
        )
        .build()
}

/// `POST /api/chat`：无状态一轮对话，SSE 流式回吐。事件：默认事件载 token 文本；
/// `event: done` 收尾；`event: error` 报错。
pub fn chat_router(agent: Arc<Agent>) -> Router {
    Router::new().route(
        "/api/chat",
        post(move |Json(req): Json<ChatRequest>| {
            let agent = agent.clone();
            async move { chat_sse(agent, req) }
        }),
    )
}

fn chat_sse(
    agent: Arc<Agent>,
    req: ChatRequest,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    // run_session：seed = history + 本轮 user 消息（parts），跑一轮，事件流式返回。
    let (stream, _hrx) = agent.run_session(
        "remote-chat",
        req.history,
        vec![ContentPart::Text(req.text)],
        LlmOpts::default(),
    );
    let sse = stream.filter_map(|ev| async move {
        match ev {
            AgentEvent::Token { text, .. } => Some(Ok(Event::default().data(text))),
            AgentEvent::Done { .. } => Some(Ok(Event::default().event("done").data(""))),
            AgentEvent::Error { message, .. } => {
                Some(Ok(Event::default().event("error").data(message)))
            }
            // 远端只读助手无工具/审批/推理流外显——其余事件不下发。
            _ => None,
        }
    });
    Sse::new(sse)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use base_types::{Decision, Llm, LlmError, LlmEvent, LlmStream, ToolSpec};

    struct OneShotLlm;
    #[async_trait]
    impl Llm for OneShotLlm {
        async fn infer(
            &self,
            _m: &[Message],
            _t: &[ToolSpec],
            _o: &LlmOpts,
        ) -> Result<LlmStream, LlmError> {
            let d = Decision {
                text: "hi from remote".into(),
                finish_reason: Some("stop".into()),
                ..Default::default()
            };
            let evs = vec![
                Ok(LlmEvent::TextDelta(d.text.clone())),
                Ok(LlmEvent::Done(d)),
            ];
            Ok(Box::pin(futures::stream::iter(evs)))
        }
    }

    #[tokio::test]
    async fn chat_router_streams_tokens_then_done() {
        let agent = Agent::builder().llm(Arc::new(OneShotLlm)).build();
        // 直接验证底层 run_session → SSE 映射的事件序：Token(s) 后 Done。
        let (stream, _hrx) = agent.run_session(
            "t",
            Vec::new(),
            vec![ContentPart::Text("hello".into())],
            LlmOpts::default(),
        );
        let kinds: Vec<String> = stream
            .filter_map(|ev| async move {
                match ev {
                    AgentEvent::Token { text, .. } => Some(format!("token:{text}")),
                    AgentEvent::Done { .. } => Some("done".into()),
                    _ => None,
                }
            })
            .collect()
            .await;
        assert!(
            kinds.iter().any(|k| k == "token:hi from remote"),
            "应有 token, got {kinds:?}"
        );
        assert_eq!(kinds.last().unwrap(), "done", "末事件应为 done");
    }

    // chat_router 构造（编译期保证签名/类型正确）。
    #[test]
    fn chat_router_builds() {
        let agent = Agent::builder().llm(Arc::new(OneShotLlm)).build();
        let _r = chat_router(agent);
    }
}
