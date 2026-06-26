//! 窗口压缩（决策⑧）：算法落在动作域 `agent-act`。
//!
//! compact 是「驱动器拦截的控制工具」(R-C)：模型看见 [`CompactTool`] 的工具定义、可主动调用；
//! 真正改写 `ctx.history` 由**驱动器**串行执行（它才持 `&mut Context`，且避开并发写）。
//! 三件套：检测=硬（驱动器注提示）/ 软=agent 调本工具 / 兜底=硬（驱动器到硬上限强压）。

use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock};
use tokenizers::Tokenizer;

use base_types::{ContentPart, History, Message, Role, Tool, ToolResult};

/// 无-LLM 窗口压缩器：钉 system、保留最近约 `keep_tokens`、切点纪律（不切断 tool 配对）。
/// token 默认用 `chars/3` 兼容估算；设置 `BOTOBOT_TOKENIZER=tokenizer.json` 后使用
/// Hugging Face tokenizer.json 做真实 token 计数。
#[derive(Clone)]
pub struct Compactor {
    /// 真实模型窗口 tokens（三级解析后的值）。
    pub window: usize,
    /// 软阈值 = window×0.75：进入压缩 / hint。
    pub soft: usize,
    /// 硬阈值 = window×0.90：强制压缩上限（留 ~10% 响应 headroom）。
    pub hard: usize,
    /// tail 保护 = window×0.4：window_drop 保留量、prune/shake 保护最近这么多 token。
    pub keep_tokens: usize,
    /// 借鉴 oh-my-pi（§7a）：prune 时保护最近这么多 token 的工具输出不截断。
    pub protect_tokens: usize,
    /// 借鉴 oh-my-pi（§7a）：prune 估算省得 < 此值就不动手（避免无谓改写、毁前缀缓存）。
    pub min_savings: usize,
    /// shake 层：较早的单个文本块超过该估算 token 数时，替换成 notice。
    pub shake_min_tokens: usize,
    /// 可回溯折叠（Q5）：Some 时三层折叠 spill 原文留 `artifact://` 指针；None 降级回有损 notice。
    artifacts: Option<Arc<crate::artifact::ArtifactStore>>,
    pub estimator: TokenEstimator,
}

/// 已裁剪工具输出的哨兵前缀（幂等标记：见此前缀即视为已剪，不重复剪）。
const PRUNE_PREFIX: &str = "[工具输出已裁剪";
/// 已 shake 大文本块的哨兵前缀（幂等标记）。
const SHAKE_PREFIX: &str = "[大块文本已移除";

fn prune_notice(tokens: usize) -> String {
    format!("{PRUNE_PREFIX} ~{tokens} tokens]")
}

fn shake_notice(tokens: usize) -> String {
    format!("{SHAKE_PREFIX} ~{tokens} tokens]")
}

/// 已 window-drop 整段消息的哨兵前缀（幂等标记）。含「裁剪」二字，兼容旧断言。
const DROP_PREFIX: &str = "[已裁剪";

/// 取原文 head 预览（~120 字，按字符截断，换行压平）。
fn head_preview(s: &str) -> String {
    let one_line: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    one_line
        .chars()
        .take(120)
        .collect::<String>()
        .trim()
        .to_string()
}

/// 可回溯折叠占位（借鉴 oh-my-pi 的 artifact 外置 / datoobot 的 spill+指针）：
/// `[<前缀> ~N tok | head:'…' | artifact://aN]`。
fn fold_notice(prefix: &str, tokens: usize, original: &str, id: &str) -> String {
    format!(
        "{prefix} ~{tokens} tok | head:'{}' | artifact://{id}]",
        head_preview(original)
    )
}

/// 取一条 tool 消息的文本（非 tool 返回 None）。
fn tool_text(m: &Message) -> Option<&str> {
    if m.role != Role::Tool {
        return None;
    }
    m.content.iter().find_map(|p| match p {
        ContentPart::Text(t) => Some(t.as_str()),
        _ => None,
    })
}

