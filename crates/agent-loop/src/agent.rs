//! Agent 本体：Step 循环 + 入口方法 + 任务局部 `CALL_CX` 调用上下文。

use futures::StreamExt;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_util::sync::CancellationToken;

use agent_act::compact::{Compactor, est_msg, estimate, tail_cutpoint};
use agent_act::lsp::{LspArgs, LspOp, run_lsp};
use base_types::{
    AgentEvent, ApprovalDecision, ApprovalResponse, Budget, ContentPart, Context, Control,
    Decision, EventSink, Flow, Llm, LlmError, LlmEvent, LlmOpts, Message, Observe, Policy, Role,
    ToolCall, ToolConcurrency, ToolCtx, ToolLoadMode, ToolLookup, ToolOutcome, ToolSpec, Verdict,
};

// 父循环把"调用上下文"经 task-local 旁路注入工具调用：叶子工具忽略它，
// AgentTool（子 agent）读它以共享 sink、级联取消、递增深度。
tokio::task_local! {
    pub(crate) static CALL_CX: CallCx;
}

#[derive(Clone)]
pub(crate) struct CallCx {
    pub(crate) session_id: String,
    pub(crate) sink: EventSink,
    pub(crate) cancel: CancellationToken,
    pub(crate) parent_run_id: String,
    pub(crate) workdir: PathBuf,
    pub(crate) depth: usize,
    pub(crate) max_depth: usize,
    pub(crate) token_budget: Option<usize>,
    pub(crate) token_spent: Arc<AtomicUsize>,
    /// subsession 落盘端口（§2.5b），逐层传播到任意深度 subagent。
    pub(crate) subsession_store: Option<Arc<dyn base_types::SubsessionStore>>,
}

pub(crate) type SteerableSessionRun = (
    UnboundedReceiverStream<AgentEvent>,
    oneshot::Receiver<Vec<Message>>,
    mpsc::UnboundedSender<Vec<ContentPart>>,
    mpsc::UnboundedSender<ApprovalResponse>,
    CancellationToken,
    // §2.6 缺陷3 阶0：本轮 finalized message 的增量流（崩溃恢复 rollout，驱动器写 scratch）。
    mpsc::UnboundedReceiver<Message>,
);

/// 同类请求的稳定去重 key（§2.11）：工具名 + 原始 arguments 串。
/// **窄定义=安全**：只对**完全相同**的调用自动放行/拒绝，绝不把「允许 git status」泛化成
/// 「允许 rm -rf」。
fn dedup_key(tc: &ToolCall) -> String {
    format!("{}::{}", tc.function.name, tc.function.arguments)
}

#[derive(Clone)]
pub(crate) struct ApprovalBroker {
    pending: Arc<AsyncMutex<HashMap<String, oneshot::Sender<ApprovalResponse>>>>,
    /// §2.11 per-session 放行集（Session/Always 决策落此；命中即静默放行）。
    session_allows: Arc<AsyncMutex<HashMap<String, HashSet<String>>>>,
    /// §2.11 per-session 拒绝锁（Deny 决策落此；命中即静默拒绝、不再追问）。
    denied: Arc<AsyncMutex<HashMap<String, HashSet<String>>>>,
    /// §2.11 跨会话永久放行集（Always）。启动从 `store` 载入；命中即静默放行。
    always: Arc<AsyncMutex<HashSet<String>>>,
    /// §2.11 Always 持久化端口；`None` 时 Always 降级为本会话行为。
    store: Option<Arc<dyn base_types::ApprovalStore>>,
}

impl ApprovalBroker {
    fn new(
        mut rx: mpsc::UnboundedReceiver<ApprovalResponse>,
        store: Option<Arc<dyn base_types::ApprovalStore>>,
    ) -> Self {
        let pending = Arc::new(AsyncMutex::new(HashMap::<
            String,
            oneshot::Sender<ApprovalResponse>,
        >::new()));
        // 启动载入持久化的 Always 放行集（跨进程/会话）。
        let always: HashSet<String> = store
            .as_ref()
            .map(|s| s.load_always().into_iter().collect())
            .unwrap_or_default();
        let broker = Self {
            pending: pending.clone(),
            session_allows: Arc::new(AsyncMutex::new(HashMap::new())),
            denied: Arc::new(AsyncMutex::new(HashMap::new())),
            always: Arc::new(AsyncMutex::new(always)),
            store,
        };
        tokio::spawn(async move {
            while let Some(response) = rx.recv().await {
                if let Some(tx) = pending.lock().await.remove(&response.approval_id) {
                    let _ = tx.send(response);
                }
            }
        });
        broker
    }

    async fn request(
        &self,
        ctx: &Context,
        tc: &ToolCall,
        args: Value,
        tier: base_types::ToolTier,
        reason: String,
    ) -> Result<(), String> {
        // §2.11 fail-closed 短路：先查拒绝锁（同会话已 Deny 同类→静默拒绝），再查放行集
        //（同会话已 Session/Always 同类→静默放行）；都不命中才真正弹批准。
        let key = dedup_key(tc);
        if self
            .denied
            .lock()
            .await
            .get(&ctx.session_id)
            .is_some_and(|s| s.contains(&key))
        {
            return Err("denied (locked this session)".into());
        }
        if self
            .session_allows
            .lock()
            .await
            .get(&ctx.session_id)
            .is_some_and(|s| s.contains(&key))
        {
            return Ok(());
        }
        // 永久放行集（Always，跨会话）。
        if self.always.lock().await.contains(&key) {
            return Ok(());
        }

        let approval_id = new_id();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(approval_id.clone(), tx);
        ctx.sink.emit(AgentEvent::ApprovalRequest {
            session_id: ctx.session_id.clone(),
            run_id: ctx.run_id.clone(),
            approval_id: approval_id.clone(),
            call_id: tc.id.clone(),
            name: tc.function.name.clone(),
            tier,
            reason,
            args,
        });

        let response = tokio::select! {
            _ = ctx.cancel.cancelled() => {
                self.pending.lock().await.remove(&approval_id);
                return Err("approval cancelled".into());
            }
            response = rx => response.map_err(|_| "approval channel closed".to_string())?,
        };
        self.pending.lock().await.remove(&approval_id);
        let approved = response.decision.allows();
        ctx.sink.emit(AgentEvent::ApprovalResolved {
            session_id: ctx.session_id.clone(),
            run_id: ctx.run_id.clone(),
            approval_id,
            approved,
            reason: response.reason.clone(),
        });
        // §2.11 落档：Session/Always 记入放行集（同会话同类自动放行）；Deny 记入拒绝锁。
        // ⚠️ Always 持久化端口未接（Stage 2），现降级为本会话放行。
        match response.decision {
            ApprovalDecision::Once => Ok(()),
            ApprovalDecision::Session => {
                self.session_allows
                    .lock()
                    .await
                    .entry(ctx.session_id.clone())
                    .or_default()
                    .insert(key);
                Ok(())
            }
            ApprovalDecision::Always => {
                // 永久：进 always 集 + 持久化（端口缺省时仅本进程内存，降级 Session 语义）。
                self.always.lock().await.insert(key.clone());
                if let Some(store) = &self.store {
                    store.persist_always(&key);
                }
                Ok(())
            }
            ApprovalDecision::Deny => {
                self.denied
                    .lock()
                    .await
                    .entry(ctx.session_id.clone())
                    .or_default()
                    .insert(key);
                Err(response.reason.unwrap_or_else(|| "approval denied".into()))
            }
        }
    }
}

impl Default for CallCx {
    fn default() -> Self {
        Self {
            session_id: new_id(),
            sink: EventSink::null(),
            cancel: CancellationToken::new(),
            parent_run_id: String::new(),
            workdir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            depth: 0,
            max_depth: Budget::default().max_depth,
            token_budget: Budget::default().token_budget,
            token_spent: Arc::new(AtomicUsize::new(0)),
            subsession_store: None,
        }
    }
}

