//! infer-tech：推理切入点 [`base_types::Llm`] 的实现层（"大脑"）。
//!
//! - Core = [`OpenAiCompat`] provider（OpenAI 兼容，含 vLLM/unsloth/llama.cpp）。
//! - 辅助 = [`sse`]（SSE wire format）、[`accumulator`]（tool-call 增量累积 + 决策组装）、
//!   [`reasoning`]（`<think>` 标签跨分片分流）。
//! - 装配入口 = `OpenAiCompat::new` / `from_env`。
//!
//! 词汇/契约（Message、Decision、Llm trait…）都在 [`base_types`]，本 crate re-export 之。

use async_trait::async_trait;
use futures::StreamExt;
use serde::Serialize;

pub use base_types::*;

mod accumulator;
mod reasoning;
mod retry;
mod sse;

pub use retry::RetryLlm;

use accumulator::Accumulator;

// ───────────────────────────── OpenAI 兼容 provider ─────────────────────────────

/// 任何兼容 OpenAI Chat Completions API 的后端。
pub struct OpenAiCompat {
    base_url: String,
    api_key: String,
    model: String,
    temperature: Option<f32>,
    /// None=服务端默认；Some(b) 经 `chat_template_kwargs.enable_thinking` 控制（Qwen/vLLM）。
    thinking: Option<bool>,
    max_tokens: Option<u32>,
    response_format: Option<serde_json::Value>,
    /// SSE 停滞超时（§2.6 缺陷2）：`Some(dur)` 时包裹流，单次等事件超 dur 即 `LlmError::Idle`。
    /// 由 `BOTOBOT_STREAM_IDLE`（秒，默认 60，`0`=关）解析。
    idle_timeout: Option<std::time::Duration>,
    http: reqwest::Client,
}

/// 从 `BOTOBOT_STREAM_IDLE`（秒，默认 60，`0`=关）解析 SSE 停滞超时。
fn idle_timeout_from_env() -> Option<std::time::Duration> {
    let secs = std::env::var("BOTOBOT_STREAM_IDLE")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(60);
    (secs > 0).then(|| std::time::Duration::from_secs(secs))
}

impl OpenAiCompat {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            temperature: None,
            thinking: None,
            max_tokens: None,
            response_format: None,
            idle_timeout: idle_timeout_from_env(),
            http: reqwest::Client::new(),
        }
    }

    /// 从环境变量构造：`OPENAI_API_KEY` + 可选 `OPENAI_BASE_URL`。
    pub fn from_env(model: impl Into<String>) -> LlmResult<Self> {
        let api_key = std::env::var("OPENAI_API_KEY").map_err(|_| LlmError::MissingApiKey)?;
        let base_url = std::env::var("OPENAI_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
        Ok(Self::new(base_url, api_key, model))
    }

    pub fn with_temperature(mut self, t: f32) -> Self {
        self.temperature = Some(t);
        self
    }

    /// 控制 thinking：`false` 让 Qwen 不再输出 `<think>` 段。经 `chat_template_kwargs.enable_thinking`。
    pub fn with_thinking(mut self, enable: bool) -> Self {
        self.thinking = Some(enable);
        self
    }

    /// 限制单次 completion 输出的最大 token 数。`None` = 不传(走服务端默认)。
    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = Some(n);
        self
    }

    /// 强制 JSON 输出(经 `response_format: { type: "json_object" }`)。本任务暂未启用,
    /// 留个 builder 以备 JSON mode 工具需要。
    pub fn with_response_format(mut self) -> Self {
        self.response_format = Some(serde_json::json!({ "type": "json_object" }));
        self
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolSpec>,
    stream: bool,
    /// SSE 增量返回 token 用量(本地 vLLM/Qwen 默认 false,开 debug 时看上下文增长很有用)。
    /// 也照你演示的请求里 `stream_options.include_usage = true` 设上去。
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    /// 强制 JSON 输出(本地 Qwen 走 json_object 兼容模式)。`None` = 不传。
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_template_kwargs: Option<serde_json::Value>,
}