impl Compactor {
    pub fn new(window: usize, artifacts: Option<Arc<crate::artifact::ArtifactStore>>) -> Self {
        let soft = window * 3 / 4; // 0.75
        let hard = window * 9 / 10; // 0.90
        let keep_tokens = window * 2 / 5; // 0.40
        Self {
            window,
            soft,
            hard,
            keep_tokens,
            protect_tokens: keep_tokens,
            min_savings: window / 4,
            // shake 是 prune 与 window_drop 之间的「温和中间层」：阈值取 window/16，
            // 让单条较大文本块（而非必须 1/4 窗口）就能被 shake，避免直接退到 window_drop。
            shake_min_tokens: (window / 16).max(32),
            artifacts,
            estimator: TokenEstimator::runtime(),
        }
    }

    /// 兜底硬上限：估算超过它，驱动器无条件强压（防 OOM）。现等于 hard。
    pub fn hard_ceiling(&self) -> usize {
        self.hard
    }

    /// 前缀 token 估算：history 开头【连续】`Role::System` 消息之和，遇首条非 System 即止。
    /// 借鉴 codex 的 BodyAfterPrefix——稳定前缀不纳入增长衡量，保 provider 前缀 KV 缓存。
    pub fn prefix_tokens(&self, msgs: &[Message]) -> usize {
        msgs.iter()
            .take_while(|m| m.role == Role::System)
            .map(|m| self.estimator.estimate_message(m))
            .sum()
    }

    /// body = total − prefix（前缀之后的增长量）。
    pub fn body_after_prefix(&self, msgs: &[Message]) -> usize {
        let total = self.estimator.estimate_messages(msgs);
        total.saturating_sub(self.prefix_tokens(msgs))
    }

    /// 是否越硬阈值（需强制压缩）。退化：`prefix > 0.5×window` 时改用 total 比 hard。
    pub fn should_force(&self, msgs: &[Message]) -> bool {
        if self.prefix_tokens(msgs) > self.window / 2 {
            self.estimator.estimate_messages(msgs) > self.hard
        } else {
            self.body_after_prefix(msgs) > self.hard
        }
    }

    /// 是否越软阈值（注 hint / 进入压缩）。同退化口径。
    pub fn over_soft(&self, msgs: &[Message]) -> bool {
        if self.prefix_tokens(msgs) > self.window / 2 {
            self.estimator.estimate_messages(msgs) > self.soft
        } else {
            self.body_after_prefix(msgs) > self.soft
        }
    }

    /// 就地压缩 `memory`（§7a 分层）：先 **prune**（截断较早的工具输出，省结构、保配对）→
    /// **shake**（移除较早的大文本块，保消息骨架）→ 仍超预算再 **window-drop**
    /// （丢整条较早消息）。返回被改动的消息条数（剪 + shake + 丢）。
    pub fn compact(&self, memory: &mut Box<dyn History>) -> usize {
        // 内部一律按 total（整段估算）收敛到 soft 以下；触发点的 body/total 口径差异
        // （normal 比 body、degrade 比 total）只决定「是否进入压缩」，进入后统一压 total。
        if self.estimator.estimate_messages(memory.view()) <= self.soft {
            return 0;
        }
        let pruned = self.prune(memory);
        let shaken = if self.estimator.estimate_messages(memory.view()) > self.soft {
            self.shake(memory)
        } else {
            0
        };
        let dropped = if self.estimator.estimate_messages(memory.view()) > self.soft {
            self.window_drop(memory)
        } else {
            0
        };
        pruned + shaken + dropped
    }