/// agent 唯一公开的错误类型，驱动器 / 顶层入口 / AgentTool 三处共用。
#[derive(Debug, Error)]
pub enum AgentError {
    #[error("llm error: {0}")]
    Llm(#[from] LlmError),
    #[error("cancelled")]
    Cancelled,
    #[error("step budget exhausted")]
    BudgetExhausted,
    #[error("token budget exhausted: used {used}, requested {requested}, budget {budget}")]
    TokenBudgetExhausted {
        used: usize,
        requested: usize,
        budget: usize,
    },
    /// 保留扩展点：未来若把 max_depth 检查从 AgentTool 错误改成驱动器终结，可启用。
    #[allow(dead_code)]
    #[error("max depth exceeded")]
    DepthExceeded,
}

pub struct Agent {
    pub(crate) llm: Arc<dyn Llm>,
    pub(crate) observe: Arc<dyn Observe>,
    /// 窗口压缩器（决策⑧）。`None`=不压缩；`Some`=驱动器跑检测/兜底，且 `CompactTool` 已注册。
    pub(crate) compactor: Option<Compactor>,
    /// 循环控制（决策⑦）：判定一步 reason 后停还是继续。默认 `UntilQuiet`。
    pub(crate) control: Arc<dyn Control>,
    /// 安全策略（决策⑦）：每个工具调用执行前过闸。默认 `AllowAll`。
    pub(crate) policy: Arc<dyn Policy>,
    pub(crate) system: Option<String>,
    pub(crate) tools: Arc<dyn ToolLookup>,
    pub(crate) history_factory: crate::HistoryFactory,
    pub(crate) budget: Budget,
    pub(crate) timeout: Option<std::time::Duration>,
    pub(crate) workdir: PathBuf,
    /// subsession 落盘端口（§2.5b）。装配处注入（webui-bin），播种进顶层 Context。
    pub(crate) subsession_store: Option<Arc<dyn base_types::SubsessionStore>>,
    /// 流中途失败的最大重放次数（§2.6 缺陷2 收尾）。0=不重放（旧行为）。
    pub(crate) stream_replays: usize,
    /// §2.11 Always 持久化端口。装配处注入（webui-bin 落 `.bot/approvals.json`）；
    /// `None` 时 `Always` 降级为本会话 Session 行为。
    pub(crate) approval_store: Option<Arc<dyn base_types::ApprovalStore>>,
    /// §1.8.8：强制记忆召回端口。`force_recall` 开时，驱动器按本轮 query 检索、把召回块
    /// 增广进当前 user 消息（不写回 history）。`None`=无召回源。（取代 §1.8.7 live_prefix。）
    pub(crate) recall: Option<Arc<dyn agent_act::memory::QueryRecall>>,
    /// §1.8.8 S4：每 turn 收口后的 episode 抽取钩子（fire-and-forget，自带限流）。`None`=不抽取。
    pub(crate) episodic: Option<Arc<dyn agent_act::episode::EpisodicHook>>,
}

/// §4.9 A2：是否「请求过大」错误（413 或 body 明示 payload/entity too large）。
fn is_payload_too_large(e: &AgentError) -> bool {
    match e {
        AgentError::Llm(LlmError::Api { status, body, .. }) => {
            *status == 413 || {
                let b = body.to_lowercase();
                b.contains("too large") || b.contains("payload") && b.contains("large")
            }
        }
        _ => false,
    }
}

/// §4.9 A2：消息里是否含图像 part。
fn has_images(msgs: &[Message]) -> bool {
    msgs.iter().any(|m| {
        m.content
            .iter()
            .any(|p| matches!(p, ContentPart::ImageUrl(_)))
    })
}

/// §4.9 A2：剥掉图像 part（换成占位文本），用于 413 恢复重试——不动持久 history，只动本次发送副本。
fn strip_images(msgs: &[Message]) -> Vec<Message> {
    msgs.iter()
        .map(|m| {
            let mut m = m.clone();
            m.content = m
                .content
                .iter()
                .map(|p| match p {
                    ContentPart::ImageUrl(_) => {
                        ContentPart::Text("[image omitted: request too large]".into())
                    }
                    other => other.clone(),
                })
                .collect();
            m
        })
        .collect()
}

/// 取一条消息的纯文本（拼接 text part；忽略图像）。
fn part_text(m: &Message) -> String {
    m.content
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl Agent {
    pub fn builder() -> crate::AgentBuilder {
        crate::AgentBuilder::default()
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// §4.5：暴露 LLM 句柄——供 team 编排的 leader 规划器（`conduct_team_planned`）复用同一模型。
    pub fn llm_handle(&self) -> Arc<dyn Llm> {
        self.llm.clone()
    }

    /// §5.5 C11：bot 的角色/人格 system prompt（= bot.md 本体；技能/书每轮另注入，不在此）。
    /// 供属性面板「bot.md」tab 只读展示。`None`=未设。
    pub fn system_prompt(&self) -> Option<&str> {
        self.system.as_deref()
    }

    /// §5.5 C11：列出已注册工具的 `(name, tier)` 简介（供 bot 属性面板「工具/subagent」tab 自省）。
    /// tier 渲染成 `read`/`write`/`exec`。按名排序，稳定显示。
    pub fn tool_brief(&self) -> Vec<(String, &'static str)> {
        let mut out: Vec<(String, &'static str)> = self
            .tools
            .list()
            .iter()
            .map(|t| {
                let tier = match t.tier() {
                    base_types::ToolTier::Read => "read",
                    base_types::ToolTier::Write => "write",
                    base_types::ToolTier::Exec => "exec",
                };
                (t.name().to_string(), tier)
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    pub fn with_workdir(&self, workdir: impl Into<PathBuf>) -> Self {
        Self {
            llm: self.llm.clone(),
            observe: self.observe.clone(),
            compactor: self.compactor.clone(),
            control: self.control.clone(),
            policy: self.policy.clone(),
            system: self.system.clone(),
            tools: self.tools.clone(),
            history_factory: self.history_factory.clone(),
            budget: self.budget.clone(),
            timeout: self.timeout,
            workdir: workdir.into(),
            subsession_store: self.subsession_store.clone(),
            stream_replays: self.stream_replays,
            approval_store: self.approval_store.clone(),
            recall: self.recall.clone(),
            episodic: self.episodic.clone(),
        }
    }

    /// §5.7：派生一个**只换角色 system prompt** 的 agent（共享 llm/tools/policy/记忆等一切其余）。
    /// 用于 Hub 按 bot profile 选不同人格——编程 bot 注入 SOP 纪律、通用 bot 用通用 prompt，
    /// 工具集与策略不变。与 [`Self::with_workdir`] 正交、可链式叠加。
    pub fn with_system(&self, system: impl Into<String>) -> Self {
        let mut next = self.with_workdir(self.workdir.clone());
        next.system = Some(system.into());
        next
    }

    /// §5.7 真异构多模型：派生一个**只换底层 LLM**（接到另一个模型端点）的 agent，
    /// 共享 system/tools/policy/记忆/workdir 等一切其余。用于 Hub 给不同 profile 的 bot
    /// 接不同模型——编程 bot 用 coder 模型、通用 bot 用对话模型。与 [`Self::with_system`]、
    /// [`Self::with_workdir`] 正交、可链式叠加。不派生时全 bot 共用同一模型（向后兼容）。
    pub fn with_llm(&self, llm: Arc<dyn Llm>) -> Self {
        let mut next = self.with_workdir(self.workdir.clone());
        next.llm = llm;
        next
    }

    /// 顶层入口（纯文本）。
    pub fn run(
        self: Arc<Self>,
        user_input: impl Into<String>,
    ) -> UnboundedReceiverStream<AgentEvent> {
        self.run_parts(vec![ContentPart::Text(user_input.into())])
    }

    /// 顶层入口（多模态）。
    pub fn run_parts(
        self: Arc<Self>,
        parts: Vec<ContentPart>,
    ) -> UnboundedReceiverStream<AgentEvent> {
        self.run_cancellable(parts, LlmOpts::default()).0
    }

    /// 同 [`Agent::run_parts`]，额外返回可取消句柄（取消沿 child_token 级联到子 agent）。
    pub fn run_cancellable(
        self: Arc<Self>,
        parts: Vec<ContentPart>,
        llm_opts: LlmOpts,
    ) -> (UnboundedReceiverStream<AgentEvent>, CancellationToken) {
        let messages = self.seed(Vec::new(), parts);
        self.spawn_run(new_id(), messages, None, None, None, None, 0, llm_opts)
    }

    /// 多轮入口：带入既有历史，跑完经 oneshot 吐回**更新后的完整历史**。
    /// 历史为空时自动前置 system；非空则视为已含 system（不重复）。
    pub fn run_session(
        self: Arc<Self>,
        session_id: impl Into<String>,
        history: Vec<Message>,
        parts: Vec<ContentPart>,
        llm_opts: LlmOpts,
    ) -> (
        UnboundedReceiverStream<AgentEvent>,
        oneshot::Receiver<Vec<Message>>,
    ) {
        let (htx, hrx) = oneshot::channel();
        let messages = self.seed(history, parts);
        let (stream, _cancel) = self.spawn_run(
            session_id.into(),
            messages,
            Some(htx),
            None,
            None,
            None,
            0,
            llm_opts,
        );
        (stream, hrx)
    }

    /// 可 steer 的多轮入口（§7a/P6）：除事件流 + 历史接收端外，再返回一个 **steer 发送端**——
    /// 在 turn 跑动期间发送 `Vec<ContentPart>`，驱动器会在**下一次 reason 之前**把它并进历史
    /// （"wait" 语义：不打断在跑的并发工具批，下一步生效）。供 [`crate::Session`] 使用。
    pub fn run_session_steerable(
        self: Arc<Self>,
        session_id: impl Into<String>,
        history: Vec<Message>,
        parts: Vec<ContentPart>,
        llm_opts: LlmOpts,
    ) -> SteerableSessionRun {
        let (htx, hrx) = oneshot::channel();
        let (stx, srx) = mpsc::unbounded_channel();
        let (atx, arx) = mpsc::unbounded_channel();
        let (dtx, drx) = mpsc::unbounded_channel();
        // §2.6 缺陷3 阶0：persist_from = 入轮前历史长度（= 驱动器 persisted_len），seed 后
        // 新增的 user 消息及其后全部 finalized message 经 dtx 增量上抛。
        let persist_from = history.len();
        let messages = self.seed(history, parts);
        let (stream, cancel) = self.spawn_run(
            session_id.into(),
            messages,
            Some(htx),
            Some(srx),
            Some(arx),
            Some(dtx),
            persist_from,
            llm_opts,
        );
        (stream, hrx, stx, atx, cancel, drx)
    }

    fn seed(&self, history: Vec<Message>, parts: Vec<ContentPart>) -> Vec<Message> {
        let mut messages = history;
        if messages.is_empty() {
            if let Some(sys) = &self.system {
                messages.push(Message::system(sys.clone()));
            }
        }
        messages.push(Message::user_parts(parts));
        messages
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_run(
        self: &Arc<Self>,
        session_id: String,
        messages: Vec<Message>,
        hist_tx: Option<oneshot::Sender<Vec<Message>>>,
        steer_rx: Option<mpsc::UnboundedReceiver<Vec<ContentPart>>>,
        approval_rx: Option<mpsc::UnboundedReceiver<ApprovalResponse>>,
        delta_tx: Option<mpsc::UnboundedSender<Message>>,
        persist_from: usize,
        llm_opts: LlmOpts,
    ) -> (UnboundedReceiverStream<AgentEvent>, CancellationToken) {
        let (sink, rx) = EventSink::channel();
        let cancel = CancellationToken::new();
        let ctx = Context {
            session_id,
            history: self.history_from(messages),
            workdir: self.workdir.clone(),
            tools: self.tools.clone(),
            sink,
            run_id: new_id(),
            parent_id: None,
            cancel: cancel.clone(),
            budget: self.budget.clone(),
            token_spent: Arc::new(AtomicUsize::new(0)),
            llm_opts,
            subsession_store: self.subsession_store.clone(),
        };

        if let Some(d) = self.timeout {
            let c = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(d).await;
                c.cancel();
            });
        }

        let me = self.clone();
        tokio::spawn(async move {
            let _ = me
                .run_loop(ctx, hist_tx, steer_rx, approval_rx, delta_tx, persist_from)
                .await;
        });
        (UnboundedReceiverStream::new(rx), cancel)
    }

    pub(crate) fn history_from(&self, messages: Vec<Message>) -> Box<dyn base_types::History> {
        (self.history_factory)(messages)
    }

    /// Step 循环引擎（心跳）。被顶层 run 与 AgentTool 共用。
    /// `hist_tx`：若有，结束时回传最终历史 `ctx.history.take()`（多轮会话用）。
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_loop(
        &self,
        mut ctx: Context,
        hist_tx: Option<oneshot::Sender<Vec<Message>>>,
        mut steer_rx: Option<mpsc::UnboundedReceiver<Vec<ContentPart>>>,
        approval_rx: Option<mpsc::UnboundedReceiver<ApprovalResponse>>,
        // §2.6 缺陷3 阶0：finalized message 增量上抛通道（崩溃恢复 rollout）。None=不上抛（子 agent/测试）。
        delta_tx: Option<mpsc::UnboundedSender<Message>>,
        persist_from: usize,
    ) -> Result<String, AgentError> {
        let approvals = approval_rx.map(|rx| ApprovalBroker::new(rx, self.approval_store.clone()));
        ctx.sink.emit(AgentEvent::Start {
            session_id: ctx.session_id.clone(),
            run_id: ctx.run_id.clone(),
            parent_id: ctx.parent_id.clone(),
        });
        tracing::info!(target: "botobot::loop", run = %ctx.run_id, depth = ctx.budget.depth, "run start");

        // §2.6 缺陷3 阶0：本轮入口的新消息（seed 加的 user 消息）先上抛——崩溃至多丢「在途未完成那条」。
        // 后续在各 push 点逐条上抛（compaction 只动旧消息且发生在迭代顶，按 push 点取值天然免疫索引漂移）。
        for m in ctx.history.view().iter().skip(persist_from) {
            emit_delta(&delta_tx, m);
        }

        let mut activated_tools = BTreeSet::new();
        let mut steps = 0usize;
        let mut hinted = false;
        // §2.7 token live：本 run 累计已花 token（always-on，独立于预算计费——`token_spent`
        // 仅在设预算时累加，故这里另开一份供 live 显示；优先 provider usage、否则估算）。
        let mut run_tokens = 0usize;
        // §4.9 B2：soft 触发的**后台预摘要**句柄 + 计划，跨 iter 存活；hard 时即时套用（不阻塞）。
        let mut pending_summary: Option<(SummarizePlan, tokio::task::JoinHandle<Option<String>>)> =
            None;

        let result = loop {
            if ctx.cancel.is_cancelled() {
                break Err(AgentError::Cancelled);
            }
            if steps >= ctx.budget.max_steps {
                break Err(AgentError::BudgetExhausted);
            }
            steps += 1;

            // steer（§7a/P6，"wait" 语义）：把 turn 跑动期间外部注入的新输入并进历史，
            // 在下一次 reason 之前生效（不打断已派发的并发工具批）。
            if let Some(rx) = steer_rx.as_mut() {
                while let Ok(parts) = rx.try_recv() {
                    let m = Message::user_parts(parts);
                    emit_delta(&delta_tx, &m);
                    ctx.history.push(m);
                }
            }

            // 窗口压缩（决策⑧）的两个**硬**环节（reason 前）；**软**环节=模型调 compact 工具（见派发拦截）。
            // 触发会计=BodyAfterPrefix（借鉴 codex）：比「前缀之后增长量」而非整段，防抖动 + 保前缀 KV 缓存。
            if let Some(c) = &self.compactor {
                if c.should_force(ctx.history.view()) {
                    let est = estimate(ctx.history.view());
                    tracing::info!(target: "botobot::loop", run = %ctx.run_id, est, hard = c.hard, "over hard → force compact");
                    // §4.9 B2：优先用 soft 时已 kick off 的**后台预摘要**即时套用（不阻塞 turn）；
                    // 无预摘要 / 已失效 / 边界已变 → 同步兜底（原行为）。
                    let summarized = match pending_summary.take() {
                        Some((plan, handle)) => match handle.await.ok().flatten() {
                            Some(s) => {
                                let n = self.apply_summary(&mut ctx, &plan, &s);
                                if n > 0 {
                                    tracing::info!(target: "botobot::loop", run = %ctx.run_id, n, "applied async pre-summary");
                                    n
                                } else {
                                    self.try_llm_summarize_history(&mut ctx, c).await
                                }
                            }
                            None => self.try_llm_summarize_history(&mut ctx, c).await,
                        },
                        None => self.try_llm_summarize_history(&mut ctx, c).await,
                    };
                    let dropped = if c.should_force(ctx.history.view()) {
                        c.compact(&mut ctx.history)
                    } else {
                        0
                    };
                    tracing::info!(target: "botobot::loop", run = %ctx.run_id, summarized, dropped, "hard compact complete");
                    // 已知限制（Q3 degrade 盲区）：若压缩后仍越 hard 且分文未减，bloat 多半在
                    // 「钉住的连续 system 前缀」——三层机械折叠都不动前缀，无法救。告警以可观测。
                    if summarized == 0 && dropped == 0 && c.should_force(ctx.history.view()) {
                        tracing::warn!(target: "botobot::loop", run = %ctx.run_id,
                            prefix = c.prefix_tokens(ctx.history.view()), hard = c.hard,
                            "compaction 无法降到 hard 以下（前缀超阈值，机械折叠压不动 system 前缀）—— 可能仍超真实窗口");
                    }
                    hinted = false;
                } else if c.over_soft(ctx.history.view()) {
                    // §4.9 B2：越软线 → **后台预摘要**一次（快照老区、不阻塞，hard 时即时套用）。
                    if pending_summary.is_none() {
                        if let Some(plan) = self.plan_summarize(ctx.history.view(), c) {
                            let llm = self.llm.clone();
                            let selected = plan.selected.clone();
                            let soft = c.soft;
                            let handle =
                                tokio::spawn(
                                    async move { run_summarize(llm, selected, soft).await },
                                );
                            pending_summary = Some((plan, handle));
                            tracing::info!(target: "botobot::loop", run = %ctx.run_id, "over soft → spawned async pre-summary");
                        }
                    }
                    if !hinted {
                        // 检测硬提示：注一条 system，提示模型可自行调用 compact（一次/越线）。
                        let m = Message::system(
                            "上下文较大，可调用 compact 工具压缩较早的历史以释放窗口。",
                        );
                        emit_delta(&delta_tx, &m);
                        ctx.history.push(m);
                        hinted = true;
                    }
                }
            }

            // §1.8.7 E 步：构造本轮发给 LLM 的消息 = history + 临时注入的易变记忆块（不写回 history）。
            let send_messages = self.build_send_messages(&ctx).await;

            // 细节(debug)：本次 reason 发给 llm 的消息上下文（含注入的记忆块）—— 也下发前端。
            ctx.sink.emit(AgentEvent::Debug {
                session_id: ctx.session_id.clone(),
                run_id: ctx.run_id.clone(),
                label: "llm_request".into(),
                data: serde_json::to_value(&send_messages).unwrap_or(Value::Null),
            });

            let prompt_tokens = estimate(&send_messages);
            if let Err(e) = charge_token_budget(&ctx, prompt_tokens) {
                break Err(e);
            }

            let specs = self.tool_specs_for(&ctx.llm_opts, &activated_tools);
            let first = tokio::select! {
                biased;
                _ = ctx.cancel.cancelled() => break Err(AgentError::Cancelled),
                d = self.infer_collect(&ctx, &send_messages, &specs) => d,
            };
            let decision = match first {
                Ok(d) => d,
                // §4.9 A2：413「请求过大」专用恢复——多为图片 base64 撑爆 body。剥掉图片重试一次
                // （区别于普通 token 预算）；仍失败才中止。无图片则无法靠剥图恢复，照常中止。
                Err(e) if is_payload_too_large(&e) && has_images(&send_messages) => {
                    let stripped = strip_images(&send_messages);
                    ctx.sink.emit(AgentEvent::Debug {
                        session_id: ctx.session_id.clone(),
                        run_id: ctx.run_id.clone(),
                        label: "payload_413_recovery".into(),
                        data: Value::String(
                            "request too large — retrying without image data".into(),
                        ),
                    });
                    let retry = tokio::select! {
                        biased;
                        _ = ctx.cancel.cancelled() => break Err(AgentError::Cancelled),
                        d = self.infer_collect(&ctx, &stripped, &specs) => d,
                    };
                    match retry {
                        Ok(d) => d,
                        Err(e2) => break Err(e2),
                    }
                }
                Err(e) => break Err(e),
            };
            let post_tokens = post_infer_tokens(&decision, prompt_tokens);
            if let Err(e) = charge_token_budget(&ctx, post_tokens) {
                break Err(e);
            }
            // §2.7 token live：累加本次 infer（prompt + 产出）并上抛累计，前端 live 显示「已用/预算」。
            run_tokens = run_tokens
                .saturating_add(prompt_tokens)
                .saturating_add(post_tokens);
            ctx.sink.emit(AgentEvent::Usage {
                session_id: ctx.session_id.clone(),
                run_id: ctx.run_id.clone(),
                spent: run_tokens,
                budget: ctx.budget.token_budget,
            });

            // 循环控制缝（决策⑦）：模型这步是收口还是继续？默认 UntilQuiet=无 tool_calls 即停。
            if let Flow::Stop = self.control.next(&decision, steps) {
                let m = Message::assistant(decision.text.clone());
                emit_delta(&delta_tx, &m);
                ctx.history.push(m);
                break Ok(decision.text);
            }

            let m = Message::assistant_calls(decision.text.clone(), decision.tool_calls.clone());
            emit_delta(&delta_tx, &m);
            ctx.history.push(m);

            // 拆分：compact 控制工具由**驱动器拦截**（持 &mut Context 串行改 memory，避并发写）；
            // 其余普通工具照常并发派发。
            let (compact_calls, normal_calls): (Vec<&ToolCall>, Vec<&ToolCall>) = decision
                .tool_calls
                .iter()
                .partition(|tc| self.compactor.is_some() && tc.function.name == "compact");

            // 动作：默认并发执行；声明 exclusive 的工具在前后并发批之间单独串行。
            // 调用上下文经 task-local 旁路注入（供 AgentTool 用）。
            let cx = CallCx {
                session_id: ctx.session_id.clone(),
                sink: ctx.sink.clone(),
                cancel: ctx.cancel.child_token(),
                parent_run_id: ctx.run_id.clone(),
                workdir: ctx.workdir.clone(),
                depth: ctx.budget.depth,
                max_depth: ctx.budget.max_depth,
                token_budget: ctx.budget.token_budget,
                token_spent: ctx.token_spent.clone(),
                subsession_store: ctx.subsession_store.clone(),
            };
            let dispatch = self.dispatch_tools(&ctx, normal_calls, &activated_tools, &approvals);
            // 与 infer 同样把工具派发 race against cancel：长工具（shell/子 agent/http）运行中
            // 取消能**当场打断**，不必等工具跑完。子 token（cx.cancel=ctx.cancel.child_token()）
            // 也已传给各工具，配合写了 cancel 分支的工具（如 shell_command 杀进程）做到优雅停。
            let outcomes = tokio::select! {
                biased;
                _ = ctx.cancel.cancelled() => break Err(AgentError::Cancelled),
                o = CALL_CX.scope(cx, dispatch) => o,
            };
            activate_tools_from_outcomes(&mut activated_tools, &outcomes);

            // 观察：把整轮普通工具结果折回上下文。
            // §2.6 缺陷3 阶0：记录 observe 前长度，observe 后上抛新增的 tool_result（此间无 compaction）。
            let before_observe = ctx.history.view().len();
            self.observe.observe(&mut ctx, outcomes).await;
            for m in ctx.history.view().iter().skip(before_observe) {
                emit_delta(&delta_tx, m);
            }

            // 软压：模型主动请求的 compact，驱动器串行执行并补 tool_result。
            for tc in compact_calls {
                self.run_compact(&mut ctx, tc).await;
                hinted = false;
            }
        };

        match &result {
            Ok(output) => {
                tracing::info!(target: "botobot::loop", run = %ctx.run_id, steps, "run done");
                ctx.sink.emit(AgentEvent::Done {
                    session_id: ctx.session_id.clone(),
                    run_id: ctx.run_id.clone(),
                    output: output.clone(),
                });
                // §1.8.8 S4：turn 成功收口后，异步角色条件化抽取 episode（hook 自带限流）。
                if let Some(ep) = &self.episodic {
                    let msgs = ctx.history.view();
                    let role = msgs
                        .iter()
                        .find(|m| m.role == Role::System)
                        .map(part_text)
                        .unwrap_or_default();
                    // 本轮 = 从最后一条 user 消息到结尾。
                    let start = msgs.iter().rposition(|m| m.role == Role::User).unwrap_or(0);
                    let transcript: Vec<Message> = msgs[start..].to_vec();
                    ep.on_turn_complete(ctx.session_id.clone(), role, transcript);
                }
            }
            Err(e) => {
                tracing::warn!(target: "botobot::loop", run = %ctx.run_id, steps, error = %e, "run terminated");
                ctx.sink.emit(AgentEvent::Error {
                    session_id: ctx.session_id.clone(),
                    run_id: ctx.run_id.clone(),
                    message: e.to_string(),
                })
            }
        }
        if let Some(tx) = hist_tx {
            let _ = tx.send(ctx.history.take());
        }
        result
    }

    // (emit_delta 见模块尾部自由函数)

    /// §1.8.8：构造**本轮发给 LLM 的消息**（不改 `ctx.history`）。
    /// `force_recall` 开时，按最后一条 user 消息文本检索记忆，把召回块**增广进该 user 消息内容**
    /// （send-time only、对话尾部、不写回 history）——不破坏可缓存的 system 前缀。
    /// （取代 §1.8.7 E 步那条每轮第二 system 块。）
    async fn build_send_messages(&self, ctx: &Context) -> Vec<Message> {
        let mut send = ctx.history.view().to_vec();
        if ctx.llm_opts.force_recall {
            if let Some(recall) = &self.recall {
                if let Some(idx) = send.iter().rposition(|m| m.role == Role::User) {
                    let query: String = send[idx]
                        .content
                        .iter()
                        .filter_map(|p| match p {
                            ContentPart::Text(t) => Some(t.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    if !query.trim().is_empty() {
                        if let Some(block) = recall.recall_block(&query).await {
                            // 增广进最后一条 user 消息（前置一段文本 part）。
                            send[idx]
                                .content
                                .insert(0, ContentPart::Text(format!("{}\n", block.trim_end())));
                        }
                    }
                }
            }
        }
        send
    }

    async fn infer_collect(
        &self,
        ctx: &Context,
        messages: &[Message],
        specs: &[ToolSpec],
    ) -> Result<Decision, AgentError> {
        // §2.6 缺陷2 收尾：mid-stream re-infer 重放。
        // 初次建流的 Err 由 `?` 上抛（RetryLlm 已在 infer() 层重试建流）；流**中途**失败
        // 则在此重放——重放前发 `StreamReset` 让前端清空本 run 已 emit 的部分文本，
        // 避免重新生成造成重复输出（幂等保证）。仅瞬时错误（含 idle timeout）且未超次数才重放。
        let mut attempt = 0usize;
        loop {
            let mut stream = self.llm.infer(messages, specs, &ctx.llm_opts).await?;
            let mut decision = Decision::default();
            let mut emitted = 0usize;
            let mut stream_err: Option<LlmError> = None;
            while let Some(ev) = stream.next().await {
                match ev {
                    Ok(ev) => {
                        emitted += 1;
                        match ev {
                            LlmEvent::TextDelta(t) => ctx.sink.emit(AgentEvent::Token {
                                session_id: ctx.session_id.clone(),
                                run_id: ctx.run_id.clone(),
                                text: t,
                            }),
                            LlmEvent::ReasoningDelta(t) => ctx.sink.emit(AgentEvent::Reasoning {
                                session_id: ctx.session_id.clone(),
                                run_id: ctx.run_id.clone(),
                                text: t,
                            }),
                            LlmEvent::Done(d) => decision = d,
                        }
                    }
                    Err(e) => {
                        stream_err = Some(e);
                        break;
                    }
                }
            }
            match stream_err {
                None => return Ok(decision),
                Some(e) => {
                    if e.is_transient() && attempt < self.stream_replays {
                        attempt += 1;
                        tracing::warn!(
                            target: "botobot::llm",
                            error = %e,
                            emitted,
                            attempt,
                            max = self.stream_replays,
                            "流中途失败，重放（清空已 emit 的部分文本后重新推理）"
                        );
                        // 幂等：清空前端本 run 已 emit 的部分答案/推理，重放重新生成不重复。
                        ctx.sink.emit(AgentEvent::StreamReset {
                            session_id: ctx.session_id.clone(),
                            run_id: ctx.run_id.clone(),
                        });
                        continue;
                    }
                    tracing::warn!(
                        target: "botobot::llm",
                        error = %e,
                        emitted,
                        "流中途中止（不可重放或已超重放次数；按两级错误中止 turn）"
                    );
                    return Err(e.into());
                }
            }
        }
    }

    async fn dispatch_tools(
        &self,
        ctx: &Context,
        calls: Vec<&ToolCall>,
        activated_tools: &BTreeSet<String>,
        approvals: &Option<ApprovalBroker>,
    ) -> Vec<ToolOutcome> {
        let mut outcomes = Vec::new();
        let mut batch = Vec::new();

        for tc in calls {
            if self.tool_concurrency(ctx, tc, activated_tools) == ToolConcurrency::Exclusive {
                if !batch.is_empty() {
                    outcomes.extend(
                        self.invoke_batch(
                            ctx,
                            std::mem::take(&mut batch),
                            activated_tools,
                            approvals,
                        )
                        .await,
                    );
                }
                outcomes.push(self.invoke_tool(ctx, tc, activated_tools, approvals).await);
            } else {
                batch.push(tc);
            }
        }

        if !batch.is_empty() {
            outcomes.extend(
                self.invoke_batch(ctx, batch, activated_tools, approvals)
                    .await,
            );
        }
        outcomes
    }

    async fn invoke_batch(
        &self,
        ctx: &Context,
        calls: Vec<&ToolCall>,
        activated_tools: &BTreeSet<String>,
        approvals: &Option<ApprovalBroker>,
    ) -> Vec<ToolOutcome> {
        futures::future::join_all(
            calls
                .into_iter()
                .map(|tc| self.invoke_tool(ctx, tc, activated_tools, approvals)),
        )
        .await
    }

    fn tool_concurrency(
        &self,
        ctx: &Context,
        tc: &ToolCall,
        activated_tools: &BTreeSet<String>,
    ) -> ToolConcurrency {
        if !self.tool_available_for(ctx, &tc.function.name, activated_tools) {
            return ToolConcurrency::Concurrent;
        }
        ctx.tools
            .get(&tc.function.name)
            .map(|tool| tool.concurrency())
            .unwrap_or(ToolConcurrency::Concurrent)
    }

    async fn invoke_tool(
        &self,
        ctx: &Context,
        tc: &ToolCall,
        activated_tools: &BTreeSet<String>,
        approvals: &Option<ApprovalBroker>,
    ) -> ToolOutcome {
        let args: Value = serde_json::from_str(&tc.function.arguments).unwrap_or(Value::Null);
        ctx.sink.emit(AgentEvent::ToolStart {
            session_id: ctx.session_id.clone(),
            run_id: ctx.run_id.clone(),
            call_id: tc.id.clone(),
            name: tc.function.name.clone(),
            args: args.clone(),
        });

        // 安全阀（决策⑦ + §7a 分级）：先解析工具拿到 tier，执行前过 Policy 闸。
        // 拒绝则不执行，理由作为带内错误回喂模型（两级错误，循环不终止）。
        let mut result = match ctx.tools.get(&tc.function.name) {
            _ if !self.tool_available_for(ctx, &tc.function.name, activated_tools) => {
                Err(format!("tool disabled for this turn: {}", tc.function.name))
            }
            None => Err(format!("tool not found: {}", tc.function.name)),
            Some(tool) => match self.policy.check(tc, tool.tier(), &ctx.workdir) {
                Verdict::Deny(reason) => Err(format!("denied by policy: {reason}")),
                Verdict::Prompt { reason } => match approvals {
                    None => Err(format!(
                        "approval required but no approval channel is available: {reason}"
                    )),
                    Some(approvals) => match approvals
                        .request(ctx, tc, args.clone(), tool.tier(), reason)
                        .await
                    {
                        Ok(()) => tool
                            .call_with_context(args.clone(), &ToolCtx::from_context(ctx))
                            .await
                            .map_err(|e| e.to_string()),
                        Err(reason) => Err(format!("denied by approval: {reason}")),
                    },
                },
                Verdict::Allow => tool
                    .call_with_context(args.clone(), &ToolCtx::from_context(ctx))
                    .await
                    .map_err(|e| e.to_string()),
            },
        };
        self.post_write_diagnostics(ctx, tc, &args, &mut result)
            .await;

        // 完整 result 进**模型历史**（observe 用 ToolOutcome，不受此影响）；前端事件只给**预览**，
        // **完整结果走 debug 日志**（用户要求：tool result 是 debug 级别，不下发前端）。
        let full = match &result {
            Ok(Value::String(s)) => s.clone(), // 字符串结果直接用，不要 JSON 引号
            Ok(v) => v.to_string(),
            Err(e) => e.clone(),
        };
        if result.is_ok() {
            tracing::info!(target: "botobot::tool", run = %ctx.run_id, name = %tc.function.name, "ok");
            tracing::debug!(target: "botobot::tool", name = %tc.function.name, result = %full, "result (full)");
        } else {
            tracing::warn!(target: "botobot::tool", run = %ctx.run_id, name = %tc.function.name, error = %full, "failed");
        }
        ctx.sink.emit(AgentEvent::ToolEnd {
            session_id: ctx.session_id.clone(),
            run_id: ctx.run_id.clone(),
            call_id: tc.id.clone(),
            ok: result.is_ok(),
            result: Value::String(tool_preview(&tc.function.name, &full)),
        });
        // 完整结果作为 debug 事件也下发前端（前端决定是否展示）；tool_end 自身只给 info 预览。
        ctx.sink.emit(AgentEvent::Debug {
            session_id: ctx.session_id.clone(),
            run_id: ctx.run_id.clone(),
            label: "tool_result".into(),
            data: match &result {
                Ok(v) => v.clone(),
                Err(_) => Value::String(full.clone()),
            },
        });
        ToolOutcome {
            call: tc.clone(),
            result,
        }
    }

    async fn post_write_diagnostics(
        &self,
        ctx: &Context,
        tc: &ToolCall,
        args: &Value,
        result: &mut Result<Value, String>,
    ) {
        if !should_run_post_write_diagnostics(tc, args, result) {
            return;
        }
        let (ok, summary, data) = if ctx.workdir.join("Cargo.toml").is_file() {
            match run_lsp(
                &ctx.workdir,
                LspArgs {
                    op: LspOp::Diagnostics,
                    path: None,
                    file: None,
                    line: None,
                    column: None,
                    end_line: None,
                    end_column: None,
                    new_name: None,
                    timeout_ms: Some(30_000),
                    max_diagnostics: Some(50),
                },
            )
            .await
            {
                Ok(data) => (true, diagnostics_summary(&data), data),
                Err(err) => (
                    false,
                    format!("写后 diagnostics 失败：{err}"),
                    serde_json::json!({ "error": err.to_string() }),
                ),
            }
        } else {
            (
                true,
                "写后 diagnostics：未发现 Cargo.toml，已跳过".into(),
                serde_json::json!({ "skipped": true, "reason": "missing Cargo.toml" }),
            )
        };

        if let Ok(Value::Object(map)) = result {
            map.insert("post_write_diagnostics".into(), data.clone());
        }
        ctx.sink.emit(AgentEvent::Diagnostics {
            session_id: ctx.session_id.clone(),
            run_id: ctx.run_id.clone(),
            source_call_id: tc.id.clone(),
            ok,
            summary,
            data,
        });
    }

    /// 驱动器拦截的 compact（R-C）：持 `&mut Context` 串行改写 `ctx.history`，发可观测事件并补 tool_result。
    async fn run_compact(&self, ctx: &mut Context, tc: &ToolCall) {
        let Some(c) = &self.compactor else { return };
        ctx.sink.emit(AgentEvent::ToolStart {
            session_id: ctx.session_id.clone(),
            run_id: ctx.run_id.clone(),
            call_id: tc.id.clone(),
            name: tc.function.name.clone(),
            args: Value::Null,
        });
        let summarized = self.try_llm_summarize_history(ctx, c).await;
        let dropped = if estimate(ctx.history.view()) > c.soft {
            c.compact(&mut ctx.history)
        } else {
            0
        };
        tracing::info!(target: "botobot::tool", run = %ctx.run_id, summarized, dropped, "compact (agent-requested)");
        let summary =
            format!("compacted: summarized {summarized} messages, dropped {dropped} messages");
        ctx.sink.emit(AgentEvent::ToolEnd {
            session_id: ctx.session_id.clone(),
            run_id: ctx.run_id.clone(),
            call_id: tc.id.clone(),
            ok: true,
            result: Value::String(summary.clone()),
        });
        ctx.history
            .push(Message::tool_result(tc.id.clone(), summary));
    }

    /// 同步摘要（兜底路径）：计划 + LLM 摘要 + 套用，行为与重构前一致。
    async fn try_llm_summarize_history(&self, ctx: &mut Context, c: &Compactor) -> usize {
        let Some(plan) = self.plan_summarize(ctx.history.view(), c) else {
            return 0;
        };
        let Some(summary) = run_summarize(self.llm.clone(), plan.selected.clone(), c.soft).await
        else {
            return 0;
        };
        self.apply_summary(ctx, &plan, &summary)
    }

    /// §2.6/§4.9 B2：计算摘要边界并**快照老区**（不调 LLM）。`None`=无需/不可摘要。
    /// 老区 `[sys_end..cut]` 是 append-only 历史的稳定前缀段，快照后可安全后台摘要。
    fn plan_summarize(&self, msgs: &[Message], c: &Compactor) -> Option<SummarizePlan> {
        if estimate(msgs) <= c.soft {
            return None;
        }
        let sys_end = msgs.iter().take_while(|m| m.role == Role::System).count();
        let cut = tail_cutpoint(msgs, c.keep_tokens)?;
        if cut <= sys_end {
            return None;
        }
        Some(SummarizePlan {
            sys_end,
            cut,
            selected: msgs[sys_end..cut].to_vec(),
        })
    }

    /// §4.9 B2：把摘要文本套用到 history——`[system 前缀] + [摘要] + msgs[cut..]`。
    /// **边界校验**（append-only 保证老区不变，仍防御性检查）：当前长度 ≥ cut、前缀仍全 System；
    /// 不满足则返回 0（调用方回退同步重算）。返回压缩掉的消息数。
    fn apply_summary(&self, ctx: &mut Context, plan: &SummarizePlan, summary: &str) -> usize {
        let summary = summary.trim();
        if summary.is_empty() {
            return 0;
        }
        let msgs = ctx.history.view();
        if msgs.len() < plan.cut
            || plan.cut <= plan.sys_end
            || msgs
                .iter()
                .take(plan.sys_end)
                .any(|m| m.role != Role::System)
        {
            return 0; // 边界已变（理论上不会，append-only）→ 让调用方走同步兜底
        }
        // 工作集结转（§2.6 压缩战术补强）：摘要头记录读/改过的文件，并集上一轮摘要头（跨压缩累积）。
        let ws = workset_with_carryover(&plan.selected);
        let mut next: Vec<Message> = msgs[..plan.sys_end].to_vec();
        next.push(Message::system(format!(
            "[历史摘要：已压缩 {} 条较早消息]\n{}\n{summary}",
            plan.selected.len(),
            workset_header(&ws)
        )));
        next.extend_from_slice(&msgs[plan.cut..]);
        let changed = plan.selected.len();
        ctx.history.set(next);
        changed
    }

    fn tool_specs_for(&self, opts: &LlmOpts, activated_tools: &BTreeSet<String>) -> Vec<ToolSpec> {
        self.tools
            .list()
            .iter()
            .filter(|t| tool_enabled_for(t.name(), opts))
            .filter(|t| {
                t.load_mode() == ToolLoadMode::Essential || activated_tools.contains(t.name())
            })
            .map(|t| ToolSpec::function(t.name(), t.description(), t.schema()))
            .collect()
    }

    fn tool_available_for(
        &self,
        ctx: &Context,
        name: &str,
        activated_tools: &BTreeSet<String>,
    ) -> bool {
        let Some(tool) = ctx.tools.get(name) else {
            return true;
        };
        tool_enabled_for(name, &ctx.llm_opts)
            && (tool.load_mode() == ToolLoadMode::Essential || activated_tools.contains(name))
    }
}

/// §2.6 缺陷3 阶0：把一条 finalized message 上抛到增量通道（崩溃恢复 rollout）。
/// 通道为 None（子 agent/测试）时无操作；接收端已关闭时丢弃（turn 收尾后驱动器不再读）。
fn emit_delta(tx: &Option<mpsc::UnboundedSender<Message>>, m: &Message) {
    if let Some(tx) = tx {
        let _ = tx.send(m.clone());
    }
}

fn activate_tools_from_outcomes(activated_tools: &mut BTreeSet<String>, outcomes: &[ToolOutcome]) {
    for outcome in outcomes {
        if outcome.call.function.name != "tool_search" {
            continue;
        }
        let Ok(value) = &outcome.result else {
            continue;
        };
        let Some(names) = value.get("activated").and_then(Value::as_array) else {
            continue;
        };
        activated_tools.extend(names.iter().filter_map(Value::as_str).map(str::to_string));
    }
}

fn tool_enabled_for(name: &str, opts: &LlmOpts) -> bool {
    match name {
        // web_search 仍按 Search pill 显隐（外部网络搜索，独立 per-turn 开关）。
        "web_search" => opts.web_search.unwrap_or(false),
        // §⓪[A]（2026-06-25）：shell/http/后台命令工具**默认可见**（删 Code 门控）。
        // 此前藏在 Code pill 后（`opts.code_execution` 默认 false），关着时模型**看不到**它们 →
        // 误判「无 shell 工具」退回硬凑 apply_patch；且 officecli 等 skill 走 `shell_command` 直接失效。
        // **默认可见 ≠ 免审批**：Exec-tier 调用仍过 exec_policy（shell_command/background 走规则表
        // Allow/Deny/Prompt，其余 Exec 如 http_request → Prompt），安全边界不变。
        _ => true,
    }
}

fn charge_token_budget(ctx: &Context, requested: usize) -> Result<(), AgentError> {
    let Some(limit) = ctx.budget.token_budget else {
        return Ok(());
    };

    let mut used = ctx.token_spent.load(Ordering::Acquire);
    loop {
        let projected = used.saturating_add(requested);
        if projected > limit {
            return Err(AgentError::TokenBudgetExhausted {
                used,
                requested,
                budget: limit,
            });
        }
        match ctx
            .token_spent
            .compare_exchange(used, projected, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => return Ok(()),
            Err(current) => used = current,
        }
    }
}

fn est_decision(decision: &Decision) -> usize {
    let mut msg = Message::assistant_calls(decision.text.clone(), decision.tool_calls.clone());
    if !decision.reasoning.is_empty() {
        msg.content
            .push(ContentPart::Text(decision.reasoning.clone()));
    }
    est_msg(&msg)
}

fn post_infer_tokens(decision: &Decision, precharged_prompt_tokens: usize) -> usize {
    decision
        .usage
        .and_then(|u| u.total_tokens)
        .map(|total| total.saturating_sub(precharged_prompt_tokens))
        .unwrap_or_else(|| est_decision(decision))
}

/// §4.9 B2 摘要计划：边界 + 待摘要老区快照（同步/异步预摘要共用）。
struct SummarizePlan {
    sys_end: usize,
    cut: usize,
    selected: Vec<Message>,
}

/// §4.9 B2：对**快照老区**做 LLM 摘要（自由 fn，供 `tokio::spawn` 后台预摘要）。`None`=失败/空。
/// 不持有 `&self`/history 锁——输入是 owned 快照，与主循环并发安全。
async fn run_summarize(llm: Arc<dyn Llm>, selected: Vec<Message>, soft: usize) -> Option<String> {
    let input = summarize_input(&selected, soft);
    let prompt = vec![
        Message::system(
            "Summarize older conversation history for an agent. Preserve durable facts, \
             user goals, decisions, tool results, file paths, errors, and constraints. \
             Be compact. Output only the summary.",
        ),
        Message::user(input),
    ];
    let mut stream = llm.infer(&prompt, &[], &LlmOpts::default()).await.ok()?;
    let mut summary = String::new();
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(LlmEvent::TextDelta(t)) => summary.push_str(&t),
            Ok(LlmEvent::Done(d)) if !d.text.is_empty() => summary = d.text,
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    let s = summary.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// 压缩工作集（§2.6 压缩战术补强·结转，借鉴 oh-my-pi）：被摘要消息里读/改过的文件集。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Workset {
    read_files: BTreeSet<String>,
    modified_files: BTreeSet<String>,
}

/// 是否普通文件路径（计入 read_files）：`file://path` 视为普通；含其它 `://` 或 `blob:` 的 scheme 排除。
fn is_plain_path(url: &str) -> bool {
    if let Some(rest) = url.strip_prefix("file://") {
        return !rest.is_empty();
    }
    !url.contains("://") && !url.starts_with("blob:")
}

/// 从 tool_call 的 JSON arguments 取字符串字段。
fn arg_str(args: &str, key: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| v.get(key).and_then(|x| x.as_str()).map(str::to_string))
}

/// 扫消息的 assistant `tool_calls`，提取读取/修改的文件集。
fn extract_workset(msgs: &[Message]) -> Workset {
    let mut ws = Workset::default();
    for m in msgs {
        for tc in &m.tool_calls {
            let args = tc.function.arguments.as_str();
            match tc.function.name.as_str() {
                "read" => {
                    if let Some(u) = arg_str(args, "url") {
                        if is_plain_path(&u) {
                            let p = u.strip_prefix("file://").unwrap_or(&u).to_string();
                            ws.read_files.insert(p);
                        }
                    }
                }
                "edit_by_hashline" => {
                    if let Some(p) = arg_str(args, "path") {
                        ws.modified_files.insert(p);
                    }
                }
                "rename_file" => {
                    for k in ["old_path", "new_path"] {
                        if let Some(p) = arg_str(args, k) {
                            ws.modified_files.insert(p);
                        }
                    }
                }
                "apply_patch" => {
                    if let Some(patch) = arg_str(args, "patch") {
                        for line in patch.lines() {
                            if let Some(rest) = line.trim().strip_prefix("*** ") {
                                for kw in [
                                    "Add File: ",
                                    "Update File: ",
                                    "Delete File: ",
                                    "Move File: ",
                                ] {
                                    if let Some(p) = rest.strip_prefix(kw) {
                                        ws.modified_files.insert(p.trim().to_string());
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    ws
}

/// 拼单行工作集头：`[工作集] read_files: a, b | modified_files: c`。
fn workset_header(ws: &Workset) -> String {
    let join = |s: &BTreeSet<String>| s.iter().cloned().collect::<Vec<_>>().join(", ");
    format!(
        "[工作集] read_files: {} | modified_files: {}",
        join(&ws.read_files),
        join(&ws.modified_files)
    )
}

/// 从摘要文本里解析工作集头（用于跨压缩结转）。
fn parse_workset_header(text: &str) -> Option<Workset> {
    let line = text
        .lines()
        .find(|l| l.trim_start().starts_with("[工作集]"))?;
    let body = line.trim_start().strip_prefix("[工作集]")?.trim();
    let (rpart, mpart) = body.split_once(" | ")?;
    let parse = |seg: &str, prefix: &str| -> BTreeSet<String> {
        seg.trim()
            .strip_prefix(prefix)
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    };
    Some(Workset {
        read_files: parse(rpart, "read_files:"),
        modified_files: parse(mpart, "modified_files:"),
    })
}

/// 取消息的文本（拼接 Text 内容片段）。
fn message_text(m: &Message) -> String {
    m.content
        .iter()
        .filter_map(|p| match p {
            base_types::ContentPart::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// 本轮工作集 = `extract_workset(msgs)` 并集 msgs 里已有摘要消息头部的工作集（跨压缩结转）。
fn workset_with_carryover(msgs: &[Message]) -> Workset {
    let mut ws = extract_workset(msgs);
    for m in msgs {
        if let Some(prev) = parse_workset_header(&message_text(m)) {
            ws.read_files.extend(prev.read_files);
            ws.modified_files.extend(prev.modified_files);
        }
    }
    ws
}

fn summarize_input(msgs: &[Message], max_tokens: usize) -> String {
    let mut text = String::new();
    for (i, msg) in msgs.iter().enumerate() {
        let role = match msg.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        text.push_str(&format!("\n--- message {i} role={role} ---\n"));
        if let Some(id) = &msg.tool_call_id {
            text.push_str(&format!("tool_call_id={id}\n"));
        }
        for part in &msg.content {
            match part {
                ContentPart::Text(t) => text.push_str(t),
                ContentPart::ImageUrl(url) => {
                    text.push_str("[image_url] ");
                    text.push_str(url);
                }
            }
            text.push('\n');
        }
        for call in &msg.tool_calls {
            text.push_str(&format!(
                "tool_call {} args={}\n",
                call.function.name, call.function.arguments
            ));
        }
    }

    let max_chars = max_tokens.saturating_mul(12).max(4096);
    if text.len() <= max_chars {
        return text;
    }
    let head_len = max_chars / 2;
    let tail_len = max_chars - head_len;
    let head = safe_prefix(&text, head_len);
    let tail = safe_suffix(&text, tail_len);
    format!("{head}\n...[middle omitted for summary prompt]...\n{tail}")
}

fn safe_prefix(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn safe_suffix(s: &str, max: usize) -> &str {
    let mut start = s.len().saturating_sub(max);
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// `pub(crate)` 让 AgentTool 能从 [`agent`] 模块借走 Agent 字段。
pub(crate) fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// 工具结果给前端的**预览**（截断）；完整内容在 debug 日志。
fn tool_preview(tool_name: &str, full: &str) -> String {
    if tool_name == "debug"
        && let Some(summary) = debug_tool_preview(full)
    {
        return summary;
    }
    preview(full)
}

fn debug_tool_preview(full: &str) -> Option<String> {
    let value: Value = serde_json::from_str(full).ok()?;
    let session_status = value
        .get("session")
        .and_then(|session| session.get("status"))
        .and_then(Value::as_str);
    let last_event = value
        .get("session")
        .and_then(|session| session.get("last_event"))
        .and_then(Value::as_str);
    let event_count = value.get("event_count").and_then(Value::as_u64);
    let event = value
        .get("event")
        .and_then(|event| event.get("event"))
        .and_then(Value::as_str);

    if let Some(timed_out) = value.get("timed_out").and_then(Value::as_bool) {
        let mut parts = vec![format!("timed_out={timed_out}")];
        if let Some(state) = value.get("state").and_then(Value::as_str) {
            parts.push(format!("state={state}"));
        }
        if let Some(event) = event {
            parts.push(format!("event={event}"));
        }
        if let Some(count) = event_count {
            parts.push(format!("events={count}"));
        }
        return Some(format!("debug wait: {}", parts.join(", ")));
    }

    if value.get("recent_events").is_some() || value.get("output_tail").is_some() {
        let Some(session) = value.get("session") else {
            return Some("debug status: no active session".into());
        };
        if session.is_null() {
            return Some("debug status: no active session".into());
        }
        let mut parts = Vec::new();
        if let Some(status) = session_status {
            parts.push(format!("status={status}"));
        }
        if let Some(last_event) = last_event {
            parts.push(format!("last_event={last_event}"));
        }
        if let Some(count) = event_count {
            parts.push(format!("events={count}"));
        }
        return Some(format!("debug status: {}", parts.join(", ")));
    }

    if let Some(status) = session_status {
        let mut parts = vec![format!("status={status}")];
        if let Some(last_event) = last_event {
            parts.push(format!("last_event={last_event}"));
        }
        Some(format!("debug: {}", parts.join(", ")))
    } else {
        None
    }
}

fn preview(s: &str) -> String {
    const MAX: usize = 200;
    let n = s.chars().count();
    if n <= MAX {
        return s.to_string();
    }
    let head: String = s.chars().take(MAX).collect();
    format!("{head}…（+{} 字符，完整见 debug 日志）", n - MAX)
}

fn should_run_post_write_diagnostics(
    tc: &ToolCall,
    args: &Value,
    result: &Result<Value, String>,
) -> bool {
    if !matches!(
        tc.function.name.as_str(),
        "apply_patch" | "edit_by_hashline" | "rename_file" | "write"
    ) {
        return false;
    }
    if args.get("dry_run").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    result
        .as_ref()
        .ok()
        .map(|value| {
            value
                .get("applied")
                .and_then(Value::as_bool)
                .unwrap_or(true)
        })
        .unwrap_or(false)
}

fn diagnostics_summary(data: &Value) -> String {
    let diagnostics = data
        .get("diagnostics")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if diagnostics.is_empty() {
        return "写后 diagnostics：0 个问题".into();
    }

    let mut errors = 0usize;
    let mut warnings = 0usize;
    let mut notes = 0usize;
    let mut others = 0usize;
    for d in diagnostics {
        match d.get("level").and_then(Value::as_str).unwrap_or_default() {
            "error" => errors += 1,
            "warning" | "warn" => warnings += 1,
            "note" | "help" => notes += 1,
            _ => others += 1,
        }
    }

    let mut parts = Vec::new();
    if errors > 0 {
        parts.push(format!("{errors} error"));
    }
    if warnings > 0 {
        parts.push(format!("{warnings} warning"));
    }
    if notes > 0 {
        parts.push(format!("{notes} note/help"));
    }
    if others > 0 {
        parts.push(format!("{others} other"));
    }
    let suffix = if data
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        "（已截断）"
    } else {
        ""
    };
    format!("写后 diagnostics：{}{suffix}", parts.join("，"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use base_types::{
        ApprovalDecision, ApprovalResponse, FunctionCall, Role, ToolConcurrency, ToolCtx,
        ToolLoadMode, ToolTier,
    };
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

    #[test]
    fn agent_tool_concurrency_declaration() {
        use crate::AgentTool;
        use base_types::Tool;
        let agent = Agent::builder().llm(ScriptedLlm::new(vec![])).build();
        let default_tool = AgentTool::new("explore", "d", agent.clone());
        assert_eq!(
            default_tool.concurrency(),
            ToolConcurrency::Concurrent,
            "默认 AgentTool 应为 Concurrent"
        );
        let ex = AgentTool::new("explore", "d", agent).with_concurrency(ToolConcurrency::Exclusive);
        assert_eq!(
            ex.concurrency(),
            ToolConcurrency::Exclusive,
            "with_concurrency(Exclusive) 应声明 Exclusive"
        );
    }

    #[test]
    fn sub_agent_exclusive_registers_named_exclusive_tool() {
        let sub = Agent::builder().llm(ScriptedLlm::new(vec![])).build();
        let mut b = Agent::builder()
            .llm(ScriptedLlm::new(vec![]))
            .sub_agent_exclusive("explore", "调查代码库", sub);
        assert!(b.tools_mut().has("explore"), "应注册名为 explore 的工具");
        let explore = b
            .tools_mut()
            .list_tools()
            .into_iter()
            .find(|t| t.name() == "explore")
            .expect("explore 工具应存在");
        assert_eq!(
            explore.concurrency(),
            ToolConcurrency::Exclusive,
            "explore 工具应声明 Exclusive 并发上限"
        );
    }

    #[test]
    fn extract_workset_and_header_roundtrip() {
        use base_types::{FunctionCall, ToolCall};
        fn call(name: &str, args: &str) -> ToolCall {
            ToolCall {
                id: "x".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: name.into(),
                    arguments: args.into(),
                },
            }
        }
        let msgs = vec![Message::assistant_calls(
            String::new(),
            vec![
                call("read", r#"{"url":"src/a.rs"}"#),
                call("read", r#"{"url":"skill://x"}"#),
                call("edit_by_hashline", r#"{"path":"src/b.rs"}"#),
                call("rename_file", r#"{"old_path":"o.rs","new_path":"n.rs"}"#),
                call(
                    "apply_patch",
                    "{\"patch\":\"*** Begin Patch\\n*** Update File: c.rs\\n*** End Patch\"}",
                ),
            ],
        )];
        let ws = extract_workset(&msgs);
        assert!(ws.read_files.contains("src/a.rs"), "read 普通路径应计入");
        assert!(
            !ws.read_files.iter().any(|f| f.contains("skill")),
            "scheme URL 不计入 read_files"
        );
        for f in ["src/b.rs", "o.rs", "n.rs", "c.rs"] {
            assert!(ws.modified_files.contains(f), "modified 应含 {f}");
        }
        let h = workset_header(&ws);
        assert!(h.starts_with("[工作集]"));
        assert_eq!(parse_workset_header(&h), Some(ws));
    }

    #[test]
    fn workset_carryover_unions_previous_summary_header() {
        use base_types::{FunctionCall, ToolCall};
        let prev_ws = Workset {
            read_files: ["a.rs".to_string()].into_iter().collect(),
            modified_files: Default::default(),
        };
        let prev_summary = Message::system(format!(
            "[历史摘要：已压缩 1 条较早消息]\n{}\n旧概要",
            workset_header(&prev_ws)
        ));
        let new_read = Message::assistant_calls(
            String::new(),
            vec![ToolCall {
                id: "y".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "read".into(),
                    arguments: r#"{"url":"b.rs"}"#.into(),
                },
            }],
        );
        let ws = workset_with_carryover(&[prev_summary, new_read]);
        assert!(
            ws.read_files.contains("a.rs") && ws.read_files.contains("b.rs"),
            "结转应并集上一轮(a.rs)与本轮(b.rs)"
        );
    }

    #[test]
    fn body_after_prefix_avoids_immediate_retrigger() {
        // window=200 → soft=150。前缀（system）稳定；压缩后只剩前缀 + 少量 tail，
        // body 应低于 soft，不会立即二次触发。
        let c = Compactor::new(200, None);
        let after = vec![
            Message::system("S".repeat(150)), // 稳定前缀
            Message::user("short tail"),
        ];
        assert!(
            !c.over_soft(&after),
            "压缩后 body 应低于 soft，不立即二次触发"
        );
    }

    #[test]
    fn debug_tool_preview_summarizes_status_and_waits() {
        let status = serde_json::json!({
            "session": { "status": "running", "last_event": "initialized" },
            "event_count": 2,
            "recent_events": [],
            "output_tail": "hello",
        })
        .to_string();
        assert_eq!(
            tool_preview("debug", &status),
            "debug status: status=running, last_event=initialized, events=2"
        );

        let wait = serde_json::json!({
            "session": { "status": "running" },
            "event_count": 3,
            "timed_out": true,
            "event": null,
            "output_tail": "",
        })
        .to_string();
        assert_eq!(
            tool_preview("debug", &wait),
            "debug wait: timed_out=true, events=3"
        );

        let inactive = serde_json::json!({
            "session": null,
            "event_count": 0,
            "recent_events": [],
            "output_tail": "",
        })
        .to_string();
        assert_eq!(
            tool_preview("debug", &inactive),
            "debug status: no active session"
        );
    }

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
        ) -> Result<base_types::LlmStream, LlmError> {
            let d = self
                .steps
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| text_decision("(end)"));
            let mut evs: Vec<base_types::LlmResult<LlmEvent>> = Vec::new();
            if !d.text.is_empty() {
                evs.push(Ok(LlmEvent::TextDelta(d.text.clone())));
            }
            evs.push(Ok(LlmEvent::Done(d)));
            Ok(Box::pin(futures::stream::iter(evs)))
        }
    }

    fn text_decision(s: &str) -> Decision {
        Decision {
            text: s.to_string(),
            finish_reason: Some("stop".into()),
            ..Default::default()
        }
    }
    fn usage_decision(s: &str, total_tokens: usize) -> Decision {
        Decision {
            text: s.to_string(),
            finish_reason: Some("stop".into()),
            usage: Some(base_types::TokenUsage {
                prompt_tokens: None,
                completion_tokens: None,
                total_tokens: Some(total_tokens),
            }),
            ..Default::default()
        }
    }
    fn call_decision(tool: &str) -> Decision {
        call_decision_args(tool, serde_json::json!({ "task": "do it" }))
    }

    fn call_decision_args(tool: &str, args: Value) -> Decision {
        Decision {
            tool_calls: vec![ToolCall {
                id: "call_1".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: tool.into(),
                    arguments: args.to_string(),
                },
            }],
            finish_reason: Some("tool_calls".into()),
            ..Default::default()
        }
    }
    fn multi_call_decision(tools: &[&str]) -> Decision {
        Decision {
            tool_calls: tools
                .iter()
                .enumerate()
                .map(|(idx, tool)| ToolCall {
                    id: format!("call_{}", idx + 1),
                    kind: "function".into(),
                    function: FunctionCall {
                        name: (*tool).into(),
                        arguments: "{}".into(),
                    },
                })
                .collect(),
            finish_reason: Some("tool_calls".into()),
            ..Default::default()
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
    async fn collect(mut s: UnboundedReceiverStream<AgentEvent>) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        while let Some(e) = s.next().await {
            out.push(e);
        }
        out
    }

    /// 首次 infer 流中途以 idle 失败（已 emit 一段文本），重放（第二次 infer）干净完成。
    struct MidStreamFailOnceLlm {
        calls: std::sync::atomic::AtomicUsize,
    }
    #[async_trait]
    impl Llm for MidStreamFailOnceLlm {
        async fn infer(
            &self,
            _m: &[Message],
            _t: &[ToolSpec],
            _opts: &LlmOpts,
        ) -> Result<base_types::LlmStream, LlmError> {
            let n = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            let evs: Vec<base_types::LlmResult<LlmEvent>> = if n == 0 {
                // 第一次：emit 部分文本后流中途 idle 失败
                vec![
                    Ok(LlmEvent::TextDelta("PARTIAL".into())),
                    Err(LlmError::Idle),
                ]
            } else {
                // 重放：干净完成
                vec![
                    Ok(LlmEvent::TextDelta("FULL_ANSWER".into())),
                    Ok(LlmEvent::Done(text_decision("FULL_ANSWER"))),
                ]
            };
            Ok(Box::pin(futures::stream::iter(evs)))
        }
    }

    #[tokio::test]
    async fn mid_stream_failure_replays_with_reset() {
        let agent = Agent::builder()
            .llm(Arc::new(MidStreamFailOnceLlm {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }))
            .stream_replays(1)
            .build();
        let events = collect(agent.run("go")).await;

        // 重放前发了 StreamReset（幂等：前端据此清空 PARTIAL）
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::StreamReset { .. })),
            "应在重放前发 StreamReset"
        );
        // 最终 Done 是重放后的完整答案，turn 未被中止
        let done = events.iter().rev().find_map(|e| match e {
            AgentEvent::Done { output, .. } => Some(output.clone()),
            _ => None,
        });
        assert_eq!(done.as_deref(), Some("FULL_ANSWER"));
        // 无 Error 事件（未中止）
        assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
    }

    #[tokio::test]
    async fn mid_stream_failure_aborts_when_replays_zero() {
        let agent = Agent::builder()
            .llm(Arc::new(MidStreamFailOnceLlm {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }))
            .stream_replays(0)
            .build();
        let events = collect(agent.run("go")).await;
        // 不重放：无 StreamReset，turn 以 Error 中止
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::StreamReset { .. }))
        );
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
    }

    #[tokio::test]
    async fn run_session_carries_history_across_turns() {
        let a1 = Agent::builder()
            .llm(ScriptedLlm::new(vec![text_decision("REPLY1")]))
            .system("SYS")
            .build();
        let (s1, hrx1) = a1.run_session(
            "s-test",
            vec![],
            vec![ContentPart::Text("q1".into())],
            LlmOpts::default(),
        );
        collect(s1).await;
        let h1 = hrx1.await.unwrap();
        assert!(h1.iter().any(|m| msg_text(m).contains("SYS")));
        assert!(h1.iter().any(|m| msg_text(m).contains("q1")));
        assert!(h1.iter().any(|m| msg_text(m).contains("REPLY1")));

        let a2 = Agent::builder()
            .llm(ScriptedLlm::new(vec![text_decision("REPLY2")]))
            .system("SYS")
            .build();
        let (s2, hrx2) = a2.run_session(
            "s-test",
            h1.clone(),
            vec![ContentPart::Text("q2".into())],
            LlmOpts::default(),
        );
        collect(s2).await;
        let h2 = hrx2.await.unwrap();
        assert!(
            h2.iter().any(|m| msg_text(m).contains("q1")),
            "应延续上一轮"
        );
        assert!(h2.iter().any(|m| msg_text(m).contains("q2")));
        assert!(h2.iter().any(|m| msg_text(m).contains("REPLY2")));
        let sys_count = h2.iter().filter(|m| m.role == Role::System).count();
        assert_eq!(sys_count, 1, "不应重复 system");
    }

    // §4.9 B2：后台预摘要自由 fn 契约——对快照老区调 LLM 返回摘要文本；空/失败→None。
    #[tokio::test]
    async fn run_summarize_returns_summary_text() {
        let llm: Arc<dyn Llm> = ScriptedLlm::new(vec![text_decision("SUMMARY-OK")]);
        let selected = vec![
            Message::user("old fact one ".repeat(3)),
            Message::assistant("old decision two ".repeat(3)),
        ];
        let out = run_summarize(llm, selected, 100).await;
        assert_eq!(out.as_deref(), Some("SUMMARY-OK"));

        // 空摘要 → None。
        let empty: Arc<dyn Llm> = ScriptedLlm::new(vec![text_decision("   ")]);
        assert_eq!(
            run_summarize(empty, vec![Message::user("x")], 100).await,
            None
        );
    }

    #[tokio::test]
    async fn summary_history_backend_compacts_session_turn() {
        let prior = vec![
            Message::system("SYS"),
            Message::user("old user fact alpha beta gamma ".repeat(4)),
            Message::assistant("old assistant decision delta epsilon ".repeat(4)),
            Message::user("recent prior should remain"),
        ];
        let agent = Agent::builder()
            .llm(ScriptedLlm::new(vec![text_decision("OK")]))
            .system("SYS")
            .summary_history(150, 2)
            .build();

        let (stream, hrx) = agent.run_session(
            "s-summary",
            prior,
            vec![ContentPart::Text("current question".into())],
            LlmOpts::default(),
        );
        collect(stream).await;
        let history = hrx.await.unwrap();
        let texts: Vec<String> = history.iter().map(msg_text).collect();

        assert!(texts.iter().any(|t| t.starts_with("[历史摘要：")));
        assert!(texts.iter().any(|t| t.contains("old user fact")));
        assert!(texts.iter().any(|t| t == "current question"));
        assert!(texts.iter().any(|t| t == "OK"));
    }

    #[tokio::test]
    async fn compact_tool_intercepted_and_summarizes_history() {
        // 预置一段超窗口的历史，模型第一步主动调 compact，第二步收尾。
        let big = vec![
            Message::system("SYS"),
            Message::user("OLD ".repeat(60)),
            Message::assistant("MID ".repeat(60)),
            Message::user("more ".repeat(60)),
        ];
        let agent = Agent::builder()
            .llm(ScriptedLlm::new(vec![
                call_decision("compact"),
                text_decision("SUMMARY"),
                text_decision("DONE"),
            ]))
            .system("SYS")
            // window=340 → soft=255 / hard=306。历史 est≈286、body≈280：
            // body<hard 不会 reason 前自动强压抢走 compact 调用；est>soft 时 agent 调 compact 仍走 LLM 摘要。
            .context_window(340)
            .build();

        let (stream, hrx) = agent.run_session(
            "s-compact",
            big,
            vec![ContentPart::Text("q".into())],
            LlmOpts::default(),
        );
        let events = collect(stream).await;

        let compacted = events.iter().any(|e| {
            matches!(e, AgentEvent::ToolEnd { result, .. }
                if result.as_str().is_some_and(|s| s.contains("compacted")))
        });
        assert!(compacted, "compact 应被驱动器拦截并发出 ToolEnd(compacted)");

        let h = hrx.await.unwrap();
        assert!(
            h.iter().any(|m| msg_text(m).contains("SUMMARY")),
            "历史应优先被 LLM 摘要"
        );
        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::Done { .. })),
            "应正常收尾"
        );
    }

    #[tokio::test]
    async fn hard_compact_falls_back_to_window_drop_when_summary_is_empty() {
        let big = vec![
            Message::system("SYS"),
            Message::user("OLD ".repeat(120)),
            Message::assistant("MID ".repeat(120)),
            Message::user("tail"),
        ];
        let agent = Agent::builder()
            .llm(ScriptedLlm::new(vec![
                Decision::default(),
                text_decision("DONE"),
            ]))
            .system("SYS")
            .context_window(50)
            .build();

        let (stream, hrx) = agent.run_session(
            "s-hard-compact",
            big,
            vec![ContentPart::Text("q".into())],
            LlmOpts::default(),
        );
        collect(stream).await;

        let h = hrx.await.unwrap();
        assert!(
            h.iter().any(|m| msg_text(m).contains("裁剪")),
            "摘要为空时应退回现有硬裁剪"
        );
    }

    #[test]
    fn summarize_input_preserves_utf8_boundaries_when_capped() {
        let input = summarize_input(&[Message::user("你好".repeat(4096))], 10);
        assert!(input.is_char_boundary(input.len()));
        assert!(input.contains("middle omitted"));
    }

    #[test]
    fn post_infer_tokens_uses_provider_usage_when_available() {
        let d = usage_decision("ok", 25);
        assert_eq!(post_infer_tokens(&d, 10), 15);
        assert_eq!(post_infer_tokens(&d, 30), 0);

        let estimated = post_infer_tokens(&text_decision("abcdef"), 10);
        assert_eq!(estimated, est_decision(&text_decision("abcdef")));
    }

    #[tokio::test]
    async fn session_carries_history_across_turns_via_commit() {
        use crate::Session;
        let agent = Agent::builder()
            .llm(ScriptedLlm::new(vec![
                text_decision("R1"),
                text_decision("R2"),
            ]))
            .system("SYS")
            .build();
        let mut s = Session::new(agent);

        let (st1, hrx1, _) = s.turn(vec![ContentPart::Text("q1".into())], LlmOpts::default());
        collect(st1).await;
        s.commit(hrx1.await.unwrap());
        assert!(s.history().iter().any(|m| msg_text(m).contains("q1")));

        let (st2, hrx2, _) = s.turn(vec![ContentPart::Text("q2".into())], LlmOpts::default());
        collect(st2).await;
        s.commit(hrx2.await.unwrap());
        assert!(
            s.history().iter().any(|m| msg_text(m).contains("q1")),
            "应延续上一轮"
        );
        assert!(s.history().iter().any(|m| msg_text(m).contains("q2")));
        let sys = s
            .history()
            .iter()
            .filter(|m| m.role == Role::System)
            .count();
        assert_eq!(sys, 1, "不应重复 system");
    }

    #[tokio::test]
    async fn session_with_id_and_history_resumes() {
        use crate::Session;
        let agent = Agent::builder()
            .llm(ScriptedLlm::new(vec![text_decision("R2")]))
            .system("SYS")
            .build();
        let history = vec![
            Message::system("SYS"),
            Message::user("old-question"),
            Message::assistant("old-answer"),
        ];
        let mut s = Session::with_id_and_history("session-restore", agent, history);

        let (stream, hrx, _) = s.turn(
            vec![ContentPart::Text("new-question".into())],
            LlmOpts::default(),
        );
        let events = collect(stream).await;
        s.commit(hrx.await.unwrap());

        assert!(events.iter().all(|e| e.session_id() == "session-restore"));
        assert!(
            s.history()
                .iter()
                .any(|m| msg_text(m).contains("old-question")),
            "恢复的历史应保留"
        );
        assert!(
            s.history()
                .iter()
                .any(|m| msg_text(m).contains("new-question")),
            "新输入应追加"
        );
    }

    /// 测试用最小工具：可指定 tier，call 直接返回 ok。
    struct StubTool {
        name: &'static str,
        tier: ToolTier,
    }
    #[async_trait]
    impl base_types::Tool for StubTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "stub"
        }
        fn schema(&self) -> Value {
            serde_json::json!({ "type": "object" })
        }
        fn tier(&self) -> ToolTier {
            self.tier
        }
        async fn call(&self, _args: Value) -> base_types::ToolResult {
            Ok(Value::String("STUB_OK".into()))
        }
    }

    struct DiscoverableTool;
    #[async_trait]
    impl base_types::Tool for DiscoverableTool {
        fn name(&self) -> &str {
            "hidden_tool"
        }
        fn description(&self) -> &str {
            "Hidden tool for discoverable search tests."
        }
        fn summary(&self) -> &str {
            "hidden capability"
        }
        fn schema(&self) -> Value {
            serde_json::json!({ "type": "object" })
        }
        fn tier(&self) -> ToolTier {
            ToolTier::Read
        }
        fn load_mode(&self) -> ToolLoadMode {
            ToolLoadMode::Discoverable
        }
        async fn call(&self, _args: Value) -> base_types::ToolResult {
            Ok(Value::String("HIDDEN_OK".into()))
        }
    }

    #[derive(Default)]
    struct ExclusiveOrderState {
        first_done: AtomicBool,
        exclusive_done: AtomicBool,
        exclusive_started_before_first_done: AtomicBool,
        after_started_before_exclusive_done: AtomicBool,
    }

    struct OrderTool {
        name: &'static str,
        concurrency: ToolConcurrency,
        state: Arc<ExclusiveOrderState>,
    }

    #[async_trait]
    impl base_types::Tool for OrderTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "records execution ordering"
        }
        fn schema(&self) -> Value {
            serde_json::json!({ "type": "object" })
        }
        fn concurrency(&self) -> ToolConcurrency {
            self.concurrency
        }
        async fn call(&self, _args: Value) -> base_types::ToolResult {
            match self.name {
                "first" => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    self.state.first_done.store(true, AtomicOrdering::SeqCst);
                }
                "exclusive" => {
                    if !self.state.first_done.load(AtomicOrdering::SeqCst) {
                        self.state
                            .exclusive_started_before_first_done
                            .store(true, AtomicOrdering::SeqCst);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    self.state
                        .exclusive_done
                        .store(true, AtomicOrdering::SeqCst);
                }
                "after" => {
                    if !self.state.exclusive_done.load(AtomicOrdering::SeqCst) {
                        self.state
                            .after_started_before_exclusive_done
                            .store(true, AtomicOrdering::SeqCst);
                    }
                }
                _ => {}
            }
            Ok(Value::String(self.name.into()))
        }
    }

    struct ContextEchoTool;

    #[async_trait]
    impl base_types::Tool for ContextEchoTool {
        fn name(&self) -> &str {
            "ctx_echo"
        }
        fn description(&self) -> &str {
            "echoes tool context"
        }
        fn schema(&self) -> Value {
            serde_json::json!({ "type": "object" })
        }
        async fn call(&self, _args: Value) -> base_types::ToolResult {
            Err(anyhow::anyhow!("call_with_context should be used"))
        }
        async fn call_with_context(&self, _args: Value, ctx: &ToolCtx) -> base_types::ToolResult {
            Ok(Value::String(format!(
                "{}|{}|{}|{}",
                ctx.session_id,
                ctx.run_id,
                ctx.depth,
                ctx.workdir.display()
            )))
        }
    }

    #[tokio::test]
    async fn exclusive_tools_run_between_concurrent_batches() {
        let state = Arc::new(ExclusiveOrderState::default());
        let mk = |name, concurrency| {
            Arc::new(OrderTool {
                name,
                concurrency,
                state: state.clone(),
            })
        };
        let agent = Agent::builder()
            .llm(ScriptedLlm::new(vec![
                multi_call_decision(&["first", "exclusive", "after"]),
                text_decision("DONE"),
            ]))
            .tool(mk("first", ToolConcurrency::Concurrent))
            .tool(mk("exclusive", ToolConcurrency::Exclusive))
            .tool(mk("after", ToolConcurrency::Concurrent))
            .build();

        let events = collect(agent.run("go")).await;

        assert!(
            !state
                .exclusive_started_before_first_done
                .load(AtomicOrdering::SeqCst),
            "exclusive tool must wait for the preceding concurrent batch"
        );
        assert!(
            !state
                .after_started_before_exclusive_done
                .load(AtomicOrdering::SeqCst),
            "following concurrent batch must wait for exclusive tool"
        );
        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::Done { .. })),
            "run should still complete"
        );
    }

    #[tokio::test]
    async fn tool_call_receives_late_bound_context() {
        let workdir = std::env::temp_dir().join("botobot-toolctx-test");
        std::fs::create_dir_all(&workdir).unwrap();
        let agent = Agent::builder()
            .llm(ScriptedLlm::new(vec![
                multi_call_decision(&["ctx_echo"]),
                text_decision("DONE"),
            ]))
            .workdir(&workdir)
            .tool(Arc::new(ContextEchoTool))
            .build();

        let (stream, _hrx) = agent.run_session(
            "ctx-session",
            vec![],
            vec![ContentPart::Text("go".into())],
            LlmOpts::default(),
        );
        let events = collect(stream).await;

        let echoed = events.iter().find_map(|e| match e {
            AgentEvent::ToolEnd {
                ok: true, result, ..
            } => result.as_str(),
            _ => None,
        });
        let echoed = echoed.expect("context echo result");
        assert!(echoed.contains("ctx-session"));
        assert!(echoed.contains(&workdir.display().to_string()));
    }

    // §5.7：with_system 派生只换 system prompt 的 agent，工具/workdir 等不变；与 with_workdir 正交可叠。
    #[test]
    fn with_system_overrides_prompt_keeping_tools_and_workdir() {
        let base = Agent::builder()
            .llm(ScriptedLlm::new(vec![text_decision("ok")]))
            .tool(Arc::new(StubTool { name: "alpha", tier: ToolTier::Read }))
            .system("CODER ROLE")
            .workdir("/base")
            .build();
        assert_eq!(base.system_prompt(), Some("CODER ROLE"));
        let general = base.with_system("GENERAL ROLE");
        assert_eq!(general.system_prompt(), Some("GENERAL ROLE"), "system 应被覆盖");
        assert_eq!(general.workdir(), base.workdir(), "workdir 应保持");
        assert!(general.tool_brief().iter().any(|(n, _)| n == "alpha"), "工具应保持");
        // 与 with_workdir 正交叠加。
        let both = base.with_system("G").with_workdir("/other");
        assert_eq!(both.system_prompt(), Some("G"));
        assert_eq!(both.workdir().to_string_lossy(), "/other");
    }

    // §5.7 真异构多模型：with_llm 派生只换底层 LLM 的 agent，system/workdir/tools 不变；
    // 派生 agent 实际用新模型推理（行为证实，非仅元数据）。
    #[tokio::test]
    async fn with_llm_swaps_model_keeping_system_tools_workdir() {
        let base = Agent::builder()
            .llm(ScriptedLlm::new(vec![text_decision("BASE_MODEL")]))
            .tool(Arc::new(StubTool { name: "alpha", tier: ToolTier::Read }))
            .system("ROLE")
            .workdir("/base")
            .build();
        let derived = base.with_llm(ScriptedLlm::new(vec![text_decision("OTHER_MODEL")]));
        // 结构保持。
        assert_eq!(derived.system_prompt(), Some("ROLE"), "system 应保持");
        assert_eq!(derived.workdir(), base.workdir(), "workdir 应保持");
        assert!(
            derived.tool_brief().iter().any(|(n, _)| n == "alpha"),
            "工具应保持"
        );
        // 行为：派生 agent 用新模型输出 OTHER_MODEL，而非 base 的 BASE_MODEL。
        let agent = Arc::new(derived);
        let events = collect(agent.run("go")).await;
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Token { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(text.contains("OTHER_MODEL"), "应使用派生的新模型: {text}");
        assert!(!text.contains("BASE_MODEL"), "不应再用旧模型: {text}");
    }

    // §5.5 C11：tool_brief 列出已注册工具的 (name, tier)，按名排序、tier 渲染为 read/write/exec。
    #[test]
    fn tool_brief_lists_registered_tools_sorted_with_tier() {
        let agent = Agent::builder()
            .llm(ScriptedLlm::new(vec![text_decision("ok")]))
            .tool(Arc::new(StubTool {
                name: "zeta",
                tier: ToolTier::Exec,
            }))
            .tool(Arc::new(StubTool {
                name: "alpha",
                tier: ToolTier::Read,
            }))
            .build();
        let brief = agent.tool_brief();
        // 至少含两个注册工具；按名排序，alpha 在 zeta 前。
        let names: Vec<&str> = brief.iter().map(|(n, _)| n.as_str()).collect();
        let ai = names
            .iter()
            .position(|n| *n == "alpha")
            .expect("应含 alpha");
        let zi = names.iter().position(|n| *n == "zeta").expect("应含 zeta");
        assert!(ai < zi, "应按名排序: {names:?}");
        assert_eq!(brief.iter().find(|(n, _)| n == "alpha").unwrap().1, "read");
        assert_eq!(brief.iter().find(|(n, _)| n == "zeta").unwrap().1, "exec");
    }

    #[test]
    fn discoverable_tools_are_hidden_until_activated() {
        let agent = Agent::builder()
            .llm(ScriptedLlm::new(vec![text_decision("ok")]))
            .tool(Arc::new(DiscoverableTool))
            .build();

        let names: Vec<String> = agent
            .tool_specs_for(&LlmOpts::default(), &BTreeSet::new())
            .into_iter()
            .map(|s| s.function.name)
            .collect();
        assert_eq!(names, vec!["tool_search"]);

        let activated = BTreeSet::from(["hidden_tool".to_string()]);
        let names: Vec<String> = agent
            .tool_specs_for(&LlmOpts::default(), &activated)
            .into_iter()
            .map(|s| s.function.name)
            .collect();
        assert_eq!(names, vec!["hidden_tool", "tool_search"]);
    }

    #[tokio::test]
    async fn tool_search_activates_discoverable_tool_for_next_step() {
        let agent = Agent::builder()
            .llm(ScriptedLlm::new(vec![
                call_decision("tool_search"),
                call_decision("hidden_tool"),
                text_decision("DONE"),
            ]))
            .tool(Arc::new(DiscoverableTool))
            .build();

        let (stream, _hrx) = agent.run_session(
            "s-discover",
            vec![],
            vec![ContentPart::Text("find hidden".into())],
            LlmOpts::default(),
        );
        let events = collect(stream).await;
        let hidden_ok = events.iter().any(|e| {
            matches!(
                e,
                AgentEvent::ToolEnd {
                    call_id,
                    ok: true,
                    result,
                    ..
                } if call_id == "call_1" && result.as_str().is_some_and(|s| s.contains("HIDDEN_OK"))
            )
        });
        assert!(hidden_ok, "tool_search 后 hidden_tool 应被激活并可执行");
    }

    #[tokio::test]
    async fn write_tool_runs_post_write_diagnostics_and_feeds_history() {
        let root = std::env::temp_dir().join(format!("botobot-postdiag-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"botobot_postdiag\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn f() -> i32 { 1 }\n").unwrap();
        let patch = "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-pub fn f() -> i32 { 1 }\n+pub fn f() -> i32 { missing }\n*** End Patch\n";
        let agent = Agent::builder()
            .llm(ScriptedLlm::new(vec![
                call_decision_args(
                    "apply_patch",
                    serde_json::json!({ "patch": patch, "dry_run": false }),
                ),
                text_decision("DONE"),
            ]))
            .workdir(&root)
            .tool(Arc::new(agent_act::patch::ApplyPatchTool))
            .build();

        let (stream, hrx) = agent.run_session(
            "postdiag",
            vec![],
            vec![ContentPart::Text("break it".into())],
            LlmOpts::default(),
        );
        let events = collect(stream).await;
        let history = hrx.await.unwrap();

        let diagnostic = events.iter().find_map(|ev| match ev {
            AgentEvent::Diagnostics {
                ok, summary, data, ..
            } => Some((*ok, summary.clone(), data.clone())),
            _ => None,
        });
        let (ok, summary, data) = diagnostic.expect("post-write diagnostics event");
        assert!(ok, "diagnostics command should run: {summary}");
        assert!(
            summary.contains("error"),
            "broken code should produce an error summary: {summary}; data={data}"
        );
        assert!(
            history
                .iter()
                .any(|m| msg_text(m).contains("post_write_diagnostics")),
            "diagnostics should be fed back through the tool result history"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn search_and_code_tools_are_per_turn_opt_in() {
        let agent = Agent::builder()
            .llm(ScriptedLlm::new(vec![text_decision("ok")]))
            .tool(Arc::new(StubTool {
                name: "web_search",
                tier: ToolTier::Read,
            }))
            .tool(Arc::new(StubTool {
                name: "code_execution",
                tier: ToolTier::Exec,
            }))
            .tool(Arc::new(StubTool {
                name: "shell_command",
                tier: ToolTier::Exec,
            }))
            .tool(Arc::new(StubTool {
                name: "http_request",
                tier: ToolTier::Exec,
            }))
            .tool(Arc::new(StubTool {
                name: "read",
                tier: ToolTier::Read,
            }))
            .build();

        // §⓪[A]：默认即可见 shell/http/code（删 Code 门控）；仅 web_search 仍需 Search pill。
        let default_names: Vec<String> = agent
            .tool_specs_for(&LlmOpts::default(), &BTreeSet::new())
            .into_iter()
            .map(|s| s.function.name)
            .collect();
        assert_eq!(
            default_names,
            vec!["code_execution", "http_request", "read", "shell_command"],
            "shell/http/code 默认可见；web_search 仍隐"
        );

        // 开 Search pill → web_search 也现身（全 5 个）。
        let enabled = LlmOpts {
            web_search: Some(true),
            ..LlmOpts::default()
        };
        let names: Vec<String> = agent
            .tool_specs_for(&enabled, &BTreeSet::new())
            .into_iter()
            .map(|s| s.function.name)
            .collect();
        assert_eq!(
            names,
            vec![
                "code_execution",
                "http_request",
                "read",
                "shell_command",
                "web_search"
            ]
        );
    }

    #[tokio::test]
    async fn steer_injects_input_into_running_turn() {
        // 多步循环给 steer 落地窗口；steer() 同步入队，先于 spawn 的循环任务起跑。
        let agent = Agent::builder()
            .llm(ScriptedLlm::new(vec![
                call_decision("noop"),
                call_decision("noop"),
                text_decision("DONE"),
            ]))
            .tool(Arc::new(StubTool {
                name: "noop",
                tier: ToolTier::Exec,
            }))
            .build();
        let mut s = crate::Session::new(agent);

        let (stream, hrx, _) = s.turn(vec![ContentPart::Text("q".into())], LlmOpts::default());
        let sent = s.steer(vec![ContentPart::Text("STEERED".into())]);
        assert!(sent, "turn 在跑，steer 应送达");

        collect(stream).await;
        let h = hrx.await.unwrap();
        s.commit(h.clone());
        assert!(
            h.iter().any(|m| msg_text(m).contains("STEERED")),
            "steer 输入应并进历史"
        );
    }

    #[tokio::test]
    async fn policy_denies_tool_and_feeds_back() {
        struct DenyNoop;
        impl Policy for DenyNoop {
            fn check(
                &self,
                call: &ToolCall,
                _tier: ToolTier,
                _workdir: &std::path::Path,
            ) -> Verdict {
                if call.function.name == "noop" {
                    Verdict::Deny("not allowed".into())
                } else {
                    Verdict::Allow
                }
            }
        }
        let agent = Agent::builder()
            .llm(ScriptedLlm::new(vec![
                call_decision("noop"),
                text_decision("DONE"),
            ]))
            .tool(Arc::new(StubTool {
                name: "noop",
                tier: ToolTier::Exec,
            }))
            .policy(Arc::new(DenyNoop))
            .build();

        let events = collect(agent.run("go")).await;

        let denied = events.iter().any(|e| {
            matches!(e, AgentEvent::ToolEnd { ok: false, result, .. }
                if result.as_str().is_some_and(|s| s.contains("denied")))
        });
        assert!(denied, "Policy 拒绝应回喂 denied 错误");
        let done = events.iter().rev().find_map(|e| match e {
            AgentEvent::Done { output, .. } => Some(output.clone()),
            _ => None,
        });
        assert_eq!(done.as_deref(), Some("DONE"), "拒绝后循环应继续并收口");
    }

    #[tokio::test]
    async fn prompt_policy_waits_for_approval_then_runs_tool() {
        use crate::Session;

        struct PromptNoop;
        impl Policy for PromptNoop {
            fn check(
                &self,
                call: &ToolCall,
                _tier: ToolTier,
                _workdir: &std::path::Path,
            ) -> Verdict {
                if call.function.name == "noop" {
                    Verdict::Prompt {
                        reason: "test approval".into(),
                    }
                } else {
                    Verdict::Allow
                }
            }
        }

        let agent = Agent::builder()
            .llm(ScriptedLlm::new(vec![
                call_decision("noop"),
                text_decision("DONE"),
            ]))
            .tool(Arc::new(StubTool {
                name: "noop",
                tier: ToolTier::Exec,
            }))
            .policy(Arc::new(PromptNoop))
            .build();
        let mut session = Session::new(agent);
        let (mut stream, hrx, _) =
            session.turn(vec![ContentPart::Text("go".into())], LlmOpts::default());
        let mut saw_request = false;
        let mut saw_tool_ok = false;

        while let Some(ev) = stream.next().await {
            match ev {
                AgentEvent::ApprovalRequest {
                    approval_id, name, ..
                } => {
                    assert_eq!(name, "noop");
                    saw_request = true;
                    assert!(session.approve(ApprovalResponse {
                        approval_id,
                        decision: ApprovalDecision::Once,
                        reason: None,
                    }));
                }
                AgentEvent::ToolEnd { ok, result, .. } => {
                    saw_tool_ok |=
                        ok && result.as_str().is_some_and(|text| text.contains("STUB_OK"));
                }
                AgentEvent::Done { .. } => break,
                _ => {}
            }
        }

        session.commit(hrx.await.unwrap());
        assert!(saw_request);
        assert!(saw_tool_ok);
    }

    // §2.11：同一会话内对相同调用做 Session 决策后，同类调用静默放行（不再弹批准）。
    #[tokio::test]
    async fn session_decision_auto_allows_same_call() {
        use crate::Session;
        struct PromptNoop;
        impl Policy for PromptNoop {
            fn check(&self, call: &ToolCall, _t: ToolTier, _w: &std::path::Path) -> Verdict {
                if call.function.name == "noop" {
                    Verdict::Prompt { reason: "t".into() }
                } else {
                    Verdict::Allow
                }
            }
        }
        let agent = Agent::builder()
            .llm(ScriptedLlm::new(vec![
                call_decision("noop"),
                call_decision("noop"),
                text_decision("DONE"),
            ]))
            .tool(Arc::new(StubTool {
                name: "noop",
                tier: ToolTier::Exec,
            }))
            .policy(Arc::new(PromptNoop))
            .build();
        let mut session = Session::new(agent);
        let (mut stream, hrx, _) =
            session.turn(vec![ContentPart::Text("go".into())], LlmOpts::default());
        let mut requests = 0;
        let mut tool_oks = 0;
        while let Some(ev) = stream.next().await {
            match ev {
                AgentEvent::ApprovalRequest { approval_id, .. } => {
                    requests += 1;
                    assert!(session.approve(ApprovalResponse {
                        approval_id,
                        decision: ApprovalDecision::Session,
                        reason: None,
                    }));
                }
                AgentEvent::ToolEnd { ok, result, .. } => {
                    if ok && result.as_str().is_some_and(|t| t.contains("STUB_OK")) {
                        tool_oks += 1;
                    }
                }
                AgentEvent::Done { .. } => break,
                _ => {}
            }
        }
        session.commit(hrx.await.unwrap());
        assert_eq!(requests, 1, "Session 决策后同类调用应静默放行,只弹一次");
        assert_eq!(tool_oks, 2, "两次工具都应执行");
    }

    // §2.11：Deny 锁存——拒绝后同类调用静默拒绝、不再追问；两次都不执行。
    #[tokio::test]
    async fn deny_locks_same_call_for_session() {
        use crate::Session;
        struct PromptNoop;
        impl Policy for PromptNoop {
            fn check(&self, call: &ToolCall, _t: ToolTier, _w: &std::path::Path) -> Verdict {
                if call.function.name == "noop" {
                    Verdict::Prompt { reason: "t".into() }
                } else {
                    Verdict::Allow
                }
            }
        }
        let agent = Agent::builder()
            .llm(ScriptedLlm::new(vec![
                call_decision("noop"),
                call_decision("noop"),
                text_decision("DONE"),
            ]))
            .tool(Arc::new(StubTool {
                name: "noop",
                tier: ToolTier::Exec,
            }))
            .policy(Arc::new(PromptNoop))
            .build();
        let mut session = Session::new(agent);
        let (mut stream, hrx, _) =
            session.turn(vec![ContentPart::Text("go".into())], LlmOpts::default());
        let mut requests = 0;
        let mut tool_oks = 0;
        while let Some(ev) = stream.next().await {
            match ev {
                AgentEvent::ApprovalRequest { approval_id, .. } => {
                    requests += 1;
                    assert!(session.approve(ApprovalResponse {
                        approval_id,
                        decision: ApprovalDecision::Deny,
                        reason: None,
                    }));
                }
                AgentEvent::ToolEnd { ok, result, .. } => {
                    if ok && result.as_str().is_some_and(|t| t.contains("STUB_OK")) {
                        tool_oks += 1;
                    }
                }
                AgentEvent::Done { .. } => break,
                _ => {}
            }
        }
        session.commit(hrx.await.unwrap());
        assert_eq!(requests, 1, "Deny 锁存后同类调用应静默拒绝,只弹一次");
        assert_eq!(tool_oks, 0, "被拒调用不应执行");
    }

    // §2.11 Stage 2a：Always 经持久化端口跨会话/进程生效——一个会话 Always 放行后，
    // 全新 agent（同 store）的新会话对同类调用静默放行，不再弹批准。
    #[tokio::test]
    async fn always_persists_across_sessions() {
        use crate::Session;
        #[derive(Default)]
        struct MemApprovalStore {
            keys: std::sync::Mutex<Vec<String>>,
        }
        impl base_types::ApprovalStore for MemApprovalStore {
            fn load_always(&self) -> Vec<String> {
                self.keys.lock().unwrap().clone()
            }
            fn persist_always(&self, key: &str) {
                self.keys.lock().unwrap().push(key.to_string());
            }
        }
        struct PromptNoop;
        impl Policy for PromptNoop {
            fn check(&self, call: &ToolCall, _t: ToolTier, _w: &std::path::Path) -> Verdict {
                if call.function.name == "noop" {
                    Verdict::Prompt { reason: "t".into() }
                } else {
                    Verdict::Allow
                }
            }
        }
        let store = Arc::new(MemApprovalStore::default());

        // 会话 1：Always 放行 noop。
        {
            let agent = Agent::builder()
                .llm(ScriptedLlm::new(vec![
                    call_decision("noop"),
                    text_decision("DONE"),
                ]))
                .tool(Arc::new(StubTool {
                    name: "noop",
                    tier: ToolTier::Exec,
                }))
                .policy(Arc::new(PromptNoop))
                .approval_store(store.clone())
                .build();
            let mut session = Session::new(agent);
            let (mut stream, hrx, _) =
                session.turn(vec![ContentPart::Text("go".into())], LlmOpts::default());
            while let Some(ev) = stream.next().await {
                match ev {
                    AgentEvent::ApprovalRequest { approval_id, .. } => {
                        assert!(session.approve(ApprovalResponse {
                            approval_id,
                            decision: ApprovalDecision::Always,
                            reason: None,
                        }));
                    }
                    AgentEvent::Done { .. } => break,
                    _ => {}
                }
            }
            session.commit(hrx.await.unwrap());
        }
        assert_eq!(
            store.keys.lock().unwrap().len(),
            1,
            "Always 应持久化一个 key"
        );

        // 会话 2：全新 agent（同 store）→ 同类调用静默放行，0 次批准请求。
        {
            let agent = Agent::builder()
                .llm(ScriptedLlm::new(vec![
                    call_decision("noop"),
                    text_decision("DONE"),
                ]))
                .tool(Arc::new(StubTool {
                    name: "noop",
                    tier: ToolTier::Exec,
                }))
                .policy(Arc::new(PromptNoop))
                .approval_store(store.clone())
                .build();
            let mut session = Session::new(agent);
            let (mut stream, hrx, _) =
                session.turn(vec![ContentPart::Text("go".into())], LlmOpts::default());
            let mut requests = 0;
            let mut tool_oks = 0;
            while let Some(ev) = stream.next().await {
                match ev {
                    AgentEvent::ApprovalRequest { approval_id, .. } => {
                        requests += 1;
                        let _ = session.approve(ApprovalResponse {
                            approval_id,
                            decision: ApprovalDecision::Deny,
                            reason: None,
                        });
                    }
                    AgentEvent::ToolEnd { ok, result, .. } => {
                        if ok && result.as_str().is_some_and(|t| t.contains("STUB_OK")) {
                            tool_oks += 1;
                        }
                    }
                    AgentEvent::Done { .. } => break,
                    _ => {}
                }
            }
            session.commit(hrx.await.unwrap());
            assert_eq!(requests, 0, "Always 持久化后新会话应静默放行,不弹批准");
            assert_eq!(tool_oks, 1, "工具应执行");
        }
    }

    // §1.8.8：force_recall 开时召回块增广进 user 消息（send-time，不写回 history）；关时不增广。
    #[tokio::test]
    async fn force_recall_augments_user_message_not_persisted() {
        use crate::Session;
        struct StubRecall;
        #[async_trait::async_trait]
        impl agent_act::memory::QueryRecall for StubRecall {
            async fn recall_block(&self, _q: &str) -> Option<String> {
                Some("RECALL-BLOCK".into())
            }
        }
        let saw_block = |opts: LlmOpts| async move {
            let agent = Agent::builder()
                .llm(ScriptedLlm::new(vec![text_decision("ok")]))
                .system("ROLE")
                .recall(Arc::new(StubRecall))
                .build();
            let mut session = Session::new(agent);
            let (mut stream, hrx, _) = session.turn(vec![ContentPart::Text("hi".into())], opts);
            let mut saw = false;
            while let Some(ev) = stream.next().await {
                if let AgentEvent::Debug { label, data, .. } = &ev {
                    if label == "llm_request" && data.to_string().contains("RECALL-BLOCK") {
                        saw = true;
                    }
                }
                if matches!(ev, AgentEvent::Done { .. }) {
                    break;
                }
            }
            let final_history = hrx.await.unwrap();
            session.commit(final_history.clone());
            let persisted = serde_json::to_string(&final_history).unwrap();
            (saw, persisted.contains("RECALL-BLOCK"))
        };
        // OFF：不增广。
        let (off_saw, _) = saw_block(LlmOpts::default()).await;
        assert!(!off_saw, "force_recall 关时不应增广");
        // ON：增广进 LLM 消息，但不写回持久 history。
        let (on_saw, on_persisted) = saw_block(LlmOpts {
            force_recall: true,
            ..Default::default()
        })
        .await;
        assert!(on_saw, "force_recall 开时召回块应增广进 LLM 消息");
        assert!(
            !on_persisted,
            "召回块不应写回持久 history（send-time only）"
        );
    }

    // §1.8.8 S4：turn 成功收口后触发 episode 抽取钩子，带上 role + 本轮 transcript。
    #[tokio::test]
    async fn episodic_hook_fires_on_turn_complete() {
        use std::sync::Mutex as StdMutex;
        #[derive(Default)]
        struct Rec {
            calls: Arc<StdMutex<Vec<(String, String, usize)>>>,
        }
        impl agent_act::episode::EpisodicHook for Rec {
            fn on_turn_complete(&self, session_id: String, role: String, transcript: Vec<Message>) {
                self.calls
                    .lock()
                    .unwrap()
                    .push((session_id, role, transcript.len()));
            }
        }
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let hook = Arc::new(Rec {
            calls: calls.clone(),
        });
        let agent = Agent::builder()
            .llm(ScriptedLlm::new(vec![text_decision("ok")]))
            .system("ROLE-XYZ")
            .episodic(hook)
            .build();
        let _ = collect(agent.run("hello")).await;
        let recs = calls.lock().unwrap();
        assert_eq!(recs.len(), 1, "turn 收口应触发一次 episode 钩子");
        assert_eq!(recs[0].1, "ROLE-XYZ", "应带 role");
        assert!(recs[0].2 >= 2, "transcript 应含本轮 user+assistant");
    }

    // §1.8.8 端到端：真 MemoryStore 当召回源——force_recall 开时 retain 的事实按 query 召回并增广。
    #[tokio::test]
    async fn force_recall_pulls_retained_fact_into_request() {
        use crate::Session;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("botobot-mem-e2e-{nanos}.jsonl"));
        let _ = std::fs::remove_file(&path);
        let store = Arc::new(agent_act::memory::MemoryStore::open(&path).unwrap());
        store.retain_in_bank("default", "我叫张三").unwrap();
        let mem_res = Arc::new(agent_act::memory::MemoryResource::new(store.clone()));

        let agent = Agent::builder()
            .llm(ScriptedLlm::new(vec![text_decision("ok")]))
            .system("ROLE")
            .recall(mem_res)
            .build();
        let mut session = Session::new(agent);
        let (mut stream, hrx, _) = session.turn(
            vec![ContentPart::Text("我叫什么".into())],
            LlmOpts {
                force_recall: true,
                ..Default::default()
            },
        );
        let mut saw = false;
        while let Some(ev) = stream.next().await {
            if let AgentEvent::Debug { label, data, .. } = &ev {
                if label == "llm_request" && data.to_string().contains("张三") {
                    saw = true;
                }
            }
            if matches!(ev, AgentEvent::Done { .. }) {
                break;
            }
        }
        session.commit(hrx.await.unwrap());
        assert!(
            saw,
            "force_recall 应把 retain 的张三按 query 召回进 LLM 消息"
        );
        let _ = std::fs::remove_file(&path);
    }

    // §1.8.3b 端到端：UnifiedRecall 的 skill 能力提示穿过驱动器到达 LLM——
    // force_recall 开 + 空记忆，仅靠 skill 语义命中也应把「skill://<name>」增广进请求。
    #[tokio::test]
    async fn force_recall_surfaces_skill_capability_hint() {
        use crate::Session;

        // 主题 stub embedder（cat/car/food 三正交主题）。
        struct StubEmb;
        impl base_types::Embedder for StubEmb {
            fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
                Ok(texts
                    .iter()
                    .map(|t| {
                        let t = t.to_lowercase();
                        let mut v: [f32; 3] = [
                            t.contains("cat") as i32 as f32,
                            t.contains("car") as i32 as f32,
                            t.contains("food") as i32 as f32,
                        ];
                        let n = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
                        if n > 0.0 {
                            for x in &mut v {
                                *x /= n;
                            }
                        } else {
                            v = [0.577, 0.577, 0.577];
                        }
                        v.to_vec()
                    })
                    .collect())
            }
            fn dim(&self) -> usize {
                3
            }
        }

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("botobot-skillhint-e2e-{nanos}.jsonl"));
        let _ = std::fs::remove_file(&path);
        // 空记忆库：验证能力提示独立于记忆命中也能出现。
        let store = Arc::new(agent_act::memory::MemoryStore::open(&path).unwrap());
        let mem_res = Arc::new(agent_act::memory::MemoryResource::new(store));

        let skills = vec![agent_act::skill::parse_skill(
            "feline",
            "---\ndescription: all about cat care\n---\nbody",
        )];
        let skill_res = Arc::new(agent_act::skill::SkillResource::new(&skills));
        skill_res.set_embedder(Arc::new(StubEmb));

        let hints: Vec<Arc<dyn agent_act::recall::CapabilityHint>> = vec![skill_res];
        let unified = Arc::new(agent_act::recall::UnifiedRecall::new(mem_res, hints));

        let agent = Agent::builder()
            .llm(ScriptedLlm::new(vec![text_decision("ok")]))
            .system("ROLE")
            .recall(unified)
            .build();
        let mut session = Session::new(agent);
        let (mut stream, hrx, _) = session.turn(
            vec![ContentPart::Text("how do I care for a cat".into())],
            LlmOpts {
                force_recall: true,
                ..Default::default()
            },
        );
        let mut saw = false;
        while let Some(ev) = stream.next().await {
            if let AgentEvent::Debug { label, data, .. } = &ev {
                if label == "llm_request" && data.to_string().contains("skill://feline") {
                    saw = true;
                }
            }
            if matches!(ev, AgentEvent::Done { .. }) {
                break;
            }
        }
        session.commit(hrx.await.unwrap());
        assert!(
            saw,
            "force_recall 应把 skill 能力提示 skill://feline 增广进 LLM 消息"
        );
        let _ = std::fs::remove_file(&path);
    }

    // §4.9 A2：首个 infer 返回 413（请求过大）+ 消息含图片 → 剥图重试一次 → 恢复成功（Done 而非 Error）。
    #[tokio::test]
    async fn payload_too_large_strips_images_and_recovers() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Flaky413(AtomicUsize);
        #[async_trait]
        impl Llm for Flaky413 {
            async fn infer(
                &self,
                _m: &[Message],
                _t: &[base_types::ToolSpec],
                _o: &LlmOpts,
            ) -> Result<base_types::LlmStream, LlmError> {
                let n = self.0.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    return Err(LlmError::Api {
                        status: 413,
                        body: "request too large".into(),
                        retry_after: None,
                    });
                }
                let d = Decision {
                    text: "RECOVERED".into(),
                    finish_reason: Some("stop".into()),
                    ..Default::default()
                };
                Ok(Box::pin(futures::stream::iter(vec![Ok(LlmEvent::Done(d))])))
            }
        }
        let agent = Agent::builder()
            .llm(Arc::new(Flaky413(AtomicUsize::new(0))))
            .build();
        let (stream, _c) = agent.run_cancellable(
            vec![
                ContentPart::Text("看这张图".into()),
                ContentPart::ImageUrl("data:image/png;base64,AAAA".into()),
            ],
            LlmOpts::default(),
        );
        let events = collect(stream).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Done { output, .. } if output == "RECOVERED")),
            "413 + 图片应剥图重试并恢复，got: {events:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(e, AgentEvent::Error { .. })),
            "恢复成功不应发 Error"
        );
    }

    #[tokio::test]
    async fn prompt_policy_fails_closed_without_approval_channel() {
        // 安全命脉：Prompt 判定但**无审批通道**（如 agent.run / subagent）→ 工具**不执行**
        // （fail-closed，绝非 fail-open 默认放行）。
        struct PromptAll;
        impl Policy for PromptAll {
            fn check(&self, _c: &ToolCall, _t: ToolTier, _w: &std::path::Path) -> Verdict {
                Verdict::Prompt {
                    reason: "needs approval".into(),
                }
            }
        }
        let agent = Agent::builder()
            .llm(ScriptedLlm::new(vec![
                call_decision("noop"),
                text_decision("DONE"),
            ]))
            .tool(Arc::new(StubTool {
                name: "noop",
                tier: ToolTier::Exec,
            }))
            .policy(Arc::new(PromptAll))
            .build();
        let events = collect(agent.run("go")).await;

        // 工具绝不能执行（无 STUB_OK）
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolEnd { ok: true, result, .. }
                if result.as_str().is_some_and(|s| s.contains("STUB_OK")))),
            "无审批通道时 Prompt 工具不得执行"
        );
        // 回喂一个 approval-required 的带内错误（ok=false）
        assert!(
            events.iter().any(
                |e| matches!(e, AgentEvent::ToolEnd { ok: false, result, .. }
                if result.as_str().is_some_and(|s| s.contains("approval")))
            ),
            "应回喂 approval required 错误"
        );
    }