#[async_trait]
impl Llm for OpenAiCompat {
    async fn infer(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        opts: &LlmOpts,
    ) -> LlmResult<LlmStream> {
        // per-call opts.thinking 优先于 builder 的 self.thinking。
        let thinking = opts.thinking.or(self.thinking);
        let url = format!("{}/chat/completions", self.base_url);
        let body = ChatRequest {
            model: &self.model,
            messages,
            tools: tools.to_vec(),
            stream: true,
            // 与你演示的请求一致:让 SSE 末尾带 usage 块,便于 trace token 增长
            //(本地 vLLM/Qwen 都支持)。None 也 OK,服务端默认不带。
            stream_options: Some(serde_json::json!({ "include_usage": true })),
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            response_format: self.response_format.clone(),
            chat_template_kwargs: thinking.map(|b| serde_json::json!({ "enable_thinking": b })),
        };

        tracing::debug!(target: "botobot::llm", model = %self.model, msgs = messages.len(), tools = tools.len(), "request");
        // 完整请求 payload 只进 debug 日志（不下发前端）；字段仅在 debug 开启时才会被序列化。
        tracing::debug!(target: "botobot::llm", payload = %serde_json::to_string(&body).unwrap_or_default(), "request payload");
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                tracing::warn!(target: "botobot::llm", error = %e, "request failed (network)");
                LlmError::Http(e.to_string())
            })?;

        let status = resp.status();
        if !status.is_success() {
            // Retry-After（§2.6 缺陷2）：先于消费 body 读 header（仅支持秒数形式）。
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok())
                .map(std::time::Duration::from_secs);
            let body = resp.text().await.unwrap_or_default();
            tracing::warn!(target: "botobot::llm", status = status.as_u16(), ?retry_after, "api error");
            return Err(LlmError::Api {
                status: status.as_u16(),
                body,
                retry_after,
            });
        }

        let mut bytes = resp.bytes_stream();
        let stream = async_stream::try_stream! {
            let mut buf = String::new();
            let mut acc = Accumulator::default();
            while let Some(chunk) = bytes.next().await {
                let chunk = chunk.map_err(|e| LlmError::Http(e.to_string()))?;
                buf.push_str(&String::from_utf8_lossy(&chunk));

                while let Some(nl) = buf.find('\n') {
                    let line: String = buf.drain(..=nl).collect();
                    let line = line.trim();
                    let Some(payload) = line.strip_prefix("data:") else { continue };
                    let payload = payload.trim();
                    // §2.6 缺陷4：坏分片/[DONE] 不致命——跳过坏 JSON（已 warn），不杀整流。
                    let Some(mut parsed) = parse_chunk_payload(payload) else { continue };
                    if let Some(u) = parsed.usage.take() {
                        // 末尾 usage 块：trace token 增长,前端不下发(只是日志)。
                        tracing::debug!(
                            target: "botobot::llm",
                            prompt = u.prompt_tokens.unwrap_or(0),
                            completion = u.completion_tokens.unwrap_or(0),
                            total = u.total_tokens.unwrap_or(0),
                            "usage"
                        );
                        acc.set_usage(u.into_token_usage());
                    }
                    for choice in parsed.choices {
                        let reasoning = choice.delta.reasoning_content.or(choice.delta.reasoning);
                        if let Some(r) = reasoning.filter(|s| !s.is_empty()) {
                            for ev in acc.push_reasoning(&r) {
                                yield ev;
                            }
                        }
                        if let Some(c) = choice.delta.content.filter(|s| !s.is_empty()) {
                            for ev in acc.feed_content(&c) {
                                yield ev;
                            }
                        }
                        for tcd in choice.delta.tool_calls {
                            acc.apply_tool(tcd);
                        }
                        acc.set_finish_reason(choice.finish_reason);
                    }
                }
            }
            for ev in acc.flush_think() {
                yield ev;
            }
            yield LlmEvent::Done(acc.into_decision());
        };

        let boxed: base_types::LlmStream = Box::pin(stream);
        // 停滞流检测（§2.6 缺陷2）：装上 idle timeout 包裹（env 配置）。
        Ok(match self.idle_timeout {
            Some(dur) => idle_timeout_stream(boxed, dur),
            None => boxed,
        })
    }
}