    /// prune 层（借鉴 oh-my-pi `pruneToolOutputs`）：自新向旧扫工具输出，保护最近
    /// `protect_tokens`，把更早的工具结果内容替换为 `[工具输出已裁剪 …]` 提示（保留 tool 配对结构）。
    /// 三招：①保护最近 N；②哨兵前缀幂等（已剪不再剪）；③省得 < `min_savings` 不动手。
    pub fn prune(&self, memory: &mut Box<dyn History>) -> usize {
        let msgs = memory.view();
        let mut acc = 0usize;
        let mut candidates: Vec<usize> = Vec::new();
        for i in (0..msgs.len()).rev() {
            let Some(text) = tool_text(&msgs[i]) else {
                continue;
            };
            let tokens = self.estimator.estimate_message(&msgs[i]);
            // 已剪过（哨兵）或仍在保护窗口内 → 计入累计但不作候选。
            if text.starts_with(PRUNE_PREFIX) || acc < self.protect_tokens {
                acc += tokens;
                continue;
            }
            candidates.push(i);
            acc += tokens;
        }
        if candidates.is_empty() {
            return 0;
        }
        // 估算总收益：原 tokens − 提示 tokens。
        let saved: usize = candidates
            .iter()
            .map(|&i| {
                let t = self.estimator.estimate_message(&msgs[i]);
                let notice = self.estimator.estimate_text(&prune_notice(t));
                t.saturating_sub(notice)
            })
            .sum();
        if saved < self.min_savings {
            return 0;
        }
        let cand: std::collections::HashSet<usize> = candidates.iter().copied().collect();
        let next: Vec<Message> = msgs
            .iter()
            .enumerate()
            .map(|(i, m)| {
                if cand.contains(&i) {
                    let t = self.estimator.estimate_message(m);
                    // 可回溯折叠：Some(store) 时 spill 原文留 artifact:// 指针；None 降级有损 notice。
                    let body = match &self.artifacts {
                        Some(store) => {
                            let original = tool_text(m).unwrap_or_default();
                            match store.put_text(original) {
                                Ok(id) => fold_notice(PRUNE_PREFIX, t, original, &id),
                                Err(_) => prune_notice(t), // spill 失败降级有损
                            }
                        }
                        None => prune_notice(t),
                    };
                    Message::tool_result(m.tool_call_id.clone().unwrap_or_default(), body)
                } else {
                    m.clone()
                }
            })
            .collect();
        let n = cand.len();
        memory.set(next);
        n
    }

    /// shake 层：自新向旧扫，保护最近 `protect_tokens`，把更早的超大文本块替换为
    /// `[大块文本已移除 …]` notice。它比 window-drop 温和：保留 role、tool_call 等消息骨架。
    pub fn shake(&self, memory: &mut Box<dyn History>) -> usize {
        let msgs = memory.view();
        let mut acc = 0usize;
        let mut candidates = Vec::new();
        for i in (0..msgs.len()).rev() {
            let tokens = self.estimator.estimate_message(&msgs[i]);
            if msgs[i].role == Role::System || acc < self.protect_tokens {
                acc += tokens;
                continue;
            }
            let has_big_text = msgs[i].content.iter().any(|p| match p {
                ContentPart::Text(t) => {
                    !t.starts_with(SHAKE_PREFIX)
                        && self.estimator.estimate_text(t) >= self.shake_min_tokens
                }
                ContentPart::ImageUrl(_) => false,
            });
            if has_big_text {
                candidates.push(i);
            }
            acc += tokens;
        }
        if candidates.is_empty() {
            return 0;
        }

        let saved: usize = candidates
            .iter()
            .map(|&i| {
                msgs[i]
                    .content
                    .iter()
                    .map(|p| match p {
                        ContentPart::Text(t)
                            if !t.starts_with(SHAKE_PREFIX)
                                && self.estimator.estimate_text(t) >= self.shake_min_tokens =>
                        {
                            let original = self.estimator.estimate_text(t);
                            let notice = self.estimator.estimate_text(&shake_notice(original));
                            original.saturating_sub(notice)
                        }
                        _ => 0,
                    })
                    .sum::<usize>()
            })
            .sum();
        if saved < self.min_savings {
            return 0;
        }

        let cand: std::collections::HashSet<usize> = candidates.iter().copied().collect();
        let next: Vec<Message> = msgs
            .iter()
            .enumerate()
            .map(|(i, m)| {
                if !cand.contains(&i) {
                    return m.clone();
                }
                let mut next = m.clone();
                next.content = next
                    .content
                    .into_iter()
                    .map(|p| match p {
                        ContentPart::Text(t)
                            if !t.starts_with(SHAKE_PREFIX)
                                && self.estimator.estimate_text(&t) >= self.shake_min_tokens =>
                        {
                            let est = self.estimator.estimate_text(&t);
                            // 可回溯折叠：Some(store) 时 spill 原块留指针；None 降级有损 notice。
                            let body = match &self.artifacts {
                                Some(store) => match store.put_text(&t) {
                                    Ok(id) => fold_notice(SHAKE_PREFIX, est, &t, &id),
                                    Err(_) => shake_notice(est),
                                },
                                None => shake_notice(est),
                            };
                            ContentPart::Text(body)
                        }
                        other => other,
                    })
                    .collect();
                next
            })
            .collect();
        let n = cand.len();
        memory.set(next);
        n
    }

