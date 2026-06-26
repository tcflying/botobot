//! 把流式分片累积成完整 [`Decision`]:tool-call 按 index 拼接，content 经 `<think>` 标签分流。
//!
//! [`Decision`]: base_types::Decision

use base_types::{Decision, FunctionCall, LlmEvent, TokenUsage, ToolCall};

use super::reasoning::{Piece, ThinkSplitter};
use super::sse::ToolCallDelta;

#[derive(Clone, Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
pub struct Accumulator {
    text: String,
    reasoning: String,
    calls: Vec<PartialCall>,
    finish_reason: Option<String>,
    usage: Option<TokenUsage>,
    think: ThinkSplitter,
}

impl Accumulator {
    /// 直接追加一段 reasoning（服务端字段 `reasoning_content`/`reasoning`，不经 think 标签分流）。
    /// 返回产出的 delta 事件。
    pub fn push_reasoning(&mut self, s: &str) -> Vec<LlmEvent> {
        self.reasoning.push_str(s);
        vec![LlmEvent::ReasoningDelta(s.to_string())]
    }

    /// 推入一段 content（含 `<think>` 标签），返回产生的 delta 事件。
    pub fn feed_content(&mut self, text: &str) -> Vec<LlmEvent> {
        let mut events = Vec::new();
        for piece in self.think.feed(text) {
            match piece {
                Piece::Reasoning(s) => {
                    self.reasoning.push_str(&s);
                    events.push(LlmEvent::ReasoningDelta(s));
                }
                Piece::Text(s) => {
                    self.text.push_str(&s);
                    events.push(LlmEvent::TextDelta(s));
                }
            }
        }
        events
    }

    /// 流结束时把 think 残余 buf 全部吐出（视作 Text）。
    pub fn flush_think(&mut self) -> Vec<LlmEvent> {
        let mut events = Vec::new();
        if let Some(rest) = self.think.flush() {
            if !rest.is_empty() {
                self.text.push_str(&rest);
                events.push(LlmEvent::TextDelta(rest));
            }
        }
        events
    }

    /// 推入一条 tool-call 增量（按 index 拼接）。
    pub fn apply_tool(&mut self, d: ToolCallDelta) {
        if self.calls.len() <= d.index {
            self.calls.resize(d.index + 1, PartialCall::default());
        }
        let slot = &mut self.calls[d.index];
        if let Some(id) = d.id {
            slot.id = id;
        }
        if let Some(f) = d.function {
            if let Some(n) = f.name {
                slot.name.push_str(&n);
            }
            if let Some(a) = f.arguments {
                slot.arguments.push_str(&a);
            }
        }
    }

    pub fn set_finish_reason(&mut self, fr: Option<String>) {
        self.finish_reason = fr;
    }

    pub fn set_usage(&mut self, usage: TokenUsage) {
        self.usage = Some(usage);
    }

    pub fn into_decision(self) -> Decision {
        Decision {
            text: self.text,
            reasoning: self.reasoning,
            finish_reason: self.finish_reason,
            usage: self.usage,
            tool_calls: self
                .calls
                .into_iter()
                .filter(|c| !c.name.is_empty())
                .map(|c| ToolCall {
                    id: c.id,
                    kind: "function".to_string(),
                    function: FunctionCall {
                        name: c.name,
                        arguments: c.arguments,
                    },
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::FunctionDelta;

    #[test]
    fn text_only() {
        let mut a = Accumulator::default();
        a.feed_content("hello ");
        a.feed_content("world");
        // feed 末段因 keep-tail 留到 flush_think 才出。
        let trailing = a.flush_think();
        let d = a.into_decision();
        assert_eq!(d.text, "hello world");
        assert!(d.reasoning.is_empty());
        // 末段可能为空(splitter 留 0~START.len()-2 字节的 tail)。
        assert!(trailing.len() <= 1);
    }

    #[test]
    fn tool_calls_concatenate_by_index() {
        let mut a = Accumulator::default();
        a.apply_tool(ToolCallDelta {
            index: 0,
            id: Some("c1".into()),
            function: Some(FunctionDelta {
                name: Some("read".into()),
                arguments: Some(r#"{"#.into()),
            }),
        });
        a.apply_tool(ToolCallDelta {
            index: 0,
            id: None,
            function: Some(FunctionDelta {
                name: None,
                arguments: Some(r#""path":"/a"}"#.into()),
            }),
        });
        a.apply_tool(ToolCallDelta {
            index: 1,
            id: Some("c2".into()),
            function: Some(FunctionDelta {
                name: Some("echo".into()),
                arguments: Some("{}".into()),
            }),
        });
        let d = a.into_decision();
        assert_eq!(d.tool_calls.len(), 2);
        assert_eq!(d.tool_calls[0].id, "c1");
        assert_eq!(d.tool_calls[0].function.name, "read");
        assert_eq!(d.tool_calls[0].function.arguments, r#"{"path":"/a"}"#);
        assert_eq!(d.tool_calls[1].function.name, "echo");
    }

    #[test]
    fn think_splits_text_and_reasoning() {
        let mut a = Accumulator::default();
        a.feed_content("hi<think>because</think>bye");
        a.flush_think();
        let d = a.into_decision();
        assert_eq!(d.text, "hibye");
        assert_eq!(d.reasoning, "because");
    }

    #[test]
    fn usage_is_carried_into_decision() {
        let mut a = Accumulator::default();
        a.feed_content("ok");
        a.set_usage(TokenUsage {
            prompt_tokens: Some(11),
            completion_tokens: Some(3),
            total_tokens: Some(14),
        });

        let d = a.into_decision();

        assert_eq!(d.usage.and_then(|u| u.total_tokens), Some(14));
    }
}
