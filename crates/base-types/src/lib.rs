//! base-types：共享类型的家（契约 + 共享词汇 + 共享运行态）。
//!
//! 这里定义 harness 的**三个切入点契约**与它们交换的**共享词汇 / 运行态**，不含任何实现。
//! 实现层（agent-infer 的 provider、agent-act 的 registry、agent-loop 的 driver）单向依赖本层。
//!
//! - 推理（Reason）：[`Llm`]，吃 `&[Message]`+`&[ToolSpec]`，吐 [`LlmEvent`] 流（收口 [`Decision`]）。
//! - 动作（Act）  ：[`Tool`]（对象安全擦除边界）；[`ToolLookup`] 是注册表的缝。
//! - 观察（Observe）：[`Observe`]，把整轮 [`ToolOutcome`] 折回 [`Context`]。
//! - 运行态：[`Context`]、[`Budget`]、[`EventSink`]、[`AgentEvent`]。
//!
//! 驱动器（reason→act→observe 循环）不在此层——它只有一个规范实现（agent-loop），
//! 本层通过"拥有它操作的三 trait + 运行态类型"来规范它，而非把 Driver 也抽象成 trait。

use async_trait::async_trait;
use futures::Stream;
use serde::de::Error as _;
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// ═══════════════════════ 词汇：消息 / 多模态 ═══════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// 多模态消息内容的一段：文本或图像。
#[derive(Debug, Clone)]
pub enum ContentPart {
    Text(String),
    /// 图像 URL，可为 `http(s)://...` 或 `data:image/png;base64,...`。
    ImageUrl(String),
}

impl Serialize for ContentPart {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(2))?;
        match self {
            ContentPart::Text(t) => {
                m.serialize_entry("type", "text")?;
                m.serialize_entry("text", t)?;
            }
            ContentPart::ImageUrl(url) => {
                m.serialize_entry("type", "image_url")?;
                m.serialize_entry("image_url", &ImageUrlInner { url })?;
            }
        }
        m.end()
    }
}

#[derive(Serialize)]
struct ImageUrlInner<'a> {
    url: &'a str,
}

#[derive(Deserialize)]
struct ContentPartWire {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    image_url: Option<ImageUrlWire>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ImageUrlWire {
    Object { url: String },
    String(String),
}

impl ImageUrlWire {
    fn into_url(self) -> String {
        match self {
            Self::Object { url } | Self::String(url) => url,
        }
    }
}

impl<'de> Deserialize<'de> for ContentPart {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let wire = ContentPartWire::deserialize(d)?;
        match wire.kind.as_str() {
            "text" => Ok(Self::Text(wire.text.unwrap_or_default())),
            "image_url" => wire
                .image_url
                .map(|u| Self::ImageUrl(u.into_url()))
                .ok_or_else(|| D::Error::custom("image_url content part missing image_url.url")),
            other => Err(D::Error::custom(format!(
                "unsupported content part type {other}"
            ))),
        }
    }
}

/// 一条对话消息（OpenAI Chat Completions 形状的子集，支持多模态 content）。
#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    /// 纯文本时序列化为 string；含图像时序列化为 parts 数组（最大兼容）。
    pub content: Vec<ContentPart>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self::text(Role::System, content)
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self::text(Role::User, content)
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::text(Role::Assistant, content)
    }
    fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![ContentPart::Text(content.into())],
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
    pub fn user_parts(parts: Vec<ContentPart>) -> Self {
        Self {
            role: Role::User,
            content: parts,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
    pub fn assistant_calls(text: String, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: if text.is_empty() {
                Vec::new()
            } else {
                vec![ContentPart::Text(text)]
            },
            tool_calls,
            tool_call_id: None,
        }
    }
    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: vec![ContentPart::Text(content.into())],
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
        }
    }
}

impl Serialize for Message {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(None)?;
        m.serialize_entry("role", &self.role)?;
        let has_image = self
            .content
            .iter()
            .any(|p| matches!(p, ContentPart::ImageUrl(_)));
        if has_image {
            m.serialize_entry("content", &self.content)?;
        } else {
            let text: String = self
                .content
                .iter()
                .map(|p| match p {
                    ContentPart::Text(t) => t.as_str(),
                    ContentPart::ImageUrl(_) => "",
                })
                .collect();
            if !text.is_empty() {
                m.serialize_entry("content", &text)?;
            }
        }
        if !self.tool_calls.is_empty() {
            m.serialize_entry("tool_calls", &self.tool_calls)?;
        }
        if let Some(id) = &self.tool_call_id {
            m.serialize_entry("tool_call_id", id)?;
        }
        m.end()
    }
}

