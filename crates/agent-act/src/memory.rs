//! 记忆后端（P-5/§8，借鉴 oh-my-pi `mnemopi` 的门面形状）。
//!
//! **跨对话**知识库（区别于 `base_types::History` 那条会话历史底座）。agent 驱动：
//! - **recall** = `read("memory://<query>")`（走统一资源路由 P-2，不另开 recall 工具）；
//! - **retain** = [`RetainTool`]（写，可选 bank）；
//! - **forget_memory** = [`ForgetMemoryTool`]（删，可选 bank）。
//!
//! v1：落盘 JSONL + 朴素字符重叠打分。旧版纯文本行会自动归入 default bank。
//! 语义向量（内嵌小模型）/ 巩固(sleep) 延后。
//! 记忆是 agent 主动调用，不做自动预取（防上下文膨胀）。

use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::resource::{Resource, ResourceDoc};
use base_types::{Tool, ToolResult, ToolTier};

/// 至少命中 query 三分之一的字才算相关（朴素阈值）。
const MIN_SCORE: f32 = 0.34;
/// 语义召回的余弦下限（低于此视为不相关）。
const SEM_FLOOR: f32 = 0.3;
const DEFAULT_BANK: &str = "default";

/// §1.8.8 记忆来源：`Retain`=手记（curated 高信号，显式 retain 工具）；`Episode`=每轮异步
/// 角色条件化抽取的情节事实（实体+关系，低信号、带 provenance）。旧行无此字段→默认 Retain。
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MemorySource {
    #[default]
    Retain,
    Episode,
    /// §1.8.5 **技能捷径 / Trail**：导航缓存——存的是「去哪找答案的指针」（`skill://X#B` /
    /// `book://…`），**不是答案本身**。低可信：跟它 `read`，对不上就回退正常渐进披露，爆炸半径极小。
    Trail,
}

impl MemorySource {
    fn is_retain(&self) -> bool {
        matches!(self, MemorySource::Retain)
    }
    fn is_trail(&self) -> bool {
        matches!(self, MemorySource::Trail)
    }
}

/// §1.8.8 provenance 指针：一条 episode 事实**来自哪轮原始对话**（指回 SessionStore
/// `messages.jsonl`），供 webui 点击回溯。`[start, end)` 为该 turn 的 message 行区间。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Provenance {
    pub session_id: String,
    pub start: usize,
    pub end: usize,
}

/// §1.8.8 召回命中：暴露 source/provenance（手记 vs 情节、可回溯原文）。
#[derive(Clone, Debug)]
pub struct FactHit {
    pub score: f32,
    pub content: String,
    pub source: MemorySource,
    pub provenance: Option<Provenance>,
}

/// §1.8.3b 召回**图**（取代原扁平「列表+扩散段」）：命中事实 + 涉及的实体(节点) + 关系(边)。
/// 把「是否扩大检索」的判断交给 LLM——它看到节点/连接后可自行 `read(memory://<节点>)` 深挖那一片，
/// 而非由 harness 预取全图（防上下文膨胀）。节点/边来自命中条目（含 1-hop 扩散条）的 entities/relations。
#[derive(Clone, Debug, Default)]
pub struct RecallGraph {
    /// 主命中事实（带分数/来源/回溯指针）。
    pub facts: Vec<FactHit>,
    /// 去重后的实体节点（命中条 entities + 边的端点）。
    pub nodes: Vec<String>,
    /// 关系边 `(from, rel, to)`，解析自条目 relations 的 `"A -rel-> B"`。
    pub edges: Vec<(String, String, String)>,
}

/// §1.8.6 B 单条记忆内容上限（字符）——结构化编撰契约的「summary≤N」落到扁平 store 上的等价护栏：
/// 防 LLM 把整段原文当事实灌进来。episode 本是短陈述句，2000 字符已很宽松。
pub const MEMORY_CONTENT_MAX_CHARS: usize = 2000;

/// §1.8.6 B **结构化记忆编撰契约**：让 LLM（或调用方）产出**结构化操作**而非自由文本抽取，经
/// [`MemoryOp::validate`] 把关后由 [`MemoryStore::apply_ops`] **事务式**落盘（任一非法则整批拒绝，
/// 不留半套）。本期落 Create/Link/Supersede 三种；update-by-id 需稳定 id，待结构化页面 schema（§1.8.6 C）
/// 后补。与现「自由抽 entities/relations」并存——本契约是更系统、可校验的上层入口。
#[derive(Clone, Debug, PartialEq)]
pub enum MemoryOp {
    /// 新建一条事实（episode），带实体/关系标签（供余弦/扩散召回）。
    Create {
        content: String,
        entities: Vec<String>,
        relations: Vec<String>,
    },
    /// 加一条关系边：落成一条 relation-only episode（复用现有 `parse_relation` 出边解析）。
    Link { from: String, rel: String, to: String },
    /// 原地修订：按 id 软取代旧条 + 新建一条新内容（entries 不可变，update=取代+新增的原子封装）。
    /// id 来自 `memory_list`；旧条找不到也照样写新条（幂等友好）。
    Update {
        id: String,
        content: String,
        entities: Vec<String>,
        relations: Vec<String>,
    },
    /// 软取代同内容的旧事实（标 `superseded`，淡出召回，留磁盘审计）。
    Supersede { content: String },
    /// 按稳定派生 id 软取代——比按内容更精确（指名某条，避免同内容歧义）。id 来自 `memory_list`。
    SupersedeById { id: String },
}

/// §1.8.6 B 校验失败原因（Validator 把关 LLM 产物，防脏写）。带索引由 `apply_ops` 报告哪条坏。
#[derive(Clone, Debug, PartialEq)]
pub enum MemoryOpError {
    /// content 为空（trim 后）。
    EmptyContent,
    /// 某字段为空（trim 后）：entity/relation/from/rel/to。
    EmptyField(&'static str),
    /// Link 自环（from==to），无信息且会污染图。
    SelfLink,
    /// content 超长（防整段原文灌入）。
    ContentTooLong { len: usize, max: usize },
}

impl MemoryOp {
    /// 纯校验（不触盘）：内容非空+不超长、字段非空、Link 非自环。`apply_ops` 事务门用它先全量过一遍。
    pub fn validate(&self) -> Result<(), MemoryOpError> {
        let nonempty = |s: &str| !s.trim().is_empty();
        match self {
            MemoryOp::Create {
                content,
                entities,
                relations,
            } => {
                if !nonempty(content) {
                    return Err(MemoryOpError::EmptyContent);
                }
                let len = content.chars().count();
                if len > MEMORY_CONTENT_MAX_CHARS {
                    return Err(MemoryOpError::ContentTooLong {
                        len,
                        max: MEMORY_CONTENT_MAX_CHARS,
                    });
                }
                if entities.iter().any(|x| x.trim().is_empty()) {
                    return Err(MemoryOpError::EmptyField("entity"));
                }
                if relations.iter().any(|x| x.trim().is_empty()) {
                    return Err(MemoryOpError::EmptyField("relation"));
                }
                Ok(())
            }
            MemoryOp::Link { from, rel, to } => {
                if !nonempty(from) {
                    return Err(MemoryOpError::EmptyField("from"));
                }
                if !nonempty(rel) {
                    return Err(MemoryOpError::EmptyField("rel"));
                }
                if !nonempty(to) {
                    return Err(MemoryOpError::EmptyField("to"));
                }
                if from.trim() == to.trim() {
                    return Err(MemoryOpError::SelfLink);
                }
                Ok(())
            }
            MemoryOp::Supersede { content } => {
                if !nonempty(content) {
                    return Err(MemoryOpError::EmptyContent);
                }
                Ok(())
            }
            MemoryOp::SupersedeById { id } => {
                if !nonempty(id) {
                    return Err(MemoryOpError::EmptyField("id"));
                }
                Ok(())
            }
            MemoryOp::Update {
                id,
                content,
                entities,
                relations,
            } => {
                if !nonempty(id) {
                    return Err(MemoryOpError::EmptyField("id"));
                }
                // 新内容沿用 Create 的内容/字段校验。
                MemoryOp::Create {
                    content: content.clone(),
                    entities: entities.clone(),
                    relations: relations.clone(),
                }
                .validate()
            }
        }
    }
}

/// 把一条 episode 关系串 `"A -rel-> B"` 解析成 `(from, rel, to)`。
/// 容错（§1.7 本地模型输出不稳）：宽松解析多种箭头变体。
/// `"A -rel-> B"`（提取 prompt 规定形）→ `(A, rel, B)`；`"A -> B"` / `"A --> B"`
/// （省略关系词，本地模型常见）→ `(A, "→", B)`。仅在无 `->` 箭头 / 缺左端 / 缺右端时返回 None
/// （该条只作节点、不成边）。
fn parse_relation(s: &str) -> Option<(String, String, String)> {
    let arrow = s.find("->")?; // "->" 全 ASCII，可安全按字节切
    let to = s[arrow + 2..].trim();
    if to.is_empty() {
        return None;
    }
    // 左侧形如 "A -rel"（末尾可能残留箭头/连字符）。去尾连字符后：
    let left = s[..arrow].trim_end_matches('-').trim();
    if left.is_empty() {
        return None;
    }
    // 有 " -rel" 分隔则取关系词；否则整体是 from，关系退化为默认箭头「→」。
    let (from, rel) = match left.rsplit_once(" -") {
        Some((a, r)) if !a.trim().is_empty() && !r.trim().is_empty() => (a.trim(), r.trim()),
        _ => (left, "→"),
    };
    Some((from.to_string(), rel.to_string(), to.to_string()))
}

/// §4.9 B3 / §1.8.3b 共享 **1-hop 扩散**：给定主命中内容集，返回**共享实体或关系提及种子实体**的
/// 其它 episode 条目引用（排主命中本身/软取代，按 `max_expand` 封顶）。`recall_expanded`（FactHit 增量
/// 召回）与 `graph_around`（建图）共用，避免两处复制扩散逻辑。调用方已持 entries 锁并传归一化 bank。
fn expand_episode_entries<'a>(
    entries: &'a [MemoryEntry],
    bank: &str,
    primary_contents: &HashSet<&str>,
    max_expand: usize,
) -> Vec<&'a MemoryEntry> {
    if max_expand == 0 {
        return Vec::new();
    }
    // 种子实体 = 主命中条目的 entities 并集。
    let mut seeds: HashSet<&str> = HashSet::new();
    for x in entries.iter() {
        if x.bank == bank && !x.superseded && primary_contents.contains(x.content.as_str()) {
            for ent in &x.entities {
                seeds.insert(ent.as_str());
            }
        }
    }
    if seeds.is_empty() {
        return Vec::new();
    }
    entries
        .iter()
        .filter(|x| {
            x.bank == bank
                && !x.superseded
                // §1.8.6 C：扩散候选含 episode + 带实体的 curated 手记（retain）；Trail 是指针不作事实扩散。
                && x.source != MemorySource::Trail
                && !primary_contents.contains(x.content.as_str())
                // 共享实体（entities 直接命中）**或** 关系文本提及某个种子实体（relations 参与扩散）。
                && (x.entities.iter().any(|ent| seeds.contains(ent.as_str()))
                    || x.relations.iter().any(|r| seeds.iter().any(|s| r.contains(s))))
        })
        .take(max_expand)
        .collect()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct MemoryEntry {
    bank: String,
    content: String,
    /// L2 归一化嵌入向量（语义召回用）。§1.8.3 B：**不再写进 store.jsonl**（保其可读），
    /// 改存 `vectors.f16` 二进制边车（位置对齐行序）；本字段仅内存态。
    #[serde(skip)]
    vec: Option<Vec<f32>>,
    /// §1.8.3 A：钉住的身份/偏好级事实——逐字常驻进开场白、绕过 recency 淘汰。
    /// 旧行无此字段→默认 false（向后兼容）。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pin: bool,
    /// §1.8.8 来源（Retain 时省略不写，保旧行字节不变）。
    #[serde(default, skip_serializing_if = "MemorySource::is_retain")]
    source: MemorySource,
    /// §1.8.8 episode 的 provenance 指针（手记为 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provenance: Option<Provenance>,
    /// §1.8.8 形成时间（unix 秒；手记可空）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ts: Option<u64>,
    /// §1.8.8 抽取的实体/关系标签（为未来图谱留口；本期仅随条存，不建图）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    entities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    relations: Vec<String>,
    /// §4.9 A3：产出 `vec` 的嵌入模型 id。**只与同 model 的向量做余弦**——模型升级后旧向量
    /// 不可比，`set_embedder` 检测到 model 变更即重嵌入并更新此字段。`None`=无向量/未知。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    /// §4.9 B3 step-1：**软取代**（类比人脑——旧记忆淡出而非抹除）。被同槽位钉住事实取代的
    /// 旧条目不再硬删，改置 `superseded=true`：**不参与召回/pinned/recent**，但留在磁盘可审计/回溯
    /// （未来 consolidation 可见取代史）。旧行无此字段→默认 false。
    /// 注：B3 完整的「superseded_by 指针链 + trust/confidence 衰减」属后续；本步先落软删（无需 id，取 90% 价值）。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    superseded: bool,
}

// 去重/forget 按 (bank, content) 比较——忽略 vec（含 f32 无法 Eq；同内容不同向量应视为同条）。
impl PartialEq for MemoryEntry {
    fn eq(&self, other: &Self) -> bool {
        self.bank == other.bank && self.content == other.content
    }
}

/// §1.8.6 B/D **稳定 entry id（派生，不持久）**：entries 不可变（改=新增条，不原地改），故
/// `(bank, content, ts)` 的哈希就是一个稳定引用——重启后确定性重得同值，无需加持久字段、不改 JSONL
/// 格式。供 supersede / 未来 update-by-id / 版本指针链按 id 精确指名。用 std `DefaultHasher`（无新依赖）。
/// 同 `(bank,content,ts)` → 同 id：dedup 保证活跃条唯一，`ts` 进一步区分软取代链里的同内容旧条。
fn derive_entry_id(bank: &str, content: &str, ts: Option<u64>) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bank.hash(&mut h);
    0x1f_u8.hash(&mut h); // 单元分隔，防 ("ab","c") 与 ("a","bc") 撞
    content.hash(&mut h);
    0x1f_u8.hash(&mut h);
    ts.unwrap_or(0).hash(&mut h);
    format!("m{:016x}", h.finish())
}

impl MemoryEntry {
    /// 本条的稳定派生 id（见 [`derive_entry_id`]）。
    fn id(&self) -> String {
        derive_entry_id(&self.bank, &self.content, self.ts)
    }

    fn new(bank: Option<&str>, content: &str) -> Option<Self> {
        let content = content.trim();
        if content.is_empty() {
            return None;
        }
        Some(Self {
            bank: normalize_bank(bank),
            content: content.to_string(),
            vec: None,
            pin: false,
            source: MemorySource::Retain,
            provenance: None,
            ts: None,
            entities: Vec::new(),
            relations: Vec::new(),
            model: None,
            superseded: false,
        })
    }
}

/// 落盘的跨对话记忆库：一行一条 JSON。
/// 注入 [`base_types::Embedder`] 后语义召回（余弦）；未注入则降级朴素字符重叠关键词。
pub struct MemoryStore {
    path: PathBuf,
    entries: Mutex<Vec<MemoryEntry>>,
    embedder: Mutex<Option<Arc<dyn base_types::Embedder>>>,
    /// §1.8.3④：单调代际计数——任何**改变向量集**的变更（retain/forget/append/consolidate/set_embedder）
    /// 都 +1，供 hnsw ANN 缓存判失效（用即回升只改 ts 不动向量，**不**计）。无 hnsw 时仅一个原子自增，开销可忽略。
    generation: std::sync::atomic::AtomicU64,
    /// §1.8.3④ Tier2 ANN 索引缓存（feature `hnsw`）：`(建索引时的代际, 索引)`；代际不匹配即重建。
    #[cfg(feature = "hnsw")]
    ann_cache: Mutex<Option<(u64, crate::ann::MemoryAnn)>>,
}

impl MemoryStore {
    /// 打开/创建记忆文件，载入已有条目。
    /// §1.8.3 B：内容(store.jsonl) 与向量(vectors.f16 边车) 分离——边车在则向量取自边车
    /// （计数对齐才用，防错位）；否则若 jsonl 含旧内联向量则**迁移**（写边车 + 重写可读 jsonl）。
    pub fn open(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let mut entries: Vec<MemoryEntry> = match std::fs::read_to_string(&path) {
            Ok(s) => s.lines().filter_map(parse_entry_line).collect(),
            Err(_) => Vec::new(),
        };
        let sidecar = vectors_path(&path);
        if sidecar.exists() {
            // 新格式：向量来自边车（权威）。计数对齐才覆盖，否则丢弃（防行序/边车错位，
            // 留 set_embedder 回填重写）。
            let vecs = read_vectors_f16(&sidecar);
            if vecs.len() == entries.len() {
                for (e, v) in entries.iter_mut().zip(vecs) {
                    e.vec = v;
                }
            }
        } else if entries.iter().any(|e| e.vec.is_some()) {
            // 旧格式（内联向量）一次性迁移：写边车 + 重写无向量的可读 jsonl。
            let _ = write_vectors_f16(&sidecar, &entries);
            let _ = write_entries(&path, &entries);
        }
        Ok(Self {
            path,
            entries: Mutex::new(entries),
            embedder: Mutex::new(None),
            generation: std::sync::atomic::AtomicU64::new(0),
            #[cfg(feature = "hnsw")]
            ann_cache: Mutex::new(None),
        })
    }

