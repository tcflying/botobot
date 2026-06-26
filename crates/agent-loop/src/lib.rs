//! 驱动器（reason→[compact]→act→observe 循环）+ 装配入口。
//!
//! 三切入点契约都在 [`base_types`]；本 crate 是把它们编排起来的**唯一规范实现**：
//! - Core = [`Agent::run_loop`]（心跳循环 + 安全阀），见 [`agent`]。
//! - 辅助 = [`AgentTool`]（Agent 即 Tool，递归闭合）+ 调用上下文旁路注入（task-local `CALL_CX`），见 [`agent`] 与 [`agent_tool`]。
//! - 装配入口 = [`AgentBuilder`]，见 [`lib`]。
//!
//! 观察实现在 `agent-observe`（默认 [`agent_observe::AppendObserver`]）；窗口压缩是
//! `agent-act::compact`（算法 + 「驱动器拦截的控制工具」`CompactTool`），经 [`AgentBuilder::context_window`] 启用。

// 契约全在 base-types；本 crate 既用它也 re-export，方便下游（bot-api/webui-bin）单点导入。
pub use base_types::*;

mod agent;
mod agent_tool;
mod exec_policy;
mod session;

pub use agent::Agent;
pub use agent_tool::AgentTool;
pub use exec_policy::{ExecRules, RuleTableExecPolicy, classify};
pub use session::Session;

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use agent_act::compact::{CompactTool, Compactor};
use agent_act::{ToolRegistry, TypedTool};
use agent_observe::AppendObserver;

pub type HistoryFactory = Arc<dyn Fn(Vec<Message>) -> Box<dyn History> + Send + Sync>;

/// 默认循环控制（决策⑦）：模型不再发起工具调用即停——把 [`Decision`] 文本作为最终输出。
pub struct UntilQuiet;

impl Control for UntilQuiet {
    fn next(&self, decision: &Decision, _step: usize) -> Flow {
        if decision.tool_calls.is_empty() {
            Flow::Stop
        } else {
            Flow::Continue
        }
    }
}

/// 默认安全策略（决策⑦）：放行一切工具调用。
pub struct AllowAll;

impl Policy for AllowAll {
    fn check(&self, _call: &ToolCall, _tier: ToolTier, _workdir: &Path) -> Verdict {
        Verdict::Allow
    }
}

/// 只读策略（§7a，借鉴 tier）：只放行 [`ToolTier::Read`] 工具，拦截 Write/Exec
/// （拒绝走两级错误回喂模型，不终止循环）。用于"只读模式"运行。
pub struct ReadOnly;

impl Policy for ReadOnly {
    fn check(&self, _call: &ToolCall, tier: ToolTier, _workdir: &Path) -> Verdict {
        match tier {
            ToolTier::Read => Verdict::Allow,
            ToolTier::Write => Verdict::Deny("read-only mode: write tools are blocked".into()),
            ToolTier::Exec => Verdict::Deny("read-only mode: exec tools are blocked".into()),
        }
    }
}

/// Minimal execution approval policy: read/write tools run directly, Exec-tier
/// tools pause for a human approval response.
pub struct PromptExecPolicy;

impl Policy for PromptExecPolicy {
    fn check(&self, call: &ToolCall, tier: ToolTier, _workdir: &Path) -> Verdict {
        match tier {
            ToolTier::Read | ToolTier::Write => Verdict::Allow,
            ToolTier::Exec => Verdict::Prompt {
                reason: format!("exec tool `{}` requires approval", call.function.name),
            },
        }
    }
}

/// 工作目录安全策略（§10d）：通用文件读写必须被限制在 workdir 内。
///
/// 本策略包在用户提供的 policy 外层：先做 workdir 闸，再把调用交给 inner。
/// 托管资源（skill/memory/artifact/blob/book）不是任务文件，走各自 scheme，不受此路径闸限制。
pub struct WorkdirPolicy {
    inner: Arc<dyn Policy>,
}

impl WorkdirPolicy {
    pub fn new(inner: Arc<dyn Policy>) -> Self {
        Self { inner }
    }

