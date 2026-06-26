//! §4.9 B1「后台工具转后台」：长命令（`cargo build`/`cargo test` 等）不阻塞 turn。
//!
//! shell 超时上限已提到 600s（见 `shell.rs`）——长构建会**独占整个 turn 长达 10 分钟**，
//! 期间 agent 干不了别的。本模块让 agent **立即拿到 job_id 继续工作**，稍后 `job_status` 轮询：
//! - `shell_background(cmd)` → 立即返回 job_id，命令在后台 tokio 任务里跑（复用 `run_shell_command` 内核）；
//! - `job_status(id)` → Running / Done{exit,stdout,stderr} / Error；
//! - `job_cancel(id)` → 取消该后台命令（`CancellationToken` + `kill_on_drop`）。
//!
//! 安全：后台命令的 `command` 同样要过 exec policy（见 `exec_policy.rs` 对 `shell_background` 的分类）。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::shell::{ShellCommandArgs, ShellCommandOut, run_shell_command};
use base_types::{Tool, ToolCtx, ToolResult, ToolTier};

/// 一个后台任务的终态/进行态（`Arc<Mutex<>>` 跨「跑命令的 spawn 任务」与「job_status 读取」共享）。
enum Outcome {
    Running,
    Done(Box<ShellCommandOut>),
    Error(String),
}

struct Job {
    command: String,
    cancel: CancellationToken,
    outcome: Arc<Mutex<Outcome>>,
}

/// 后台任务注册表（每 agent 一个，注入进 shell_background/job_status/job_cancel 三工具）。
pub struct BackgroundJobs {
    jobs: Mutex<BTreeMap<String, Job>>,
    counter: AtomicU64,
    /// 同时**在跑**的后台任务上限（背压：超过则拒绝新 start，防失控刷爆机器）。
    max_running: usize,
    /// 注册表**总条目**上限（含已完成）——超过则淘汰最旧的已完成任务，防长 session 内存泄漏
    /// （每条已完成任务持有最多 64k 输出）。运行中的任务不淘汰。
    max_total: usize,
}

impl Default for BackgroundJobs {
    fn default() -> Self {
        Self::new(4)
    }
}

impl BackgroundJobs {
    pub fn new(max_running: usize) -> Self {
        Self {
            jobs: Mutex::new(BTreeMap::new()),
            counter: AtomicU64::new(0),
            max_running: max_running.max(1),
            max_total: 32,
        }
    }

    fn running_count(jobs: &BTreeMap<String, Job>) -> usize {
        jobs.values()
            .filter(|j| matches!(*j.outcome.lock().unwrap(), Outcome::Running))
            .count()
    }

