//! 外置式观察（P-4/§8）：大工具输出存 [`ArtifactStore`]，历史只留 `artifact://id` 引用。
//!
//! 小输出照常 inline；超过 `max_inline` 字节的，外置 + 写一条短提示（无损、可 `read` 取回）。

use std::sync::Arc;

use agent_act::artifact::ArtifactStore;
use agent_act::output::spill_or_inline;
use async_trait::async_trait;
use base_types::{Context, Message, Observe, ToolOutcome};

pub struct ArtifactObserver {
    store: Arc<ArtifactStore>,
    max_inline: usize,
}

impl ArtifactObserver {
    pub fn new(store: Arc<ArtifactStore>, max_inline: usize) -> Self {
        Self { store, max_inline }
    }
}

/// 受保护工具：其输出不外置（借鉴 oh-my-pi protected-tools）。`read` 是检索工具——
/// 它的输出正是模型"主动要看"的内容，外置会导致读 artifact:// 又被再外置的增长循环。
fn is_protected(tool_name: &str) -> bool {
    tool_name == "read"
}

#[async_trait]
impl Observe for ArtifactObserver {
    async fn observe(&self, ctx: &mut Context, outcomes: Vec<ToolOutcome>) {
        for o in outcomes {
            let content = match o.result {
                Ok(v) => v.to_string(),
                Err(e) => format!("error: {e}"),
            };
            // 超阈值、非受保护工具、且外置成功 → 历史只留短引用；否则 inline。
            if !is_protected(&o.call.function.name)
                && let Ok(spill) =
                    spill_or_inline(&self.store, &content, self.max_inline, self.max_inline / 2)
                && spill.artifact_uri.is_some()
            {
                ctx.history
                    .push(Message::tool_result(o.call.id, spill.inline_text));
                continue;
            }
            ctx.history.push(Message::tool_result(o.call.id, content));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base_types::{Budget, EventSink, LlmOpts, ToolCall, VecHistory};
    use std::sync::Arc as StdArc;
    use tokio_util::sync::CancellationToken;

    fn ctx() -> Context {
        Context {
            session_id: "s-test".into(),
            history: Box::new(VecHistory::new()),
            workdir: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            tools: StdArc::new(agent_act::ToolRegistry::new()),
            sink: EventSink::null(),
            run_id: "t".into(),
            parent_id: None,
            cancel: CancellationToken::new(),
            budget: Budget::default(),
            token_spent: StdArc::new(std::sync::atomic::AtomicUsize::new(0)),
            llm_opts: LlmOpts::default(),
            subsession_store: None,
        }
    }
    fn outcome(id: &str, val: &str) -> ToolOutcome {
        outcome_named(id, "t", val)
    }
    fn outcome_named(id: &str, tool: &str, val: &str) -> ToolOutcome {
        ToolOutcome {
            call: ToolCall {
                id: id.into(),
                kind: "function".into(),
                function: base_types::FunctionCall {
                    name: tool.into(),
                    arguments: "{}".into(),
                },
            },
            result: Ok(serde_json::Value::String(val.into())),
        }
    }

    #[tokio::test]
    async fn externalizes_large_inlines_small() {
        let dir = std::env::temp_dir().join("botobot-artifact-obs-test");
        let store = Arc::new(ArtifactStore::new(&dir).unwrap());
        let obs = ArtifactObserver::new(store, 20);
        let mut c = ctx();
        let big = "x".repeat(100);
        obs.observe(&mut c, vec![outcome("c1", &big), outcome("c2", "ok")])
            .await;
        let texts: Vec<String> = c
            .history
            .view()
            .iter()
            .map(|m| {
                m.content
                    .iter()
                    .filter_map(|p| match p {
                        base_types::ContentPart::Text(t) => Some(t.clone()),
                        _ => None,
                    })
                    .collect::<String>()
            })
            .collect();
        assert!(
            texts.iter().any(|t| t.contains("artifact://")),
            "大输出应外置"
        );
        assert!(texts.iter().any(|t| t.contains("tail")), "应保留 tail 预览");
        assert!(texts.iter().any(|t| t.contains("ok")), "小输出应 inline");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_tool_output_is_protected_from_externalization() {
        let dir = std::env::temp_dir().join("botobot-artifact-prot-test");
        let store = Arc::new(ArtifactStore::new(&dir).unwrap());
        let obs = ArtifactObserver::new(store, 20);
        let mut c = ctx();
        let big = "y".repeat(100);
        obs.observe(&mut c, vec![outcome_named("c1", "read", &big)])
            .await;
        let t: String = c.history.view()[0]
            .content
            .iter()
            .filter_map(|p| match p {
                base_types::ContentPart::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert!(!t.contains("artifact://"), "read 输出不应被外置");
        assert!(t.contains(&big), "read 输出应原样 inline");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