    /// window-drop 层：钉 system、保留最近约 `keep_tokens`，丢弃中间较早整条消息，
    /// 遵循切点纪律（只在 user/assistant 边界切，不切断 tool 配对）。返回丢弃条数。
    pub fn window_drop(&self, memory: &mut Box<dyn History>) -> usize {
        let msgs = memory.view();
        let total = self.estimator.estimate_messages(msgs);
        if total <= self.soft {
            return 0;
        }

        let sys_end = msgs.iter().take_while(|m| m.role == Role::System).count();
        let Some(cut) = tail_cutpoint_after_system_with_estimator(
            msgs,
            sys_end,
            self.keep_tokens,
            &self.estimator,
        ) else {
            return 0;
        };

        let dropped = cut - sys_end;
        let mut next: Vec<Message> = msgs[..sys_end].to_vec();
        // 可回溯折叠：被丢弃整段先序列化 spill 留 artifact:// 指针；None 降级裸 notice。
        let notice = match &self.artifacts {
            Some(store) => {
                let json = serde_json::to_string(&msgs[sys_end..cut]).unwrap_or_default();
                match store.put_text(&json) {
                    Ok(id) => format!("{DROP_PREFIX} {dropped} 条较早消息 | artifact://{id}]"),
                    Err(_) => format!("{DROP_PREFIX} {dropped} 条较早消息以适配上下文窗口]"),
                }
            }
            None => format!("{DROP_PREFIX} {dropped} 条较早消息以适配上下文窗口]"),
        };
        next.push(Message::system(notice));
        next.extend(msgs[cut..].to_vec());
        memory.set(next);
        dropped
    }
}

/// Token 估算辅助组件。默认是轻量 `chars/3` 启发式；设置
/// `BOTOBOT_TOKENIZER=path/to/tokenizer.json` 后会使用 HF tokenizer 做真实计数。
#[derive(Clone)]
pub struct TokenEstimator {
    backend: TokenEstimatorBackend,
}

#[derive(Clone)]
enum TokenEstimatorBackend {
    Heuristic {
        chars_per_token: usize,
        image_tokens: usize,
        message_overhead: usize,
    },
    Tokenizer {
        tokenizer: Arc<Tokenizer>,
        image_tokens: usize,
        message_overhead: usize,
    },
}

impl std::fmt::Debug for TokenEstimator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenEstimator")
            .field("backend", &self.backend.name())
            .finish()
    }
}

impl TokenEstimatorBackend {
    fn name(&self) -> &'static str {
        match self {
            Self::Heuristic { .. } => "heuristic",
            Self::Tokenizer { .. } => "tokenizer",
        }
    }

    fn image_tokens(&self) -> usize {
        match self {
            Self::Heuristic { image_tokens, .. } | Self::Tokenizer { image_tokens, .. } => {
                *image_tokens
            }
        }
    }

    fn message_overhead(&self) -> usize {
        match self {
            Self::Heuristic {
                message_overhead, ..
            }
            | Self::Tokenizer {
                message_overhead, ..
            } => *message_overhead,
        }
    }
}

impl Default for TokenEstimator {
    fn default() -> Self {
        Self::heuristic()
    }
}

impl TokenEstimator {
    pub fn heuristic() -> Self {
        Self {
            backend: TokenEstimatorBackend::Heuristic {
                chars_per_token: 3,
                image_tokens: 800,
                message_overhead: 4,
            },
        }
    }

    pub fn from_tokenizer_file(path: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        let tokenizer = Tokenizer::from_file(path.as_ref())
            .map_err(|e| anyhow::anyhow!("failed to load tokenizer: {e}"))?;
        Ok(Self {
            backend: TokenEstimatorBackend::Tokenizer {
                tokenizer: Arc::new(tokenizer),
                image_tokens: 800,
                message_overhead: 4,
            },
        })
    }