    /// §1.8.3④：标记向量集已变（使 ANN 缓存失效）。在所有改变向量集的变更收尾调用。
    fn bump_generation(&self) {
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// 注入/升级嵌入器（§语义召回）：装配时可先 `open` 起服，后台加载完模型再 `set_embedder` 升级。
    /// **§4.9 A3 向量空间安全**：重嵌入所有**陈旧**条目（缺向量 **或** model 与新模型不一致——
    /// 模型升级后旧向量不可比），并更新其 `model` 标签；旧字符（同模型已嵌入）不动。
    pub fn set_embedder(&self, embedder: Arc<dyn base_types::Embedder>) {
        self.bump_generation(); // 向量空间变（重嵌入/换模型）→ ANN 缓存失效
        let new_model = {
            let m = embedder.model_id();
            (!m.is_empty()).then(|| m.to_string())
        };
        // 采集陈旧条目内容（缺 vec 或 model 不匹配）。不在持锁期间 embed（candle 多条数秒，阻塞 recall）。
        let stale: Vec<String> = {
            let entries = self.entries.lock().unwrap();
            entries
                .iter()
                .filter(|e| e.vec.is_none() || e.model != new_model)
                .map(|e| e.content.clone())
                .collect()
        };
        if !stale.is_empty() {
            let texts: Vec<&str> = stale.iter().map(|s| s.as_str()).collect();
            if let Ok(vecs) = embedder.embed(&texts) {
                let mut entries = self.entries.lock().unwrap();
                for (content, v) in stale.iter().zip(vecs.into_iter()) {
                    // 按内容更新仍陈旧的条目（去重保证 content 唯一）。
                    if let Some(e) = entries.iter_mut().find(|e| {
                        &e.content == content && (e.vec.is_none() || e.model != new_model)
                    }) {
                        e.vec = Some(v);
                        e.model = new_model.clone();
                    }
                }
                // 向量 + model 标签变更 → 重写边车（model 进 jsonl，故也重写 jsonl）。
                let _ = write_vectors_f16(&vectors_path(&self.path), &entries);
                let _ = write_entries(&self.path, &entries);
            }
        }
        *self.embedder.lock().unwrap() = Some(embedder);
    }

    /// §1.5/§4 非对称检索：query 端可加指令前缀（bge 模型卡推荐 s2p 检索）。存储侧用 [`Self::embed_one`]
    /// **不加**前缀（保非对称）。前缀取自 `BOTOBOT_MEMORY_QUERY_PREFIX`，未设/空=与对称召回一致（默认关）。
    fn embed_query(&self, query: &str) -> Option<Vec<f32>> {
        match query_prefix_env() {
            Some(prefix) => self.embed_one(&format!("{prefix}{query}")),
            None => self.embed_one(query),
        }
    }

    fn embed_one(&self, text: &str) -> Option<Vec<f32>> {
        self.embedder
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|e| e.embed(&[text]).ok())
            .and_then(|mut v| v.drain(..).next())
    }

    /// §4.9 A3：当前嵌入器的模型 id（无 / 空则 None）。
    fn current_model_id(&self) -> Option<String> {
        self.embedder
            .lock()
            .unwrap()
            .as_ref()
            .map(|e| e.model_id().to_string())
            .filter(|s| !s.is_empty())
    }

    /// 记住一条（去重；追加落盘）。
    pub fn retain(&self, text: &str) -> std::io::Result<()> {
        self.retain_in_bank(DEFAULT_BANK, text)
    }

    /// 记住一条到指定 bank（去重；追加落盘）。
    pub fn retain_in_bank(&self, bank: &str, text: &str) -> std::io::Result<()> {
        self.retain_in_bank_pinned(bank, text, false)
    }

    /// §1.8.3 A：记住一条，`pin=true` 钉住为身份/偏好级事实（逐字常驻开场白、绕过淘汰）。
    /// 已存在同内容时：若新请求钉住而原未钉，升级为钉住（重写落盘）。
    pub fn retain_in_bank_pinned(&self, bank: &str, text: &str, pin: bool) -> std::io::Result<()> {
        self.retain_with_entities(bank, text, pin, Vec::new())
    }

    /// §1.8.6 C：带实体标签的 retain——让**curated 手记**也能参与 1-hop 实体扩散召回（此前仅自动抽取的
    /// episode 有实体）。`entities` 空=与 [`Self::retain_in_bank_pinned`] 同（向后兼容）。eval 实验 C 证
    /// 实体扩散救多事实召回，故给 retain 也开这扇门（agent 经 retain 工具可选传）。
    pub fn retain_with_entities(
        &self,
        bank: &str,
        text: &str,
        pin: bool,
        entities: Vec<String>,
    ) -> std::io::Result<()> {
        let Some(mut entry) = MemoryEntry::new(Some(bank), text) else {
            return Ok(());
        };
        self.bump_generation(); // 新增/取代条目 → ANN 缓存失效
        entry.pin = pin;
        entry.entities = entities.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        // 注入嵌入器时顺带算内容向量随条落盘（供语义召回）；失败则 vec=None 降级关键词。
        entry.vec = self.embed_one(&entry.content);
        entry.model = self.current_model_id(); // §4.9 A3 标注向量空间
        let mut e = self.entries.lock().unwrap();
        let (nb, nc) = (entry.bank.clone(), entry.content.clone());
        let changed = if pin {
            // §1.8.3 A + §4.9 B3 step-1 钉住写入：
            // ① 同槽位**近重复**的已钉事实（改名/改偏好，如「叫张三」→「叫李四」）→ **软取代**
            //    （`superseded=true`，淡出召回但留磁盘可审计）；
            // ② **完全相同**内容（升级/再说一次）→ 硬移除旧行（同内容无审计价值），随后追加新条。
            // 这样「再说一次李四」也能软取代旧「张三」，避免矛盾身份并存。
            let mut changed = false;
            for x in e.iter_mut() {
                if x.bank == nb
                    && x.pin
                    && !x.superseded
                    && x.content != nc
                    && near_duplicate(&x.content, &nc)
                {
                    x.superseded = true;
                    changed = true;
                }
            }
            let before = e.len();
            e.retain(|x| !(x.bank == nb && x.content == nc)); // 完全相同：硬刷新
            if e.len() != before {
                changed = true;
            }
            changed
        } else {
            // 非钉住：存在**未被取代**的完全相同条目则跳过（不重复存）。
            if e.iter()
                .any(|x| x.bank == nb && x.content == nc && !x.superseded)
            {
                return Ok(());
            }
            false
        };
        let superseded = changed;
        e.push(entry);
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if superseded {
            // 取代移除了旧行 → 全量重写两份（保行序对齐）。
            write_entries(&self.path, &e)?;
            return write_vectors_f16(&vectors_path(&self.path), &e);
        }
        // 纯新增：追加内容行（O(1)）+ 重写紧凑向量边车保对齐。
        let needs_newline = e.len() > 1; // push 后；之前非空则需换行
        let line = entry_to_line(e.last().unwrap())?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        if needs_newline {
            writeln!(file)?;
        }
        write!(file, "{line}")?;
        write_vectors_f16(&vectors_path(&self.path), &e)
    }

    /// 删除指定 bank 中内容完全匹配的一条或多条记忆，返回删除数量。
    pub fn forget_in_bank(&self, bank: &str, text: &str) -> std::io::Result<usize> {
        let Some(entry) = MemoryEntry::new(Some(bank), text) else {
            return Ok(0);
        };
        self.bump_generation(); // 删条目（行索引重排）→ ANN 缓存失效
        let mut e = self.entries.lock().unwrap();
        let before = e.len();
        e.retain(|x| x != &entry);
        let removed = before - e.len();
        if removed > 0 {
            write_entries(&self.path, &e)?;
            // §1.8.3 B：内容与向量边车一起重写，保持行序对齐。
            write_vectors_f16(&vectors_path(&self.path), &e)?;
        }
        Ok(removed)
    }

    /// §1.8.8 S2 召回排序：cosine 为主序，但**保底**至少 `retain_floor` 条手记(Retain)命中
    /// （若候选池里有），避免高分 episode 把 curated 高信号事实全挤掉。返回 ≤`top_k` 条。
    pub fn recall_ranked(
        &self,
        bank: &str,
        query: &str,
        top_k: usize,
        retain_floor: usize,
    ) -> Vec<FactHit> {
        self.recall_ranked_with_trail(bank, query, top_k, retain_floor, 0)
    }

    /// 同 [`Self::recall_ranked`]，但额外**保底** `trail_floor` 条捷径(Trail)命中（§1.8.5）：
    /// 与 retain 保底同机制——若 top_k 内 Trail 不足且候选池有，则踢掉最低分 episode 腾位。
    /// 两个保底都不挤占对方：腾位只踢 `Episode`（最低优先级），retain/trail 互不踢。
    pub fn recall_ranked_with_trail(
        &self,
        bank: &str,
        query: &str,
        top_k: usize,
        retain_floor: usize,
        trail_floor: usize,
    ) -> Vec<FactHit> {
        if top_k == 0 {
            return Vec::new();
        }
        // 取更大候选池（cosine 降序、已过相关性下限）。
        let pool = self.recall_facts_in_bank(bank, query, top_k.saturating_mul(3).max(top_k));
        if pool.len() <= top_k {
            return pool;
        }
        let mut result: Vec<FactHit> = pool.iter().take(top_k).cloned().collect();
        // 保底一类来源：top_k 内该来源不足 floor 时，从候选池后段补足，腾位只踢最低分 episode。
        let ensure_floor = |result: &mut Vec<FactHit>, src: MemorySource, floor: usize| {
            let have = result.iter().filter(|h| h.source == src).count();
            if have >= floor {
                return;
            }
            let need = floor - have;
            let extra: Vec<FactHit> = pool
                .iter()
                .skip(top_k)
                .filter(|h| h.source == src && !result.iter().any(|r| r.content == h.content))
                .take(need)
                .cloned()
                .collect();
            for r in extra {
                if let Some(pos) = result
                    .iter()
                    .rposition(|h| h.source == MemorySource::Episode)
                {
                    result.remove(pos);
                }
                result.push(r);
            }
        };
        ensure_floor(&mut result, MemorySource::Retain, retain_floor);
        ensure_floor(&mut result, MemorySource::Trail, trail_floor);
        result.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        result
    }

    /// §4.9 B3 / SAG 风格 **1-hop 实体扩散召回**（多跳联想的最小形态）。
    /// 先取主召回命中（`recall_facts_in_bank`），再以命中条目的 `entities` 为种子，补充**共享至少
    /// 一个实体**的其它 episode（去重、排除主命中本身/软取代条），按 `max_expand` 封顶。
    /// 返回 `(主命中, 扩散命中)`——**纯增量**，不改主排序；扩散条无相似分（`score=0.0`），仅作
    /// 「相关上下文」附在主命中之后。种子实体为空（如纯关键词手记无 entities）则扩散为空。
    pub fn recall_expanded(
        &self,
        bank: &str,
        query: &str,
        top_k: usize,
        max_expand: usize,
    ) -> (Vec<FactHit>, Vec<FactHit>) {
        let primary = self.recall_facts_in_bank(bank, query, top_k);
        if primary.is_empty() || max_expand == 0 {
            return (primary, Vec::new());
        }
        let bank = normalize_bank(Some(bank));
        let e = self.entries.lock().unwrap();
        let primary_contents: HashSet<&str> = primary.iter().map(|h| h.content.as_str()).collect();
        let expanded: Vec<FactHit> =
            expand_episode_entries(&e, &bank, &primary_contents, max_expand)
                .into_iter()
                .map(|x| FactHit {
                    score: 0.0, // 扩散条非相似命中，作「相关上下文」
                    content: x.content.clone(),
                    source: x.source,
                    provenance: x.provenance.clone(),
                })
                .collect();
        (primary, expanded)
    }

    /// §1.8.3b **召回图**（纯函数·无嵌入/无写盘）：给定**已取的主命中** `primary`，以其 entities 为种子做
    /// 1-hop 扩散，再把命中条目（含扩散条）的 `entities` 收成节点、`relations` 解析成边。`facts` = 传入的
    /// 主命中（保持与调用方展示的事实**一致**）；节点/边去重、边端点补进节点集。
    /// 拆出此原语让 `recall_block` 复用 `recall_ranked` 的命中、**不再二次召回**（省一次嵌入 + decay 写盘）。
    /// §1.8.6 C（eval 实验 C 验证：1-hop 实体扩散把多事实召回 1/3→3/3）：从**已取的主命中** `primary`
    /// 出发做 1-hop 实体扩散，返回扩散到的**相关事实**（复用 primary、不二次嵌入；score=0.0 作「相关」）。
    /// 与 `graph_around` 共用 `expand_episode_entries`，但返回事实供 recall_block 直接（有界）纳入正文。
    pub fn expand_facts(&self, bank: &str, primary: &[FactHit], max_expand: usize) -> Vec<FactHit> {
        if primary.is_empty() || max_expand == 0 {
            return Vec::new();
        }
        let bank = normalize_bank(Some(bank));
        let e = self.entries.lock().unwrap();
        let primary_contents: HashSet<&str> = primary.iter().map(|h| h.content.as_str()).collect();
        expand_episode_entries(&e, &bank, &primary_contents, max_expand)
            .into_iter()
            .map(|x| FactHit {
                score: 0.0,
                content: x.content.clone(),
                source: x.source,
                provenance: x.provenance.clone(),
            })
            .collect()
    }

    pub fn graph_around(&self, bank: &str, primary: &[FactHit], max_expand: usize) -> RecallGraph {
        if primary.is_empty() {
            return RecallGraph::default();
        }
        let bank = normalize_bank(Some(bank));
        let e = self.entries.lock().unwrap();
        let primary_contents: HashSet<&str> = primary.iter().map(|h| h.content.as_str()).collect();
        // 命中集 = 主命中 + 1-hop 扩散条（都贡献节点/边）。
        let mut hit_contents: HashSet<&str> = primary_contents.clone();
        let expanded = expand_episode_entries(&e, &bank, &primary_contents, max_expand);
        for x in &expanded {
            hit_contents.insert(x.content.as_str());
        }
        let mut nodes: Vec<String> = Vec::new();
        let mut seen_nodes: HashSet<String> = HashSet::new();
        let push_node = |n: &str, nodes: &mut Vec<String>, seen: &mut HashSet<String>| {
            let n = n.trim();
            if !n.is_empty() && seen.insert(n.to_string()) {
                nodes.push(n.to_string());
            }
        };
        let mut edges: Vec<(String, String, String)> = Vec::new();
        let mut seen_edges: HashSet<String> = HashSet::new();
        for x in e.iter() {
            if x.bank != bank || x.superseded || !hit_contents.contains(x.content.as_str()) {
                continue;
            }
            for ent in &x.entities {
                push_node(ent, &mut nodes, &mut seen_nodes);
            }
            for r in &x.relations {
                if let Some((from, rel, to)) = parse_relation(r) {
                    let key = format!("{from}\u{1}{rel}\u{1}{to}");
                    if seen_edges.insert(key) {
                        push_node(&from, &mut nodes, &mut seen_nodes);
                        push_node(&to, &mut nodes, &mut seen_nodes);
                        edges.push((from, rel, to));
                    }
                }
            }
        }
        drop(e);
        RecallGraph {
            facts: primary.to_vec(),
            nodes,
            edges,
        }
    }

    /// §1.8.3b 召回图（自带召回）：取主命中后委托 [`Self::graph_around`]。standalone/单测用；
    /// `recall_block` 走 `recall_ranked` + `graph_around` 复用命中（避免二次召回）。
    pub fn recall_graph(
        &self,
        bank: &str,
        query: &str,
        top_k: usize,
        max_expand: usize,
    ) -> RecallGraph {
        let primary = self.recall_facts_in_bank(bank, query, top_k);
        self.graph_around(bank, &primary, max_expand)
    }

    /// §1.8.3 ③ consolidation 支撑：列出某 bank 中**够旧**（`ts < older_than`）、未钉住、未取代的
    /// episode 原文，供后台合成 gist。`ts=None` 的不计（手记不参与巩固）。
    pub fn episodes_older_than(&self, bank: &str, older_than: u64) -> Vec<String> {
        let bank = normalize_bank(Some(bank));
        let e = self.entries.lock().unwrap();
        e.iter()
            .filter(|x| {
                x.bank == bank
                    && !x.pin
                    && !x.superseded
                    && x.source == MemorySource::Episode
                    && x.ts.is_some_and(|t| t < older_than)
            })
            .map(|x| x.content.clone())
            .collect()
    }