#[derive(Deserialize)]
struct MessageWire {
    role: Role,
    #[serde(default)]
    content: Option<MessageContentWire>,
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
    #[serde(default)]
    tool_call_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum MessageContentWire {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl<'de> Deserialize<'de> for Message {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let wire = MessageWire::deserialize(d)?;
        let content = match wire.content {
            Some(MessageContentWire::Text(text)) if !text.is_empty() => {
                vec![ContentPart::Text(text)]
            }
            Some(MessageContentWire::Text(_)) | None => Vec::new(),
            Some(MessageContentWire::Parts(parts)) => parts,
        };
        Ok(Self {
            role: wire.role,
            content,
            tool_calls: wire.tool_calls,
            tool_call_id: wire.tool_call_id,
        })
    }
}

// ═══════════════════════ 词汇：工具调用 / 规格 ═══════════════════════

/// 模型发起的一次工具调用。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "function_kind")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

fn function_kind() -> String {
    "function".to_string()
}

/// 告诉模型"有哪些工具可用"的描述。
#[derive(Debug, Clone, Serialize)]
pub struct ToolSpec {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionSpec,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl ToolSpec {
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
    ) -> Self {
        Self {
            kind: "function".to_string(),
            function: FunctionSpec {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
}

// ═══════════════════════ 词汇：决策 / 事件 / 错误 ═══════════════════════

/// 一次推理的完整决策（流的收口值）。
#[derive(Debug, Clone, Default)]
pub struct Decision {
    pub text: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    /// Provider-reported token usage for this inference, when the backend returns it.
    ///
    /// This is optional because local OpenAI-compatible servers vary: some emit usage
    /// only with `stream_options.include_usage`, some omit it entirely.
    pub usage: Option<TokenUsage>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: Option<usize>,
    pub completion_tokens: Option<usize>,
    pub total_tokens: Option<usize>,
}

/// 推理过程中的流式事件。
#[derive(Debug, Clone)]
pub enum LlmEvent {
    TextDelta(String),
    ReasoningDelta(String),
    Done(Decision),
}

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("http error: {0}")]
    Http(String),
    #[error("api error: {status} {body}")]
    Api {
        status: u16,
        body: String,
        /// 服务端 `Retry-After` 提示（秒数解析）；瞬时重试时退避至少等到此时机。
        retry_after: Option<std::time::Duration>,
    },
    #[error("sse parse error: {0}")]
    Sse(String),
    /// 流停滞超时（§2.6 缺陷2，借鉴 codex `client.rs` 的 stream_idle_timeout）：
    /// SSE 在 idle 窗口内无新事件——瞬时，值得重试。
    #[error("stream idle timeout")]
    Idle,
    #[error("missing api key: set OPENAI_API_KEY or pass api_key explicitly")]
    MissingApiKey,
}

impl LlmError {
    /// 是否为**瞬时**错误（值得重试）——借鉴 oh-my-pi 的 retry 分类（P-1/§8）：
    /// 网络/连接、限流(429)、5xx 服务端、流首包失败。配置类(401/403/400/MissingApiKey)不重试。
    pub fn is_transient(&self) -> bool {
        match self {
            // 网络/连接层失败（reqwest 包装在 Http）：连接拒绝/重置/超时/断流等。
            LlmError::Http(_) => true,
            // 限流 + 5xx 服务端类可重试；4xx（除 429）是请求/鉴权问题，不重试。
            LlmError::Api { status, .. } => *status == 429 || (500..=599).contains(status),
            // SSE 流解析/首包失败：多为瞬时传输问题。
            LlmError::Sse(_) => true,
            // 停滞流超时：瞬时传输问题。
            LlmError::Idle => true,
            // 配置错误，重试无意义。
            LlmError::MissingApiKey => false,
        }
    }
}

pub type LlmResult<T> = Result<T, LlmError>;
pub type LlmStream = Pin<Box<dyn Stream<Item = LlmResult<LlmEvent>> + Send>>;

// ═══════════════════════ 推理切入点 ═══════════════════════

/// 单次推理的 per-call 开关。`thinking/web_search/code_execution` 透传到下层 LLM provider
/// （如 Qwen 的 `chat_template_kwargs.enable_thinking`），`None` = 走服务端默认。
/// `force_recall`（§1.8.8）是 **harness 侧**开关：true 时驱动器在发送前按本轮 query 检索记忆、
/// 增广进当前 user 消息（不透传给 provider）。
#[derive(Debug, Clone, Default)]
pub struct LlmOpts {
    pub thinking: Option<bool>,
    pub web_search: Option<bool>,
    pub code_execution: Option<bool>,
    /// §1.8.8 harness 强制记忆召回（per-turn opt-in，默认 false）。
    pub force_recall: bool,
}

/// 大脑：给定对话与可用工具，流式产出推理事件。
#[async_trait]
pub trait Llm: Send + Sync {
    async fn infer(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        opts: &LlmOpts,
    ) -> LlmResult<LlmStream>;
}

// ═══════════════════════ 动作切入点 ═══════════════════════

pub type ToolResult = anyhow::Result<Value>;

/// 工具能力分级（借鉴 oh-my-pi 的 approval tier，§7a）：供安全策略自动分级放行/拦截。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolTier {
    /// 只读：不改外部状态（如读文件、查询），最安全。
    Read,
    /// 写：改文件 / 持久状态。
    Write,
    /// 执行：shell / 网络 / 子 agent 等不可预知副作用，最危险（默认）。
    Exec,
}

/// 工具调度并发语义。默认并发；声明为 [`ToolConcurrency::Exclusive`] 的工具会由
/// driver 在前后批次之间单独串行执行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolConcurrency {
    Concurrent,
    Exclusive,
}

/// 工具加载模式：essential 每轮直接给模型，discoverable 只通过 tool_search 激活后给模型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolLoadMode {
    Essential,
    Discoverable,
}

/// 工具调用时由 driver 晚绑定注入的只读上下文快照。
///
/// 这不是 `&mut Context`，工具不能直接改写 history；需要改写上下文的能力仍应通过
/// driver/observe 等有明确串行边界的位置落地。
#[derive(Debug, Clone)]
pub struct ToolCtx {
    pub session_id: String,
    pub run_id: String,
    pub parent_id: Option<String>,
    pub workdir: PathBuf,
    pub cancel: CancellationToken,
    pub depth: usize,
    pub max_depth: usize,
    pub token_budget: Option<usize>,
    pub llm_opts: LlmOpts,
}