/// 解析一条 SSE `data:` payload 为 [`sse::StreamChunk`]（§2.6 缺陷4，借鉴 codex/omp 多 provider 容错）。
/// `[DONE]` 哨兵或坏 JSON 返回 `None`（坏 JSON 打 warn）——让单条畸形分片**跳过而非杀整流**。
fn parse_chunk_payload(payload: &str) -> Option<sse::StreamChunk> {
    if payload == "[DONE]" {
        return None;
    }
    match serde_json::from_str::<sse::StreamChunk>(payload) {
        Ok(chunk) => Some(chunk),
        Err(e) => {
            let head: String = payload.chars().take(120).collect();
            tracing::warn!(target: "botobot::llm", error = %e, head = %head, "skip malformed sse chunk");
            None
        }
    }
}

/// 停滞流检测（§2.6 缺陷2，借鉴 codex `core/src/client.rs` 的 stream_idle_timeout）：
/// 把一个 [`LlmStream`] 包裹成「单次等待下一个事件超过 `dur` 即产出 [`base_types::LlmError::Idle`] 并终止」。
/// 正常事件透传；上游自然结束（`None`）则结束。
pub fn idle_timeout_stream(
    mut inner: base_types::LlmStream,
    dur: std::time::Duration,
) -> base_types::LlmStream {
    Box::pin(async_stream::stream! {
        loop {
            match tokio::time::timeout(dur, inner.next()).await {
                Ok(Some(ev)) => yield ev,
                Ok(None) => break,
                Err(_elapsed) => {
                    yield Err(base_types::LlmError::Idle);
                    break;
                }
            }
        }
    })
}

#[cfg(test)]
mod idle_tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn idle_timeout_stream_yields_idle_on_stall() {
        let inner: base_types::LlmStream = Box::pin(futures::stream::pending::<
            base_types::LlmResult<base_types::LlmEvent>,
        >());
        let mut s = idle_timeout_stream(inner, Duration::from_millis(10));
        match s.next().await {
            Some(Err(base_types::LlmError::Idle)) => {}
            other => panic!("停滞流应产出 Idle，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn idle_timeout_stream_passes_through_events() {
        let evs: Vec<base_types::LlmResult<base_types::LlmEvent>> = vec![Ok(
            base_types::LlmEvent::Done(base_types::Decision::default()),
        )];
        let inner: base_types::LlmStream = Box::pin(futures::stream::iter(evs));
        let mut s = idle_timeout_stream(inner, Duration::from_secs(5));
        assert!(matches!(
            s.next().await,
            Some(Ok(base_types::LlmEvent::Done(_)))
        ));
        assert!(s.next().await.is_none());
    }
}

#[cfg(test)]
mod sse_robust_tests {
    use super::*;

    #[test]
    fn toolcalldelta_tolerates_missing_index() {
        let d: sse::ToolCallDelta = serde_json::from_str(r#"{"function":{"name":"f"}}"#).unwrap();
        assert_eq!(d.index, 0, "缺省 index 应为 0");
    }

    #[test]
    fn parse_chunk_payload_skips_bad_and_done() {
        assert!(parse_chunk_payload("{bad json").is_none(), "坏 JSON→None");
        assert!(parse_chunk_payload("[DONE]").is_none(), "[DONE]→None");
        let c = parse_chunk_payload(r#"{"choices":[{"delta":{"content":"hi"}}]}"#)
            .expect("正常分片→Some");
        assert_eq!(c.choices[0].delta.content.as_deref(), Some("hi"));
    }
}