    fn file_target(call: &ToolCall) -> Option<String> {
        match call.function.name.as_str() {
            "read" => {
                let args: serde_json::Value = serde_json::from_str(&call.function.arguments)
                    .unwrap_or(serde_json::Value::Null);
                let url = args.get("url")?.as_str()?;
                // §2.12：策略与读取共用 `normalize_file_path`（单一归一化入口），消灭两套逻辑。
                match url.find("://") {
                    Some(i) if &url[..i] == "file" => {
                        let after = &url[i + 3..];
                        let norm = agent_act::resource::normalize_file_path(after);
                        Some(agent_act::resource::strip_file_selector(&norm).to_string())
                    }
                    Some(_) => None,
                    None if url.starts_with("blob:") => None,
                    None => {
                        let norm = agent_act::resource::normalize_file_path(url);
                        Some(agent_act::resource::strip_file_selector(&norm).to_string())
                    }
                }
            }
            "read_file" => {
                let args: serde_json::Value = serde_json::from_str(&call.function.arguments)
                    .unwrap_or(serde_json::Value::Null);
                Some(args.get("path")?.as_str()?.to_string())
            }
            _ => None,
        }
    }

    fn normalize(path: impl AsRef<Path>) -> PathBuf {
        let mut out = PathBuf::new();
        for c in path.as_ref().components() {
            match c {
                Component::CurDir => {}
                Component::ParentDir => {
                    out.pop();
                }
                other => out.push(other.as_os_str()),
            }
        }
        out
    }

    fn within_workdir(workdir: &Path, raw: &str) -> bool {
        let root = std::fs::canonicalize(workdir).unwrap_or_else(|_| Self::normalize(workdir));
        let raw_path = Path::new(raw);
        let joined = if raw_path.is_absolute() {
            raw_path.to_path_buf()
        } else {
            root.join(raw_path)
        };
        let lexical = Self::normalize(joined);
        let target = std::fs::canonicalize(&lexical).unwrap_or(lexical);
        target.starts_with(&root)
    }
}

impl Policy for WorkdirPolicy {
    fn check(&self, call: &ToolCall, tier: ToolTier, workdir: &Path) -> Verdict {
        if let Some(raw) = Self::file_target(call) {
            if !Self::within_workdir(workdir, &raw) {
                // §2.12 教学化错误：可恢复错误 = 给模型的纠错梯子，掐断试错瀑布。
                return Verdict::Deny(format!(
                    "路径越界: {raw}。请用相对 workdir 的路径（如 \"Cargo.toml\" 或 \"crates/foo/src/lib.rs\"），\
                     不要加 file:// 或前导 /；绝对路径只在确实落在 workdir 内时才接受。(workdir: {})",
                    workdir.display()
                ));
            }
        }
        self.inner.check(call, tier, workdir)
    }
}

#[derive(Default)]
pub struct AgentBuilder {
    llm: Option<Arc<dyn Llm>>,
    observe: Option<Arc<dyn Observe>>,
    compactor: Option<Compactor>,
    compaction_artifacts: Option<Arc<agent_act::artifact::ArtifactStore>>,
    control: Option<Arc<dyn Control>>,
    policy: Option<Arc<dyn Policy>>,
    system: Option<String>,
    tools: ToolRegistry,
    history_factory: Option<HistoryFactory>,
    budget: Budget,
    timeout: Option<std::time::Duration>,
    workdir: Option<PathBuf>,
    subsession_store: Option<Arc<dyn base_types::SubsessionStore>>,
    stream_replays: Option<usize>,
    approval_store: Option<Arc<dyn base_types::ApprovalStore>>,
    recall: Option<Arc<dyn agent_act::memory::QueryRecall>>,
    episodic: Option<Arc<dyn agent_act::episode::EpisodicHook>>,
}

impl AgentBuilder {
    pub fn llm(mut self, llm: Arc<dyn Llm>) -> Self {
        self.llm = Some(llm);
        self
    }
    /// 替换观察切入点（默认 [`agent_observe::AppendObserver`]）。
    pub fn observe(mut self, observe: Arc<dyn Observe>) -> Self {
        self.observe = Some(observe);
        self
    }
    /// 替换循环控制切入点（默认 [`UntilQuiet`]）。
    pub fn control(mut self, control: Arc<dyn Control>) -> Self {
        self.control = Some(control);
        self
    }
    /// 替换安全策略切入点（默认 [`AllowAll`]）：每个工具调用执行前过闸。
    pub fn policy(mut self, policy: Arc<dyn Policy>) -> Self {
        self.policy = Some(policy);
        self
    }
    /// 设置任务工作目录（§10d）。通用文件读写会被 [`WorkdirPolicy`] 限制在此目录内。
    /// 不设置时使用当前进程 cwd。
    pub fn workdir(mut self, path: impl Into<PathBuf>) -> Self {
        self.workdir = Some(path.into());
        self
    }
    /// 启用窗口压缩（决策⑧）：`window` = **真实模型窗口 tokens**，Compactor 内部派生
    /// soft=0.75W / hard=0.9W / keep=0.4W。注册「控制工具」[`CompactTool`]，模型可主动调用，
    /// 驱动器按名拦截改写 `ctx.history`（检测=硬 / 软=agent 调 / 兜底=硬，见 [`Agent::run_loop`]）。
    /// 折叠用的 ArtifactStore 经 [`Self::compaction_artifacts`] 注入；未注入则降级有损 notice。
    pub fn context_window(mut self, window: usize) -> Self {
        self.compactor = Some(Compactor::new(window, self.compaction_artifacts.clone()));
        self.tools.register(Arc::new(CompactTool));
        self
    }