/// 手：对象安全的擦除边界（所有工具对外都长成 `Value -> Value`）。
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> Value;
    /// 能力分级（默认最严格 [`ToolTier::Exec`]）。安全策略据此自动放行/拦截。
    fn tier(&self) -> ToolTier {
        ToolTier::Exec
    }
    /// 一句话简介（默认=`description`），供未来工具发现/搜索索引用（§8 P-8 预留）。
    fn summary(&self) -> &str {
        self.description()
    }
    /// 工具加载模式。默认 essential；工具数量膨胀时可把低频工具改为 discoverable。
    fn load_mode(&self) -> ToolLoadMode {
        ToolLoadMode::Essential
    }
    /// 调度并发语义（默认并发）。需要独占上下文窗口/外部资源的工具可覆写为 Exclusive。
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }
    async fn call(&self, args: Value) -> ToolResult;
    /// 带只读上下文的调用入口。默认退回旧 `call`，保持所有现有工具兼容。
    async fn call_with_context(&self, args: Value, _ctx: &ToolCtx) -> ToolResult {
        self.call(args).await
    }
}

/// 工具查找缝：让 [`Context`] 持有"按名取工具"的能力而不依赖具体注册表实现。
pub trait ToolLookup: Send + Sync {
    fn get(&self, name: &str) -> Option<Arc<dyn Tool>>;
    fn list(&self) -> Vec<Arc<dyn Tool>>;
}

// ═══════════════════════ 会话历史切入点（底座） ═══════════════════════

/// 会话历史的存储**底座**（第 4 契约，决策⑦）。它不属于 reason/act/observe 任一 slot——
/// 三者操作的状态都在它里面：`infer` 经 [`Context`] **读**它（[`History::view`]），
/// `observe` **写**它（[`History::push`]）。默认实现 [`VecHistory`]（Vec 背书，进程内）；
/// 可选持久化实现 [`FileHistory`] 会把消息写回 JSON 文件。
///
/// 注意：这是**会话消息历史**（session 级、每个 agent/subagent 各一份），
/// 与 `agent-act::memory` 的**跨对话 retain 主库**（bot 级共享、`memory://` 召回）是两层，
/// 命名上严格区分——"记忆/memory" 专指后者，详见 `docs/todo.md §10`。
pub trait History: Send + Sync {
    /// 完整历史视图（喂给 infer / 压缩器读取）。
    fn view(&self) -> &[Message];
    /// 追加一条消息。
    fn push(&mut self, msg: Message);
    /// 整体替换（窗口压缩重写 / 会话载入）。
    fn set(&mut self, msgs: Vec<Message>);
    /// 取走全部历史（结束时回传，多轮会话用）。
    fn take(&mut self) -> Vec<Message>;
}

/// 把 subagent run 落盘为第一级 subsession 的端口（§2.5b）。
///
/// `AgentTool` 在 `agent-loop`，`SessionStore` 在 `bot-api`（上层）；依赖单向向上禁止反依赖，
/// 故抽象放 `base-types`，由 `bot-api` 用 SessionStore 实现、`webui-bin` 注入。
/// `Context` 经 task-local 调用上下文逐层 seed，使任意深度的 subagent 都能落盘。
pub trait SubsessionStore: Send + Sync {
    /// 建 subsession 档案（kind=subagent，parent_session=parent）。
    fn record_subsession(&self, child: &str, parent: &str) -> Result<(), String>;
    /// 落盘 subsession 完整历史（messages.jsonl，一行一条）。
    fn persist_subsession_messages(&self, child: &str, msgs: &[Message]) -> Result<(), String>;
}

/// 工具批准 `Always` 档的持久化端口（§2.11）：跨进程/会话记住「永久放行」的 dedup_key。
/// 重依赖（文件落盘）实现经此端口注入，使 `agent-loop` 不直接碰磁盘。无注入时 `Always`
/// 降级为本会话行为。实现需自行容错（失败不应崩溃 agent）。
pub trait ApprovalStore: Send + Sync {
    /// 启动时载入已持久化的永久放行 dedup_key 集。
    fn load_always(&self) -> Vec<String>;
    /// 追加一个永久放行 dedup_key（幂等）。
    fn persist_always(&self, key: &str);
}

/// 文本嵌入器（§记忆语义召回）：把文本编码成 **L2 归一化**稠密向量，供语义检索
/// （记忆召回 / RAG）。重依赖（candle 模型）的实现（`model-embed`）经此端口注入，
/// 使上层（`agent-act` 记忆）**不直接依赖 candle**。`embed` 返回归一化向量
/// （两两点积即余弦相似度）；同步执行（CPU 推理，无需 async）。
pub trait Embedder: Send + Sync {
    /// 批量把文本编码成 L2 归一化向量，每个长度 = [`Self::dim`]。空输入返回空。
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String>;
    /// 向量维度。
    fn dim(&self) -> usize;
    /// §4.9 A3：产出这些向量的模型 id（如 `bge-small-zh-v1.5`）。记忆按此**只比较同一向量空间**——
    /// 模型升级后旧向量不可比，需重嵌入。缺省 `""`（未知）。
    fn model_id(&self) -> &str {
        ""
    }
}

/// 默认会话历史：`Vec<Message>` 背书，进程内、无持久化。
#[derive(Default)]
pub struct VecHistory(Vec<Message>);

impl VecHistory {
    pub fn new() -> Self {
        Self(Vec::new())
    }
    pub fn with(msgs: Vec<Message>) -> Self {
        Self(msgs)
    }
}