    /// §1.8.3 ③ consolidation 落地：把 `supersede` 列出的旧 episode **软取代**（留盘可审计），
    /// 并追加一条合成 gist 作 **Retain 手记**（curated，带 ts）。一次重写、行序对齐。
    /// 返回实际取代的条数。`gist` 为空则只取代不加手记。
    pub fn consolidate_into_note(
        &self,
        bank: &str,
        supersede: &[String],
        gist: &str,
        ts: u64,
    ) -> std::io::Result<usize> {
        self.bump_generation(); // 取代 + 追加 gist → ANN 缓存失效
        let bank = normalize_bank(Some(bank));
        let drop_set: HashSet<&str> = supersede.iter().map(|s| s.as_str()).collect();
        let gist = gist.trim();
        let mut e = self.entries.lock().unwrap();
        let mut n = 0usize;
        for x in e.iter_mut() {
            if x.bank == bank && !x.superseded && drop_set.contains(x.content.as_str()) {
                x.superseded = true;
                n += 1;
            }
        }
        if n == 0 && gist.is_empty() {
            return Ok(0);
        }
        if !gist.is_empty() {
            // gist 作 Retain 手记（带 ts，便于后续衰减/再巩固）；去重：已存在同内容活条则跳过。
            if !e
                .iter()
                .any(|x| x.bank == bank && x.content == gist && !x.superseded)
            {
                let vec = self.embed_one(gist);
                let model = self.current_model_id();
                e.push(MemoryEntry {
                    bank: bank.clone(),
                    content: gist.to_string(),
                    vec,
                    pin: false,
                    source: MemorySource::Retain,
                    provenance: None,
                    ts: Some(ts),
                    entities: Vec::new(),
                    relations: Vec::new(),
                    model,
                    superseded: false,
                });
            }
        }
        write_entries(&self.path, e.as_slice())?;
        write_vectors_f16(&vectors_path(&self.path), e.as_slice())?;
        Ok(n)
    }

    /// §1.8.8 写入一条 **episode 情节事实**（每轮异步角色条件化抽取的产物）。
    /// `statement`=渲染成一句话的实体/关系；`entities/relations`=标签（图谱留口）；
    /// `provenance`=指回原始 turn。去重：同 bank 完全同内容则跳过（返回 `false`）。
    pub fn append_episode(
        &self,
        bank: &str,
        statement: &str,
        entities: Vec<String>,
        relations: Vec<String>,
        provenance: Option<Provenance>,
        ts: Option<u64>,
    ) -> std::io::Result<bool> {
        let statement = statement.trim();
        if statement.is_empty() {
            return Ok(false);
        }
        let mut entry = MemoryEntry {
            bank: normalize_bank(Some(bank)),
            content: statement.to_string(),
            vec: None,
            pin: false,
            source: MemorySource::Episode,
            provenance,
            ts,
            entities,
            relations,
            model: None,
            superseded: false,
        };
        entry.vec = self.embed_one(&entry.content);
        entry.model = self.current_model_id();
        self.append_new_entry(entry)
    }

    /// §1.8.5 写入一条 **技能捷径 / Trail**（导航缓存）。`intent`=人读的请求意图，`target`=资源指针
    /// （`skill://X#B` / `book://…`）。content 渲染成 `"{intent} → {target}"`，并合成一条
    /// `"{intent} -solved-by-> {target}"` 关系（复用 `parse_relation` 出边）。`entities`=供余弦/扩散命中的标签。
    /// 走 [`Self::append_new_entry`]——同内容去重、向量边车自动对齐。返回是否真的新增。
    pub fn append_trail(
        &self,
        bank: &str,
        intent: &str,
        target: &str,
        entities: Vec<String>,
        ts: Option<u64>,
    ) -> std::io::Result<bool> {
        let intent = intent.trim();
        let target = target.trim();
        if intent.is_empty() || target.is_empty() {
            return Ok(false);
        }
        let content = format!("{intent} → {target}");
        let relations = vec![format!("{intent} -solved-by-> {target}")];
        let mut entry = MemoryEntry {
            bank: normalize_bank(Some(bank)),
            content,
            vec: None,
            pin: false,
            source: MemorySource::Trail,
            provenance: None,
            ts,
            entities,
            relations,
            model: None,
            superseded: false,
        };
        entry.vec = self.embed_one(&entry.content);
        entry.model = self.current_model_id();
        self.append_new_entry(entry)
    }

    /// §1.8.6 B **事务式应用一批结构化记忆操作**：先全量校验（任一非法→整批拒绝、不写一条），
    /// 全过再逐条落盘。返回成功应用的条数。校验失败返回 `(索引, 原因)` 列表，便于定位 LLM 产物哪条坏。
    /// 与「自由抽取」并存——这是更系统、可校验的写入上层入口（B 的 Validator + apply 半身；LLM 产 JSON
    /// 端是后续切片，需真模型）。
    pub fn apply_ops(
        &self,
        bank: &str,
        ops: &[MemoryOp],
    ) -> Result<usize, Vec<(usize, MemoryOpError)>> {
        // 阶段一：事务门——全量校验，任一非法则整批拒绝（不留半套）。
        let errs: Vec<(usize, MemoryOpError)> = ops
            .iter()
            .enumerate()
            .filter_map(|(i, op)| op.validate().err().map(|e| (i, e)))
            .collect();
        if !errs.is_empty() {
            return Err(errs);
        }
        // 阶段二：逐条应用（校验已过；剩仅 I/O/去重可能不增——降级跳过该条，不破其余）。
        let mut applied = 0usize;
        for op in ops {
            let ok = match op {
                MemoryOp::Create {
                    content,
                    entities,
                    relations,
                } => self
                    .append_episode(bank, content, entities.clone(), relations.clone(), None, None)
                    .unwrap_or(false),
                MemoryOp::Link { from, rel, to } => {
                    let rel_line = format!("{from} -{rel}-> {to}");
                    self.append_episode(
                        bank,
                        &format!("{from} {rel} {to}"),
                        vec![from.clone(), to.clone()],
                        vec![rel_line],
                        None,
                        None,
                    )
                    .unwrap_or(false)
                }
                MemoryOp::Supersede { content } => self.supersede_content(bank, content),
                MemoryOp::SupersedeById { id } => self.supersede_by_id(bank, id),
                MemoryOp::Update {
                    id,
                    content,
                    entities,
                    relations,
                } => {
                    // 原子修订：先按 id 软取代旧条（找不到也不报错），再写新内容。
                    self.supersede_by_id(bank, id);
                    self.append_episode(bank, content, entities.clone(), relations.clone(), None, None)
                        .unwrap_or(false)
                }
            };
            if ok {
                applied += 1;
            }
        }
        Ok(applied)
    }

    /// §1.8.6 B SupersedeById 落地：按**派生 id** 精确取代（标 `superseded`）。比按内容更准——
    /// 同内容歧义时指名某条。id 由 `memory_list` 给出（见 [`derive_entry_id`]）。返回是否真有取代。
    fn supersede_by_id(&self, bank: &str, id: &str) -> bool {
        let nb = normalize_bank(Some(bank));
        let mut e = self.entries.lock().unwrap();
        let mut changed = false;
        for x in e.iter_mut() {
            if x.bank == nb && !x.superseded && x.id() == id {
                x.superseded = true;
                changed = true;
            }
        }
        if changed {
            self.bump_generation();
            let _ = write_entries(&self.path, &e);
            let _ = write_vectors_f16(&vectors_path(&self.path), &e);
        }
        changed
    }

    /// §1.8.6 B Supersede 落地：把同 bank 同内容的**未取代**条目标 `superseded`（淡出召回、留磁盘）。
    /// 返回是否真有取代发生。复用 pin 写入同款软取代语义，但按精确内容匹配（结构化 op 指名取代）。
    fn supersede_content(&self, bank: &str, content: &str) -> bool {
        let nb = normalize_bank(Some(bank));
        let mut e = self.entries.lock().unwrap();
        let mut changed = false;
        for x in e.iter_mut() {
            if x.bank == nb && x.content == content && !x.superseded {
                x.superseded = true;
                changed = true;
            }
        }
        if changed {
            self.bump_generation(); // 取代改变可召回集 → ANN 缓存失效
            let _ = write_entries(&self.path, &e);
            let _ = write_vectors_f16(&vectors_path(&self.path), &e);
        }
        changed
    }

    /// 共享追加：同 bank 完全同内容则跳过；否则 push + append 内容行 + 重写向量边车保对齐。
    /// 返回是否真的新增。
    fn append_new_entry(&self, entry: MemoryEntry) -> std::io::Result<bool> {
        let mut e = self.entries.lock().unwrap();
        if e.iter()
            .any(|x| x.bank == entry.bank && x.content == entry.content && !x.superseded)
        {
            return Ok(false);
        }
        self.bump_generation(); // 追加 episode → ANN 缓存失效
        e.push(entry);
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let needs_newline = e.len() > 1;
        let line = entry_to_line(e.last().unwrap())?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        if needs_newline {
            writeln!(file)?;
        }
        write!(file, "{line}")?;
        write_vectors_f16(&vectors_path(&self.path), &e)?;
        Ok(true)
    }

    /// 召回最相关的前 `top_k` 条（朴素字符重叠打分，score≥MIN_SCORE）。
    pub fn recall(&self, query: &str, top_k: usize) -> Vec<String> {
        self.recall_in_bank(DEFAULT_BANK, query, top_k)
    }

    /// 从指定 bank 召回最相关的前 `top_k` 条。
    pub fn recall_in_bank(&self, bank: &str, query: &str, top_k: usize) -> Vec<String> {
        self.recall_in_bank_scored(bank, query, top_k)
            .into_iter()
            .map(|(_, c)| c)
            .collect()
    }

    /// 同 [`Self::recall_in_bank`]，但额外暴露召回分数（§1.5：memory 低可信，read 输出带 score）。
    pub fn recall_in_bank_scored(
        &self,
        bank: &str,
        query: &str,
        top_k: usize,
    ) -> Vec<(f32, String)> {
        self.recall_facts_in_bank(bank, query, top_k)
            .into_iter()
            .map(|h| (h.score, h.content))
            .collect()
    }

    /// §1.8.8 结构化召回：返回 [`FactHit`]（含 source/provenance），供召回块标注「手记 vs 情节」
    /// 与回溯指针。单一打分实现（其余召回 API 皆委托此处）。
    /// §4.9 B3 confidence 衰减：读 env（`BOTOBOT_MEMORY_DECAY` 开关 + `BOTOBOT_MEMORY_HALF_LIFE_SECS`）
    /// 后委托 [`Self::recall_facts_at`]——默认关时零行为变化。
    pub fn recall_facts_in_bank(&self, bank: &str, query: &str, top_k: usize) -> Vec<FactHit> {
        self.recall_facts_at(bank, query, top_k, decay_half_life_env(), now_unix_secs())
    }

    /// 同上但显式注入衰减半衰期（`None`=不衰减）与当前 unix 秒——供确定性单测。
    /// 衰减只作用于**带 ts 且未钉住**的条目（episode；手记 ts=None / pinned 不衰减），
    /// 在阈值过滤前乘以 `0.5^(age/half_life)`：越旧分越低，足够旧即跌破下限而淡出（用即…回升属后续）。
    pub fn recall_facts_at(
        &self,
        bank: &str,
        query: &str,
        top_k: usize,
        decay_half_life: Option<u64>,
        now: u64,
    ) -> Vec<FactHit> {
        let bank = normalize_bank(Some(bank));
        let mut e = self.entries.lock().unwrap();
        // 语义召回：注入了嵌入器且 query 可嵌入 → 余弦（条目须有 vec）；否则降级字符重叠。
        let qvec = self.embed_query(query);
        // §4.9 A3：只与**同一向量空间**（同 model）的条目做余弦，防模型升级后跨空间错误比较。
        let cur_model = self.current_model_id();
        let adj = |raw: f32, x: &MemoryEntry| -> f32 {
            match (decay_half_life, x.ts) {
                (Some(hl), Some(ts)) if !x.pin && hl > 0 => {
                    raw * decay_multiplier(now.saturating_sub(ts), hl)
                }
                _ => raw,
            }
        };
        // §1.8.3④ Tier2 ANN：feature `hnsw` + 条目数超阈值时，只给 query 的最近邻候选打分（避免全扫）；
        // 否则线性全扫。**默认构建（无 hnsw）走原直接迭代，零额外开销**。
        #[cfg(feature = "hnsw")]
        let ann_rows: Option<Vec<usize>> = match (&qvec, e.len() > ann_threshold()) {
            (Some(qv), true) => Some(self.ann_candidate_rows(&e, qv, &cur_model, top_k)),
            _ => None,
        };
        let mut scored: Vec<(f32, &MemoryEntry)> = if let Some(qv) = &qvec {
            #[cfg(feature = "hnsw")]
            let iter: Box<dyn Iterator<Item = &MemoryEntry>> = match &ann_rows {
                Some(rows) => Box::new(rows.iter().filter_map(|&r| e.get(r))),
                None => Box::new(e.iter()),
            };
            #[cfg(not(feature = "hnsw"))]
            let iter = e.iter();
            iter.filter(|x| x.bank == bank && !x.superseded && x.model == cur_model)
                .filter_map(|x| x.vec.as_ref().map(|v| (adj(cosine(qv, v), x), x)))
                .filter(|(s, _)| *s >= SEM_FLOOR)
                .collect()
        } else {
            e.iter()
                .filter(|x| x.bank == bank && !x.superseded)
                .map(|x| (adj(score(query, &x.content), x), x))
                .filter(|(s, _)| *s >= MIN_SCORE)
                .collect()
        };
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let result: Vec<FactHit> = scored
            .into_iter()
            .take(top_k)
            .map(|(s, x)| FactHit {
                score: s,
                content: x.content.clone(),
                source: x.source,
                provenance: x.provenance.clone(),
            })
            .collect();
        // §4.9 B3「用即回升」：decay 开时，把被召回的 episode `ts` 刷新到 `now`——
        // 之后从**使用时刻**重新衰减（常用记忆不淡出，类比人脑）。手记(ts=None)/pinned 不动。
        // 仅 decay 开时才写盘（opt-in 承担其代价；默认关零 I/O）。
        if decay_half_life.is_some() {
            let hits: HashSet<&str> = result
                .iter()
                .filter(|h| h.source == MemorySource::Episode)
                .map(|h| h.content.as_str())
                .collect();
            let mut changed = false;
            for x in e.iter_mut() {
                if x.bank == bank
                    && !x.pin
                    && x.ts.is_some()
                    && x.ts != Some(now)
                    && hits.contains(x.content.as_str())
                {
                    x.ts = Some(now);
                    changed = true;
                }
            }
            if changed {
                // 仅 ts 变更 → 重写可读 jsonl（向量不变，边车无需重写）。
                let _ = write_entries(&self.path, e.as_slice());
            }
        }
        result
    }

    /// §1.8.3④ 取 query 的 ANN 候选**行索引**（feature `hnsw`）：按当前代际复用/重建索引
    /// （索引建在 `!superseded && model==cur_model && 有向量` 的行上，跨 bank；bank 过滤在调用方做）。
    /// 多取候选（`top_k×8`）留 bank/floor 过滤余量。空索引（无可比条目）→ 空候选。
    #[cfg(feature = "hnsw")]
    fn ann_candidate_rows(
        &self,
        entries: &[MemoryEntry],
        qv: &[f32],
        cur_model: &Option<String>,
        top_k: usize,
    ) -> Vec<usize> {
        use std::sync::atomic::Ordering;
        let cur_gen = self.generation.load(Ordering::Relaxed);
        let mut cache = self.ann_cache.lock().unwrap();
        let stale = !matches!(cache.as_ref(), Some((g, _)) if *g == cur_gen);
        if stale {
            let items: Vec<(usize, Vec<f32>)> = entries
                .iter()
                .enumerate()
                .filter(|(_, x)| !x.superseded && &x.model == cur_model && x.vec.is_some())
                .map(|(i, x)| (i, x.vec.clone().unwrap()))
                .collect();
            *cache = crate::ann::MemoryAnn::build(items).map(|a| (cur_gen, a));
        }
        match cache.as_ref() {
            Some((_, ann)) => ann
                .query(qv, top_k.saturating_mul(8).max(top_k))
                .into_iter()
                .map(|(row, _)| row)
                .collect(),
            None => Vec::new(),
        }
    }
}

/// §1.8.3④ ANN 触发阈值（feature `hnsw`）：条目数超过此值才走 ANN（小规模线性已够快）。
/// `BOTOBOT_MEMORY_ANN_THRESHOLD` 覆盖，默认 1024。
#[cfg(feature = "hnsw")]
fn ann_threshold() -> usize {
    std::env::var("BOTOBOT_MEMORY_ANN_THRESHOLD")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(1024)
}

impl MemoryStore {
    /// 指定 bank 最近 N 条**未钉住**记忆原文（append 顺序≈时间序，返回旧→新）。
    /// §1.8.3①：常驻概要的占位实现（钉住的另由 [`Self::pinned_in_bank`] 逐字常驻；
    /// 后续 ③ consolidation 把这里的「最近原文」换成 LLM 合成 gist）。
    pub fn recent_in_bank(&self, bank: &str, max_items: usize) -> Vec<String> {
        let bank = normalize_bank(Some(bank));
        let entries = self.entries.lock().unwrap();
        let mut tail: Vec<String> = entries
            .iter()
            .filter(|e| e.bank == bank && !e.pin && !e.superseded)
            .rev()
            .take(max_items)
            .map(|e| e.content.clone())
            .collect();
        tail.reverse(); // 恢复时间序（旧→新）
        tail
    }

    /// default bank 最近 N 条未钉住记忆。
    pub fn recent(&self, max_items: usize) -> Vec<String> {
        self.recent_in_bank(DEFAULT_BANK, max_items)
    }

    /// §1.8.6 B 同 [`Self::recent_in_bank`]，但每条附**稳定派生 id**（供 memory_list 展示→
    /// agent 可按 id 精确 supersede）。返回 `(id, content)`。
    pub fn recent_with_ids_in_bank(&self, bank: &str, max_items: usize) -> Vec<(String, String)> {
        let bank = normalize_bank(Some(bank));
        let entries = self.entries.lock().unwrap();
        let mut tail: Vec<(String, String)> = entries
            .iter()
            .filter(|e| e.bank == bank && !e.pin && !e.superseded)
            .rev()
            .take(max_items)
            .map(|e| (e.id(), e.content.clone()))
            .collect();
        tail.reverse();
        tail
    }