    /// 注入折叠用的 [`ArtifactStore`]（Q5，可回溯折叠）。与 [`Self::context_window`] 调用顺序无关：
    /// 这里记录句柄，若 Compactor 已建则用既有 window 重建以带上 store。
    pub fn compaction_artifacts(mut self, store: Arc<agent_act::artifact::ArtifactStore>) -> Self {
        self.compaction_artifacts = Some(store.clone());
        if let Some(existing) = &self.compactor {
            let window = existing.window;
            self.compactor = Some(Compactor::new(window, Some(store)));
        }
        self
    }
    /// 启用摘要式 History 后端：较早消息会同步滚成 system 摘要，保留最近 `keep_recent` 条。
    pub fn summary_history(mut self, max_chars: usize, keep_recent: usize) -> Self {
        self.history_factory = Some(Arc::new(move |messages| {
            Box::new(SummaryHistory::with_messages(
                max_chars,
                keep_recent,
                messages,
            ))
        }));
        self
    }
    pub fn system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }
    pub fn tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.register(tool);
        self
    }
    pub fn typed_tool<T: TypedTool + 'static>(mut self, tool: T) -> Self {
        self.tools.register_typed(tool);
        self
    }
    /// 暴露底层注册器，便于一次性装入一组叶子工具（如 `tools::register_all(&mut builder.tools_mut())`）。
    pub fn tools_mut(&mut self) -> &mut ToolRegistry {
        &mut self.tools
    }
    /// 注册一个子 agent 作为工具（递归闭合）。
    pub fn sub_agent(
        mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        agent: Arc<Agent>,
    ) -> Self {
        self.tools
            .register(Arc::new(AgentTool::new(name, description, agent)));
        self
    }
    /// 注册一个**独占调度**的子 agent（§2.5a）：声明 [`ToolConcurrency::Exclusive`]，
    /// 多次调用串行，避免一次派多个子 agent 打爆本地单端点（对齐 codex/oh-my-pi 并发上限）。
    pub fn sub_agent_exclusive(
        mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        agent: Arc<Agent>,
    ) -> Self {
        self.tools.register(Arc::new(
            AgentTool::new(name, description, agent).with_concurrency(ToolConcurrency::Exclusive),
        ));
        self
    }
    pub fn max_steps(mut self, n: usize) -> Self {
        self.budget.max_steps = n;
        self
    }
    pub fn max_depth(mut self, n: usize) -> Self {
        self.budget.max_depth = n;
        self
    }
    pub fn token_budget(mut self, n: usize) -> Self {
        self.budget.token_budget = Some(n);
        self
    }
    pub fn timeout(mut self, d: std::time::Duration) -> Self {
        self.timeout = Some(d);
        self
    }
    /// 注入 subsession 落盘端口（§2.5b）。装配处（webui-bin）用 bot-api adapter 注入。
    pub fn subsession_store(mut self, store: Arc<dyn base_types::SubsessionStore>) -> Self {
        self.subsession_store = Some(store);
        self
    }
    /// 流中途失败的最大重放次数（§2.6 缺陷2）。默认 1；`0`=关闭重放（旧行为）。
    pub fn stream_replays(mut self, n: usize) -> Self {
        self.stream_replays = Some(n);
        self
    }
    /// 注入工具批准 `Always` 持久化端口（§2.11）。装配处（webui-bin）落 `.bot/approvals.json`。
    pub fn approval_store(mut self, store: Arc<dyn base_types::ApprovalStore>) -> Self {
        self.approval_store = Some(store);
        self
    }
    /// §1.8.8：注入强制召回源（`force_recall` 开时按 query 检索、增广进当前 user 消息）。
    pub fn recall(mut self, src: Arc<dyn agent_act::memory::QueryRecall>) -> Self {
        self.recall = Some(src);
        self
    }
    /// §1.8.8 S4：注入 episode 抽取钩子（每 turn 收口后异步角色条件化抽取）。
    pub fn episodic(mut self, hook: Arc<dyn agent_act::episode::EpisodicHook>) -> Self {
        self.episodic = Some(hook);
        self
    }

    pub fn build(self) -> Arc<Agent> {
        let workdir = self
            .workdir
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let workdir = std::fs::canonicalize(&workdir).unwrap_or(workdir);
        let inner_policy = self.policy.unwrap_or_else(|| Arc::new(AllowAll));
        let mut tools = self.tools;
        if tools.has_discoverable() && !tools.has("tool_search") {
            let search_index = tools.list_tools();
            tools.register(Arc::new(agent_act::tools::ToolSearchTool::new(
                search_index,
            )));
        }
        let history_factory = self
            .history_factory
            .unwrap_or_else(|| Arc::new(|messages| Box::new(VecHistory::with(messages))));
        Arc::new(Agent {
            llm: self.llm.expect("AgentBuilder: llm is required"),
            observe: self.observe.unwrap_or_else(|| Arc::new(AppendObserver)),
            compactor: self.compactor,
            control: self.control.unwrap_or_else(|| Arc::new(UntilQuiet)),
            policy: Arc::new(WorkdirPolicy::new(inner_policy)),
            system: self.system,
            tools: Arc::new(tools),
            history_factory,
            budget: self.budget,
            timeout: self.timeout,
            workdir,
            subsession_store: self.subsession_store,
            stream_replays: self.stream_replays.unwrap_or(1),
            approval_store: self.approval_store,
            recall: self.recall,
            episodic: self.episodic,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base_types::FunctionCall;
    use serde_json::json;

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: args.to_string(),
            },
        }
    }

    #[test]
    fn workdir_policy_allows_file_reads_inside_workdir() {
        let wd = std::env::temp_dir().join(format!("botobot-workdir-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&wd).unwrap();
        std::fs::write(wd.join("ok.txt"), "ok").unwrap();

        let p = WorkdirPolicy::new(Arc::new(AllowAll));
        let verdict = p.check(&call("read", json!({"url":"ok.txt"})), ToolTier::Read, &wd);
        assert!(matches!(verdict, Verdict::Allow));

        let _ = std::fs::remove_dir_all(&wd);
    }

    #[test]
    fn workdir_policy_allows_file_urls_inside_workdir() {
        let wd = std::env::temp_dir().join(format!("botobot-workdir-{}", uuid::Uuid::new_v4()));
        let file = wd.join("ok.txt");
        std::fs::create_dir_all(&wd).unwrap();
        std::fs::write(&file, "ok").unwrap();
        let file_url = format!("file:///{}", file.display().to_string().replace('\\', "/"));

        let p = WorkdirPolicy::new(Arc::new(AllowAll));
        let verdict = p.check(&call("read", json!({"url": file_url})), ToolTier::Read, &wd);
        assert!(matches!(verdict, Verdict::Allow));

        let _ = std::fs::remove_dir_all(&wd);
    }

    #[test]
    fn workdir_policy_denies_file_reads_outside_workdir() {
        let root = std::env::temp_dir().join(format!("botobot-workdir-{}", uuid::Uuid::new_v4()));
        let wd = root.join("wd");
        std::fs::create_dir_all(&wd).unwrap();
        std::fs::write(root.join("secret.txt"), "secret").unwrap();

        let p = WorkdirPolicy::new(Arc::new(AllowAll));
        let verdict = p.check(
            &call("read", json!({"url":"../secret.txt"})),
            ToolTier::Read,
            &wd,
        );
        assert!(matches!(verdict, Verdict::Deny(_)));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn workdir_policy_understands_read_selectors() {
        let wd = std::env::temp_dir().join(format!("botobot-workdir-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&wd).unwrap();
        std::fs::write(wd.join("ok.txt"), "one\ntwo\n").unwrap();

        let p = WorkdirPolicy::new(Arc::new(AllowAll));
        let verdict = p.check(
            &call("read", json!({"url":"ok.txt:2"})),
            ToolTier::Read,
            &wd,
        );
        assert!(matches!(verdict, Verdict::Allow));

        let verdict = p.check(
            &call("read", json!({"url":"../secret.txt:raw"})),
            ToolTier::Read,
            &wd,
        );
        assert!(matches!(verdict, Verdict::Deny(_)));

        let _ = std::fs::remove_dir_all(&wd);
    }

    #[test]
    fn workdir_policy_does_not_block_managed_resource_schemes() {
        let p = WorkdirPolicy::new(Arc::new(AllowAll));
        for url in [
            "skill://review",
            "http://127.0.0.1:8787/api/health",
            "blob:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            let verdict = p.check(
                &call("read", json!({ "url": url })),
                ToolTier::Read,
                Path::new("D:/definitely-not-used"),
            );
            assert!(matches!(verdict, Verdict::Allow), "{url} should pass");
        }
    }
}
