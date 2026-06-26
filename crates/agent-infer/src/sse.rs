//! OpenAI 兼容 Chat Completions SSE 响应的 JSON 反序列化类型。
//!
//! 本模块只描述 wire format，**不分流不分流**，不引入领域概念。
//! 分流在 [`super::accumulator`] 完成。

use serde::Deserialize;

use base_types::TokenUsage;

#[derive(Deserialize)]
pub struct StreamChunk {
    #[serde(default)]
    pub choices: Vec<StreamChoice>,
    /// 末尾 usage 块（仅当请求 `stream_options.include_usage = true` 时出现）。
    /// 字段保留供 trace 打印用，不影响分流。
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Deserialize)]
pub struct Usage {
    /// 方言容错：部分 Anthropic-compat 网关用 `input_tokens` 而非 `prompt_tokens`。
    #[serde(default, alias = "input_tokens")]
    pub prompt_tokens: Option<u32>,
    /// 方言容错：部分网关用 `output_tokens` 而非 `completion_tokens`。
    #[serde(default, alias = "output_tokens")]
    pub completion_tokens: Option<u32>,
    #[serde(default)]
    pub total_tokens: Option<u32>,
}

impl Usage {
    pub fn into_token_usage(self) -> TokenUsage {
        TokenUsage {
            prompt_tokens: self.prompt_tokens.map(|n| n as usize),
            completion_tokens: self.completion_tokens.map(|n| n as usize),
            total_tokens: self.total_tokens.map(|n| n as usize),
        }
    }
}

#[derive(Deserialize)]
pub struct StreamChoice {
    #[serde(default)]
    pub delta: Delta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct Delta {
    #[serde(default)]
    pub content: Option<String>,
    /// 方言容错：`reasoning_content`（DeepSeek 等）或别名 `thinking`（部分网关）。
    #[serde(default, alias = "thinking")]
    pub reasoning_content: Option<String>,
    /// 旧字段名：少数实现（如老版 vLLM）用 `reasoning` 而非 `reasoning_content`。
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallDelta>,
}

#[derive(Deserialize)]
pub struct ToolCallDelta {
    /// 某些 provider 省略 `index`（单工具调用方言）；缺省 0 以容错，不致整流失败。
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionDelta>,
}

#[derive(Deserialize, Default)]
pub struct FunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_accepts_input_output_token_dialect() {
        // 标准字段。
        let std: StreamChunk = serde_json::from_str(
            r#"{"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
        )
        .unwrap();
        let u = std.usage.unwrap().into_token_usage();
        assert_eq!(u.prompt_tokens, Some(10));
        assert_eq!(u.completion_tokens, Some(5));
        // Anthropic-compat 网关方言：input_tokens/output_tokens。
        let alt: StreamChunk =
            serde_json::from_str(r#"{"usage":{"input_tokens":20,"output_tokens":7}}"#).unwrap();
        let u2 = alt.usage.unwrap().into_token_usage();
        assert_eq!(u2.prompt_tokens, Some(20), "input_tokens 应映射 prompt");
        assert_eq!(
            u2.completion_tokens,
            Some(7),
            "output_tokens 应映射 completion"
        );
    }

    #[test]
    fn delta_accepts_thinking_reasoning_dialect() {
        let chunk: StreamChunk =
            serde_json::from_str(r#"{"choices":[{"delta":{"thinking":"让我想想"}}]}"#).unwrap();
        assert_eq!(
            chunk.choices[0].delta.reasoning_content.as_deref(),
            Some("让我想想"),
            "thinking 别名应映射 reasoning_content"
        );
    }
}
