//! 作者面：写强类型工具，由 [`Erased`] 自动擦成 `dyn Tool` 并从 `Args` 生成 schema。
//!
//! 这是 act-tech 的"Core + Erased 擦除器"层，ToolRegistry 在 [`super::registry`]。

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use base_types::{Tool, ToolConcurrency, ToolLoadMode, ToolResult, ToolTier};

#[async_trait]
pub trait TypedTool: Send + Sync {
    type Args: DeserializeOwned + JsonSchema + Send;
    type Out: Serialize;
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// 能力分级（默认 `Exec`，§7a）。只读工具应覆写为 `Read`。
    fn tier(&self) -> ToolTier {
        ToolTier::Exec
    }
    /// 调度并发语义（默认并发）。需要独占执行的 typed tool 可覆写。
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }
    /// 加载模式（默认 essential）。低频工具可覆写为 discoverable。
    fn load_mode(&self) -> ToolLoadMode {
        ToolLoadMode::Essential
    }
    async fn run(&self, args: Self::Args) -> anyhow::Result<Self::Out>;
}

/// 把任意 [`TypedTool`] 擦除成对象安全的 [`Tool`].
/// 与手写 `impl Tool for AgentTool` 区分（见 CLAUDE.md D2 双层）。
pub struct Erased<T>(pub T);

#[async_trait]
impl<T: TypedTool> Tool for Erased<T> {
    fn name(&self) -> &str {
        self.0.name()
    }
    fn description(&self) -> &str {
        self.0.description()
    }
    fn tier(&self) -> ToolTier {
        self.0.tier()
    }
    fn concurrency(&self) -> ToolConcurrency {
        self.0.concurrency()
    }
    fn load_mode(&self) -> ToolLoadMode {
        self.0.load_mode()
    }
    fn schema(&self) -> Value {
        let schema = schemars::schema_for!(T::Args);
        serde_json::to_value(schema).unwrap_or(Value::Null)
    }
    async fn call(&self, args: Value) -> ToolResult {
        let parsed: T::Args = serde_json::from_value(args)?;
        let out = self.0.run(parsed).await?;
        Ok(serde_json::to_value(out)?)
    }
}