impl History for VecHistory {
    fn view(&self) -> &[Message] {
        &self.0
    }
    fn push(&mut self, msg: Message) {
        self.0.push(msg);
    }
    fn set(&mut self, msgs: Vec<Message>) {
        self.0 = msgs;
    }
    fn take(&mut self) -> Vec<Message> {
        std::mem::take(&mut self.0)
    }
}

/// 摘要式会话历史：保留最近 tail，把更早消息滚成一条 system 摘要。
///
/// 这是一个同步、确定性的 History 后端，适合在不启用 compact 工具时给长会话一个轻量兜底。
/// 它不调 LLM；摘要内容来自旧消息的角色 + 文本预览，因此不会把异步 provider 调用塞进
/// [`History`] trait。
pub struct SummaryHistory {
    messages: Vec<Message>,
    max_chars: usize,
    keep_recent: usize,
}

impl SummaryHistory {
    pub fn new(max_chars: usize, keep_recent: usize) -> Self {
        Self::with_messages(max_chars, keep_recent, Vec::new())
    }

    pub fn with_messages(max_chars: usize, keep_recent: usize, messages: Vec<Message>) -> Self {
        let mut history = Self {
            messages,
            max_chars: max_chars.max(1),
            keep_recent,
        };
        history.compact_if_needed();
        history
    }

    fn compact_if_needed(&mut self) {
        if history_chars(&self.messages) <= self.max_chars {
            return;
        }

        let sys_end = self
            .messages
            .iter()
            .take_while(|m| m.role == Role::System && !is_summary_message(m))
            .count();
        let tail_start = self
            .messages
            .len()
            .saturating_sub(self.keep_recent)
            .max(sys_end);
        if tail_start <= sys_end {
            return;
        }

        let selected = &self.messages[sys_end..tail_start];
        let summary_limit = self.max_chars.saturating_div(3).clamp(80, 4000);
        let summary = summarize_messages(selected, summary_limit);

        let mut next = self.messages[..sys_end].to_vec();
        next.push(Message::system(format!(
            "[历史摘要：SummaryHistory 已压缩 {} 条较早消息]\n{summary}",
            selected.len()
        )));
        next.extend_from_slice(&self.messages[tail_start..]);
        self.messages = next;
    }
}

impl History for SummaryHistory {
    fn view(&self) -> &[Message] {
        &self.messages
    }
    fn push(&mut self, msg: Message) {
        self.messages.push(msg);
        self.compact_if_needed();
    }
    fn set(&mut self, msgs: Vec<Message>) {
        self.messages = msgs;
        self.compact_if_needed();
    }
    fn take(&mut self) -> Vec<Message> {
        std::mem::take(&mut self.messages)
    }
}

/// 文件背书的会话历史：把完整 `Vec<Message>` 存成 JSON 数组。
///
/// `History` trait 的写方法不返回 `Result`，因此 `push`/`set`/`take` 会先更新内存，
/// 再尽力写回文件；最近一次自动写回失败可通过 [`FileHistory::last_error`] 查看。需要强
/// 保证时，调用方可在构造或批量替换后显式调用 [`FileHistory::sync`]。
pub struct FileHistory {
    path: PathBuf,
    messages: Vec<Message>,
    last_error: Option<String>,
}

impl FileHistory {
    pub fn open(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let messages = match std::fs::read(&path) {
            Ok(bytes) if bytes.iter().all(u8::is_ascii_whitespace) => Vec::new(),
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(invalid_history_json)?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => return Err(err),
        };
        let mut history = Self {
            path,
            messages,
            last_error: None,
        };
        history.sync()?;
        Ok(history)
    }

    pub fn with_messages(
        path: impl Into<PathBuf>,
        messages: Vec<Message>,
    ) -> std::io::Result<Self> {
        let mut history = Self {
            path: path.into(),
            messages,
            last_error: None,
        };
        history.sync()?;
        Ok(history)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn sync(&mut self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(&self.messages).map_err(invalid_history_json)?;
        std::fs::write(&self.path, bytes)?;
        self.last_error = None;
        Ok(())
    }

    fn persist_best_effort(&mut self) {
        if let Err(err) = self.sync() {
            self.last_error = Some(err.to_string());
        }
    }
}

impl History for FileHistory {
    fn view(&self) -> &[Message] {
        &self.messages
    }
    fn push(&mut self, msg: Message) {
        self.messages.push(msg);
        self.persist_best_effort();
    }
    fn set(&mut self, msgs: Vec<Message>) {
        self.messages = msgs;
        self.persist_best_effort();
    }
    fn take(&mut self) -> Vec<Message> {
        let messages = std::mem::take(&mut self.messages);
        self.persist_best_effort();
        messages
    }
}

fn invalid_history_json(err: serde_json::Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, err)
}

fn history_chars(messages: &[Message]) -> usize {
    messages.iter().map(message_chars).sum()
}

fn message_chars(message: &Message) -> usize {
    message
        .content
        .iter()
        .map(|part| match part {
            ContentPart::Text(text) | ContentPart::ImageUrl(text) => text.chars().count(),
        })
        .sum()
}

fn is_summary_message(message: &Message) -> bool {
    message.role == Role::System
        && message
            .content
            .iter()
            .any(|p| matches!(p, ContentPart::Text(t) if t.starts_with("[历史摘要：")))
}

fn summarize_messages(messages: &[Message], limit: usize) -> String {
    let mut out = String::new();
    for message in messages {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(role_label(message.role));
        out.push_str(": ");
        append_limited(&mut out, &message_text(message), limit);
        if out.chars().count() >= limit {
            out.push_str("\n...[history summary truncated]");
            break;
        }
    }
    out
}