    /// §1.8.6 B 同 [`Self::pinned_in_bank`]，但每条附稳定派生 id。
    pub fn pinned_with_ids_in_bank(&self, bank: &str) -> Vec<(String, String)> {
        let bank = normalize_bank(Some(bank));
        let entries = self.entries.lock().unwrap();
        entries
            .iter()
            .filter(|e| e.bank == bank && e.pin && !e.superseded)
            .map(|e| (e.id(), e.content.clone()))
            .collect()
    }

    /// 指定 bank 的**钉住**记忆原文（§1.8.3 A：身份/偏好级，逐字常驻、不淘汰）。
    pub fn pinned_in_bank(&self, bank: &str) -> Vec<String> {
        let bank = normalize_bank(Some(bank));
        let entries = self.entries.lock().unwrap();
        entries
            .iter()
            .filter(|e| e.bank == bank && e.pin && !e.superseded)
            .map(|e| e.content.clone())
            .collect()
    }

    /// default bank 的钉住记忆。
    pub fn pinned(&self) -> Vec<String> {
        self.pinned_in_bank(DEFAULT_BANK)
    }

    /// 各 bank 的**活跃**（未取代）条目数，按 bank 名排序。供 `memory_list` 概览枚举
    /// （recall 只能按 query 检索，无法回答「我都记着哪些 bank / 总共记了多少」）。
    pub fn banks(&self) -> Vec<(String, usize)> {
        let entries = self.entries.lock().unwrap();
        let mut counts: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for e in entries.iter() {
            if !e.superseded {
                *counts.entry(e.bank.clone()).or_insert(0) += 1;
            }
        }
        counts.into_iter().collect()
    }
}

/// §4.9 B3 衰减乘子：`0.5^(age/half_life)`——经过一个半衰期降一半，单调递减、∈(0,1]。
fn decay_multiplier(age_secs: u64, half_life_secs: u64) -> f32 {
    if half_life_secs == 0 {
        return 1.0;
    }
    0.5_f32.powf(age_secs as f32 / half_life_secs as f32)
}

/// 读 env 决定是否启用衰减及半衰期（默认**关**——零行为变化）。
/// `BOTOBOT_MEMORY_DECAY`=1/true/on 开启；`BOTOBOT_MEMORY_HALF_LIFE_SECS` 覆盖半衰期（默认 30 天）。
fn decay_half_life_env() -> Option<u64> {
    let on = std::env::var("BOTOBOT_MEMORY_DECAY")
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "on" | "yes"
            )
        })
        .unwrap_or(false);
    if !on {
        return None;
    }
    let hl = std::env::var("BOTOBOT_MEMORY_HALF_LIFE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(2_592_000); // 30 天
    Some(hl)
}

/// §1.5/§4 非对称检索 query 指令前缀（默认关）。`BOTOBOT_MEMORY_QUERY_PREFIX` 设为非空即启用，
/// 典型 bge-zh 推荐值「为这个句子生成表示以用于检索相关文章：」。
fn query_prefix_env() -> Option<String> {
    std::env::var("BOTOBOT_MEMORY_QUERY_PREFIX")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 余弦相似度（输入已 L2 归一化时即点积）。
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

fn normalize_bank(bank: Option<&str>) -> String {
    let bank = bank.unwrap_or(DEFAULT_BANK).trim();
    let bank = if bank.is_empty() { DEFAULT_BANK } else { bank };
    bank.chars()
        .map(|c| {
            if c.is_control() || c == '/' || c == '\\' {
                '_'
            } else {
                c
            }
        })
        .collect()
}

fn parse_entry_line(line: &str) -> Option<MemoryEntry> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    // 用 Raw 解析（**含旧内联 `vec`**，供 §1.8.3 B 迁移读取；新格式行无 vec → None）；
    // 解析失败则把整行当 default bank 的纯文本（兼容最旧纯文本行）。
    #[derive(Deserialize)]
    struct Raw {
        #[serde(default)]
        bank: Option<String>,
        content: String,
        #[serde(default)]
        vec: Option<Vec<f32>>,
        #[serde(default)]
        pin: bool,
        #[serde(default)]
        source: MemorySource,
        #[serde(default)]
        provenance: Option<Provenance>,
        #[serde(default)]
        ts: Option<u64>,
        #[serde(default)]
        entities: Vec<String>,
        #[serde(default)]
        relations: Vec<String>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        superseded: bool,
    }
    if let Ok(r) = serde_json::from_str::<Raw>(line) {
        let content = r.content.trim();
        if content.is_empty() {
            return None;
        }
        Some(MemoryEntry {
            bank: normalize_bank(r.bank.as_deref()),
            content: content.to_string(),
            vec: r.vec,
            pin: r.pin,
            source: r.source,
            provenance: r.provenance,
            ts: r.ts,
            entities: r.entities,
            relations: r.relations,
            model: r.model,
            superseded: r.superseded,
        })
    } else {
        MemoryEntry::new(Some(DEFAULT_BANK), line)
    }
}

/// §1.8.3 B：向量边车路径（随 store 文件名派生，保证每个 store 一份独立边车）。
/// 如 `.bot/memory/store.jsonl` → `.bot/memory/store.vectors.f16`。
fn vectors_path(store: &std::path::Path) -> PathBuf {
    store.with_extension("vectors.f16")
}

/// 编码全部条目的向量为二进制（每条记录：`[len:u16 LE][len × (f16 bits:u16 LE)]`；len=0=无向量）。
/// 顺序与 store.jsonl 行序对齐。
fn encode_vectors_f16(entries: &[MemoryEntry]) -> Vec<u8> {
    let mut buf = Vec::new();
    for e in entries {
        match &e.vec {
            Some(v) => {
                buf.extend_from_slice(&(v.len() as u16).to_le_bytes());
                for &x in v {
                    buf.extend_from_slice(&half::f16::from_f32(x).to_bits().to_le_bytes());
                }
            }
            None => buf.extend_from_slice(&0u16.to_le_bytes()),
        }
    }
    buf
}

fn write_vectors_f16(path: &std::path::Path, entries: &[MemoryEntry]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, encode_vectors_f16(entries))
}

/// 读边车 → 每条 `Option<Vec<f32>>`（f16→f32 解码）。损坏/截断即停（已读到的保留）。
fn read_vectors_f16(path: &std::path::Path) -> Vec<Option<Vec<f32>>> {
    let Ok(buf) = std::fs::read(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 2 <= buf.len() {
        let len = u16::from_le_bytes([buf[i], buf[i + 1]]) as usize;
        i += 2;
        if len == 0 {
            out.push(None);
            continue;
        }
        if i + len * 2 > buf.len() {
            break; // 截断
        }
        let mut v = Vec::with_capacity(len);
        for _ in 0..len {
            let bits = u16::from_le_bytes([buf[i], buf[i + 1]]);
            i += 2;
            v.push(half::f16::from_bits(bits).to_f32());
        }
        out.push(Some(v));
    }
    out
}

fn entry_to_line(entry: &MemoryEntry) -> std::io::Result<String> {
    serde_json::to_string(entry)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn write_entries(path: &PathBuf, entries: &[MemoryEntry]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::File::create(path)?;
    for (idx, entry) in entries.iter().enumerate() {
        if idx > 0 {
            writeln!(file)?;
        }
        write!(file, "{}", entry_to_line(entry)?)?;
    }
    Ok(())
}

/// 朴素相关度：query 的不重复字符有多少比例出现在条目里（CJK + 英文都凑合）。
fn score(query: &str, entry: &str) -> f32 {
    let q: HashSet<char> = query
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    if q.is_empty() {
        return 0.0;
    }
    let e = entry.to_lowercase();
    let hit = q.iter().filter(|c| e.contains(**c)).count();
    hit as f32 / q.len() as f32
}

/// 两条记忆是否「同槽位近重复」（如改名/改偏好：「叫张三」↔「叫李四」）——双向字符重合都高。
/// 用于钉住事实的取代判定（§1.8.3 A）。窄阈值防误取代不相关事实。
fn near_duplicate(a: &str, b: &str) -> bool {
    const T: f32 = 0.8;
    score(a, b) >= T && score(b, a) >= T
}

/// `memory://<query>` 资源：召回相关记忆（read 工具据此实现"语义检索"路线）。
///
/// `memory://<query>` 走 default bank；`memory://<bank>/<query>` 走指定 bank。
pub struct MemoryResource {
    store: Arc<MemoryStore>,
}

impl MemoryResource {
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Resource for MemoryResource {
    fn scheme(&self) -> &str {
        "memory"
    }
    fn immutable(&self) -> bool {
        true
    }
    async fn resolve(&self, query: &str) -> anyhow::Result<ResourceDoc> {
        let (bank, recall_query) = parse_memory_url_path(query);
        let hits = self.store.recall_in_bank_scored(&bank, &recall_query, 5);
        // §1.8.7：可信度按匹配分给三档措辞（不再一律 low-confidence）——
        // 高=确实记得(可信)/中=记得(一般)/低=隐约(模糊,核实再用)。
        let (header, body) = if hits.is_empty() {
            (
                "[MEMORY] no clear match — recall is empty or faint; search past sessions if needed.",
                "(no relevant memories)".to_string(),
            )
        } else {
            let top = hits.iter().map(|(s, _)| *s).fold(0.0_f32, f32::max);
            let header = if top >= 0.75 {
                "[MEMORY] strong match — you clearly remember this; rely on it."
            } else if top >= 0.5 {
                "[MEMORY] partial match — you remember this; sanity-check if it matters."
            } else {
                "[MEMORY] faint match — vague recollection; verify before relying."
            };
            let body = hits
                .iter()
                .map(|(s, h)| format!("- (score={s:.2}) {h}"))
                .collect::<Vec<_>>()
                .join("\n");
            (header, body)
        };
        let content = format!("{header}\n{body}");
        let url = if bank == DEFAULT_BANK {
            format!("memory://{recall_query}")
        } else {
            format!("memory://{bank}/{recall_query}")
        };
        Ok(ResourceDoc {
            url,
            content,
            content_type: "text/markdown",
            immutable: true,
        })
    }
}

#[async_trait]
impl crate::context::ContextSource for MemoryResource {
    fn scheme(&self) -> &str {
        "memory"
    }

    fn facets(&self) -> crate::context::ContextFacets {
        use crate::context::*;
        // 记忆最低可信、随对话累积（WorldState）、语义召回、自由写回（retain/forget）。
        ContextFacets {
            trust: Trust::Memory,
            volatility: Volatility::WorldState,
            retrieval: Retrieval::Semantic,
            writeback: Writeback::Free,
        }
    }

    async fn handle(&self, budget: usize) -> Option<crate::context::Handle> {
        use crate::context::*;
        // §1.8.3 A 常驻概要 = 钉住的身份/偏好事实（逐字、不淘汰）+ 最近 N 条未钉原文（占位，
        // 后续 ③ consolidation 换成 LLM 合成 gist）。让本地模型开口前即见钉住事实,无需主动召回。
        let pinned = self.store.pinned();
        let recent = self.store.recent(8);
        if pinned.is_empty() && recent.is_empty() {
            return None;
        }
        // §1.8.7：「现在记着的」小节——钉住/最近=可信可直接用；可信度由新近度体现,不贴死标签。
        let mut body = String::from("## Holding now\n");
        if !pinned.is_empty() {
            // 钉住=用户明确要你记住的身份/偏好：直接采用，无需每次反问确认。
            body.push_str("Facts to rely on (the user told you / pinned):\n");
            for m in &pinned {
                let line: String = m.chars().take(200).collect();
                body.push_str("- ");
                body.push_str(&line);
                body.push('\n');
            }
        }
        if !recent.is_empty() {
            body.push_str("Recently noted (use if clearly relevant):\n");
            for m in &recent {
                // 单条裁短，避免长记忆把概要撑爆（全文走 read 下钻）。
                let line: String = m.chars().take(120).collect();
                body.push_str("- ");
                body.push_str(&line);
                body.push('\n');
            }
        }
        body.push_str("(Older or more memories: read(memory://<topic>).)\n");
        let digest = fit_budget(&body, budget);
        Some(Handle {
            est_tokens: est_tokens(&digest),
            digest,
            trust: Trust::Memory,
        })
    }

    async fn expand(&self, query: &str) -> anyhow::Result<ResourceDoc> {
        Resource::resolve(self, query).await
    }
}

/// §1.8.8 强制召回端口：按本轮 query 检索记忆、渲染成可**增广进 user 消息**的低可信块。
/// `force_recall` 开时由驱动器调用（agent-loop 经此 trait 解耦，不依赖具体 MemoryStore）。
#[async_trait]
pub trait QueryRecall: Send + Sync {
    async fn recall_block(&self, query: &str) -> Option<String>;
}

#[async_trait]
impl QueryRecall for MemoryResource {
    async fn recall_block(&self, query: &str) -> Option<String> {
        // §1.8.3b 召回**图**：事实（S2 排序：top5、retain 保底 2、§1.8.5 捷径保底 2）+ 节点 + 连接。
        let hits = self
            .store
            .recall_ranked_with_trail(DEFAULT_BANK, query, 5, 2, 2);
        if hits.is_empty() {
            return None;
        }
        // 节点/边来自展示命中（含 1-hop 扩散）的 entities/relations——复用 hits 不二次召回
        // （省一次嵌入 + decay 写盘；且图种子与展示事实一致）。
        let graph = self.store.graph_around(DEFAULT_BANK, &hits, 3);
        // §1.8.6 C：1-hop 实体扩散到的相关事实，有界（≤3）直接纳入正文——eval 实验 C 证其救多事实召回
        // （部署 query 1/3→3/3）。防膨胀：仅 ≤3 条、且渲染为独立「相关」小节、低可信标注。
        let related = self.store.expand_facts(DEFAULT_BANK, &hits, 3);
        Some(render_memory_graph(&hits, &graph, &related))
    }
}

/// 单条裁短到 `max` 字符（超出加省略号）。force_recall 默认开后召回块**每 turn**注入，
/// 长 retain 事实若不裁会把上下文撑爆（全文走 read 下钻）——与常驻概要的逐行裁短一致。
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…")
}

/// §1.8.3b 把召回结果渲染成「图」块：事实 + 节点 + 连接 + 深挖提示。
/// 拆出为纯函数便于单测（不碰 store）。`hits` 决定可信度档措辞与事实列表，`graph` 供节点/边。
/// 每行裁短（事实 200 / 节点·边 80）防长记忆撑爆每轮注入。
/// §1.8.5 Phase 2 非思考最低相似度闸：捷径(Trail)余弦达此值才在非思考模式标「可直接打开」直奔
/// target（省 meta→A→B 导航）；低于此只标「仅参考」、照常渐进披露。偏保守（bge 近义意图通常 ≥0.7，
/// 设 0.62 留余量，宁可多走一次正常披露也不误奔过期 target——符合「最坏回退正常披露」立论）。
/// 思考模式不看此标（并发权威重定位 + 自愈）。
pub const TRAIL_MIN_ACTIONABLE_SIM: f32 = 0.62;

fn render_memory_graph(hits: &[FactHit], graph: &RecallGraph, related: &[FactHit]) -> String {
    const FACT_MAX: usize = 200;
    const NODE_MAX: usize = 80;
    let top = hits.iter().map(|h| h.score).fold(0.0_f32, f32::max);
    // §1.8.7 可信度档措辞（按 top 分）。
    let tier = if top >= 0.75 {
        "strong — rely on it"
    } else if top >= 0.5 {
        "partial — sanity-check if it matters"
    } else {
        "faint — verify before relying"
    };
    let mut s = format!("[记忆图 / memory graph · {tier} · 低可信，需核实]\n事实:\n");
    for h in hits.iter().filter(|h| !h.source.is_trail()) {
        let src = match h.source {
            MemorySource::Retain => "note",
            MemorySource::Episode => "episode",
            MemorySource::Trail => "trail", // 已被上面 filter 排除，留全匹配
        };
        s.push_str(&format!(
            "- ({src}, score={:.2}) {}\n",
            h.score,
            truncate_chars(&h.content, FACT_MAX)
        ));
    }
    // §1.8.5 捷径段：低可信指针，跟它 read；对不上就照常 meta→A→B（爆炸半径极小）。
    // §1.8.5 Phase 2 非思考最低相似度闸：捷径只省导航不损正确——但仅在余弦够高（请求与缓存意图
    // 真近义）时才标「可直接打开」直奔 target；低于闸（近义不足）只标「仅参考」，照常 meta→A→B。
    // 偏保守（最坏回退正常披露，符合「爆炸半径极小」立论）；思考模式无视此标、并发两路权威自愈。
    let trails: Vec<&FactHit> = hits.iter().filter(|h| h.source.is_trail()).collect();
    if !trails.is_empty() {
        s.push_str(
            "捷径/shortcuts (低可信指针·非思考模式按下方相似度标办，对不上就照常 meta→A→B):\n",
        );
        for h in trails {
            let gate = if h.score >= TRAIL_MIN_ACTIONABLE_SIM {
                "可直接打开 target 省导航"
            } else {
                "相似度低·仅参考，请照常 meta→A→B"
            };
            s.push_str(&format!(
                "- (score={:.2}·{gate}) {}\n",
                h.score,
                truncate_chars(&h.content, FACT_MAX)
            ));
        }
    }
    if !graph.nodes.is_empty() {
        let nodes: Vec<String> = graph
            .nodes
            .iter()
            .map(|n| truncate_chars(n, NODE_MAX))
            .collect();
        s.push_str("节点: ");
        s.push_str(&nodes.join(" · "));
        s.push('\n');
    }
    if !graph.edges.is_empty() {
        s.push_str("连接:\n");
        for (from, rel, to) in &graph.edges {
            s.push_str(&format!(
                "- {} -{}-> {}\n",
                truncate_chars(from, NODE_MAX),
                truncate_chars(rel, NODE_MAX),
                truncate_chars(to, NODE_MAX)
            ));
        }
    }
    // §1.8.6 C：1-hop 实体扩散到的相关事实（有界≤3）——救多事实召回（eval 实验 C：部署 1/3→3/3）。
    // 低可信、作「相关上下文」，与主命中分列；防膨胀靠条数封顶（调用方传 ≤3）。
    if !related.is_empty() {
        s.push_str("相关 (同实体扩散·低可信，需核实):\n");
        for h in related {
            s.push_str(&format!("- {}\n", truncate_chars(&h.content, FACT_MAX)));
        }
    }
    // 把「是否扩大检索」交给 LLM：顺某节点深挖即可拉那一片。
    if !graph.nodes.is_empty() {
        s.push_str("(顺某节点深挖可扩大检索: read(memory://<节点>))\n");
    }
    s
}

fn parse_memory_url_path(path: &str) -> (String, String) {
    let path = path.trim_start_matches('/');
    match path.split_once('/') {
        Some((bank, query)) if !bank.trim().is_empty() && !query.trim().is_empty() => {
            (normalize_bank(Some(bank)), query.trim().to_string())
        }
        _ => (DEFAULT_BANK.to_string(), path.trim().to_string()),
    }
}

/// `retain` 工具（写）：把一条值得长期记住的事实存入记忆库。
pub struct RetainTool {
    store: Arc<MemoryStore>,
}

impl RetainTool {
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for RetainTool {
    fn name(&self) -> &str {
        "retain"
    }
    fn description(&self) -> &str {
        "Save a durable fact to long-term memory. Optionally set bank to namespace it. \
         Recall later via read with memory://<query> or memory://<bank>/<query>. \
         Use for stable user facts, decisions, preferences. \
         Set pin=true for identity/preference facts the user states about themselves \
         (e.g. their name, what to call you, a standing preference) so they stay always-visible."
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Write
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "The fact to remember." },
                "bank": { "type": "string", "description": "Optional namespace; defaults to 'default'." },
                "pin": { "type": "boolean", "description": "Pin as an identity/preference fact to keep it always visible (default false)." },
                "entities": { "type": "array", "items": { "type": "string" }, "description": "Optional key entities/topics this fact is about (e.g. names, modules, projects). Tagging them links related facts so a later query that hits one surfaces the others." }
            },
            "required": ["content"]
        })
    }
    async fn call(&self, args: Value) -> ToolResult {
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("retain: missing 'content'"))?;
        let bank = args
            .get("bank")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_BANK);
        let pin = args.get("pin").and_then(|v| v.as_bool()).unwrap_or(false);
        let entities: Vec<String> = args
            .get("entities")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();
        self.store.retain_with_entities(bank, content, pin, entities)?;
        Ok(json!(if pin {
            "remembered (pinned)"
        } else {
            "remembered"
        }))
    }
}

