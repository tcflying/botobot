//! §1.8.8 S4 写入侧：每 turn 完成后**异步**做角色条件化抽取（实体+关系 = episode），
//! 向量化落 [`MemoryStore`]（带 provenance 指回原始 turn）。质量门：抽不到该角色关心的就不写。
//!
//! 解耦：agent-loop 经 [`EpisodicHook`] 端口在 turn 收口后 fire-and-forget 调用；实现
//! [`EpisodeWriter`] 自带**有界限流**（拿不到许可即跳过，不堆积）。

use std::sync::Arc;

use serde::Deserialize;

use base_types::{Decision, Llm, LlmEvent, LlmOpts, Message, Role};

use crate::memory::{MemoryStore, Provenance};

/// turn 收口后的钩子（非阻塞——实现自行 spawn / 限流）。
pub trait EpisodicHook: Send + Sync {
    fn on_turn_complete(&self, session_id: String, role: String, transcript: Vec<Message>);
}

const DEFAULT_BANK: &str = "default";
/// 单 turn 最多抽取入库的事实条数（防一轮灌爆）。
const MAX_FACTS_PER_TURN: usize = 8;

/// 每 turn 异步 episode 抽取器。持轻量 LLM 句柄 + 记忆库 + 限流信号量。
pub struct EpisodeWriter {
    llm: Arc<dyn Llm>,
    store: Arc<MemoryStore>,
    sem: Arc<tokio::sync::Semaphore>,
    /// §1.8.3 ③ consolidation 触发：累计 turn 数（共享，跨 on_turn_complete 递增）。
    turns: Arc<std::sync::atomic::AtomicU64>,
}

#[derive(Deserialize)]
struct Extract {
    #[serde(default)]
    entities: Vec<String>,
    #[serde(default)]
    relations: Vec<String>,
    /// §1.8.5 技能捷径：**仅当**本轮咨询了某 `skill://`/`book://` 节点并据此解决了请求时才吐。
    /// 省略（旧形态 / 普通轮次）→ 不写捷径。
    #[serde(default)]
    trail: Option<Trail>,
}

/// §1.8.5 一条捷径候选：把请求意图 `intent` 链到被咨询的资源指针 `target`。
#[derive(Deserialize)]
struct Trail {
    intent: String,
    target: String,
}

