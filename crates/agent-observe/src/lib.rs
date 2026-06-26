//! observe-tech：观察切入点 [`base_types::Observe`] 的实现（act 后把结果折回上下文）。
//!
//! - [`AppendObserver`]：默认 Core，逐个 append tool 结果。
//! - [`SummarizingObserver`]：大输出先用 `Llm` 摘要再折回——"观察内部递归调 Llm"的样例，在 [`summarizing`]。

use async_trait::async_trait;
use base_types::{Context, Message, Observe, ToolOutcome};

mod artifact;
mod summarizing;

pub use artifact::ArtifactObserver;
pub use summarizing::SummarizingObserver;

/// 默认观察：逐个把工具结果作为 tool 消息追加（带内错误也在此回喂）。
pub struct AppendObserver;

#[async_trait]
impl Observe for AppendObserver {
    async fn observe(&self, ctx: &mut Context, outcomes: Vec<ToolOutcome>) {
        for o in outcomes {
            let content = match o.result {
                Ok(v) => v.to_string(),
                Err(e) => format!("error: {e}"),
            };
            ctx.history.push(Message::tool_result(o.call.id, content));
        }
    }
}
