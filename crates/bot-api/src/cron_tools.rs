//! Agent-facing cron 工具（§2.10）：让 bot 在对话里给「自己」排一个未来的 turn。
//!
//! 与 team_tools 同模式：工具就近定义在 bot-api（持共享 `CronJobs`，与 Hub 的 CronHandler
//! 同一份表），webui-bin 装配时构造并注册进 coder agent。
//!
//! 语义：`schedule_task` 的目标会话 = **调用方自己的会话**（`ToolCtx.session_id`）——
//! 「过 N 秒（可周期）再用这段 prompt 唤醒我」。到点由心跳 CronHandler 异步 submit 起 turn。

use async_trait::async_trait;
use serde_json::{Value, json};

use base_types::{Tool, ToolCtx, ToolResult, ToolTier};

use crate::cron::CronJobs;

/// 三个工具共享的句柄：与 Hub 同持的 job 表。
#[derive(Clone)]
pub struct CronToolCtx {
    jobs: CronJobs,
}

impl CronToolCtx {
    pub fn new(jobs: CronJobs) -> Self {
        Self { jobs }
    }
}

fn arg_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(|v| v.as_u64())
}

// ───────────────────────── schedule_task ─────────────────────────

pub struct ScheduleTaskTool {
    cx: CronToolCtx,
}
impl ScheduleTaskTool {
    pub fn new(cx: CronToolCtx) -> Self {
        Self { cx }
    }
}

#[async_trait]
impl Tool for ScheduleTaskTool {
    fn name(&self) -> &str {
        "schedule_task"
    }
    fn description(&self) -> &str {
        "Schedule a future turn in THIS session: after `delay_secs` (default 0), you will be \
         re-invoked with `prompt`. Pass `interval_secs` to repeat periodically (omit for one-shot). \
         Returns the job id (use cancel_task to stop it). Args: prompt, delay_secs?, interval_secs?."
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Write
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": { "type": "string", "description": "The instruction to wake up with." },
                "delay_secs": { "type": "integer", "description": "Seconds until first run (default 0)." },
                "interval_secs": { "type": "integer", "description": "Repeat period in seconds; omit for one-shot." }
            },
            "required": ["prompt"]
        })
    }
    async fn call(&self, args: Value) -> ToolResult {
        // 无上下文时无法定位目标会话——拒绝（正常经 call_with_context）。
        self.schedule(args, "").await
    }
    async fn call_with_context(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        self.schedule(args, &ctx.session_id).await
    }
}

impl ScheduleTaskTool {
    async fn schedule(&self, args: Value, session_id: &str) -> ToolResult {
        if session_id.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "schedule_task needs a calling session context"
            ));
        }
        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("missing required arg: prompt"))?
            .to_string();
        let delay = std::time::Duration::from_secs(arg_u64(&args, "delay_secs").unwrap_or(0));
        let interval = arg_u64(&args, "interval_secs").map(std::time::Duration::from_secs);
        let id = crate::cron::schedule(&self.cx.jobs, session_id, prompt, delay, interval);
        Ok(json!({
            "id": id,
            "recurring": interval.is_some(),
            "note": "scheduled; the heartbeat will start a turn when due"
        }))
    }
}

// ───────────────────────── list_tasks ─────────────────────────

pub struct ListTasksTool {
    cx: CronToolCtx,
}
impl ListTasksTool {
    pub fn new(cx: CronToolCtx) -> Self {
        Self { cx }
    }
}

#[async_trait]
impl Tool for ListTasksTool {
    fn name(&self) -> &str {
        "list_tasks"
    }
    fn description(&self) -> &str {
        "List currently scheduled tasks (id, target session, prompt, recurring). No args."
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Read
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn call(&self, _args: Value) -> ToolResult {
        let jobs: Vec<Value> = crate::cron::list(&self.cx.jobs)
            .into_iter()
            .map(|(id, session_id, prompt, recurring)| {
                json!({ "id": id, "session_id": session_id, "prompt": prompt, "recurring": recurring })
            })
            .collect();
        Ok(json!({ "tasks": jobs }))
    }
}

// ───────────────────────── cancel_task ─────────────────────────

pub struct CancelTaskTool {
    cx: CronToolCtx,
}
impl CancelTaskTool {
    pub fn new(cx: CronToolCtx) -> Self {
        Self { cx }
    }
}

#[async_trait]
impl Tool for CancelTaskTool {
    fn name(&self) -> &str {
        "cancel_task"
    }
    fn description(&self) -> &str {
        "Cancel a scheduled task by id. Arg: id."
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Write
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "id": { "type": "string" } },
            "required": ["id"]
        })
    }
    async fn call(&self, args: Value) -> ToolResult {
        let id = args
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("missing required arg: id"))?;
        let removed = crate::cron::cancel(&self.cx.jobs, id);
        Ok(json!({ "id": id, "cancelled": removed }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn ctx() -> CronToolCtx {
        CronToolCtx::new(Arc::new(Mutex::new(Vec::new())))
    }

    fn tool_ctx(session_id: &str) -> ToolCtx {
        ToolCtx {
            session_id: session_id.into(),
            run_id: "r".into(),
            parent_id: None,
            workdir: "/tmp".into(),
            cancel: Default::default(),
            depth: 0,
            max_depth: 4,
            token_budget: None,
            llm_opts: Default::default(),
        }
    }

    #[tokio::test]
    async fn schedule_targets_calling_session_then_list_and_cancel() {
        let cx = ctx();
        let sched = ScheduleTaskTool::new(cx.clone());
        let out = sched
            .call_with_context(
                json!({ "prompt": "ping", "delay_secs": 60, "interval_secs": 30 }),
                &tool_ctx("sess-A"),
            )
            .await
            .unwrap();
        let id = out["id"].as_str().unwrap().to_string();
        assert_eq!(out["recurring"], true);

        // list 能看见，目标会话是调用方自己。
        let listed = ListTasksTool::new(cx.clone())
            .call(json!({}))
            .await
            .unwrap();
        let tasks = listed["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["session_id"], "sess-A");
        assert_eq!(tasks[0]["prompt"], "ping");

        // cancel 命中后表清空。
        let cancelled = CancelTaskTool::new(cx.clone())
            .call(json!({ "id": id }))
            .await
            .unwrap();
        assert_eq!(cancelled["cancelled"], true);
        let after = ListTasksTool::new(cx).call(json!({})).await.unwrap();
        assert!(after["tasks"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn schedule_without_session_context_errors() {
        let sched = ScheduleTaskTool::new(ctx());
        assert!(sched.call(json!({ "prompt": "x" })).await.is_err());
    }
}