/// §1.8.6 B `memory_ops` 工具（写）：**结构化批量记忆编撰**——一次提交多条 create/link/supersede，
/// 经 Validator 事务式校验（任一非法整批拒绝、回报哪条坏让模型改），过关才落盘。比 `retain` 系统：
/// 适合「一段对话沉淀多条相互关联的事实 + 取代旧结论」。JSON 契约即「LLM 产结构化操作」的产出端
/// （schema 在工具边界强制、模型按错误重试）。
pub struct MemoryOpsTool {
    store: Arc<MemoryStore>,
}

impl MemoryOpsTool {
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self { store }
    }

    /// 把一个 op JSON 对象解析成 [`MemoryOp`]（按 `op` 字段分派）。结构缺失返回人读错误供模型修。
    fn parse_op(v: &Value) -> Result<MemoryOp, String> {
        let kind = v
            .get("op")
            .and_then(|x| x.as_str())
            .ok_or_else(|| "missing 'op' (create|link|supersede)".to_string())?;
        let str_field = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let arr_field = |k: &str| -> Vec<String> {
            v.get(k)
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default()
        };
        match kind {
            "create" => Ok(MemoryOp::Create {
                content: str_field("content"),
                entities: arr_field("entities"),
                relations: arr_field("relations"),
            }),
            "link" => Ok(MemoryOp::Link {
                from: str_field("from"),
                rel: str_field("rel"),
                to: str_field("to"),
            }),
            "update" => Ok(MemoryOp::Update {
                id: str_field("id"),
                content: str_field("content"),
                entities: arr_field("entities"),
                relations: arr_field("relations"),
            }),
            "supersede" => {
                // 给了 id 则按 id 精确取代（来自 memory_list）；否则按内容。
                let id = str_field("id");
                if !id.trim().is_empty() {
                    Ok(MemoryOp::SupersedeById { id })
                } else {
                    Ok(MemoryOp::Supersede {
                        content: str_field("content"),
                    })
                }
            }
            other => Err(format!("unknown op '{other}' (create|link|supersede)")),
        }
    }
}

#[async_trait]
impl Tool for MemoryOpsTool {
    fn name(&self) -> &str {
        "memory_ops"
    }
    fn description(&self) -> &str {
        "Commit a batch of structured memory edits in one transaction. Each op is one of: \
         create (a new fact with optional entities/relations), update (revise a fact by id: fades the \
         old and writes new content), link (a relation edge from→rel→to), supersede (fade an exact old \
         fact by content or id, kept on disk for audit). The whole batch is validated \
         first — if any op is invalid the entire batch is rejected and the offending indexes are \
         returned, so fix and resubmit. Prefer this over retain when sinking several related facts \
         or replacing a prior conclusion."
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Write
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "bank": { "type": "string", "description": "Optional namespace; defaults to 'default'." },
                "ops": {
                    "type": "array",
                    "description": "The structured edits to apply atomically.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "op": { "type": "string", "enum": ["create", "update", "link", "supersede"] },
                            "content": { "type": "string", "description": "For create/update: the fact text. For supersede: the exact old fact (or use 'id' instead)." },
                            "id": { "type": "string", "description": "For update: id of the fact to revise. For supersede: id of the fact to fade. Ids come from memory_list (more precise than content)." },
                            "entities": { "type": "array", "items": { "type": "string" }, "description": "For create: key topics/names this fact is about." },
                            "relations": { "type": "array", "items": { "type": "string" }, "description": "For create: relation strings like 'A -uses-> B'." },
                            "from": { "type": "string", "description": "For link: edge source." },
                            "rel": { "type": "string", "description": "For link: relation word." },
                            "to": { "type": "string", "description": "For link: edge target." }
                        },
                        "required": ["op"]
                    }
                }
            },
            "required": ["ops"]
        })
    }
    async fn call(&self, args: Value) -> ToolResult {
        let bank = args
            .get("bank")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_BANK);
        let raw = args
            .get("ops")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow::anyhow!("memory_ops: missing 'ops' array"))?;
        // 先把 JSON 解析成 MemoryOp（结构错误立即回报，不进 apply）。
        let mut ops = Vec::with_capacity(raw.len());
        for (i, v) in raw.iter().enumerate() {
            match Self::parse_op(v) {
                Ok(op) => ops.push(op),
                Err(e) => return Ok(json!({ "ok": false, "parse_error": { "index": i, "reason": e } })),
            }
        }
        match self.store.apply_ops(bank, &ops) {
            Ok(applied) => Ok(json!({ "ok": true, "applied": applied })),
            Err(errs) => {
                let invalid: Vec<Value> = errs
                    .iter()
                    .map(|(i, e)| json!({ "index": i, "reason": format!("{e:?}") }))
                    .collect();
                Ok(json!({ "ok": false, "rejected_batch": true, "invalid": invalid }))
            }
        }
    }
}

/// `forget_memory` 工具（写）：从记忆库中删除完全匹配的一条事实。
pub struct ForgetMemoryTool {
    store: Arc<MemoryStore>,
}

impl ForgetMemoryTool {
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for ForgetMemoryTool {
    fn name(&self) -> &str {
        "forget_memory"
    }
    fn description(&self) -> &str {
        "Remove an exact durable fact from long-term memory. Optionally set bank to match the \
         namespace used by retain."
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Write
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "The exact fact to forget." },
                "bank": { "type": "string", "description": "Optional namespace; defaults to 'default'." }
            },
            "required": ["content"]
        })
    }
    async fn call(&self, args: Value) -> ToolResult {
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("forget_memory: missing 'content'"))?;
        let bank = args
            .get("bank")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_BANK);
        let removed = self.store.forget_in_bank(bank, content)?;
        Ok(json!({ "forgotten": removed }))
    }
}

/// `memory_list` 工具（读）：**枚举**记忆概览——各 bank 计数 + 指定 bank 的钉住事实 + 最近 N 条。
/// 补 recall 的盲区：recall 只能按 query 检索，无法回答「我都记着哪些 bank / 我的 profile 里有什么」。
pub struct MemoryListTool {
    store: Arc<MemoryStore>,
}