    #[tokio::test]
    async fn token_budget_blocks_before_infer_when_prompt_would_exceed() {
        let agent = Agent::builder()
            .llm(ScriptedLlm::new(vec![text_decision("SHOULD_NOT_RUN")]))
            .token_budget(1)
            .build();

        let events = collect(agent.run("hello")).await;

        assert!(events.iter().any(|e| {
            matches!(e, AgentEvent::Error { message, .. }
                if message.contains("token budget exhausted"))
        }));
        assert!(
            !events.iter().any(|e| {
                matches!(e, AgentEvent::Token { text, .. } if text == "SHOULD_NOT_RUN")
            })
        );
    }

    #[tokio::test]
    async fn token_budget_uses_provider_usage_after_infer() {
        let agent = Agent::builder()
            .llm(ScriptedLlm::new(vec![usage_decision("TOO_EXPENSIVE", 25)]))
            .token_budget(20)
            .build();

        let events = collect(agent.run("go")).await;

        assert!(events.iter().any(|e| {
            matches!(e, AgentEvent::Error { message, .. }
                if message.contains("token budget exhausted"))
        }));
        assert!(
            !events.iter().any(|e| matches!(e, AgentEvent::Done { .. })),
            "actual provider usage over budget should prevent Done"
        );
    }

    // §2.7 token live：每次 infer 后发 Usage 事件（累计 spent>0），即使未设预算（budget=None）。
    #[tokio::test]
    async fn emits_usage_event_after_infer_even_without_budget() {
        let agent = Agent::builder()
            .llm(ScriptedLlm::new(vec![usage_decision("DONE", 42)]))
            .build();
        let events = collect(agent.run("go")).await;
        let usage = events.iter().find_map(|e| match e {
            AgentEvent::Usage { spent, budget, .. } => Some((*spent, *budget)),
            _ => None,
        });
        let (spent, budget) = usage.expect("应发出 Usage 事件");
        assert!(spent > 0, "累计 spent 应 > 0，got {spent}");
        assert_eq!(budget, None, "未设预算时 budget=None");
    }

