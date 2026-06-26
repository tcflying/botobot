//! 摘要式观察：超过 `max_chars` 的工具输出先用 `Llm` 压成摘要再折回。
//!
//! 同构递归（观察内部再调 Llm）。摘要失败回退为截断。

use async_trait::async_trait;
use futures::StreamExt;
use std::sync::Arc;

use base_types::{Context, Llm, LlmEvent, LlmOpts, Message, Observe, ToolOutcome};

pub struct SummarizingObserver {
    llm: Arc<dyn Llm>,
    max_chars: usize,
}

impl SummarizingObserver {
    pub fn new(llm: Arc<dyn Llm>, max_chars: usize) -> Self {
        Self { llm, max_chars }
    }

    async fn summarize(&self, big: &str) -> String {
        let msgs = vec![
            Message::system(
                "You compress tool output for an agent's working memory. Keep facts, numbers, \
                 names, and errors. Be terse. Output only the summary.",
            ),
            Message::user(big.to_string()),
        ];
        // summarize 走默认 opts(无 thinking 开关需求)
        match self.llm.infer(&msgs, &[], &LlmOpts::default()).await {
            Ok(mut stream) => {
                let mut text = String::new();
                while let Some(ev) = stream.next().await {
                    if let Ok(LlmEvent::Done(d)) = ev {
                        text = d.text;
                    }
                }
                if text.is_empty() {
                    truncate(big, self.max_chars)
                } else {
                    format!("[summarized {} chars]\n{text}", big.len())
                }
            }
            Err(_) => truncate(big, self.max_chars),
        }
    }
}

#[async_trait]
impl Observe for SummarizingObserver {
    async fn observe(&self, ctx: &mut Context, outcomes: Vec<ToolOutcome>) {
        for o in outcomes {
            let content = match o.result {
                Ok(v) => {
                    let s = v.to_string();
                    if s.len() > self.max_chars {
                        self.summarize(&s).await
                    } else {
                        s
                    }
                }
                Err(e) => format!("error: {e}"),
            };
            ctx.history.push(Message::tool_result(o.call.id, content));
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    let mut end = max.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated {} chars]", &s[..end], s.len() - end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use base_types::{
        Budget, ContentPart, Decision, EventSink, FunctionCall, LlmResult, LlmStream, ToolCall,
        ToolSpec, VecHistory,
    };
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    struct ScriptedLlm {
        steps: Mutex<VecDeque<Decision>>,
    }
    impl ScriptedLlm {
        fn new(steps: Vec<Decision>) -> Arc<Self> {
            Arc::new(Self {
                steps: Mutex::new(steps.into()),
            })
        }
    }
    #[async_trait]
    impl Llm for ScriptedLlm {
        async fn infer(
            &self,
            _m: &[Message],
            _t: &[ToolSpec],
            _opts: &LlmOpts,
        ) -> LlmResult<LlmStream> {
            let d = self.steps.lock().unwrap().pop_front().unwrap_or_default();
            let evs: Vec<LlmResult<LlmEvent>> = vec![Ok(LlmEvent::Done(d))];
            Ok(Box::pin(futures::stream::iter(evs)))
        }
    }

    fn ctx() -> Context {
        Context {
            session_id: "s-test".into(),
            history: Box::new(VecHistory::new()),
            workdir: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            tools: Arc::new(agent_act::ToolRegistry::new()),
            sink: EventSink::null(),
            run_id: "t".into(),
            parent_id: None,
            cancel: CancellationToken::new(),
            budget: Budget::default(),
            token_spent: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            llm_opts: LlmOpts::default(),
            subsession_store: None,
        }
    }
    fn outcome(id: &str, val: &str) -> ToolOutcome {
        ToolOutcome {
            call: ToolCall {
                id: id.into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "t".into(),
                    arguments: "{}".into(),
                },
            },
            result: Ok(serde_json::Value::String(val.into())),
        }
    }
    fn msg_text(m: &Message) -> String {
        m.content
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn summarizing_observer_compresses_large_outputs() {
        let summarizer = ScriptedLlm::new(vec![Decision {
            text: "SUMMARY".into(),
            ..Default::default()
        }]);
        let obs = SummarizingObserver::new(summarizer, 10);
        let mut c = ctx();
        let big = "x".repeat(50);
        obs.observe(&mut c, vec![outcome("c1", &big), outcome("c2", "ok")])
            .await;
        let texts: Vec<String> = c.history.view().iter().map(msg_text).collect();
        assert!(
            texts.iter().any(|t| t.contains("SUMMARY")),
            "大输出应被摘要"
        );
        assert!(texts.iter().any(|t| t.contains("ok")), "小输出应原样保留");
    }
}