fn role_label(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn message_text(message: &Message) -> String {
    message
        .content
        .iter()
        .map(|part| match part {
            ContentPart::Text(text) => text.as_str(),
            ContentPart::ImageUrl(url) => url.as_str(),
        })
        .collect()
}

fn append_limited(out: &mut String, text: &str, limit: usize) {
    let remaining = limit.saturating_sub(out.chars().count());
    if remaining == 0 {
        return;
    }
    if text.chars().count() <= remaining {
        out.push_str(text);
        return;
    }
    out.extend(text.chars().take(remaining));
}

// ═══════════════════════ 运行态 ═══════════════════════

/// Agent 运行时向外发出的事件（可观测流，与返回值的数据流解耦）。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    Start {
        session_id: String,
        run_id: String,
        parent_id: Option<String>,
    },
    Token {
        session_id: String,
        run_id: String,
        text: String,
    },
    Reasoning {
        session_id: String,
        run_id: String,
        text: String,
    },
    ToolStart {
        session_id: String,
        run_id: String,
        call_id: String,
        name: String,
        args: Value,
    },
    ToolEnd {
        session_id: String,
        run_id: String,
        call_id: String,
        ok: bool,
        result: Value,
    },
    Diagnostics {
        session_id: String,
        run_id: String,
        source_call_id: String,
        ok: bool,
        summary: String,
        data: Value,
    },
    ApprovalRequest {
        session_id: String,
        run_id: String,
        approval_id: String,
        call_id: String,
        name: String,
        tier: ToolTier,
        reason: String,
        args: Value,
    },
    ApprovalResolved {
        session_id: String,
        run_id: String,
        approval_id: String,
        approved: bool,
        reason: Option<String>,
    },
    Done {
        session_id: String,
        run_id: String,
        output: String,
    },
    /// §2.7 token live：一次 infer 计费后发出**累计**已花 token（`spent`=本 run 至今累加，
    /// 来自 `Context::token_spent`，优先 provider usage 校准、否则估算）与可选预算（`budget`）。
    /// 让前端 live 显示「已用 / 预算」而无需等 turn 收口或重启延续。
    Usage {
        session_id: String,
        run_id: String,
        spent: usize,
        budget: Option<usize>,
    },
    Error {
        session_id: String,
        run_id: String,
        message: String,
    },
    /// 流中途失败后**重放前**发出：通知前端清空本 run 已 emit 的部分答案/推理文本，
    /// 避免重放重新生成时出现重复输出（§2.6 mid-stream re-infer 重放的幂等保证）。
    StreamReset { session_id: String, run_id: String },
    /// 细节（调试用）：llm 请求 payload、完整 tool result 等。`level=Debug`——
    /// **同样下发前端**，但由前端自行决定是否展示（默认折叠/隐藏）。`label` 标识种类。
    Debug {
        session_id: String,
        run_id: String,
        label: String,
        data: Value,
    },
}

/// 前后端交互事件级别（分级，**仅两档**）：
/// - `Info`：用户该看到的——告诉用户 agent 还在工作（没卡死）的进度信号 + 错误。
/// - `Debug`：细节（llm 请求 payload、完整 tool result 等），仍会进入事件流，由前端决定是否展示。
///
/// 两档都会下发前端；服务端日志另由 `bot-api` 按事件 kind 控制详略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EventLevel {
    Debug,
    Info,
}

impl AgentEvent {
    /// 事件类别短名（与序列化 `type` 一致），便于日志 / UI 分组。
    pub fn kind(&self) -> &'static str {
        match self {
            AgentEvent::Start { .. } => "start",
            AgentEvent::Token { .. } => "token",
            AgentEvent::Reasoning { .. } => "reasoning",
            AgentEvent::ToolStart { .. } => "tool_start",
            AgentEvent::ToolEnd { .. } => "tool_end",
            AgentEvent::Diagnostics { .. } => "diagnostics",
            AgentEvent::ApprovalRequest { .. } => "approval_request",
            AgentEvent::ApprovalResolved { .. } => "approval_resolved",
            AgentEvent::Done { .. } => "done",
            AgentEvent::Usage { .. } => "usage",
            AgentEvent::Error { .. } => "error",
            AgentEvent::StreamReset { .. } => "stream_reset",
            AgentEvent::Debug { .. } => "debug",
        }
    }

    /// 交互事件级别：[`AgentEvent::Debug`]=`Debug`（细节，前端决定是否展示），其余皆 `Info`
    /// （用户可见的进度/答案/错误）。两者**都下发前端**，前端按级过滤显示。
    pub fn level(&self) -> EventLevel {
        match self {
            AgentEvent::Debug { .. } => EventLevel::Debug,
            _ => EventLevel::Info,
        }
    }

    /// 该事件所属 run。
    pub fn run_id(&self) -> &str {
        match self {
            AgentEvent::Start { run_id, .. }
            | AgentEvent::Token { run_id, .. }
            | AgentEvent::Reasoning { run_id, .. }
            | AgentEvent::ToolStart { run_id, .. }
            | AgentEvent::ToolEnd { run_id, .. }
            | AgentEvent::Diagnostics { run_id, .. }
            | AgentEvent::ApprovalRequest { run_id, .. }
            | AgentEvent::ApprovalResolved { run_id, .. }
            | AgentEvent::Done { run_id, .. }
            | AgentEvent::Usage { run_id, .. }
            | AgentEvent::Error { run_id, .. }
            | AgentEvent::StreamReset { run_id, .. }
            | AgentEvent::Debug { run_id, .. } => run_id,
        }
    }

    /// 该事件所属 session。
    pub fn session_id(&self) -> &str {
        match self {
            AgentEvent::Start { session_id, .. }
            | AgentEvent::Token { session_id, .. }
            | AgentEvent::Reasoning { session_id, .. }
            | AgentEvent::ToolStart { session_id, .. }
            | AgentEvent::ToolEnd { session_id, .. }
            | AgentEvent::Diagnostics { session_id, .. }
            | AgentEvent::ApprovalRequest { session_id, .. }
            | AgentEvent::ApprovalResolved { session_id, .. }
            | AgentEvent::Done { session_id, .. }
            | AgentEvent::Usage { session_id, .. }
            | AgentEvent::Error { session_id, .. }
            | AgentEvent::StreamReset { session_id, .. }
            | AgentEvent::Debug { session_id, .. } => session_id,
        }
    }
}