    #[tokio::test]
    async fn token_budget_is_shared_with_sub_agents() {
        let child = Agent::builder()
            .llm(ScriptedLlm::new(vec![text_decision(
                "CHILD_SHOULD_NOT_RUN",
            )]))
            .build();
        let parent = Agent::builder()
            .llm(ScriptedLlm::new(vec![
                call_decision("child"),
                text_decision("PARENT_DONE"),
            ]))
            .sub_agent("child", "child agent", child)
            .token_budget(17)
            .build();

        let events = collect(parent.run("go")).await;

        assert!(events.iter().any(|e| {
            matches!(e, AgentEvent::Error { message, .. }
                if message.contains("token budget exhausted"))
        }));
        assert!(!events.iter().any(|e| {
            matches!(e, AgentEvent::Token { text, .. } if text == "CHILD_SHOULD_NOT_RUN")
        }));
    }

    #[tokio::test]
    async fn readonly_policy_allows_read_blocks_exec() {
        // read 级工具放行、exec 级工具拦截（§7a tier）。
        let agent = Agent::builder()
            .llm(ScriptedLlm::new(vec![
                call_decision("rd"),
                call_decision("ex"),
                text_decision("DONE"),
            ]))
            .tool(Arc::new(StubTool {
                name: "rd",
                tier: ToolTier::Read,
            }))
            .tool(Arc::new(StubTool {
                name: "ex",
                tier: ToolTier::Exec,
            }))
            .policy(Arc::new(crate::ReadOnly))
            .build();

        let events = collect(agent.run("go")).await;

        let read_ok = events.iter().any(|e| {
            matches!(e, AgentEvent::ToolEnd { ok: true, result, .. }
                if result.as_str() == Some("STUB_OK"))
        });
        let exec_denied = events.iter().any(|e| {
            matches!(e, AgentEvent::ToolEnd { ok: false, result, .. }
                if result.as_str().is_some_and(|s| s.contains("read-only")))
        });
        assert!(read_ok, "read 级工具应放行");
        assert!(exec_denied, "exec 级工具应被只读策略拦截");
    }