impl MemoryListTool {
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for MemoryListTool {
    fn name(&self) -> &str {
        "memory_list"
    }
    fn description(&self) -> &str {
        "List/enumerate stored memory (not query-based): bank names with counts, plus the pinned \
         facts and most-recent notes for a bank. Use to answer 'what do you remember' / inspect a \
         bank; use read(memory://<query>) for relevance search."
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Read
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "bank": { "type": "string", "description": "Bank to detail (default 'default')." },
                "recent": { "type": "integer", "description": "Max recent notes to include (default 10)." }
            }
        })
    }
    async fn call(&self, args: Value) -> ToolResult {
        let bank = args
            .get("bank")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_BANK);
        let recent_n = args
            .get("recent")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).clamp(1, 50))
            .unwrap_or(10);
        let banks: Vec<Value> = self
            .store
            .banks()
            .into_iter()
            .map(|(name, count)| json!({ "bank": name, "count": count }))
            .collect();
        // §1.8.6 B 每条带稳定 id，agent 可据此 memory_ops supersede {id} 精确取代。
        let with_id = |rows: Vec<(String, String)>| -> Vec<Value> {
            rows.into_iter()
                .map(|(id, content)| json!({ "id": id, "content": content }))
                .collect()
        };
        Ok(json!({
            "banks": banks,
            "bank": bank,
            "pinned": with_id(self.store.pinned_with_ids_in_bank(bank)),
            "recent": with_id(self.store.recent_with_ids_in_bank(bank, recent_n)),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_memory_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("botobot-{name}-{nanos}.jsonl"))
    }

    /// 确定性 stub 嵌入器：把每条文本映射到一个 3 维「主题」向量（按关键词），
    /// 用于在不引 candle 的情况下验证语义召回的排序与回退逻辑。
    struct StubEmbedder;
    impl base_types::Embedder for StubEmbedder {
        fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
            Ok(texts
                .iter()
                .map(|t| {
                    let t = t.to_lowercase();
                    // 三个正交主题：cat / car / food
                    let mut v = [
                        (t.contains("cat") || t.contains("猫") || t.contains("kitten")) as i32
                            as f32,
                        (t.contains("car") || t.contains("车") || t.contains("drive")) as i32
                            as f32,
                        (t.contains("food") || t.contains("eat") || t.contains("吃")) as i32 as f32,
                    ];
                    // L2 归一化（全零则给个小 default）
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

    // §1.8.3 B：向量进 f16 边车、store.jsonl 保持可读、重开从边车恢复。
    #[test]
    fn vectors_go_to_f16_sidecar_and_jsonl_stays_readable() {
        let path = temp_memory_path("mem-sidecar");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
        let store = Arc::new(MemoryStore::open(&path).unwrap());
        store.set_embedder(Arc::new(StubEmbedder));
        store.retain("cat kitten").unwrap();

        // store.jsonl 可读：含内容、不含向量浮点数组。
        let jsonl = std::fs::read_to_string(&path).unwrap();
        assert!(jsonl.contains("cat kitten"));
        assert!(!jsonl.contains("vec"), "向量不应出现在 jsonl: {jsonl}");
        assert!(!jsonl.contains("0.577"), "向量浮点不应在 jsonl");
        // 边车文件已建。
        assert!(vectors_path(&path).exists(), "应生成 vectors.f16 边车");

        // 重开 → 向量从边车恢复（f16 近似，非 None）。
        let store2 = MemoryStore::open(&path).unwrap();
        let v = store2.entries.lock().unwrap()[0].vec.clone();
        assert!(v.is_some(), "重开应从边车恢复向量");
        assert_eq!(v.unwrap().len(), 3);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
    }

    // §1.8.3 B：旧内联向量 jsonl 一次性迁移为「可读 jsonl + f16 边车」。
    #[test]
    fn migrates_old_inline_vec_to_sidecar() {
        let path = temp_memory_path("mem-migrate");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
        std::fs::write(
            &path,
            r#"{"bank":"default","content":"老记忆","vec":[0.1,0.2,0.3]}"#,
        )
        .unwrap();

        // open 触发迁移。
        let store = MemoryStore::open(&path).unwrap();
        assert!(
            store.entries.lock().unwrap()[0].vec.is_some(),
            "迁移应保留向量"
        );
        // jsonl 重写为可读（无 vec），内容保留。
        let jsonl = std::fs::read_to_string(&path).unwrap();
        assert!(!jsonl.contains("vec"), "迁移后 jsonl 不应含 vec: {jsonl}");
        assert!(jsonl.contains("老记忆"));
        // 边车已建。
        assert!(vectors_path(&path).exists());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
    }

    #[test]
    fn semantic_recall_ranks_by_meaning_with_embedder() {
        let path = temp_memory_path("mem-sem");
        let _ = std::fs::remove_file(&path);
        let store = MemoryStore::open(&path).unwrap();
        store.set_embedder(Arc::new(StubEmbedder));
        store.retain("my kitten is sleeping").unwrap(); // cat 主题
        store.retain("I drive a fast car").unwrap(); // car 主题
        store.retain("good food to eat").unwrap(); // food 主题

        // 查询「a cat」应优先召回 kitten 那条（语义），而非字符重叠
        let hits = store.recall("a cat", 3);
        assert!(!hits.is_empty(), "应有语义召回");
        assert!(
            hits[0].contains("kitten"),
            "语义召回 top1 应是 cat 主题，实际: {hits:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn keyword_fallback_when_no_embedder() {
        let path = temp_memory_path("mem-kw");
        let _ = std::fs::remove_file(&path);
        let store = MemoryStore::open(&path).unwrap();
        // 未注入嵌入器 → 走字符重叠关键词，不 panic
        store.retain("hello world").unwrap();
        let hits = store.recall("hello", 3);
        assert!(hits.iter().any(|h| h.contains("hello")));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn set_embedder_backfills_missing_vectors() {
        let path = temp_memory_path("mem-backfill");
        let _ = std::fs::remove_file(&path);
        let store = MemoryStore::open(&path).unwrap();
        store.retain("my cat purrs").unwrap(); // 无嵌入器 → vec=None
        store.set_embedder(Arc::new(StubEmbedder)); // 回填
        // 回填后语义召回可用
        let hits = store.recall("kitten", 3);
        assert!(
            hits.iter().any(|h| h.contains("cat")),
            "回填后应能语义召回, 实际: {hits:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn memory_read_has_low_confidence_header_and_scores() {
        let path = temp_memory_path("mem-prov");
        let _ = std::fs::remove_file(&path);
        let store = Arc::new(MemoryStore::open(&path).unwrap());
        RetainTool::new(store.clone())
            .call(json!({ "content": "我叫 Zoe" }))
            .await
            .unwrap();

        // scored 召回内容应与普通召回一致。
        let plain = store.recall_in_bank(DEFAULT_BANK, "我叫什么", 5);
        let scored = store.recall_in_bank_scored(DEFAULT_BANK, "我叫什么", 5);
        let scored_contents: Vec<String> = scored.iter().map(|(_, c)| c.clone()).collect();
        assert_eq!(plain, scored_contents);

        let res = MemoryResource::new(store.clone());
        let doc = res.resolve("我叫什么").await.unwrap();
        assert!(
            doc.content.starts_with("[MEMORY"),
            "首行应为 §1.8.7 可信度档头，got: {}",
            doc.content
        );
        assert!(doc.content.contains("match"), "应含匹配档措辞");
        assert!(doc.content.contains("(score="), "命中应带分数");

        // 无命中仍有头。
        let empty = res.resolve("zzqqxx-nomatch").await.unwrap();
        assert!(empty.content.starts_with("[MEMORY"));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn retain_then_recall_finds_it() {
        let path = temp_memory_path("mem-test");
        let _ = std::fs::remove_file(&path);
        let store = Arc::new(MemoryStore::open(&path).unwrap());

        let retain = RetainTool::new(store.clone());
        retain.call(json!({ "content": "我叫 Zoe" })).await.unwrap();
        retain
            .call(json!({ "content": "今天天气晴" }))
            .await
            .unwrap();

        // 经 memory:// 资源召回。
        let res = MemoryResource::new(store.clone());
        let doc = res.resolve("我叫什么").await.unwrap();
        assert!(
            doc.content.contains("Zoe"),
            "应召回相关记忆，得到: {}",
            doc.content
        );

        // 重开（持久化）后仍在。
        let store2 = MemoryStore::open(&path).unwrap();
        assert!(!store2.recall("我叫什么", 5).is_empty(), "持久化后仍可召回");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn recent_and_context_handle_concept_block() {
        use crate::context::{ContextSource, Trust};
        let path = temp_memory_path("mem-recent");
        let _ = std::fs::remove_file(&path);
        let store = Arc::new(MemoryStore::open(&path).unwrap());
        let retain = RetainTool::new(store.clone());
        retain
            .call(json!({ "content": "用户偏好深色主题" }))
            .await
            .unwrap();
        retain
            .call(json!({ "content": "项目用 Rust edition 2024" }))
            .await
            .unwrap();

        // recent 返回时间序（旧→新）。
        let recent = store.recent(8);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0], "用户偏好深色主题");
        assert_eq!(recent[1], "项目用 Rust edition 2024");

        // ContextSource handle = 低可信概要块（含下钻引导 + 条目）。
        let res = MemoryResource::new(store.clone());
        assert_eq!(ContextSource::facets(&res).trust, Trust::Memory);
        let h = ContextSource::handle(&res, 1000)
            .await
            .expect("有记忆应有概要把手");
        assert_eq!(h.trust, Trust::Memory);
        assert!(h.digest.contains("Holding now"));
        assert!(h.digest.contains("read(memory://"));
        assert!(h.digest.contains("深色主题"));

        // 空库不常驻。
        let empty = Arc::new(MemoryStore::open(temp_memory_path("mem-empty")).unwrap());
        assert!(
            ContextSource::handle(&MemoryResource::new(empty), 1000)
                .await
                .is_none()
        );
        let _ = std::fs::remove_file(&path);
    }

    // §1.8.3 A：钉住的身份事实逐字进概要块、不被 recency 挤掉、重开仍在。
    #[tokio::test]
    async fn pinned_facts_stay_resident_and_persist() {
        use crate::context::ContextSource;
        let path = temp_memory_path("mem-pin");
        let _ = std::fs::remove_file(&path);
        let store = Arc::new(MemoryStore::open(&path).unwrap());
        let retain = RetainTool::new(store.clone());
        // 钉住身份事实。
        retain
            .call(json!({ "content": "用户叫张三", "pin": true }))
            .await
            .unwrap();
        // 再灌一堆普通记忆（足以把非钉的挤出 recent(8)）。
        for i in 0..12 {
            retain
                .call(json!({ "content": format!("普通记忆 {i}") }))
                .await
                .unwrap();
        }

        // pinned 单列、recent 不含钉住项。
        assert_eq!(store.pinned(), vec!["用户叫张三".to_string()]);
        assert!(!store.recent(8).iter().any(|m| m.contains("张三")));

        // 概要块仍含「张三」（钉住逐字），即使普通记忆很多。
        let res = MemoryResource::new(store.clone());
        let h = ContextSource::handle(&res, 1000).await.unwrap();
        assert!(h.digest.contains("Facts to rely on"));
        assert!(
            h.digest.contains("张三"),
            "钉住事实应常驻概要: {}",
            h.digest
        );

        // 重开后钉住标记仍在（持久化）。
        let store2 = Arc::new(MemoryStore::open(&path).unwrap());
        assert_eq!(store2.pinned(), vec!["用户叫张三".to_string()]);
        let _ = std::fs::remove_file(&path);
    }

    // §1.8.3 A：同槽位钉住事实「新取代旧」（改名），不同槽位各留。
    #[tokio::test]
    async fn pinned_near_duplicate_supersedes() {
        let path = temp_memory_path("mem-supersede");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
        let store = Arc::new(MemoryStore::open(&path).unwrap());
        let retain = RetainTool::new(store.clone());
        retain
            .call(json!({ "content": "用户要求叫我张三，这是我的名字/称呼。", "pin": true }))
            .await
            .unwrap();
        // 同槽位改名 → 取代张三。
        retain
            .call(json!({ "content": "用户要求叫我李四，这是我的名字/称呼。", "pin": true }))
            .await
            .unwrap();
        // 不同槽位 → 各留。
        retain
            .call(json!({ "content": "用户住在北京。", "pin": true }))
            .await
            .unwrap();

        let pinned = store.pinned();
        assert!(
            pinned.iter().any(|m| m.contains("李四")),
            "应保留李四: {pinned:?}"
        );
        assert!(
            !pinned.iter().any(|m| m.contains("张三")),
            "张三应被取代: {pinned:?}"
        );
        assert!(
            pinned.iter().any(|m| m.contains("北京")),
            "不同槽位应各留: {pinned:?}"
        );
        assert_eq!(pinned.len(), 2, "应只剩李四 + 北京: {pinned:?}");

        // 重开后取代结果持久。
        let store2 = Arc::new(MemoryStore::open(&path).unwrap());
        assert_eq!(store2.pinned().len(), 2);
        assert!(!store2.pinned().iter().any(|m| m.contains("张三")));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
    }

    // §1.8.3 A：已有矛盾钉住对（修复前遗留），「再说一次」新名即可清掉旧名。
    #[tokio::test]
    async fn restating_name_clears_preexisting_duplicate() {
        let path = temp_memory_path("mem-restate");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
        // 模拟修复前遗留：张三 + 李四 两条都钉住。
        std::fs::create_dir_all(path.parent().unwrap()).ok();
        std::fs::write(
            &path,
            "{\"bank\":\"default\",\"content\":\"用户要求叫我张三，这是我的名字/称呼。\",\"pin\":true}\n{\"bank\":\"default\",\"content\":\"用户要求叫我李四，这是我的名字/称呼。\",\"pin\":true}",
        )
        .unwrap();
        let store = Arc::new(MemoryStore::open(&path).unwrap());
        assert_eq!(store.pinned().len(), 2, "遗留双名");

        // 再说一次李四（完全相同）→ 清掉张三、只剩一条李四。
        RetainTool::new(store.clone())
            .call(json!({ "content": "用户要求叫我李四，这是我的名字/称呼。", "pin": true }))
            .await
            .unwrap();
        let pinned = store.pinned();
        assert_eq!(pinned.len(), 1, "应只剩李四: {pinned:?}");
        assert!(pinned[0].contains("李四"));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
    }

    // §4.9 B3 step-1：软取代——被改名取代的旧钉住事实淡出召回/pinned，但留磁盘可审计；重开仍淡出。
    #[tokio::test]
    async fn superseded_fact_fades_from_recall_but_persists_on_disk() {
        let path = temp_memory_path("mem-soft-supersede");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
        let store = Arc::new(MemoryStore::open(&path).unwrap());
        let retain = RetainTool::new(store.clone());
        retain
            .call(json!({ "content": "用户要求叫我张三，这是我的名字/称呼。", "pin": true }))
            .await
            .unwrap();
        // 改名 → 软取代张三。
        retain
            .call(json!({ "content": "用户要求叫我李四，这是我的名字/称呼。", "pin": true }))
            .await
            .unwrap();

        // pinned 只见李四（张三淡出）。
        let pinned = store.pinned();
        assert_eq!(pinned.len(), 1, "pinned 应只剩李四: {pinned:?}");
        assert!(pinned[0].contains("李四"));
        // 召回也不返回张三。
        let hits = store.recall_in_bank(DEFAULT_BANK, "张三", 5);
        assert!(
            !hits.iter().any(|h| h.contains("张三")),
            "软取代条不应被召回: {hits:?}"
        );

        // 但张三仍在磁盘（superseded=true），可审计/回溯。
        let jsonl = std::fs::read_to_string(&path).unwrap();
        assert!(jsonl.contains("张三"), "软取代条应留磁盘: {jsonl}");
        assert!(
            jsonl.contains("\"superseded\":true"),
            "应标 superseded: {jsonl}"
        );

        // 重开后张三仍淡出（superseded 持久）。
        let store2 = Arc::new(MemoryStore::open(&path).unwrap());
        assert_eq!(store2.pinned().len(), 1);
        assert!(
            !store2
                .recall_in_bank(DEFAULT_BANK, "张三", 5)
                .iter()
                .any(|h| h.contains("张三"))
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
    }

    // §4.9 B3 / SAG：1-hop 实体扩散——主命中外补充共享实体的关联 episode，去重、不改主排序。
    #[tokio::test]
    async fn recall_expanded_pulls_shared_entity_episodes() {
        let path = temp_memory_path("mem-expand");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
        let store = Arc::new(MemoryStore::open(&path).unwrap());
        // 三条 episode 都带实体「登录模块」；另一条无关。
        store
            .append_episode(
                DEFAULT_BANK,
                "登录模块 改用 方案B",
                vec!["登录模块".into(), "方案B".into()],
                vec![],
                None,
                None,
            )
            .unwrap();
        store
            .append_episode(
                DEFAULT_BANK,
                "登录模块 依赖 redis 会话",
                vec!["登录模块".into(), "redis".into()],
                vec![],
                None,
                None,
            )
            .unwrap();
        store
            .append_episode(
                DEFAULT_BANK,
                "前端 用 vue3",
                vec!["前端".into()],
                vec![],
                None,
                None,
            )
            .unwrap();

        // 查询命中「方案B」那条；扩散应补出共享「登录模块」的另一条，但不含无关的 vue3。
        let (primary, expanded) = store.recall_expanded(DEFAULT_BANK, "登录模块改用什么方案", 1, 3);
        assert!(!primary.is_empty(), "应有主命中");
        assert!(
            expanded.iter().any(|h| h.content.contains("redis")),
            "应扩散出共享实体的关联 episode: {:?}",
            expanded.iter().map(|h| &h.content).collect::<Vec<_>>()
        );
        // 主命中本身不重复进扩散。
        let primary_contents: Vec<&str> = primary.iter().map(|h| h.content.as_str()).collect();
        assert!(
            !expanded
                .iter()
                .any(|h| primary_contents.contains(&h.content.as_str())),
            "扩散不应含主命中"
        );
        // 无关 episode（无共享实体）不被扩散。
        assert!(
            !expanded.iter().any(|h| h.content.contains("vue3")),
            "无共享实体不应扩散"
        );

        // relations 参与扩散：一条 episode 无共享 entity，但其 relation 文本提及种子实体「登录模块」→ 也召出。
        store
            .append_episode(
                DEFAULT_BANK,
                "排查 超时 问题",
                vec!["超时".into()],
                vec!["登录模块 -出现-> 超时".into()],
                None,
                None,
            )
            .unwrap();
        let (_, exp2) = store.recall_expanded(DEFAULT_BANK, "登录模块改用什么方案", 1, 5);
        assert!(
            exp2.iter().any(|h| h.content.contains("超时")),
            "relation 文本含种子实体应参与扩散: {:?}",
            exp2.iter().map(|h| &h.content).collect::<Vec<_>>()
        );

        // 无实体种子（纯手记）→ 扩散为空。
        let path2 = temp_memory_path("mem-expand-none");
        let _ = std::fs::remove_file(&path2);
        let store2 = Arc::new(MemoryStore::open(&path2).unwrap());
        RetainTool::new(store2.clone())
            .call(json!({ "content": "纯手记无实体" }))
            .await
            .unwrap();
        let (p2, e2) = store2.recall_expanded(DEFAULT_BANK, "纯手记", 5, 3);
        assert!(!p2.is_empty());
        assert!(e2.is_empty(), "无实体种子时扩散应为空");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
        let _ = std::fs::remove_file(&path2);
    }

    // §1.5/§4 非对称检索：query 端加指令前缀（env 开），存储端不加。
    #[tokio::test]
    async fn asymmetric_query_prefix_applies_to_query_only() {
        use std::sync::Mutex as StdMutex;
        // 记录所有被 embed 的文本。
        struct RecEmbedder(Arc<StdMutex<Vec<String>>>);
        impl base_types::Embedder for RecEmbedder {
            fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
                let mut log = self.0.lock().unwrap();
                for t in texts {
                    log.push((*t).to_string());
                }
                Ok(texts.iter().map(|_| vec![1.0_f32, 0.0]).collect())
            }
            fn dim(&self) -> usize {
                2
            }
        }
        let path = temp_memory_path("mem-qprefix");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
        let log = Arc::new(StdMutex::new(Vec::new()));
        let store = Arc::new(MemoryStore::open(&path).unwrap());
        store.set_embedder(Arc::new(RecEmbedder(log.clone())));
        store.retain("内容事实A").unwrap(); // 存储端：不应带前缀

        const PFX: &str = "TESTPFX::";
        unsafe { std::env::set_var("BOTOBOT_MEMORY_QUERY_PREFIX", PFX) };
        let _ = store.recall_facts_at(DEFAULT_BANK, "查询B", 5, None, 0);
        unsafe { std::env::remove_var("BOTOBOT_MEMORY_QUERY_PREFIX") };

        let recorded = log.lock().unwrap().clone();
        // 存储内容无前缀。
        assert!(
            recorded.iter().any(|t| t == "内容事实A"),
            "存储端应原文嵌入: {recorded:?}"
        );
        assert!(
            !recorded
                .iter()
                .any(|t| t.contains(PFX) && t.contains("内容事实A")),
            "存储端不应带前缀"
        );
        // 查询带前缀。
        assert!(
            recorded.iter().any(|t| t == &format!("{PFX}查询B")),
            "查询端应带前缀: {recorded:?}"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
    }

    // §4.9 B3 confidence 衰减：纯乘子单调递减；半衰期处减半。
    #[test]
    fn decay_multiplier_halves_at_half_life() {
        assert!((decay_multiplier(0, 100) - 1.0).abs() < 1e-6);
        assert!((decay_multiplier(100, 100) - 0.5).abs() < 1e-6);
        assert!((decay_multiplier(200, 100) - 0.25).abs() < 1e-6);
        assert_eq!(decay_multiplier(999, 0), 1.0); // 半衰期 0 = 不衰减
        // 单调：越旧越小。
        assert!(decay_multiplier(50, 100) > decay_multiplier(150, 100));
    }

    // §4.9 B3「用即回升」：decay 开时召回会把 episode ts 刷新到 now，从使用时刻重新衰减——
    // 频繁使用的记忆不淡出；持久化。
    #[tokio::test]
    async fn decay_recall_refreshes_used_episode_ts() {
        let path = temp_memory_path("mem-refresh");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
        let store = Arc::new(MemoryStore::open(&path).unwrap());
        // episode ts=0。
        store
            .append_episode(
                DEFAULT_BANK,
                "登录 关键 episode",
                vec![],
                vec![],
                None,
                Some(0),
            )
            .unwrap();
        let hl = 1000u64;

        // 在 now=500 召回（age=500，0.5^0.5≈0.71，命中）→ 用即回升把 ts 刷到 500。
        let r1 = store.recall_facts_at(DEFAULT_BANK, "登录", 5, Some(hl), 500);
        assert!(
            r1.iter().any(|h| h.content.contains("关键")),
            "应召回: {r1:?}"
        );

        // ts 已刷到 500（持久化到 jsonl）。
        let jsonl = std::fs::read_to_string(&path).unwrap();
        assert!(
            jsonl.contains("\"ts\":500"),
            "用即回升应把 ts 刷到 500: {jsonl}"
        );

        // 在 now=1200 召回：若未回升 age=1200（0.5^1.2≈0.43 仍过 MIN_SCORE 0.34，边界）；
        // 回升后 age=1200-500=700（0.5^0.7≈0.62，更高）→ 稳召回。重开验证持久。
        let store2 = Arc::new(MemoryStore::open(&path).unwrap());
        let r2 = store2.recall_facts_at(DEFAULT_BANK, "登录", 5, Some(hl), 1200);
        assert!(
            r2.iter().any(|h| h.content.contains("关键")),
            "回升后应仍召回: {r2:?}"
        );
        let jsonl2 = std::fs::read_to_string(&path).unwrap();
        assert!(
            jsonl2.contains("\"ts\":1200"),
            "再次使用应再刷到 1200: {jsonl2}"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
    }

    // §4.9 B3：衰减开启时，足够旧的 episode 跌破下限淡出；手记(ts=None)不衰减；默认关时不变。
    #[tokio::test]
    async fn decay_fades_old_episodes_but_spares_notes() {
        let path = temp_memory_path("mem-decay");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
        let store = Arc::new(MemoryStore::open(&path).unwrap());
        let now = 100_000_000u64;
        // 手记（ts=None，不衰减）+ 一条很旧的 episode（ts 远早于 now）。
        RetainTool::new(store.clone())
            .call(json!({ "content": "登录 决定 用方案B 手记" }))
            .await
            .unwrap();
        store
            .append_episode(
                DEFAULT_BANK,
                "登录 旧流水 episode",
                vec![],
                vec![],
                None,
                Some(now - 10_000_000),
            )
            .unwrap();

        // 不衰减（None）：两条都召回（关键词都命中「登录」）。
        let no_decay = store.recall_facts_at(DEFAULT_BANK, "登录", 5, None, now);
        assert!(no_decay.iter().any(|h| h.content.contains("手记")));
        assert!(
            no_decay.iter().any(|h| h.content.contains("旧流水")),
            "不衰减时旧 episode 应在: {no_decay:?}"
        );

        // 衰减开（半衰期很短）：旧 episode 分数被乘到跌破 MIN_SCORE 淡出，手记不受影响仍在。
        let decayed = store.recall_facts_at(DEFAULT_BANK, "登录", 5, Some(100), now);
        assert!(
            decayed.iter().any(|h| h.content.contains("手记")),
            "手记不衰减应保留: {decayed:?}"
        );
        assert!(
            !decayed.iter().any(|h| h.content.contains("旧流水")),
            "极旧 episode 应衰减淡出: {decayed:?}"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
    }

    #[tokio::test]
    async fn banks_keep_recall_separate_and_persist() {
        let path = temp_memory_path("mem-bank-test");
        let _ = std::fs::remove_file(&path);
        let store = Arc::new(MemoryStore::open(&path).unwrap());

        let retain = RetainTool::new(store.clone());
        retain
            .call(json!({ "bank": "profile", "content": "我叫 Zoe" }))
            .await
            .unwrap();
        retain
            .call(json!({ "bank": "project", "content": "项目叫 botobot" }))
            .await
            .unwrap();

        let res = MemoryResource::new(store.clone());
        let profile = res.resolve("profile/我叫什么").await.unwrap();
        assert!(profile.content.contains("Zoe"));
        assert!(!profile.content.contains("botobot"));

        let default = res.resolve("我叫什么").await.unwrap();
        assert!(default.content.starts_with("[MEMORY"));
        assert!(default.content.contains("(no relevant memories)"));

        let store2 = MemoryStore::open(&path).unwrap();
        assert!(
            store2
                .recall_in_bank("profile", "我叫什么", 5)
                .iter()
                .any(|m| m.contains("Zoe"))
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn forget_removes_exact_memory_and_rewrites_file() {
        let path = temp_memory_path("mem-forget-test");
        let _ = std::fs::remove_file(&path);
        let store = Arc::new(MemoryStore::open(&path).unwrap());

        let retain = RetainTool::new(store.clone());
        retain
            .call(json!({ "bank": "profile", "content": "我喜欢蓝色" }))
            .await
            .unwrap();
        assert!(!store.recall_in_bank("profile", "蓝色", 5).is_empty());

        let forget = ForgetMemoryTool::new(store.clone());
        let out = forget
            .call(json!({ "bank": "profile", "content": "我喜欢蓝色" }))
            .await
            .unwrap();
        assert_eq!(out, json!({ "forgotten": 1 }));
        assert!(store.recall_in_bank("profile", "蓝色", 5).is_empty());

        let store2 = MemoryStore::open(&path).unwrap();
        assert!(store2.recall_in_bank("profile", "蓝色", 5).is_empty());
        let _ = std::fs::remove_file(&path);
    }

    // §1.8.8 S1：episode 写入带 source/provenance/标签；jsonl 仍可读（无 vec）；重开恢复；召回暴露之。
    #[tokio::test]
    async fn episode_roundtrips_with_source_and_provenance() {
        let path = temp_memory_path("mem-episode");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
        let store = Arc::new(MemoryStore::open(&path).unwrap());
        let prov = Provenance {
            session_id: "s-1".into(),
            start: 4,
            end: 6,
        };
        let added = store
            .append_episode(
                DEFAULT_BANK,
                "登录模块 改用 方案B",
                vec!["登录模块".into(), "方案B".into()],
                vec!["登录模块 -改用-> 方案B".into()],
                Some(prov.clone()),
                Some(1_700_000_000),
            )
            .unwrap();
        assert!(added);
        // 完全同内容再写 → 跳过。
        assert!(
            !store
                .append_episode(
                    DEFAULT_BANK,
                    "登录模块 改用 方案B",
                    vec![],
                    vec![],
                    None,
                    None
                )
                .unwrap()
        );

        // jsonl 可读：含 statement + source=episode + provenance，不含向量浮点。
        let jsonl = std::fs::read_to_string(&path).unwrap();
        assert!(jsonl.contains("登录模块 改用 方案B"));
        assert!(
            jsonl.contains("\"source\":\"episode\""),
            "应标 source=episode: {jsonl}"
        );
        assert!(
            jsonl.contains("\"session_id\":\"s-1\""),
            "应含 provenance: {jsonl}"
        );
        assert!(!jsonl.contains("vec"), "向量不应进 jsonl");

        // 结构化召回暴露 source/provenance。
        let hits = store.recall_facts_in_bank(DEFAULT_BANK, "登录模块改用什么", 5);
        assert!(!hits.is_empty());
        let h = &hits[0];
        assert_eq!(h.source, MemorySource::Episode);
        assert_eq!(h.provenance.as_ref().unwrap(), &prov);

        // 重开从 jsonl 恢复 source/provenance。
        let store2 = MemoryStore::open(&path).unwrap();
        let hits2 = store2.recall_facts_in_bank(DEFAULT_BANK, "登录模块", 5);
        assert_eq!(hits2[0].source, MemorySource::Episode);
        assert_eq!(hits2[0].provenance.as_ref().unwrap().session_id, "s-1");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
    }

    // §4.9 A3：模型升级 → 重嵌入所有陈旧条目 + 重标 model，避免跨向量空间错误比较。
    #[test]
    fn model_change_reembeds_and_retags() {
        struct ModelStub(&'static str);
        impl base_types::Embedder for ModelStub {
            fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
                Ok(texts.iter().map(|_| vec![1.0_f32, 0.0]).collect())
            }
            fn dim(&self) -> usize {
                2
            }
            fn model_id(&self) -> &str {
                self.0
            }
        }
        let path = temp_memory_path("mem-a3");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
        let store = MemoryStore::open(&path).unwrap();
        store.set_embedder(Arc::new(ModelStub("model-A")));
        store.retain("hello world").unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("\"model\":\"model-A\""),
            "新写条目应标当前模型"
        );
        // 升级模型 → 重嵌入 + 重标。
        store.set_embedder(Arc::new(ModelStub("model-B")));
        let jsonl = std::fs::read_to_string(&path).unwrap();
        assert!(
            jsonl.contains("\"model\":\"model-B\""),
            "升级应重标 model-B: {jsonl}"
        );
        assert!(!jsonl.contains("model-A"), "旧模型标签应被替换: {jsonl}");
        // 同空间仍可召回。
        assert!(!store.recall("hello", 5).is_empty());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
    }

    // §1.8.8 S2：召回排序——手记(Retain)保底，即使 episode 分数更高也保证 curated 命中露出。
    #[tokio::test]
    async fn recall_ranked_floors_retain_hits() {
        let path = temp_memory_path("mem-rank");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
        let store = Arc::new(MemoryStore::open(&path).unwrap());
        // 一条高信号手记 + 一堆 episode，都含「登录」（关键词召回都命中）。
        RetainTool::new(store.clone())
            .call(json!({ "content": "登录 决定：用方案B（手记）" }))
            .await
            .unwrap();
        for i in 0..6 {
            store
                .append_episode(
                    DEFAULT_BANK,
                    &format!("登录 流水 episode {i}"),
                    vec![],
                    vec![],
                    None,
                    None,
                )
                .unwrap();
        }
        // top_k=3、retain_floor=1：结果里必须有那条手记。
        let hits = store.recall_ranked(DEFAULT_BANK, "登录", 3, 1);
        assert_eq!(hits.len(), 3);
        assert!(
            hits.iter()
                .any(|h| h.source == MemorySource::Retain && h.content.contains("手记")),
            "retain 应被保底进结果: {:?}",
            hits.iter()
                .map(|h| (&h.content, h.source))
                .collect::<Vec<_>>()
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
    }

    // §1.8.5 Trail：append_trail 去重（同 intent→target 二次写跳过）+ 边车对齐 + 关系可解析成边。
    #[test]
    fn append_trail_dedups_and_forms_edge() {
        let path = temp_memory_path("mem-trail-append");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
        let store = MemoryStore::open(&path).unwrap();
        let added = store
            .append_trail(
                DEFAULT_BANK,
                "想给 pptx 加柱状图",
                "skill://officecli-pptx#add-chart",
                vec!["pptx".into(), "图表".into(), "officecli-pptx".into()],
                Some(100),
            )
            .unwrap();
        assert!(added, "首次应新增");
        // 同 intent→target 再写 → 去重跳过。
        let again = store
            .append_trail(
                DEFAULT_BANK,
                "想给 pptx 加柱状图",
                "skill://officecli-pptx#add-chart",
                vec![],
                Some(200),
            )
            .unwrap();
        assert!(!again, "同捷径二次写应去重");

        // 落盘行带 source=trail；关系可解析出一条 solved-by 边。
        let jsonl = std::fs::read_to_string(&path).unwrap();
        assert!(
            jsonl.contains("\"source\":\"trail\""),
            "应标 source=trail: {jsonl}"
        );
        let hits = store.recall_facts_in_bank(DEFAULT_BANK, "pptx 加柱状图", 5);
        assert!(
            hits.iter().any(|h| h.source == MemorySource::Trail
                && h.content.contains("skill://officecli-pptx#add-chart")),
            "应召回该捷径指针: {hits:?}"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
    }

    // §1.8.6 B Validator：各非法 op 被精确拒绝（空内容/超长/空字段/自环）。
    #[test]
    fn memory_op_validate_rejects_malformed() {
        use MemoryOp::*;
        assert_eq!(
            Create { content: "  ".into(), entities: vec![], relations: vec![] }.validate(),
            Err(MemoryOpError::EmptyContent)
        );
        let long = "x".repeat(MEMORY_CONTENT_MAX_CHARS + 1);
        assert!(matches!(
            Create { content: long, entities: vec![], relations: vec![] }.validate(),
            Err(MemoryOpError::ContentTooLong { .. })
        ));
        assert_eq!(
            Create { content: "ok".into(), entities: vec![" ".into()], relations: vec![] }.validate(),
            Err(MemoryOpError::EmptyField("entity"))
        );
        assert_eq!(
            Link { from: "A".into(), rel: "r".into(), to: "A".into() }.validate(),
            Err(MemoryOpError::SelfLink)
        );
        assert_eq!(
            Link { from: "".into(), rel: "r".into(), to: "B".into() }.validate(),
            Err(MemoryOpError::EmptyField("from"))
        );
        // 合法 op 通过。
        assert!(Create { content: "登录用 JWT".into(), entities: vec!["登录".into()], relations: vec![] }.validate().is_ok());
        assert!(Link { from: "A".into(), rel: "用".into(), to: "B".into() }.validate().is_ok());
        assert!(Supersede { content: "旧事实".into() }.validate().is_ok());
    }

    // §1.8.6 B apply_ops 事务门：批中任一非法则整批拒绝、一条不写。
    #[test]
    fn apply_ops_is_transactional_all_or_nothing() {
        let path = temp_memory_path("mem-ops-tx");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
        let store = MemoryStore::open(&path).unwrap();
        // 第 2 条非法（自环）→ 整批拒绝。
        let ops = vec![
            MemoryOp::Create { content: "好事实".into(), entities: vec![], relations: vec![] },
            MemoryOp::Link { from: "A".into(), rel: "r".into(), to: "A".into() },
        ];
        let err = store.apply_ops(DEFAULT_BANK, &ops).unwrap_err();
        assert_eq!(err.len(), 1);
        assert_eq!(err[0].0, 1, "应报告索引 1 那条坏");
        assert_eq!(err[0].1, MemoryOpError::SelfLink);
        // 一条都不该落盘（事务性）。
        let hits = store.recall_facts_in_bank(DEFAULT_BANK, "好事实", 5);
        assert!(
            !hits.iter().any(|h| h.content == "好事实"),
            "整批拒绝后不应写入任何一条: {hits:?}"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
    }

    // §1.8.6 B apply_ops 落地：全合法批 → create/link 入库可召回、supersede 软取代旧事实淡出。
    #[test]
    fn apply_ops_creates_links_and_supersedes() {
        let path = temp_memory_path("mem-ops-apply");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
        let store = MemoryStore::open(&path).unwrap();
        // 先有一条旧事实，待会儿被 supersede。
        store.append_episode(DEFAULT_BANK, "旧方案用 MD5", vec!["哈希".into()], vec![], None, None).unwrap();
        let ops = vec![
            MemoryOp::Create { content: "新方案用 SHA256".into(), entities: vec!["哈希".into()], relations: vec![] },
            MemoryOp::Link { from: "SHA256".into(), rel: "属于".into(), to: "哈希算法".into() },
            MemoryOp::Supersede { content: "旧方案用 MD5".into() },
        ];
        let applied = store.apply_ops(DEFAULT_BANK, &ops).unwrap();
        assert_eq!(applied, 3, "三条全应用");
        // create 可召回。
        let hits = store.recall_facts_in_bank(DEFAULT_BANK, "SHA256 哈希", 8);
        assert!(hits.iter().any(|h| h.content.contains("SHA256")), "新事实应可召回: {hits:?}");
        // supersede 的旧事实淡出召回（标 superseded）。
        assert!(
            !hits.iter().any(|h| h.content == "旧方案用 MD5"),
            "被取代的旧事实应淡出: {hits:?}"
        );
        // 但 supersede 行仍在磁盘（审计留痕）。
        let jsonl = std::fs::read_to_string(&path).unwrap();
        assert!(jsonl.contains("旧方案用 MD5"), "取代行应留磁盘审计: {jsonl}");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
    }

    // §1.8.6 B memory_ops 工具：JSON 批解析→事务校验→落盘；坏批整体拒绝并回报索引。
    #[tokio::test]
    async fn memory_ops_tool_applies_and_rejects() {
        let path = temp_memory_path("mem-ops-tool");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
        let store = Arc::new(MemoryStore::open(&path).unwrap());
        let tool = MemoryOpsTool::new(store.clone());
        // 合法批：create + link。
        let ok = tool
            .call(json!({
                "ops": [
                    { "op": "create", "content": "用 tokio 跑异步", "entities": ["tokio", "异步"] },
                    { "op": "link", "from": "tokio", "rel": "属于", "to": "运行时" }
                ]
            }))
            .await
            .unwrap();
        assert_eq!(ok["ok"], json!(true), "合法批应成功: {ok}");
        assert_eq!(ok["applied"], json!(2), "应应用 2 条: {ok}");
        assert!(
            store
                .recall_facts_in_bank(DEFAULT_BANK, "tokio 异步", 5)
                .iter()
                .any(|h| h.content.contains("tokio")),
            "create 的事实应可召回"
        );
        // 未知 op → parse_error（含索引）。
        let bad = tool
            .call(json!({ "ops": [ { "op": "frobnicate" } ] }))
            .await
            .unwrap();
        assert_eq!(bad["ok"], json!(false));
        assert_eq!(bad["parse_error"]["index"], json!(0), "应报告坏 op 索引: {bad}");
        // 校验失败（自环 link）→ 整批拒绝，invalid 带索引。
        let rej = tool
            .call(json!({
                "ops": [
                    { "op": "create", "content": "好的" },
                    { "op": "link", "from": "X", "rel": "r", "to": "X" }
                ]
            }))
            .await
            .unwrap();
        assert_eq!(rej["ok"], json!(false));
        assert_eq!(rej["rejected_batch"], json!(true));
        assert_eq!(rej["invalid"][0]["index"], json!(1), "应指出第 1 条非法: {rej}");
        // 整批拒绝 → 那条「好的」不应落盘。
        assert!(
            !store
                .recall_facts_in_bank(DEFAULT_BANK, "好的", 5)
                .iter()
                .any(|h| h.content == "好的"),
            "整批拒绝后 create 也不应写入"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
    }

    // §1.8.6 B 稳定派生 id：确定性（同 bank+content+ts→同 id）+ ts 区分 + 跨重开稳定。
    #[test]
    fn derive_entry_id_is_deterministic_and_stable() {
        let a = derive_entry_id("default", "用 tokio", Some(100));
        let b = derive_entry_id("default", "用 tokio", Some(100));
        assert_eq!(a, b, "同输入应同 id");
        assert_ne!(a, derive_entry_id("default", "用 tokio", Some(101)), "ts 不同 id 应不同");
        assert_ne!(a, derive_entry_id("other", "用 tokio", Some(100)), "bank 不同 id 应不同");
        assert!(a.starts_with('m'), "id 形如 m<hex>: {a}");

        // 跨重开：同一条 episode 的 id 在 reopen 后不变（派生于稳定字段）。
        let path = temp_memory_path("mem-id-stable");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
        {
            let store = MemoryStore::open(&path).unwrap();
            store.append_episode(DEFAULT_BANK, "稳定事实", vec![], vec![], None, Some(7)).unwrap();
        }
        let id_now = derive_entry_id("default", "稳定事实", Some(7));
        {
            let store = MemoryStore::open(&path).unwrap();
            let rows = store.recent_with_ids_in_bank(DEFAULT_BANK, 10);
            assert!(
                rows.iter().any(|(id, c)| id == &id_now && c == "稳定事实"),
                "reopen 后 id 应与首次派生一致: {rows:?}"
            );
        }
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
    }

    // §1.8.6 B supersede-by-id 端到端：memory_list 给 id → memory_ops supersede{id} 精确取代该条。
    #[tokio::test]
    async fn memory_ops_supersede_by_id_end_to_end() {
        let path = temp_memory_path("mem-ops-supid");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
        let store = Arc::new(MemoryStore::open(&path).unwrap());
        store.append_episode(DEFAULT_BANK, "过期结论 A", vec![], vec![], None, Some(1)).unwrap();
        store.append_episode(DEFAULT_BANK, "有效结论 B", vec![], vec![], None, Some(2)).unwrap();
        // memory_list 暴露每条的 id。
        let list = MemoryListTool::new(store.clone())
            .call(json!({ "bank": DEFAULT_BANK, "recent": 10 }))
            .await
            .unwrap();
        let recent = list["recent"].as_array().unwrap();
        let id_a = recent
            .iter()
            .find(|r| r["content"] == json!("过期结论 A"))
            .and_then(|r| r["id"].as_str())
            .expect("memory_list 应给出 A 的 id")
            .to_string();
        // agent 按 id 精确取代 A。
        let res = MemoryOpsTool::new(store.clone())
            .call(json!({ "ops": [ { "op": "supersede", "id": id_a } ] }))
            .await
            .unwrap();
        assert_eq!(res["ok"], json!(true));
        assert_eq!(res["applied"], json!(1), "应取代 1 条: {res}");
        // A 淡出召回、B 仍在。
        let hits = store.recall_facts_in_bank(DEFAULT_BANK, "结论", 8);
        assert!(!hits.iter().any(|h| h.content == "过期结论 A"), "A 应淡出: {hits:?}");
        assert!(hits.iter().any(|h| h.content == "有效结论 B"), "B 应仍在: {hits:?}");
        // A 行仍在磁盘（审计）。
        assert!(std::fs::read_to_string(&path).unwrap().contains("过期结论 A"), "A 应留磁盘审计");
        // 空 id 被 Validator 拒。
        let bad = MemoryOpsTool::new(store)
            .call(json!({ "ops": [ { "op": "supersede", "id": "  " } ] }))
            .await
            .unwrap();
        // 空白 id 不进 SupersedeById（parse 回退按 content，content 也空）→ EmptyContent 拒绝。
        assert_eq!(bad["ok"], json!(false), "空 id+空 content 应被拒: {bad}");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
    }

    // §1.8.6 B update：按 id 原子修订——旧条淡出 + 新内容入库可召回；空 id 被拒。
    #[tokio::test]
    async fn memory_ops_update_revises_by_id() {
        let path = temp_memory_path("mem-ops-update");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
        let store = Arc::new(MemoryStore::open(&path).unwrap());
        store.append_episode(DEFAULT_BANK, "端口是 8080", vec!["端口".into()], vec![], None, Some(1)).unwrap();
        let id = derive_entry_id("default", "端口是 8080", Some(1));
        let res = MemoryOpsTool::new(store.clone())
            .call(json!({ "ops": [
                { "op": "update", "id": id, "content": "端口改为 9090", "entities": ["端口"] }
            ] }))
            .await
            .unwrap();
        assert_eq!(res["ok"], json!(true));
        assert_eq!(res["applied"], json!(1), "update 应计 1: {res}");
        let hits = store.recall_facts_in_bank(DEFAULT_BANK, "端口", 8);
        assert!(hits.iter().any(|h| h.content == "端口改为 9090"), "新内容应可召回: {hits:?}");
        assert!(!hits.iter().any(|h| h.content == "端口是 8080"), "旧内容应淡出: {hits:?}");
        // 旧条留磁盘审计。
        assert!(std::fs::read_to_string(&path).unwrap().contains("端口是 8080"), "旧条应留磁盘");
        // 空 id 的 update → Validator 拒。
        let bad = MemoryOpsTool::new(store)
            .call(json!({ "ops": [ { "op": "update", "id": "", "content": "x" } ] }))
            .await
            .unwrap();
        assert_eq!(bad["ok"], json!(false), "空 id update 应被拒: {bad}");
        assert_eq!(bad["invalid"][0]["reason"], json!("EmptyField(\"id\")"));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
    }

    // §1.8.6 C 写侧：retain_with_entities 让 curated 手记也带实体 → 也能参与扩散（此前仅 episode）。
    #[test]
    fn retain_with_entities_enables_expansion() {
        let path = temp_memory_path("mem-retain-ent");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
        let store = MemoryStore::open(&path).unwrap();
        // 两条共享实体「登录」的手记（retain，非 episode）。
        store.retain_with_entities(DEFAULT_BANK, "登录用 JWT", false, vec!["登录".into()]).unwrap();
        store.retain_with_entities(DEFAULT_BANK, "登录失败查 session", false, vec!["登录".into()]).unwrap();
        // 行内带 entities 落盘（向后兼容：空 entities 的 retain 行不含该字段）。
        let jsonl = std::fs::read_to_string(&path).unwrap();
        assert!(jsonl.contains("登录"), "实体应落盘: {jsonl}");
        // 主命中一条 → 扩散补回另一条（共享实体「登录」）——curated 手记现也扩散。
        let primary = vec![FactHit { score: 0.9, content: "登录用 JWT".into(), source: MemorySource::Retain, provenance: None }];
        let related = store.expand_facts(DEFAULT_BANK, &primary, 3);
        assert!(related.iter().any(|r| r.content.contains("session")), "应扩散补回同实体手记: {related:?}");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
    }

    // §1.8.6 C：expand_facts 从主命中做 1-hop 实体扩散，补回共享实体的其它 episode（救多事实召回）。
    #[test]
    fn expand_facts_recovers_shared_entity_episodes() {
        let path = temp_memory_path("mem-expand-facts");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
        let store = MemoryStore::open(&path).unwrap();
        for s in ["部署用 docker-compose", "部署关闭 debug 日志", "部署健康检查 /health"] {
            store
                .append_episode(DEFAULT_BANK, s, vec!["部署".into()], vec![], None, Some(0))
                .unwrap();
        }
        let primary = vec![FactHit {
            score: 0.9,
            content: "部署用 docker-compose".into(),
            source: MemorySource::Episode,
            provenance: None,
        }];
        let related = store.expand_facts(DEFAULT_BANK, &primary, 3);
        assert_eq!(related.len(), 2, "应扩散补回 2 条: {related:?}");
        assert!(related.iter().all(|r| r.content != "部署用 docker-compose"));
        assert!(related.iter().any(|r| r.content.contains("debug 日志")));
        assert!(related.iter().any(|r| r.content.contains("健康检查")));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
    }

    // §1.8.5 Trail：召回排序中捷径保底——即使一堆高分 episode 也保证捷径指针露出。
    #[test]
    fn recall_ranked_floors_trail_hits() {
        let path = temp_memory_path("mem-trail-floor");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
        let store = MemoryStore::open(&path).unwrap();
        store
            .append_trail(
                DEFAULT_BANK,
                "pptx 加图表",
                "skill://officecli-pptx#add-chart",
                vec![],
                Some(0),
            )
            .unwrap();
        for i in 0..6 {
            store
                .append_episode(
                    DEFAULT_BANK,
                    &format!("pptx 流水 episode {i}"),
                    vec![],
                    vec![],
                    None,
                    None,
                )
                .unwrap();
        }
        // top_k=3、retain_floor=0、trail_floor=1：结果里必须有那条捷径。
        let hits = store.recall_ranked_with_trail(DEFAULT_BANK, "pptx", 3, 0, 1);
        assert_eq!(hits.len(), 3);
        assert!(
            hits.iter().any(|h| h.source == MemorySource::Trail),
            "trail 应被保底进结果: {:?}",
            hits.iter()
                .map(|h| (&h.content, h.source))
                .collect::<Vec<_>>()
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
    }

    // §1.8.5 Trail：render 把捷径渲染进独立「捷径/shortcuts」段，不混进「事实」。
    #[test]
    fn render_memory_graph_emits_shortcuts_section() {
        let facts = vec![
            FactHit {
                score: 0.6,
                content: "登录用方案B".into(),
                source: MemorySource::Episode,
                provenance: None,
            },
            FactHit {
                score: 0.4, // 低于相似度闸 → 仅参考
                content: "pptx 加柱状图 → skill://officecli-pptx#add-chart".into(),
                source: MemorySource::Trail,
                provenance: None,
            },
            FactHit {
                score: 0.80, // 高于相似度闸 → 可直接打开
                content: "docx 加目录 → skill://officecli-docx#toc".into(),
                source: MemorySource::Trail,
                provenance: None,
            },
        ];
        let block = render_memory_graph(&facts, &RecallGraph::default(), &[]);
        assert!(block.contains("捷径/shortcuts"), "应有捷径段: {block}");
        assert!(
            block.contains("skill://officecli-pptx#add-chart"),
            "捷径指针应在段内: {block}"
        );
        // 捷径不出现在「事实」列表里（事实段只列非 trail）。
        let facts_section = block.split("捷径/shortcuts").next().unwrap();
        assert!(
            !facts_section.contains("officecli-pptx"),
            "事实段不应含捷径: {facts_section}"
        );
        assert!(
            facts_section.contains("登录用方案B"),
            "事实段应含 episode: {facts_section}"
        );
        // §1.8.5 Phase 2 相似度闸：高分捷径标「可直接打开」，低分标「仅参考」。
        let trail_section = block.split("捷径/shortcuts").nth(1).unwrap();
        // 低分（pptx, 0.40）那行应是「仅参考」。
        let pptx_line = trail_section
            .lines()
            .find(|l| l.contains("officecli-pptx"))
            .unwrap();
        assert!(
            pptx_line.contains("仅参考"),
            "低相似度捷径应标仅参考: {pptx_line}"
        );
        // 高分（docx, 0.80）那行应是「可直接打开」。
        let docx_line = trail_section
            .lines()
            .find(|l| l.contains("officecli-docx"))
            .unwrap();
        assert!(
            docx_line.contains("可直接打开"),
            "高相似度捷径应标可直接打开: {docx_line}"
        );
    }

    // §1.8.8 S1：旧行（无 source 字段）迁移为 source=Retain；retain 写出的行不含 source（字节干净）。
    #[tokio::test]
    async fn old_rows_default_to_retain_and_retain_stays_clean() {
        let path = temp_memory_path("mem-src-migrate");
        let _ = std::fs::remove_file(&path);
        // 旧格式行：无 source 字段。
        std::fs::write(&path, "{\"bank\":\"default\",\"content\":\"老事实\"}").unwrap();
        let store = Arc::new(MemoryStore::open(&path).unwrap());
        let hits = store.recall_facts_in_bank(DEFAULT_BANK, "老事实", 5);
        assert_eq!(hits[0].source, MemorySource::Retain, "旧行应默认 Retain");

        // retain 新写一条 → 行内不应出现 source/provenance（skip_serializing_if 保旧读者兼容）。
        RetainTool::new(store.clone())
            .call(json!({ "content": "新手记" }))
            .await
            .unwrap();
        let jsonl = std::fs::read_to_string(&path).unwrap();
        let retain_line = jsonl.lines().find(|l| l.contains("新手记")).unwrap();
        assert!(
            !retain_line.contains("source"),
            "retain 行不应含 source: {retain_line}"
        );
        assert!(
            !retain_line.contains("provenance"),
            "retain 行不应含 provenance: {retain_line}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn old_plain_text_lines_load_into_default_bank() {
        let path = temp_memory_path("mem-legacy-test");
        std::fs::write(&path, "我叫 Zoe\n").unwrap();

        let store = MemoryStore::open(&path).unwrap();
        assert!(
            store
                .recall("我叫什么", 5)
                .iter()
                .any(|m| m.contains("Zoe"))
        );
        assert!(store.recall_in_bank("profile", "我叫什么", 5).is_empty());
        let _ = std::fs::remove_file(&path);
    }

    // memory_list：枚举 bank 计数 + 钉住 + 最近（recall 盲区：无法枚举）。
    #[tokio::test]
    async fn memory_list_enumerates_banks_pinned_recent() {
        let path = temp_memory_path("mem-list");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
        let store = Arc::new(MemoryStore::open(&path).unwrap());
        store
            .retain_in_bank_pinned("profile", "我叫张三", true)
            .unwrap();
        store.retain_in_bank("profile", "喜欢 Rust").unwrap();
        store.retain_in_bank("work", "项目用 axum").unwrap();

        let out = MemoryListTool::new(store)
            .call(json!({ "bank": "profile" }))
            .await
            .unwrap();
        // banks 概览含 profile(2) 与 work(1)。
        let banks = out["banks"].as_array().unwrap();
        assert!(
            banks
                .iter()
                .any(|b| b["bank"] == "profile" && b["count"] == 2)
        );
        assert!(banks.iter().any(|b| b["bank"] == "work" && b["count"] == 1));
        // profile 的钉住事实可枚举（§1.8.6 B：现为 {id, content} 对象，带稳定 id）。
        assert!(
            out["pinned"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["content"] == "我叫张三" && p["id"].as_str().is_some_and(|s| s.starts_with('m')))
        );
        // recent 含非钉住的「喜欢 Rust」，且带 id。
        assert!(
            out["recent"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["content"] == "喜欢 Rust" && r["id"].as_str().is_some_and(|s| s.starts_with('m')))
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
    }

    // §1.8.3b 关系串解析：标准形 + 箭头变体宽松容错（本地模型输出不稳）。
    #[test]
    fn parse_relation_handles_standard_and_variants() {
        // 规定形：带关系词。
        assert_eq!(
            parse_relation("登录模块 -改用-> 方案B"),
            Some(("登录模块".into(), "改用".into(), "方案B".into()))
        );
        assert_eq!(
            parse_relation("A -depends on-> B"),
            Some(("A".into(), "depends on".into(), "B".into()))
        );
        // 变体：省略关系词 → 默认箭头「→」（本地模型常见 "A -> B" / "A --> B"）。
        assert_eq!(
            parse_relation("登录模块 -> redis"),
            Some(("登录模块".into(), "→".into(), "redis".into()))
        );
        assert_eq!(
            parse_relation("A --> B"),
            Some(("A".into(), "→".into(), "B".into()))
        );
        assert_eq!(
            parse_relation("缺空格-> B"),
            Some(("缺空格".into(), "→".into(), "B".into()))
        );
        // 真无效：无箭头 / 缺左端 / 缺右端 → None。
        assert_eq!(parse_relation("没有箭头的句子"), None);
        assert_eq!(parse_relation("-> 只有右端"), None);
        assert_eq!(parse_relation("A -rel-> "), None);
    }

    // §1.8.3b 召回图：命中条目的 entities→节点、relations→边（去重、端点补进节点）。
    #[test]
    fn recall_graph_builds_nodes_and_edges_from_hits() {
        let path = temp_memory_path("mem-graph");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
        let store = MemoryStore::open(&path).unwrap();
        store
            .append_episode(
                DEFAULT_BANK,
                "登录模块 改用 方案B",
                vec!["登录模块".into(), "方案B".into()],
                vec!["登录模块 -改用-> 方案B".into()],
                None,
                Some(0),
            )
            .unwrap();
        store
            .append_episode(
                DEFAULT_BANK,
                "登录模块 依赖 redis",
                vec!["登录模块".into(), "redis".into()],
                vec!["登录模块 -依赖-> redis".into()],
                None,
                Some(0),
            )
            .unwrap();

        let g = store.recall_graph(DEFAULT_BANK, "登录模块", 5, 5);
        assert!(!g.facts.is_empty(), "应有命中事实");
        // 节点含三实体（去重）。
        for n in ["登录模块", "方案B", "redis"] {
            assert!(
                g.nodes.contains(&n.to_string()),
                "节点应含 {n}: {:?}",
                g.nodes
            );
        }
        // 两条边解析正确。
        assert!(
            g.edges
                .contains(&("登录模块".into(), "改用".into(), "方案B".into()))
        );
        assert!(
            g.edges
                .contains(&("登录模块".into(), "依赖".into(), "redis".into()))
        );

        // 渲染块含「记忆图」「节点」「连接」「深挖提示」。
        let block = render_memory_graph(&g.facts, &g, &[]);
        assert!(block.contains("记忆图"));
        assert!(block.contains("节点:"));
        assert!(block.contains("登录模块 -改用-> 方案B"));
        assert!(block.contains("read(memory://"));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
    }

    // §1.8.3b 渲染裁短：长 retain 事实不把每轮注入撑爆（含省略号、短串不动）。
    #[test]
    fn render_memory_graph_truncates_long_facts() {
        assert_eq!(truncate_chars("短", 200), "短");
        let long = "长".repeat(500);
        let cut = truncate_chars(&long, 200);
        assert_eq!(cut.chars().count(), 201, "200 字符 + 省略号");
        assert!(cut.ends_with('…'));

        let facts = vec![FactHit {
            score: 0.9,
            content: "x".repeat(400),
            source: MemorySource::Retain,
            provenance: None,
        }];
        let block = render_memory_graph(&facts, &RecallGraph::default(), &[]);
        // 事实行被裁到 200 + 省略号，不会整段 400 字注入。
        assert!(block.contains('…'));
        assert!(!block.contains(&"x".repeat(400)));
    }

    // §1.8.3④ ANN：超阈值时经 HNSW 候选召回，结果与语义期望一致（feature `hnsw`）。
    #[cfg(feature = "hnsw")]
    #[test]
    fn ann_recall_finds_nearest_above_threshold() {
        let path = temp_memory_path("mem-ann");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
        // 阈值压到 2，插 5 条即触发 ANN 路径。
        unsafe { std::env::set_var("BOTOBOT_MEMORY_ANN_THRESHOLD", "2") };
        let store = Arc::new(MemoryStore::open(&path).unwrap());
        store.set_embedder(Arc::new(StubEmbedder));
        for t in [
            "cat kitten",
            "car drive",
            "food eat",
            "cat meow",
            "car wheel",
        ] {
            store.retain(t).unwrap();
        }
        let hits = store.recall_in_bank(DEFAULT_BANK, "cat", 3);
        unsafe { std::env::remove_var("BOTOBOT_MEMORY_ANN_THRESHOLD") };
        assert!(
            hits.iter().any(|h| h.contains("cat")),
            "ANN 召回应含 cat: {hits:?}"
        );
        assert!(
            !hits.iter().any(|h| h.contains("food")),
            "不相关 food 不应进 top: {hits:?}"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(vectors_path(&path));
    }
}