/// 事件汇聚点。可廉价 clone 并在嵌套 run 间共享。`null()` 丢弃事件。
#[derive(Clone)]
pub struct EventSink(Option<mpsc::UnboundedSender<AgentEvent>>);

impl EventSink {
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<AgentEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self(Some(tx)), rx)
    }
    pub fn null() -> Self {
        Self(None)
    }
    pub fn emit(&self, ev: AgentEvent) {
        if let Some(tx) = &self.0 {
            let _ = tx.send(ev);
        }
    }
}

/// 安全阀。
#[derive(Debug, Clone)]
pub struct Budget {
    pub max_steps: usize,
    pub max_depth: usize,
    pub depth: usize,
    /// Estimated per-run token budget. `None` means unlimited.
    ///
    /// This is intentionally a safety valve: drivers may preflight with a
    /// configured tokenizer/estimator and then calibrate with provider-reported
    /// usage when available.
    pub token_budget: Option<usize>,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_steps: 16,
            max_depth: 4,
            depth: 0,
            token_budget: None,
        }
    }
}

/// 一次运行贯穿始终的上下文（显式结构 + `&mut` 贯穿）。
pub struct Context {
    /// 会话 id。顶层 CLI 会自动生成临时 id；Web/Hub 侧由 Session id 注入。
    pub session_id: String,
    /// 会话历史底座（决策⑦）：经 [`History`] 缝持有，不再裸 `Vec<Message>`。
    pub history: Box<dyn History>,
    /// 任务工作目录（§10d）：通用文件读写必须限制在此目录内。
    /// 托管资源（skill/book/memory/artifact）走专用 scheme，不把 bot home 暴露成通用文件面。
    pub workdir: std::path::PathBuf,
    pub tools: Arc<dyn ToolLookup>,
    pub sink: EventSink,
    pub run_id: String,
    pub parent_id: Option<String>,
    pub cancel: CancellationToken,
    pub budget: Budget,
    /// Shared estimated token spend for this top-level run. Sub-agents receive
    /// the same counter through task-local call context, so a token budget caps
    /// the whole recursive run rather than each nested loop separately.
    pub token_spent: Arc<std::sync::atomic::AtomicUsize>,
    /// 单次推理的 per-call 开关（透传到 Llm::infer）。Turn 创建时填入，
    /// 驱动器不动它。
    pub llm_opts: LlmOpts,
    /// subsession 落盘端口（§2.5b）。`None` = 不落盘（降级现状）。
    /// 经 task-local 调用上下文逐层传播，使任意深度的 subagent 都能落盘。
    pub subsession_store: Option<Arc<dyn SubsessionStore>>,
}

impl ToolCtx {
    pub fn from_context(ctx: &Context) -> Self {
        Self {
            session_id: ctx.session_id.clone(),
            run_id: ctx.run_id.clone(),
            parent_id: ctx.parent_id.clone(),
            workdir: ctx.workdir.clone(),
            cancel: ctx.cancel.clone(),
            depth: ctx.budget.depth,
            max_depth: ctx.budget.max_depth,
            token_budget: ctx.budget.token_budget,
            llm_opts: ctx.llm_opts.clone(),
        }
    }
}

// ═══════════════════════ 观察切入点 ═══════════════════════

/// 一次工具调用的结果——观察的输入单元。
pub struct ToolOutcome {
    pub call: ToolCall,
    /// Ok=工具返回值；Err=带内错误（回喂模型）。
    pub result: Result<Value, String>,
}

/// 观察：把整轮动作结果折回上下文（第三条 harness 缝）。副作用-only：
/// 是否继续由模型看到折回内容后决定。持 `&mut Context` 故可摘要/写记忆/裁剪窗口。
#[async_trait]
pub trait Observe: Send + Sync {
    async fn observe(&self, ctx: &mut Context, outcomes: Vec<ToolOutcome>);
}

// 窗口压缩不再是契约 trait（P3/决策⑧）：它是 `agent-act::compact` 的算法 + 一个
// 「驱动器拦截的控制工具」(`CompactTool`)，由驱动器在持 `&mut Context` 时串行改写
// `ctx.history`。base-types 不再定义 `Compact`。

// ═══════════════════════ 循环控制切入点（驱动器插座） ═══════════════════════

/// 一步 reason 之后的去向。
pub enum Flow {
    /// 继续循环（去执行本步 [`Decision`] 的工具调用）。
    Continue,
    /// 停止，以 `Decision::text` 作为最终输出。
    Stop,
}