    #[tokio::test]
    async fn custom_control_stops_early_despite_tool_calls() {
        struct StopNow;
        impl Control for StopNow {
            fn next(&self, _d: &Decision, _s: usize) -> Flow {
                Flow::Stop
            }
        }
        let agent = Agent::builder()
            .llm(ScriptedLlm::new(vec![call_decision("noop")])) // 模型发起了工具调用
            .control(Arc::new(StopNow))
            .build();

        let events = collect(agent.run("go")).await;

        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolStart { .. })),
            "Stop 控制下不应派发任何工具"
        );
        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::Done { .. })),
            "应直接收口"
        );
    }

    #[tokio::test]
    async fn sub_agent_happy_path_nests_and_returns() {
        let leaf = Agent::builder()
            .llm(ScriptedLlm::new(vec![text_decision("LEAF_RESULT")]))
            .build();
        let top = Agent::builder()
            .llm(ScriptedLlm::new(vec![
                call_decision("leaf"),
                text_decision("TOP_DONE"),
            ]))
            .sub_agent("leaf", "a sub agent", leaf)
            .build();

        let events = collect(top.run("go")).await;
        let root_session = events
            .first()
            .map(|e| e.session_id().to_string())
            .expect("应有事件");

        let nested = events
            .iter()
            .any(|e| matches!(e, AgentEvent::Start { parent_id: Some(p), .. } if !p.is_empty()));
        assert!(nested, "应出现带 parent_id 的子 agent Start 事件");
        assert!(
            events.iter().all(|e| e.session_id() == root_session),
            "子 agent 应继承父 session_id"
        );

        let fed = events.iter().any(|e| {
            matches!(e, AgentEvent::ToolEnd { ok: true, result, .. }
                if result.as_str() == Some("LEAF_RESULT"))
        });
        assert!(fed, "子 agent 输出应作为 ToolEnd 结果");

        let done = events.iter().rev().find_map(|e| match e {
            AgentEvent::Done { output, .. } => Some(output.clone()),
            _ => None,
        });
        assert_eq!(done.as_deref(), Some("TOP_DONE"));
    }

    #[tokio::test]
    async fn sub_agent_persists_subsession_when_store_injected() {
        use std::sync::Mutex as StdMutex;

        #[derive(Default)]
        struct RecordingStore {
            records: StdMutex<Vec<(String, String, usize)>>,
        }
        impl base_types::SubsessionStore for RecordingStore {
            fn record_subsession(&self, child: &str, parent: &str) -> Result<(), String> {
                self.records
                    .lock()
                    .unwrap()
                    .push((child.to_string(), parent.to_string(), 0));
                Ok(())
            }
            fn persist_subsession_messages(
                &self,
                child: &str,
                msgs: &[Message],
            ) -> Result<(), String> {
                let mut recs = self.records.lock().unwrap();
                if let Some(r) = recs.iter_mut().find(|r| r.0 == child) {
                    r.2 = msgs.len();
                }
                Ok(())
            }
        }

        let store = Arc::new(RecordingStore::default());
        let leaf = Agent::builder()
            .llm(ScriptedLlm::new(vec![text_decision("LEAF_RESULT")]))
            .build();
        let top = Agent::builder()
            .llm(ScriptedLlm::new(vec![
                call_decision("leaf"),
                text_decision("TOP_DONE"),
            ]))
            .sub_agent("leaf", "a sub agent", leaf)
            .subsession_store(store.clone())
            .build();

        let _ = collect(top.run("go")).await;

        let recs = store.records.lock().unwrap();
        assert_eq!(recs.len(), 1, "应落盘一个 subsession");
        let (child, parent, msg_count) = &recs[0];
        assert_ne!(child, parent, "子 session_id 应独立于父");
        assert!(!parent.is_empty(), "parent_session 应为父会话");
        assert!(*msg_count > 0, "子会话历史应非空");
    }

    #[tokio::test]
    async fn sub_agent_no_store_is_noop() {
        // 未注入 store 时 subagent 正常返回，不 panic（降级现状）。
        let leaf = Agent::builder()
            .llm(ScriptedLlm::new(vec![text_decision("LEAF_RESULT")]))
            .build();
        let top = Agent::builder()
            .llm(ScriptedLlm::new(vec![
                call_decision("leaf"),
                text_decision("TOP_DONE"),
            ]))
            .sub_agent("leaf", "a sub agent", leaf)
            .build();
        let events = collect(top.run("go")).await;
        let done = events.iter().rev().find_map(|e| match e {
            AgentEvent::Done { output, .. } => Some(output.clone()),
            _ => None,
        });
        assert_eq!(done.as_deref(), Some("TOP_DONE"));
    }

    #[tokio::test]
    async fn max_depth_blocks_and_terminates() {
        let leaf = Agent::builder()
            .llm(ScriptedLlm::new(vec![text_decision("LEAF_RESULT")]))
            .build();
        let top = Agent::builder()
            .llm(ScriptedLlm::new(vec![
                call_decision("leaf"),
                text_decision("TOP_DONE"),
            ]))
            .max_depth(0)
            .sub_agent("leaf", "a sub agent", leaf)
            .build();

        let events = collect(top.run("go")).await;

        let blocked = events.iter().any(|e| {
            matches!(e, AgentEvent::ToolEnd { ok: false, result, .. }
                if result.as_str().is_some_and(|s| s.contains("max depth")))
        });
        assert!(blocked, "max_depth 应拒绝子 agent 调用并回喂错误");
        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::Done { .. })),
            "顶层应正常终止"
        );
    }

    #[tokio::test]
    async fn precancelled_run_terminates_with_error() {
        let agent = Agent::builder()
            .llm(ScriptedLlm::new(vec![call_decision("noop"); 100]))
            .max_steps(100)
            .build();

        let (stream, cancel) =
            agent.run_cancellable(vec![ContentPart::Text("go".into())], LlmOpts::default());
        cancel.cancel();
        let events = collect(stream).await;

        let cancelled = events.iter().any(
            |e| matches!(e, AgentEvent::Error { message, .. } if message.contains("cancelled")),
        );
        assert!(cancelled, "取消应立即终止并发出 cancelled 错误事件");
    }

    /// 工具执行中途取消应**当场打断**（不等工具跑完）。
    struct HangTool;
    #[async_trait]
    impl base_types::Tool for HangTool {
        fn name(&self) -> &str {
            "hang"
        }
        fn description(&self) -> &str {
            "hangs"
        }
        fn schema(&self) -> Value {
            serde_json::json!({ "type": "object" })
        }
        async fn call(&self, _args: Value) -> base_types::ToolResult {
            // 久等：若 cancel 不打断派发，run 会卡 60s
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            Ok(Value::String("LATE".into()))
        }
    }

    #[tokio::test]
    async fn cancel_during_tool_interrupts_promptly() {
        let agent = Agent::builder()
            .llm(ScriptedLlm::new(vec![call_decision("hang")]))
            .tool(Arc::new(HangTool))
            .build();
        let (stream, cancel) =
            agent.run_cancellable(vec![ContentPart::Text("go".into())], LlmOpts::default());
        // 等工具进入执行后再取消
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            cancel.cancel();
        });
        // 若派发未 race against cancel，这里会卡到 60s 而非秒级返回
        let events = tokio::time::timeout(std::time::Duration::from_secs(5), collect(stream))
            .await
            .expect("取消应在工具执行中途当场打断，而非等工具跑完 60s");
        assert!(
            events.iter().any(
                |e| matches!(e, AgentEvent::Error { message, .. } if message.contains("cancelled"))
            ),
            "工具执行中取消应发出 cancelled 错误事件"
        );
    }
}