    pub fn runtime() -> Self {
        static RUNTIME: OnceLock<TokenEstimator> = OnceLock::new();
        RUNTIME
            .get_or_init(|| match std::env::var("BOTOBOT_TOKENIZER") {
                Ok(path) if !path.trim().is_empty() => Self::from_tokenizer_file(path.trim())
                    .unwrap_or_else(|e| {
                        eprintln!(
                            "(tokenizer: failed to load {}; falling back to chars/3: {e})",
                            path.trim()
                        );
                        Self::heuristic()
                    }),
                _ => Self::heuristic(),
            })
            .clone()
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    pub fn estimate_messages(&self, msgs: &[Message]) -> usize {
        msgs.iter().map(|m| self.estimate_message(m)).sum()
    }

    pub fn estimate_message(&self, m: &Message) -> usize {
        let mut n = self.backend.message_overhead();
        for p in &m.content {
            match p {
                ContentPart::Text(t) => n += self.estimate_text(t),
                ContentPart::ImageUrl(_) => n += self.backend.image_tokens(),
            }
        }
        for tc in &m.tool_calls {
            n += self.estimate_text(&tc.function.name) + self.estimate_text(&tc.function.arguments);
        }
        n
    }

    pub fn estimate_text(&self, s: &str) -> usize {
        match &self.backend {
            TokenEstimatorBackend::Heuristic {
                chars_per_token, ..
            } => s.chars().count() / (*chars_per_token).max(1) + 1,
            TokenEstimatorBackend::Tokenizer { tokenizer, .. } => tokenizer
                .encode(s, false)
                .map(|encoding| encoding.len())
                .unwrap_or_else(|_| s.chars().count() / 3 + 1),
        }
    }
}

/// 估算整段历史的 token（兼容 wrapper）。
pub fn estimate(msgs: &[Message]) -> usize {
    TokenEstimator::runtime().estimate_messages(msgs)
}

/// 单条消息的 token 估算（兼容 wrapper）。
pub fn est_msg(m: &Message) -> usize {
    TokenEstimator::runtime().estimate_message(m)
}

/// 按切点纪律寻找 tail 起点：钉住开头 system，保留最近约 `keep_tokens`，并把切点前移到
/// user/assistant 边界，避免从 tool 消息中间切入。
pub fn tail_cutpoint(msgs: &[Message], keep_tokens: usize) -> Option<usize> {
    let sys_end = msgs.iter().take_while(|m| m.role == Role::System).count();
    tail_cutpoint_after_system(msgs, sys_end, keep_tokens)
}

fn tail_cutpoint_after_system(
    msgs: &[Message],
    sys_end: usize,
    keep_tokens: usize,
) -> Option<usize> {
    tail_cutpoint_after_system_with_estimator(
        msgs,
        sys_end,
        keep_tokens,
        &TokenEstimator::runtime(),
    )
}

fn tail_cutpoint_after_system_with_estimator(
    msgs: &[Message],
    sys_end: usize,
    keep_tokens: usize,
    estimator: &TokenEstimator,
) -> Option<usize> {
    let mut acc = 0usize;
    let mut cut = msgs.len();
    for i in (sys_end..msgs.len()).rev() {
        acc += estimator.estimate_message(&msgs[i]);
        if acc >= keep_tokens {
            cut = i;
            while cut < msgs.len() && !matches!(msgs[cut].role, Role::User | Role::Assistant) {
                cut += 1;
            }
            break;
        }
    }

    (cut > sys_end && cut < msgs.len()).then_some(cut)
}

/// 暴露给模型的控制工具：驱动器**按名拦截**，`call` 不会被真正执行
/// （真正改写 memory 在驱动器里串行做）。保留无害返回以防被误派发。
pub struct CompactTool;

#[async_trait]
impl Tool for CompactTool {
    fn name(&self) -> &str {
        "compact"
    }
    fn description(&self) -> &str {
        "Compress the conversation history when the context grows large: keeps the system \
         prompt and recent messages, drops older ones. Call this if told the context is large."
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }
    async fn call(&self, _args: Value) -> ToolResult {
        Ok(json!("compaction handled by runtime"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base_types::VecHistory;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokenizers::models::wordlevel::WordLevel;
    use tokenizers::pre_tokenizers::whitespace::Whitespace;

    fn mem(msgs: Vec<Message>) -> Box<dyn History> {
        Box::new(VecHistory::with(msgs))
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
    fn temp_tokenizer_path(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("botobot-{name}-{nanos}.json"))
    }

    fn store() -> Arc<crate::artifact::ArtifactStore> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("botobot-compact-fold-{nanos}"));
        Arc::new(crate::artifact::ArtifactStore::new(dir).unwrap())
    }

    /// 从折叠占位文本里抠出 artifact id（`artifact://aN`）。
    fn artifact_id(s: &str) -> String {
        let start = s.find("artifact://").expect("占位应含 artifact://") + "artifact://".len();
        s[start..].trim_end_matches(']').trim().to_string()
    }

    #[test]
    fn prefix_counts_only_leading_consecutive_system() {
        let c = Compactor::new(1000, None);
        let msgs = vec![
            Message::system("A".repeat(30)),
            Message::system("B".repeat(30)),
            Message::user("hi"),
            Message::system("mid-system-should-not-count"),
        ];
        let prefix = c.prefix_tokens(&msgs);
        // 只数开头两条 system（遇首条非 system 即止）。
        let head_two =
            c.estimator.estimate_message(&msgs[0]) + c.estimator.estimate_message(&msgs[1]);
        assert_eq!(prefix, head_two);
        let total = c.estimator.estimate_messages(&msgs);
        assert_eq!(c.body_after_prefix(&msgs), total - head_two);
    }

    #[test]
    fn degrade_compares_total_when_prefix_over_half_window() {
        // window=100 → hard=90，soft=75，degrade 阈值=0.5W=50。
        let c = Compactor::new(100, None);
        let msgs = vec![
            Message::system("S".repeat(300)), // ~100 tok 前缀，> 0.5W
            Message::user("tiny"),
        ];
        assert!(c.prefix_tokens(&msgs) > c.window / 2, "前缀应超过 0.5W");
        assert!(
            c.should_force(&msgs),
            "前缀吃满窗口应退化按 total 比 hard 触发"
        );
    }

    #[test]
    fn normal_uses_body_not_total() {
        let c = Compactor::new(1000, None);
        let msgs = vec![
            Message::system("S".repeat(60)), // ~20 tok，< 0.5W
            Message::user("small body"),
        ];
        assert!(!c.should_force(&msgs));
        assert!(!c.over_soft(&msgs));
    }

    #[test]
    fn derives_soft_hard_keep_from_window() {
        let c = Compactor::new(32768, None);
        assert_eq!(c.window, 32768);
        assert_eq!(c.soft, 24576); // 0.75 × 32768
        assert_eq!(c.hard, 29491); // 0.90 × 32768（向下取整）
        assert_eq!(c.keep_tokens, 13107); // 0.40 × 32768（向下取整）
        assert_eq!(c.hard_ceiling(), c.hard, "hard_ceiling 不再是 1.5×window");
    }

    #[test]
    fn pins_system_keeps_tail_drops_old() {
        let c = Compactor::new(64, None);
        let mut m = mem(vec![
            Message::system("SYS"),
            Message::user("OLD ".repeat(40)),
            Message::assistant("MID ".repeat(40)),
            Message::user("recent-question"),
        ]);
        let dropped = c.compact(&mut m);
        assert!(dropped > 0, "应发生压缩");

        let v = m.view();
        assert_eq!(v[0].role, Role::System, "system 应钉在开头");
        assert!(
            v.iter().any(|x| msg_text(x).contains("裁剪")),
            "应插入裁剪提示"
        );
        assert!(
            v.iter().any(|x| msg_text(x).contains("recent-question")),
            "最近消息应保留"
        );
        assert!(
            !v.iter().any(|x| msg_text(x).contains("OLD")),
            "较早消息应被丢弃"
        );
    }

    #[test]
    fn prune_spills_original_and_is_recoverable() {
        let st = store();
        let mut c = Compactor::new(64, Some(st.clone()));
        c.protect_tokens = 5;
        c.min_savings = 5;
        let original = "ORIG-TOOL-OUTPUT ".repeat(40);
        let mut m = mem(vec![
            Message::system("SYS"),
            Message::tool_result("c1", original.clone()),
            Message::tool_result("c2", "recent-small"),
        ]);
        let n = c.prune(&mut m);
        assert!(n >= 1);
        let placeholder = msg_text(&m.view()[1]);
        assert!(placeholder.starts_with(PRUNE_PREFIX));
        assert!(placeholder.contains("artifact://"));
        let id = artifact_id(&placeholder);
        assert_eq!(st.get_text(&id).unwrap(), original, "read 应无损取回原文");
        // 幂等：再 prune 不重复 spill / 不改写。
        assert_eq!(c.prune(&mut m), 0);
    }

    #[test]
    fn shake_spills_big_block_recoverable() {
        let st = store();
        let mut c = Compactor::new(100, Some(st.clone()));
        c.protect_tokens = 5;
        c.min_savings = 1;
        c.shake_min_tokens = 10;
        let big = "OLD ".repeat(80);
        let mut m = mem(vec![
            Message::system("SYS"),
            Message::user(big.clone()),
            Message::assistant("recent answer"),
        ]);
        assert_eq!(c.shake(&mut m), 1);
        let ph = msg_text(&m.view()[1]);
        assert!(ph.starts_with(SHAKE_PREFIX) && ph.contains("artifact://"));
        let id = artifact_id(&ph);
        assert_eq!(st.get_text(&id).unwrap(), big);
    }

    #[test]
    fn window_drop_spills_dropped_messages_recoverable() {
        let st = store();
        let c = Compactor::new(64, Some(st.clone()));
        let mut m = mem(vec![
            Message::system("SYS"),
            Message::user("OLD ".repeat(40)),
            Message::assistant("MID ".repeat(40)),
            Message::user("recent-question"),
        ]);
        let dropped = c.window_drop(&mut m);
        assert!(dropped > 0);
        let v = m.view();
        let notice = v
            .iter()
            .find(|x| msg_text(x).starts_with(DROP_PREFIX))
            .expect("应有裁剪占位");
        let ph = msg_text(notice);
        assert!(ph.contains("artifact://"));
        let id = artifact_id(&ph);
        // 取回的序列化文本应含被丢弃消息的原文片段。
        assert!(st.get_text(&id).unwrap().contains("OLD"));
    }

    #[test]
    fn fold_degrades_to_lossy_notice_when_no_store() {
        // None 时 prune 仍裸 notice（旧行为），占位不含 artifact://。
        let mut c = Compactor::new(64, None);
        c.protect_tokens = 5;
        c.min_savings = 5;
        let mut m = mem(vec![
            Message::system("SYS"),
            Message::tool_result("c1", "x".repeat(150)),
            Message::tool_result("c2", "y".repeat(150)),
            Message::tool_result("c3", "recent-small"),
        ]);
        assert!(c.prune(&mut m) >= 2);
        let ph = msg_text(&m.view()[1]);
        assert!(ph.starts_with(PRUNE_PREFIX));
        assert!(!ph.contains("artifact://"), "无 store 时降级裸 notice");
    }

    #[test]
    fn noop_when_under_budget() {
        let c = Compactor::new(10_000, None);
        let mut m = mem(vec![Message::system("s"), Message::user("hi")]);
        assert_eq!(c.compact(&mut m), 0, "未超预算不应改动");
        assert_eq!(m.view().len(), 2);
    }

    #[test]
    fn token_estimator_is_the_single_compatibility_path() {
        let estimator = TokenEstimator::default();
        let msg = Message::user("abcdef");
        assert_eq!(estimator.estimate_text("abcdef"), 3);
        assert_eq!(est_msg(&msg), estimator.estimate_message(&msg));
        assert_eq!(estimate(&[msg]), 7);
    }

    #[test]
    fn token_estimator_loads_tokenizer_json() {
        let vocab_path = temp_tokenizer_path("vocab");
        std::fs::write(&vocab_path, r#"{"[UNK]":0,"hello":1,"world":2}"#).unwrap();
        let model = WordLevel::builder()
            .files(vocab_path.to_string_lossy().into_owned())
            .unk_token("[UNK]".to_string())
            .build()
            .unwrap();
        let mut tokenizer = Tokenizer::new(model);
        tokenizer.with_pre_tokenizer(Some(Whitespace));

        let path = temp_tokenizer_path("tokenizer");
        tokenizer.save(&path, false).unwrap();

        let estimator = TokenEstimator::from_tokenizer_file(&path).unwrap();
        assert_eq!(estimator.backend_name(), "tokenizer");
        assert_eq!(estimator.estimate_text("hello world"), 2);
        assert_eq!(
            estimator.estimate_message(&Message::user("hello world")),
            6,
            "message overhead 4 + two tokenizer tokens"
        );

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(vocab_path);
    }

    #[test]
    fn tail_cutpoint_respects_tool_pair_boundaries() {
        let msgs = vec![
            Message::system("SYS"),
            Message::user("old question"),
            Message::assistant("old answer"),
            Message::tool_result("c1", "tool output ".repeat(20)),
            Message::assistant("recent answer"),
        ];

        let cut = tail_cutpoint(&msgs, 20).expect("tail should need a cut");

        assert_eq!(msgs[cut].role, Role::Assistant);
        assert_ne!(msgs[cut].role, Role::Tool, "不能从 tool 消息中间切入");
    }

    #[test]
    fn prune_truncates_old_tool_outputs_protecting_recent() {
        let mut c = Compactor::new(64, None);
        c.protect_tokens = 5; // 仅保护最近一条小输出
        c.min_savings = 5;
        let mut m = mem(vec![
            Message::system("SYS"),
            Message::tool_result("c1", "x".repeat(150)), // 最早，大
            Message::tool_result("c2", "y".repeat(150)), // 大
            Message::tool_result("c3", "recent-small"),  // 最近，受保护
        ]);
        let n = c.prune(&mut m);
        assert!(n >= 2, "应截断较早的两条工具输出");

        let v = m.view();
        let txt = |i: usize| msg_text(&v[i]);
        assert!(txt(1).starts_with("[工具输出已裁剪"), "c1 应被截断");
        assert!(txt(2).starts_with("[工具输出已裁剪"), "c2 应被截断");
        assert!(txt(3).contains("recent-small"), "最近一条工具输出应受保护");

        // 幂等：哨兵前缀使已剪的不被重复剪。
        assert_eq!(c.prune(&mut m), 0, "已剪过的不应重复剪");
    }

    #[test]
    fn shake_removes_old_large_text_blocks_but_keeps_recent() {
        let mut c = Compactor::new(100, None);
        c.protect_tokens = 5;
        c.min_savings = 1;
        c.shake_min_tokens = 10;
        let mut m = mem(vec![
            Message::system("SYS"),
            Message::user("OLD ".repeat(80)),
            Message::assistant("recent answer"),
        ]);

        let n = c.shake(&mut m);

        assert_eq!(n, 1);
        let v = m.view();
        assert!(msg_text(&v[1]).starts_with("[大块文本已移除"));
        assert!(msg_text(&v[2]).contains("recent answer"));
        assert_eq!(c.shake(&mut m), 0, "shake 应通过哨兵保持幂等");
    }

    #[test]
    fn compact_uses_shake_before_dropping_whole_messages() {
        let mut c = Compactor::new(64, None);
        c.protect_tokens = 5;
        c.min_savings = 1;
        c.shake_min_tokens = 10;
        let mut m = mem(vec![
            Message::system("SYS"),
            Message::user("HUGE ".repeat(80)),
            Message::assistant("tail"),
        ]);

        let changed = c.compact(&mut m);

        assert!(changed > 0);
        let v = m.view();
        assert!(
            v.iter().any(|x| msg_text(x).starts_with("[大块文本已移除")),
            "应先 shake 大文本"
        );
        assert!(
            v.iter().any(|x| msg_text(x).contains("tail")),
            "最近消息应保留"
        );
    }
}
