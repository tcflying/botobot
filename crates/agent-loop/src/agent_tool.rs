//! 递归闭合：把一个 Agent 包装成 Tool，使其可作为子 agent 被父 agent 调用。
//!
//! 读取 `CALL_CX` 任务局部以共享 sink、级联取消、递增 depth。

use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use base_types::{Budget, Context, Message, Tool, ToolConcurrency, ToolResult};

use crate::agent::CALL_CX;

/// §4.9 A4：子 agent 收尾结构化报告纪律（追加到任务尾部，纯 prompt 注入、零架构改动）。
const COMPLETION_REMINDER: &str = "\n\n<system-reminder>\nBefore you finish, end your reply with a tight structured report:\n- Result: what you found/did (cite path:line where relevant)\n- Validation: how you verified it (or \"unverified\")\n- Follow-up / blockers: what's left or what blocked you (or \"none\")\n</system-reminder>";

pub struct AgentTool {
    agent: Arc<crate::agent::Agent>,
    name: String,
    description: String,
    /// 调度并发语义（§2.5a）：默认 `Concurrent`；只读 explore 子 agent 用 `Exclusive`
    /// 串行，避免一次派多个子 agent 打爆本地单端点（对齐 codex/oh-my-pi 并发上限）。
    concurrency: ToolConcurrency,
}

impl AgentTool {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        agent: Arc<crate::agent::Agent>,
    ) -> Self {
        Self {
            agent,
            name: name.into(),
            description: description.into(),
            concurrency: ToolConcurrency::Concurrent,
        }
    }

    /// 覆写并发语义（§2.5a）。explore 子 agent 用 `Exclusive` 防并发过载。
    pub fn with_concurrency(mut self, concurrency: ToolConcurrency) -> Self {
        self.concurrency = concurrency;
        self
    }
}

#[async_trait]
impl Tool for AgentTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": { "type": "string", "description": "The task/instruction for this sub-agent." }
            },
            "required": ["task"]
        })
    }
    fn concurrency(&self) -> ToolConcurrency {
        self.concurrency
    }
    async fn call(&self, args: Value) -> ToolResult {
        let task = args
            .get("task")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        let cx = CALL_CX.try_with(|c| c.clone()).unwrap_or_default();
        if cx.depth + 1 > cx.max_depth {
            return Err(anyhow::anyhow!(
                "max depth exceeded: {} > {}",
                cx.depth + 1,
                cx.max_depth
            ));
        }

        let mut messages = Vec::new();
        if let Some(sys) = &self.agent.system {
            messages.push(Message::system(sys.clone()));
        }
        // §4.9 A4：派活尾部注入结构化完成报告纪律——让递归子 agent 收口可控（向上汇报）。
        messages.push(Message::user(format!("{task}{COMPLETION_REMINDER}")));

        // §2.5b：给子 run 分配独立 session_id（持久化用，区别于 run_id）。
        // live 事件仍挂父 session（sub.session_id=父），不重标记到子 id，不破坏父 UI。
        let child_session = crate::agent::new_id();
        let sub = Context {
            session_id: cx.session_id.clone(),
            history: self.agent.history_from(messages),
            workdir: cx.workdir.clone(),
            tools: self.agent.tools.clone(),
            sink: cx.sink.clone(),
            run_id: crate::agent::new_id(),
            parent_id: Some(cx.parent_run_id.clone()),
            cancel: cx.cancel.clone(),
            budget: Budget {
                depth: cx.depth + 1,
                max_depth: cx.max_depth,
                max_steps: self.agent.budget.max_steps,
                token_budget: cx.token_budget,
            },
            token_spent: cx.token_spent.clone(),
            // 子 agent 暂不继承父 turn 的 llm_opts,统一 default(避免 thinking 在子 agent
            // 嵌套时被无意开启/关闭)。后续要支持可在 AgentTool 调用处透传。
            llm_opts: base_types::LlmOpts::default(),
            subsession_store: cx.subsession_store.clone(),
        };

        // 捕获子会话最终历史以落盘（run_loop 结束时回传 ctx.history.take()）。
        let (htx, hrx) = tokio::sync::oneshot::channel();
        let out = self
            .agent
            .run_loop(sub, Some(htx), None, None, None, 0)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;

        // 旁路落盘：失败仅 warn，不影响 subagent 返回（落盘是增强非主路径）。
        if let Some(store) = &cx.subsession_store {
            if let Ok(history) = hrx.await {
                if let Err(err) = store.record_subsession(&child_session, &cx.session_id) {
                    tracing::warn!("record subsession failed: {err}");
                } else if let Err(err) = store.persist_subsession_messages(&child_session, &history)
                {
                    tracing::warn!("persist subsession failed: {err}");
                }
            }
        }
        Ok(Value::String(out))
    }
}
