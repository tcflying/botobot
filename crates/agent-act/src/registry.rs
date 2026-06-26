//! 按名查找的工具注册表。可廉价 `clone`（内部 `Arc`）。impl [`ToolLookup`]。

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use base_types::{Tool, ToolConcurrency, ToolCtx, ToolLoadMode, ToolLookup, ToolResult, ToolTier};
use serde_json::Value;

use super::typed::{Erased, TypedTool};

#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> &mut Self {
        self.tools.insert(tool.name().to_string(), tool);
        self
    }
    pub fn register_discoverable(&mut self, tool: Arc<dyn Tool>) -> &mut Self {
        self.register(Arc::new(LoadModeTool {
            inner: tool,
            mode: ToolLoadMode::Discoverable,
        }))
    }
    pub fn register_typed<T: TypedTool + 'static>(&mut self, tool: T) -> &mut Self {
        self.register(Arc::new(Erased(tool)))
    }
    pub fn register_discoverable_typed<T: TypedTool + 'static>(&mut self, tool: T) -> &mut Self {
        self.register_discoverable(Arc::new(Erased(tool)))
    }
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
    pub fn has(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }
    pub fn has_discoverable(&self) -> bool {
        self.tools
            .values()
            .any(|t| t.load_mode() == ToolLoadMode::Discoverable)
    }
    pub fn list_tools(&self) -> Vec<Arc<dyn Tool>> {
        // 按名排序 → 跨轮稳定的工具规格字节，命中 provider 前缀缓存（P-7/§8）。
        // （HashMap 迭代序是随机的，直接 collect 会让每轮 tool-spec 顺序漂移。）
        let mut v: Vec<Arc<dyn Tool>> = self.tools.values().cloned().collect();
        v.sort_by(|a, b| a.name().cmp(b.name()));
        v
    }
}

impl ToolLookup for ToolRegistry {
    fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }
    fn list(&self) -> Vec<Arc<dyn Tool>> {
        self.list_tools()
    }
}

struct LoadModeTool {
    inner: Arc<dyn Tool>,
    mode: ToolLoadMode,
}

#[async_trait]
impl Tool for LoadModeTool {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn schema(&self) -> Value {
        self.inner.schema()
    }
    fn tier(&self) -> ToolTier {
        self.inner.tier()
    }
    fn summary(&self) -> &str {
        self.inner.summary()
    }
    fn load_mode(&self) -> ToolLoadMode {
        self.mode
    }
    fn concurrency(&self) -> ToolConcurrency {
        self.inner.concurrency()
    }
    async fn call(&self, args: Value) -> ToolResult {
        self.inner.call(args).await
    }
    async fn call_with_context(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        self.inner.call_with_context(args, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};

    #[derive(Deserialize, JsonSchema)]
    struct EchoArgs {
        text: String,
    }
    #[derive(Serialize)]
    struct EchoOut {
        echoed: String,
    }
    struct Echo;
    #[async_trait]
    impl TypedTool for Echo {
        type Args = EchoArgs;
        type Out = EchoOut;
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "echoes its input"
        }
        async fn run(&self, args: Self::Args) -> anyhow::Result<Self::Out> {
            Ok(EchoOut { echoed: args.text })
        }
    }

    #[test]
    fn list_is_name_sorted_for_stable_prefix() {
        let mut reg = ToolRegistry::new();
        for n in ["zebra", "alpha", "mid"] {
            reg.register(std::sync::Arc::new(Named(n)));
        }
        let tools = reg.list();
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(
            names,
            vec!["alpha", "mid", "zebra"],
            "list 应按名排序，跨轮稳定"
        );
    }

    #[test]
    fn register_discoverable_sets_load_mode() {
        let mut reg = ToolRegistry::new();
        reg.register_discoverable(std::sync::Arc::new(Named("hidden")));
        assert!(reg.has_discoverable());
        assert_eq!(
            reg.list_tools()[0].load_mode(),
            base_types::ToolLoadMode::Discoverable
        );
    }

    struct Named(&'static str);
    #[async_trait]
    impl base_types::Tool for Named {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "n"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn call(&self, _a: serde_json::Value) -> base_types::ToolResult {
            Ok(serde_json::Value::Null)
        }
    }

    #[tokio::test]
    async fn typed_tool_erases_and_roundtrips() {
        let mut reg = ToolRegistry::new();
        reg.register_typed(Echo);
        let tool = reg.get("echo").expect("registered");
        assert!(tool.schema().to_string().contains("text"));
        let out = tool
            .call(serde_json::json!({ "text": "hi" }))
            .await
            .expect("call ok");
        assert_eq!(out, serde_json::json!({ "echoed": "hi" }));
    }
}