/// 循环控制（P4/决策⑦，**driver 插座**）：一步 reason 产出 [`Decision`] 后判定停还是继续。
///
/// 默认实现 `UntilQuiet`（无 tool_calls 即停）在 `agent-loop`。可换：跑满 N 步、LLM 判官、
/// 出错重试、升级给上层。注：`cancel` / `max_steps` 是**硬**安全阀（[`Budget`]，集中在驱动器），
/// 不走本缝；本缝只管"模型说完了没"这条**软**判定。
pub trait Control: Send + Sync {
    fn next(&self, decision: &Decision, step: usize) -> Flow;
}

// ═══════════════════════ 安全策略切入点（驱动器安全阀） ═══════════════════════

/// 一次工具调用的授权裁决。
pub enum Verdict {
    /// 放行。
    Allow,
    /// 拒绝；`reason` 作为带内错误回喂模型（走两级错误的"工具失败"路径，循环继续）。
    Deny(String),
    /// 暂停并请求人审；批准后继续执行，拒绝则回喂工具失败。
    Prompt { reason: String },
}

/// 工具批准四档（§2.11，借鉴 hermes）。`Once/Session/Always` 都放行（`allows()`=true），
/// 区别在「记住多久」；`Deny` 拒绝并锁存同类（同会话不再追问）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalDecision {
    /// 仅这次放行。
    Once,
    /// 本会话剩余轮次自动放行同类。
    Session,
    /// 写入持久策略，永久放行同类（持久化端口未注入时降级为 Session 行为，Stage 2 接）。
    Always,
    /// 拒绝 + dedup_key 锁存：同类请求本会话不再追问。
    Deny,
}

impl ApprovalDecision {
    /// 是否放行（三档放行 vs 仅 Deny 拒绝）。
    pub fn allows(self) -> bool {
        !matches!(self, ApprovalDecision::Deny)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalResponse {
    pub approval_id: String,
    pub decision: ApprovalDecision,
    #[serde(default)]
    pub reason: Option<String>,
}

/// 工具授权（P5/决策⑦，**driver 安全阀**）：每个工具调用**执行前**过闸。
///
/// 与 `timeout` / `max_depth` 同属驱动器**安全阀**家族（集中在 driver、贴 act 边界），
/// 不是 trait-Core slot。默认 `AllowAll` 在 `agent-loop`。可换：白名单、参数沙箱、只读模式。
/// `tier` 由驱动器解析目标工具后传入（§7a：声明式分级），策略据此可按等级放行。
/// 注：HITL（人审）是本缝未来的一种 [`Verdict`]（暂停等人确认）——需人交互通道，暂不实现，
/// 但**不单开新缝**，归 Policy。
pub trait Policy: Send + Sync {
    fn check(&self, call: &ToolCall, tier: ToolTier, workdir: &Path) -> Verdict;
}

/// §4 命令沙箱：OS 级隔离的**接缝**。`wrap` 把一条 shell 命令包装成「在隔离环境里执行」的形式
/// （如 Linux `bwrap …` / macOS `sandbox-exec …` / Windows 隔离视图）。默认 [`NoopSandbox`] 原样返回
/// （无隔离=现状）。**纪律（别学 codex 体量）**：trait + Noop 先立，真后端**只做 0~1 个**（主力平台一个），
/// 其余 unsupported。升级路径：真隔离到位（`isolates()==true`）后，exec policy 可从「猜路径 Prompt」
/// 退化为「只拦破坏性」——命令物理出不了 workdir，启发式越界判定即可删。
pub trait Sandbox: Send + Sync {
    /// 把命令包装成沙箱内执行形式（`workdir` = 隔离根）。`NoopSandbox` 原样返回。
    fn wrap(&self, command: &str, workdir: &Path) -> String;
    /// 是否提供**真**隔离（OS 级文件系统/进程隔离）。`NoopSandbox`=false。
    /// exec policy 可据此放松越界启发式（隔离时命令出不了 workdir）。
    fn isolates(&self) -> bool {
        false
    }
}

/// 默认沙箱：不隔离，命令原样执行（= 引入 [`Sandbox`] 前的现状）。
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopSandbox;

impl Sandbox for NoopSandbox {
    fn wrap(&self, command: &str, _workdir: &Path) -> String {
        command.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    // §4 NoopSandbox：wrap 原样返回、isolates=false（无隔离=现状）。
    #[test]
    fn noop_sandbox_is_identity_and_not_isolating() {
        let sb = NoopSandbox;
        assert_eq!(
            sb.wrap("cargo build", Path::new(".")),
            "cargo build",
            "Noop 原样返回"
        );
        assert!(!sb.isolates(), "NoopSandbox 不提供真隔离");
        // 经 &dyn Sandbox 也成立（trait object 可用）。
        let dynsb: &dyn Sandbox = &sb;
        assert_eq!(dynsb.wrap("ls -la", Path::new("/tmp")), "ls -la");
    }

    #[test]
    fn llm_error_idle_is_transient_and_api_carries_retry_after() {
        assert!(
            LlmError::Idle.is_transient(),
            "Idle（停滞流超时）应为瞬时错误"
        );
        let e = LlmError::Api {
            status: 429,
            body: "rate limited".into(),
            retry_after: Some(Duration::from_secs(5)),
        };
        assert!(e.is_transient());
        match e {
            LlmError::Api { retry_after, .. } => {
                assert_eq!(retry_after, Some(Duration::from_secs(5)))
            }
            _ => panic!("应为 Api"),
        }
    }

    #[test]
    fn agent_event_classification() {
        let tok = AgentEvent::Token {
            session_id: "s".into(),
            run_id: "r".into(),
            text: "x".into(),
        };
        assert_eq!(tok.kind(), "token");
        assert_eq!(tok.session_id(), "s");
        assert_eq!(tok.run_id(), "r");
        let diagnostics = AgentEvent::Diagnostics {
            session_id: "s".into(),
            run_id: "r".into(),
            source_call_id: "c".into(),
            ok: true,
            summary: "diagnostics: 0 issues".into(),
            data: Value::Null,
        };
        assert_eq!(diagnostics.kind(), "diagnostics");
        assert_eq!(diagnostics.session_id(), "s");
        assert_eq!(diagnostics.run_id(), "r");

        // 进度/答案/错误 = info（前端默认展示）。
        for ev in [
            AgentEvent::Start {
                session_id: "s".into(),
                run_id: "r".into(),
                parent_id: None,
            },
            AgentEvent::Token {
                session_id: "s".into(),
                run_id: "r".into(),
                text: "x".into(),
            },
            AgentEvent::ToolEnd {
                session_id: "s".into(),
                run_id: "r".into(),
                call_id: "c".into(),
                ok: false,
                result: Value::Null,
            },
            diagnostics,
            AgentEvent::Error {
                session_id: "s".into(),
                run_id: "r".into(),
                message: "m".into(),
            },
        ] {
            assert_eq!(ev.level(), EventLevel::Info);
        }
        // 细节 = debug（也下发前端，前端决定是否展示）。
        let dbg = AgentEvent::Debug {
            session_id: "s".into(),
            run_id: "r".into(),
            label: "tool_result".into(),
            data: Value::Null,
        };
        assert_eq!(dbg.level(), EventLevel::Debug);
        assert_eq!(dbg.kind(), "debug");
    }

    #[test]
    fn event_level_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&EventLevel::Info).unwrap(),
            "\"info\""
        );
        assert_eq!(
            serde_json::to_string(&EventLevel::Debug).unwrap(),
            "\"debug\""
        );
    }