    /// 启动一条后台命令，立即返回 job_id（命令在 spawn 任务里跑到完成/超时/取消）。
    /// `workdir` 取自调用 ctx；`timeout_ms` 默认 600s（长构建）。超过 `max_running` 个在跑则拒绝。
    pub fn start(
        &self,
        command: String,
        workdir: std::path::PathBuf,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<String> {
        let mut jobs = self.jobs.lock().unwrap();
        if Self::running_count(&jobs) >= self.max_running {
            anyhow::bail!(
                "too many background jobs running ({} max); wait or cancel one before starting another",
                self.max_running
            );
        }
        // 淘汰最旧的已完成任务，把总条目压到上限内（防长 session 累积已完成任务泄漏内存）。
        if jobs.len() >= self.max_total {
            let finished: Vec<String> = jobs
                .iter()
                .filter(|(_, j)| !matches!(*j.outcome.lock().unwrap(), Outcome::Running))
                .map(|(id, _)| id.clone())
                .collect();
            // 只为**界定内存**：删任意已完成任务直到回到上限内即可（不追求严格按龄淘汰）。
            for id in finished {
                if jobs.len() < self.max_total {
                    break;
                }
                jobs.remove(&id);
            }
        }
        let id = format!("job-{}", self.counter.fetch_add(1, Ordering::Relaxed) + 1);
        let outcome = Arc::new(Mutex::new(Outcome::Running));
        let cancel = CancellationToken::new();
        let out2 = outcome.clone();
        let cancel2 = cancel.clone();
        let cmd2 = command.clone();
        tokio::spawn(async move {
            let ctx = ToolCtx {
                session_id: String::new(),
                run_id: String::new(),
                parent_id: None,
                workdir,
                cancel: cancel2,
                depth: 0,
                max_depth: 0,
                token_budget: None,
                llm_opts: Default::default(),
            };
            let res = run_shell_command(
                ShellCommandArgs {
                    command: cmd2,
                    cwd: None,
                    env: None,
                    timeout_ms: Some(timeout_ms.unwrap_or(600_000)),
                    max_output_bytes: None,
                },
                &ctx,
            )
            .await;
            *out2.lock().unwrap() = match res {
                Ok(out) => Outcome::Done(Box::new(out)),
                Err(e) => Outcome::Error(e.to_string()),
            };
        });
        jobs.insert(
            id.clone(),
            Job {
                command,
                cancel,
                outcome,
            },
        );
        Ok(id)
    }

    /// 查某任务当前状态/输出（未知 id → None）。
    pub fn status(&self, id: &str) -> Option<JobReport> {
        let jobs = self.jobs.lock().unwrap();
        let job = jobs.get(id)?;
        let outcome = job.outcome.lock().unwrap();
        Some(JobReport::new(id, &job.command, &outcome))
    }

    /// 取消某任务（返回是否存在该 id）。已完成的取消无副作用。
    pub fn cancel(&self, id: &str) -> bool {
        let jobs = self.jobs.lock().unwrap();
        match jobs.get(id) {
            Some(job) => {
                job.cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// 列出所有任务（id + 是否在跑），按 id 排序。
    pub fn list(&self) -> Vec<(String, bool)> {
        let jobs = self.jobs.lock().unwrap();
        jobs.iter()
            .map(|(id, j)| {
                (
                    id.clone(),
                    matches!(*j.outcome.lock().unwrap(), Outcome::Running),
                )
            })
            .collect()
    }
}

/// 对外可序列化的任务状态视图。
#[derive(Debug, Serialize, PartialEq)]
pub struct JobReport {
    pub id: String,
    pub command: String,
    /// `running` | `done` | `error`。
    pub phase: &'static str,
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub timed_out: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cancelled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl JobReport {
    fn new(id: &str, command: &str, outcome: &Outcome) -> Self {
        let mut r = JobReport {
            id: id.to_string(),
            command: command.to_string(),
            phase: "running",
            running: false,
            status: None,
            stdout: None,
            stderr: None,
            timed_out: false,
            cancelled: false,
            error: None,
        };
        match outcome {
            Outcome::Running => {
                r.phase = "running";
                r.running = true;
            }
            Outcome::Done(out) => {
                r.phase = "done";
                r.status = out.status;
                r.stdout = Some(out.stdout.clone());
                r.stderr = Some(out.stderr.clone());
                r.timed_out = out.timed_out;
                r.cancelled = out.cancelled;
            }
            Outcome::Error(e) => {
                r.phase = "error";
                r.error = Some(e.clone());
            }
        }
        r
    }
}

// ───────────────────────── 工具 ─────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ShellBackgroundArgs {
    /// Command to run in the background (e.g. a long `cargo build`/`cargo test`).
    pub command: String,
    /// Optional timeout in milliseconds. Defaults to 600000 (10 min); capped by the shell kernel.
    pub timeout_ms: Option<u64>,
}

/// `shell_background`（Exec）：启动后台命令，立即返回 job_id；用 `job_status` 轮询、`job_cancel` 取消。
pub struct ShellBackgroundTool {
    jobs: Arc<BackgroundJobs>,
}

impl ShellBackgroundTool {
    pub fn new(jobs: Arc<BackgroundJobs>) -> Self {
        Self { jobs }
    }
}

#[async_trait]
impl Tool for ShellBackgroundTool {
    fn name(&self) -> &str {
        "shell_background"
    }
    fn description(&self) -> &str {
        "Start a long-running shell command in the background and return a job_id immediately so you \
         can keep working. Poll it with job_status(job_id) and stop it with job_cancel(job_id). \
         Use for builds/tests/servers that would otherwise block the turn. Only when Code is enabled."
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Exec
    }
    fn schema(&self) -> Value {
        json!(schema_for!(ShellBackgroundArgs))
    }
    async fn call(&self, args: Value) -> ToolResult {
        let ctx = ToolCtx {
            session_id: String::new(),
            run_id: String::new(),
            parent_id: None,
            workdir: std::env::current_dir()?,
            cancel: CancellationToken::new(),
            depth: 0,
            max_depth: 0,
            token_budget: None,
            llm_opts: Default::default(),
        };
        self.call_with_context(args, &ctx).await
    }
    async fn call_with_context(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let args: ShellBackgroundArgs = serde_json::from_value(args)?;
        let id = self
            .jobs
            .start(args.command, ctx.workdir.clone(), args.timeout_ms)?;
        Ok(json!({
            "job_id": id,
            "message": format!("started in background; poll with job_status(\"{id}\")"),
        }))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct JobIdArgs {
    /// The job_id returned by shell_background.
    pub job_id: String,
}

/// `job_status`（Read）：查后台命令状态/输出。
pub struct JobStatusTool {
    jobs: Arc<BackgroundJobs>,
}

impl JobStatusTool {
    pub fn new(jobs: Arc<BackgroundJobs>) -> Self {
        Self { jobs }
    }
}

#[async_trait]
impl Tool for JobStatusTool {
    fn name(&self) -> &str {
        "job_status"
    }
    fn description(&self) -> &str {
        "Check a background command started with shell_background: returns phase (running/done/error), \
         exit status, and stdout/stderr when finished."
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Read
    }
    fn schema(&self) -> Value {
        json!(schema_for!(JobIdArgs))
    }
    async fn call(&self, args: Value) -> ToolResult {
        let args: JobIdArgs = serde_json::from_value(args)?;
        match self.jobs.status(&args.job_id) {
            Some(r) => Ok(serde_json::to_value(r)?),
            None => Err(anyhow::anyhow!("unknown job_id: {}", args.job_id)),
        }
    }
}

/// `job_cancel`（Exec）：取消后台命令。
pub struct JobCancelTool {
    jobs: Arc<BackgroundJobs>,
}

impl JobCancelTool {
    pub fn new(jobs: Arc<BackgroundJobs>) -> Self {
        Self { jobs }
    }
}

#[async_trait]
impl Tool for JobCancelTool {
    fn name(&self) -> &str {
        "job_cancel"
    }
    fn description(&self) -> &str {
        "Cancel a background command started with shell_background, by job_id."
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Exec
    }
    fn schema(&self) -> Value {
        json!(schema_for!(JobIdArgs))
    }
    async fn call(&self, args: Value) -> ToolResult {
        let args: JobIdArgs = serde_json::from_value(args)?;
        let found = self.jobs.cancel(&args.job_id);
        if found {
            Ok(json!({ "cancelled": args.job_id }))
        } else {
            Err(anyhow::anyhow!("unknown job_id: {}", args.job_id))
        }
    }
}

/// `job_list`（Read）：列出所有后台任务（id + 是否在跑），便于找回遗失的 job_id。
pub struct JobListTool {
    jobs: Arc<BackgroundJobs>,
}

impl JobListTool {
    pub fn new(jobs: Arc<BackgroundJobs>) -> Self {
        Self { jobs }
    }
}

#[async_trait]
impl Tool for JobListTool {
    fn name(&self) -> &str {
        "job_list"
    }
    fn description(&self) -> &str {
        "List all background commands (id + running flag) started with shell_background. \
         Use to recover a job_id or see what is still running."
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Read
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn call(&self, _args: Value) -> ToolResult {
        let jobs: Vec<Value> = self
            .jobs
            .list()
            .into_iter()
            .map(|(id, running)| json!({ "job_id": id, "running": running }))
            .collect();
        Ok(json!({ "jobs": jobs }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn workdir() -> std::path::PathBuf {
        std::env::temp_dir()
    }

    async fn wait_done(jobs: &BackgroundJobs, id: &str) -> JobReport {
        for _ in 0..200 {
            let r = jobs.status(id).unwrap();
            if !r.running {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("job did not finish in time");
    }

    // 后台跑一条快命令 → 立即拿 id、状态先 Running 后 Done、输出/退出码可读。
    #[tokio::test]
    async fn background_job_runs_and_reports_output() {
        let jobs = BackgroundJobs::new(4);
        #[cfg(windows)]
        let cmd = "Write-Output hello-bg";
        #[cfg(not(windows))]
        let cmd = "printf 'hello-bg\\n'";
        let id = jobs.start(cmd.into(), workdir(), Some(10_000)).unwrap();

        let done = wait_done(&jobs, &id).await;
        assert_eq!(done.phase, "done");
        assert_eq!(done.status, Some(0));
        assert!(done.stdout.unwrap().contains("hello-bg"));
        assert!(!done.timed_out && !done.cancelled);
    }

    // 未知 id → None / false。
    #[tokio::test]
    async fn unknown_job_id_is_none() {
        let jobs = BackgroundJobs::new(4);
        assert!(jobs.status("nope").is_none());
        assert!(!jobs.cancel("nope"));
    }

    // 背压：超过 max_running 个在跑 → 拒绝新 start。
    #[tokio::test]
    async fn rejects_when_too_many_running() {
        let jobs = BackgroundJobs::new(1);
        #[cfg(windows)]
        let slow = "Start-Sleep -Seconds 5";
        #[cfg(not(windows))]
        let slow = "sleep 5";
        let _id1 = jobs.start(slow.into(), workdir(), Some(30_000)).unwrap();
        let err = jobs
            .start(slow.into(), workdir(), Some(30_000))
            .unwrap_err();
        assert!(err.to_string().contains("too many background jobs"));
    }

    // job_list 列出所有任务（找回遗失 id）。
    #[tokio::test]
    async fn lists_all_jobs() {
        let jobs = BackgroundJobs::new(4);
        #[cfg(windows)]
        let cmd = "Write-Output x";
        #[cfg(not(windows))]
        let cmd = "printf x";
        let id1 = jobs.start(cmd.into(), workdir(), Some(10_000)).unwrap();
        let id2 = jobs.start(cmd.into(), workdir(), Some(10_000)).unwrap();
        let listed: Vec<String> = jobs.list().into_iter().map(|(id, _)| id).collect();
        assert!(listed.contains(&id1) && listed.contains(&id2));
        assert_eq!(listed.len(), 2);
    }

    // 内存界定：跑很多快命令，注册表条目数不超过 max_total（已完成被淘汰）。
    #[tokio::test]
    async fn evicts_finished_jobs_to_bound_memory() {
        let jobs = BackgroundJobs::new(4);
        #[cfg(windows)]
        let cmd = "Write-Output x";
        #[cfg(not(windows))]
        let cmd = "printf x";
        // 起 50 条快命令，逐条等完成（确保已完成可被淘汰）。
        for _ in 0..50 {
            let id = jobs.start(cmd.into(), workdir(), Some(10_000)).unwrap();
            wait_done(&jobs, &id).await;
        }
        assert!(
            jobs.list().len() <= 32,
            "注册表应被 max_total 界定: {}",
            jobs.list().len()
        );
    }

    // 取消进行中的后台命令 → 终态 cancelled。
    #[tokio::test]
    async fn cancel_stops_running_job() {
        let jobs = BackgroundJobs::new(4);
        #[cfg(windows)]
        let slow = "Start-Sleep -Seconds 30";
        #[cfg(not(windows))]
        let slow = "sleep 30";
        let id = jobs.start(slow.into(), workdir(), Some(60_000)).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(jobs.cancel(&id));
        let done = wait_done(&jobs, &id).await;
        assert_eq!(done.phase, "done");
        assert!(done.cancelled, "取消后应标 cancelled");
    }
}