impl EpisodeWriter {
    /// `max_inflight` = 同时进行的抽取上限（背压：超过则新 turn 的抽取被跳过）。
    pub fn new(llm: Arc<dyn Llm>, store: Arc<MemoryStore>, max_inflight: usize) -> Self {
        Self {
            llm,
            store,
            sem: Arc::new(tokio::sync::Semaphore::new(max_inflight.max(1))),
            turns: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// 同步入口（测试/复用）：抽取 + 落库，返回新增条数。
    pub async fn extract_now(
        &self,
        session_id: String,
        role: String,
        transcript: Vec<Message>,
    ) -> usize {
        let convo = render_transcript(&transcript);
        if convo.trim().is_empty() {
            return 0;
        }
        let sys = format!(
            "You are the long-term-memory extraction step for an agent with this role:\n\
             ---\n{role}\n---\n\
             From the conversation turn below, extract ONLY entities and relationships that THIS \
             role would want to remember long-term (decisions, conventions, file/module facts, \
             user preferences, bug root causes). Ignore chit-chat and transient detail. \
             When a person is identified by BOTH a role/title and a name, put BOTH into entities \
             (e.g. 局长 and 李四 — list each separately) so later recall can connect them. \
             ALSO: if this turn consulted a specific skill://… or book://… node (its citation \
             appears in the transcript) AND that resolved the user's request, emit a navigation \
             shortcut \"trail\" linking the request intent to that resource pointer — so a similar \
             future request can jump straight there. Omit \"trail\" for ordinary turns that did not \
             consult such a node. \
             Reply with STRICT JSON only: \
             {{\"entities\":[\"...\"],\"relations\":[\"A -rel-> B\"],\
             \"trail\":{{\"intent\":\"<short request intent>\",\"target\":\"skill://…#section\"}}}} \
             — \"trail\" is optional; if nothing is worth remembering, reply {{\"entities\":[],\"relations\":[]}}."
        );
        let msgs = vec![Message::system(sys), Message::user(convo)];
        let Some(decision) = collect_decision(self.llm.as_ref(), &msgs).await else {
            return 0;
        };
        let Some((entities, relations, trail)) = parse_extract(&decision.text) else {
            return 0;
        };
        // 质量门：什么都没抽到（连捷径也无）→ 不写。
        if entities.is_empty() && relations.is_empty() && trail.is_none() {
            return 0;
        }
        let prov = Some(Provenance {
            session_id,
            start: 0,
            end: transcript.len(),
        });
        let ts = now_unix();
        // §1.8.5：本轮咨询了 skill://|book:// 并解决请求 → 顺手写一条捷径（导航缓存）。
        // 复用同一抽取的 entities 供余弦/扩散命中；target 必须是资源指针（否则丢弃）。
        if let Some(t) = trail.as_ref() {
            let is_pointer = t.target.starts_with("skill://") || t.target.starts_with("book://");
            if is_pointer && !t.intent.trim().is_empty() {
                let _ = self.store.append_trail(
                    DEFAULT_BANK,
                    &t.intent,
                    &t.target,
                    entities.clone(),
                    ts,
                );
            }
        }
        // 关系是核心 statement；无关系时退化为各实体单独成条。
        let statements: Vec<String> = if relations.is_empty() {
            entities.clone()
        } else {
            relations.clone()
        };
        let mut added = 0;
        for st in statements.iter().take(MAX_FACTS_PER_TURN) {
            if let Ok(true) = self.store.append_episode(
                DEFAULT_BANK,
                st,
                entities.clone(),
                relations.clone(),
                prov.clone(),
                ts,
            ) {
                added += 1;
            }
        }
        added
    }
}

impl EpisodeWriter {
    /// §1.8.3 ③ **consolidation pass**：把某 bank 中够旧（`ts < now-older_than_secs`）的 episode
    /// 经 LLM 合成一条紧凑 gist 手记，原 episode **软取代**（留盘可审计）。少于 `min_count` 条不做
    /// （不值当）。返回被巩固的条数。失败 degrade（不写、返回 0）——后台周期调用，不阻塞 turn。
    /// 注：调度（周期触发）由上层（§2.10 心跳）接线；本方法是可单测的纯巩固动作。
    pub async fn consolidate(
        &self,
        bank: &str,
        older_than_secs: u64,
        now: u64,
        min_count: usize,
    ) -> usize {
        let cutoff = now.saturating_sub(older_than_secs);
        let old = self.store.episodes_older_than(bank, cutoff);
        if old.len() < min_count.max(2) {
            return 0; // 太少不值当巩固
        }
        let joined = old.join("\n- ");
        let prompt = vec![
            Message::system(
                "Consolidate these older episodic memories into ONE compact durable note. \
                 Preserve decisions, entities, file/module facts, and conventions; drop redundancy \
                 and transient detail. Output only the consolidated note, no preamble.",
            ),
            Message::user(format!("- {joined}")),
        ];
        let Some(decision) = collect_decision(self.llm.as_ref(), &prompt).await else {
            return 0;
        };
        let gist = decision.text.trim();
        if gist.is_empty() {
            return 0;
        }
        self.store
            .consolidate_into_note(bank, &old, gist, now)
            .unwrap_or_default()
    }
}

impl EpisodicHook for EpisodeWriter {
    fn on_turn_complete(&self, session_id: String, role: String, transcript: Vec<Message>) {
        // 背压：拿不到许可（已有 max_inflight 个在跑）→ 跳过本轮抽取，不堆积。
        let Ok(permit) = self.sem.clone().try_acquire_owned() else {
            tracing::debug!(target: "botobot::episode", "extract backpressure: skip turn");
            return;
        };
        let llm = self.llm.clone();
        let store = self.store.clone();
        // §1.8.3 ③：累计 turn；env 开启且到周期 → 本轮收口顺带跑一次 consolidation（默认关）。
        let turn = self
            .turns
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let consolidate =
            consolidate_config().filter(|(every, ..)| *every > 0 && turn % *every == 0);
        tokio::spawn(async move {
            let _permit = permit; // 持到结束
            let writer = EpisodeWriter {
                llm,
                store,
                sem: Arc::new(tokio::sync::Semaphore::new(1)),
                turns: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            };
            let n = writer.extract_now(session_id, role, transcript).await;
            if n > 0 {
                tracing::debug!(target: "botobot::episode", added = n, "episode facts stored");
            }
            // 巩固在抽取之后跑（同一 spawn，串行；失败 degrade 不影响抽取结果）。
            if let (Some((_, older, min)), Some(now)) = (consolidate, now_unix()) {
                let c = writer.consolidate(DEFAULT_BANK, older, now, min).await;
                if c > 0 {
                    tracing::info!(target: "botobot::episode", consolidated = c, "memory consolidation pass");
                }
            }
        });
    }
}

/// §1.8.3 ③ consolidation 周期触发配置（默认关）。`BOTOBOT_MEMORY_CONSOLIDATE_EVERY`=N turns 启用；
/// `BOTOBOT_MEMORY_CONSOLIDATE_OLDER_SECS`（默认 7 天）/ `BOTOBOT_MEMORY_CONSOLIDATE_MIN`（默认 5）。
fn consolidate_config() -> Option<(u64, u64, usize)> {
    let every = std::env::var("BOTOBOT_MEMORY_CONSOLIDATE_EVERY")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&v| v > 0)?;
    let older = std::env::var("BOTOBOT_MEMORY_CONSOLIDATE_OLDER_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(604_800); // 7 天
    let min = std::env::var("BOTOBOT_MEMORY_CONSOLIDATE_MIN")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(5);
    Some((every, older, min))
}

async fn collect_decision(llm: &dyn Llm, msgs: &[Message]) -> Option<Decision> {
    use futures::StreamExt;
    let mut stream = llm.infer(msgs, &[], &LlmOpts::default()).await.ok()?;
    let mut last = None;
    while let Some(ev) = stream.next().await {
        if let Ok(LlmEvent::Done(d)) = ev {
            last = Some(d);
        }
    }
    last
}

/// 容错解析抽取 JSON：剥 markdown 围栏、截首尾大括号、serde 解析。
/// 返回 `(entities, relations, trail?)`——`trail` 仅在模型按契约吐出且字段完整时为 `Some`。
type Extracted = (Vec<String>, Vec<String>, Option<Trail>);
fn parse_extract(text: &str) -> Option<Extracted> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end < start {
        return None;
    }
    let e: Extract = serde_json::from_str(&text[start..=end]).ok()?;
    // 清洗：去空白条。
    let clean = |v: Vec<String>| -> Vec<String> {
        v.into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };
    // 捷径：intent/target 任一空白即丢弃（半成品指针无意义）。
    let trail = e
        .trail
        .filter(|t| !t.intent.trim().is_empty() && !t.target.trim().is_empty());
    Some((clean(e.entities), clean(e.relations), trail))
}

fn render_transcript(transcript: &[Message]) -> String {
    let mut out = String::new();
    for m in transcript {
        let who = match m.role {
            Role::System => continue, // 角色已单列，不混进抽取语料
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        let text: String = m
            .content
            .iter()
            .filter_map(|p| match p {
                base_types::ContentPart::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        if !text.trim().is_empty() {
            out.push_str(who);
            out.push_str(": ");
            out.push_str(text.trim());
            out.push('\n');
        }
    }
    out
}

fn now_unix() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use base_types::{LlmResult, ToolSpec};
    use std::path::PathBuf;

    fn temp_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("botobot-{name}-{nanos}.jsonl"))
    }

    /// 脚本化 LLM：忽略输入，回吐固定 JSON。
    struct JsonLlm(&'static str);
    #[async_trait]
    impl Llm for JsonLlm {
        async fn infer(
            &self,
            _m: &[Message],
            _t: &[ToolSpec],
            _o: &LlmOpts,
        ) -> LlmResult<base_types::LlmStream> {
            let d = Decision {
                text: self.0.to_string(),
                finish_reason: Some("stop".into()),
                ..Default::default()
            };
            let evs: Vec<LlmResult<LlmEvent>> = vec![Ok(LlmEvent::Done(d))];
            Ok(Box::pin(futures::stream::iter(evs)))
        }
    }

    fn writer(json: &'static str, path: &PathBuf) -> EpisodeWriter {
        let store = Arc::new(MemoryStore::open(path).unwrap());
        EpisodeWriter::new(Arc::new(JsonLlm(json)), store, 2)
    }

    // §1.8.3 ③ consolidation：够旧的多条 episode → LLM gist 手记，原条软取代留盘；太少不做。
    #[tokio::test]
    async fn consolidate_synthesizes_gist_and_supersedes_old_episodes() {
        let path = temp_path("ep-consolidate");
        let _ = std::fs::remove_file(&path);
        let w = writer("巩固后的 gist：登录模块改用方案B并依赖 redis", &path);
        // 三条够旧的 episode（ts=0）。
        for s in [
            "登录模块 改用 方案B",
            "登录模块 依赖 redis",
            "登录模块 加 超时重试",
        ] {
            w.store
                .append_episode(
                    DEFAULT_BANK,
                    s,
                    vec!["登录模块".into()],
                    vec![],
                    None,
                    Some(0),
                )
                .unwrap();
        }
        let now = 1_000_000u64;
        // older_than 半年、min_count 2：三条都够旧 → 巩固。
        let n = w.consolidate(DEFAULT_BANK, 100, now, 2).await;
        assert_eq!(n, 3, "应巩固 3 条 episode");

        // gist 手记进库（可召回）、原 episode 淡出召回。
        let hits = w.store.recall_in_bank(DEFAULT_BANK, "登录模块", 5);
        assert!(
            hits.iter().any(|h| h.contains("巩固后的 gist")),
            "gist 应可召回: {hits:?}"
        );
        assert!(
            !hits.iter().any(|h| h.contains("加 超时重试")),
            "原 episode 应淡出: {hits:?}"
        );
        // 原条留盘（superseded）可审计。
        let jsonl = std::fs::read_to_string(&path).unwrap();
        assert!(jsonl.contains("超时重试") && jsonl.contains("\"superseded\":true"));

        // 太少（min_count 高）→ 不做。
        let path2 = temp_path("ep-consolidate-few");
        let _ = std::fs::remove_file(&path2);
        let w2 = writer("gist", &path2);
        w2.store
            .append_episode(DEFAULT_BANK, "孤单 episode", vec![], vec![], None, Some(0))
            .unwrap();
        assert_eq!(
            w2.consolidate(DEFAULT_BANK, 100, now, 2).await,
            0,
            "少于 min_count 不巩固"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&path2);
    }

    #[tokio::test]
    async fn extracts_relations_into_episodes_with_provenance() {
        let path = temp_path("ep-extract");
        let _ = std::fs::remove_file(&path);
        let w = writer(
            r#"{"entities":["登录模块","方案B"],"relations":["登录模块 -改用-> 方案B"]}"#,
            &path,
        );
        let n = w
            .extract_now(
                "s-9".into(),
                "Coder Bot".into(),
                vec![
                    Message::user("把登录改成方案B"),
                    Message::assistant("好的，已改用方案B"),
                ],
            )
            .await;
        assert_eq!(n, 1, "应入库 1 条关系 episode");
        let hits = w
            .store
            .recall_facts_in_bank(DEFAULT_BANK, "登录用什么方案", 5);
        assert!(hits.iter().any(|h| h.content.contains("方案B")));
        assert_eq!(hits[0].provenance.as_ref().unwrap().session_id, "s-9");
        let _ = std::fs::remove_file(&path);
    }

    // §1.8.5 Phase 0：人物同时标身份与名字 → 两个实体都落库，名字行也能被身份 query 经实体扩散连上。
    #[tokio::test]
    async fn co_labels_role_and_name_as_entities() {
        let path = temp_path("ep-colabel");
        let _ = std::fs::remove_file(&path);
        let w = writer(
            r#"{"entities":["局长","李四"],"relations":["李四 -担任-> 局长"]}"#,
            &path,
        );
        let n = w
            .extract_now(
                "s-1".into(),
                "Coder Bot".into(),
                vec![Message::user("局长是李四"), Message::assistant("记下了")],
            )
            .await;
        assert_eq!(n, 1, "应入库 1 条关系 episode");
        // 共标贯穿：召回的 statement 同时携带身份与名字（后续按任一侧扩散都连得上）。
        let by_role = w.store.recall_facts_in_bank(DEFAULT_BANK, "局长是谁", 5);
        assert!(
            by_role
                .iter()
                .any(|h| h.content.contains("局长") && h.content.contains("李四")),
            "应召回含身份+名字双方的 statement: {by_role:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn empty_extraction_writes_nothing() {
        let path = temp_path("ep-empty");
        let _ = std::fs::remove_file(&path);
        let w = writer(r#"{"entities":[],"relations":[]}"#, &path);
        let n = w
            .extract_now("s".into(), "Coder Bot".into(), vec![Message::user("在吗")])
            .await;
        assert_eq!(n, 0, "无关心内容 → 不写（质量门）");
        let _ = std::fs::remove_file(&path);
    }

    // §1.8.5 Phase 1：本轮咨询了 skill:// 节点并解决请求 → 抽取顺手写一条捷径（导航缓存）。
    #[tokio::test]
    async fn extracts_trail_when_node_consulted() {
        let path = temp_path("ep-trail");
        let _ = std::fs::remove_file(&path);
        let w = writer(
            r#"{"entities":["pptx","图表"],"relations":["pptx -加-> 柱状图"],
                "trail":{"intent":"pptx 加柱状图","target":"skill://officecli-pptx#add-chart"}}"#,
            &path,
        );
        w.extract_now(
            "s-1".into(),
            "Coder Bot".into(),
            vec![
                Message::user("pptx 怎么加柱状图"),
                Message::assistant("用 officecli add-chart"),
            ],
        )
        .await;
        // 捷径以 source=trail 落库，可按近义请求召回。
        let hits = w
            .store
            .recall_facts_in_bank(DEFAULT_BANK, "给 pptx 加图表", 5);
        assert!(
            hits.iter()
                .any(|h| h.content.contains("skill://officecli-pptx#add-chart")),
            "应写入并召回捷径指针: {hits:?}"
        );
    }

    // §1.8.5 Phase 1：target 不是资源指针（普通文字）→ 丢弃，不写捷径。
    #[tokio::test]
    async fn rejects_trail_with_non_pointer_target() {
        let path = temp_path("ep-trail-bad");
        let _ = std::fs::remove_file(&path);
        let w = writer(
            r#"{"entities":["X"],"relations":[],"trail":{"intent":"做某事","target":"随便聊聊"}}"#,
            &path,
        );
        w.extract_now(
            "s".into(),
            "r".into(),
            vec![Message::user("hi"), Message::assistant("ok")],
        )
        .await;
        let jsonl = std::fs::read_to_string(&path).unwrap();
        assert!(
            !jsonl.contains("\"source\":\"trail\""),
            "非指针 target 不应写捷径: {jsonl}"
        );
    }

    #[tokio::test]
    async fn tolerates_markdown_fenced_json() {
        let path = temp_path("ep-fence");
        let _ = std::fs::remove_file(&path);
        let w = writer(
            "好的，抽取结果：\n```json\n{\"entities\":[\"X\"],\"relations\":[\"X -is-> Y\"]}\n```",
            &path,
        );
        let n = w
            .extract_now(
                "s".into(),
                "r".into(),
                vec![Message::user("hi"), Message::assistant("ok")],
            )
            .await;
        assert_eq!(n, 1, "应能从围栏包裹里解析出 JSON");
        let _ = std::fs::remove_file(&path);
    }
}