    #[test]
    fn message_json_roundtrip_supports_text_and_multimodal_content() {
        let messages = vec![
            Message::user("hello"),
            Message::user_parts(vec![
                ContentPart::Text("look".into()),
                ContentPart::ImageUrl("data:image/png;base64,abc".into()),
            ]),
            Message::tool_result("call-1", "ok"),
        ];

        let decoded: Vec<Message> =
            serde_json::from_str(&serde_json::to_string(&messages).unwrap()).unwrap();

        assert_eq!(decoded[0].role, Role::User);
        assert_eq!(text_content(&decoded[0]), "hello");
        assert_eq!(decoded[1].role, Role::User);
        assert_eq!(decoded[1].content.len(), 2);
        assert!(matches!(decoded[1].content[1], ContentPart::ImageUrl(_)));
        assert_eq!(decoded[2].tool_call_id.as_deref(), Some("call-1"));
    }

    #[test]
    fn file_history_persists_push_and_reopens() {
        let path = temp_history_path("push");
        let mut history = FileHistory::open(&path).unwrap();

        history.push(Message::user("persist me"));
        assert!(history.last_error().is_none());
        drop(history);

        let reopened = FileHistory::open(&path).unwrap();
        assert_eq!(reopened.view().len(), 1);
        assert_eq!(text_content(&reopened.view()[0]), "persist me");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn file_history_set_and_take_sync_file() {
        let path = temp_history_path("take");
        let mut history = FileHistory::with_messages(
            &path,
            vec![Message::system("sys"), Message::assistant("answer")],
        )
        .unwrap();

        history.set(vec![Message::user("new")]);
        assert_eq!(text_content(&history.view()[0]), "new");

        let taken = history.take();
        assert_eq!(text_content(&taken[0]), "new");
        assert!(history.view().is_empty());

        let reopened = FileHistory::open(&path).unwrap();
        assert!(reopened.view().is_empty());
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn summary_history_rolls_older_messages_into_system_summary() {
        let mut history = SummaryHistory::new(90, 2);
        history.push(Message::system("base rules"));
        history.push(Message::user("first user fact alpha beta gamma"));
        history.push(Message::assistant("first answer delta epsilon zeta"));
        history.push(Message::user("recent question should remain"));
        history.push(Message::assistant("recent answer should remain"));

        let view = history.view();
        assert_eq!(text_content(&view[0]), "base rules");
        assert!(text_content(&view[1]).starts_with("[历史摘要："));
        assert!(text_content(&view[1]).contains("first user fact"));
        assert_eq!(
            text_content(&view[view.len() - 2]),
            "recent question should remain"
        );
        assert_eq!(
            text_content(&view[view.len() - 1]),
            "recent answer should remain"
        );
        assert!(view.len() < 5);
    }

    #[test]
    fn summary_history_set_compacts_and_take_clears() {
        let mut history = SummaryHistory::with_messages(
            70,
            1,
            vec![
                Message::system("sys"),
                Message::user("old user message with useful constraint"),
                Message::assistant("old assistant answer with useful decision"),
                Message::user("tail"),
            ],
        );

        assert_eq!(text_content(&history.view()[0]), "sys");
        assert!(text_content(&history.view()[1]).contains("old user message"));
        assert_eq!(text_content(history.view().last().unwrap()), "tail");

        let taken = history.take();
        assert!(!taken.is_empty());
        assert!(history.view().is_empty());
    }

    fn text_content(msg: &Message) -> String {
        msg.content
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text(text) => Some(text.as_str()),
                ContentPart::ImageUrl(_) => None,
            })
            .collect()
    }

    fn temp_history_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("botobot-{name}-{nanos}.history.json"))
    }
}
