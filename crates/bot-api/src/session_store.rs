//! SessionStore：会话消息（append-only）+ 会话元信息 + bot 注册表的持久化。
//!
//! 取代旧 `thread_store`（整份 history 快照 append）。落盘结构对齐前身 datoobot
//! `workspace/.bot/` 并扩展多 bot：
//!
//! ```text
//! <root>/                       root = .bot
//!   bots.json                   bot 注册表 { bots: [BotEntry] }
//!   sessions/<sid>/
//!     meta.json                 SessionMeta（原子写）
//!     messages.jsonl            一行一条 base_types::Message（append-only）
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base_types::{Message, SubsessionStore};
use serde::{Deserialize, Serialize};

use crate::hub::BotEntry;

/// 会话种类。`team_member` 是协作委派 session，不走普通父子树。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    #[default]
    Chat,
    Fork,
    Subagent,
    TeamMember,
}

/// 会话元信息（datoobot 全字段 + bot_id + kind + parent/fork + team 预留）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: String,
    pub bot_id: String,
    #[serde(default)]
    pub kind: SessionKind,
    #[serde(default)]
    pub parent_session: Option<String>,
    #[serde(default)]
    pub fork_point: Option<usize>,
    // 协作层预留（§4.5），本条不写入
    #[serde(default)]
    pub team_id: Option<String>,
    #[serde(default)]
    pub requested_by_session: Option<String>,
    #[serde(default)]
    pub role_in_team: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub message_count: usize,
    #[serde(default)]
    pub start: usize,
    #[serde(default)]
    pub total_prompt_tokens: u64,
    #[serde(default)]
    pub total_completion_tokens: u64,
}

impl SessionMeta {
    /// 新建一个 chat 会话档案（created_at = updated_at = now）。
    pub fn new_chat(session_id: impl Into<String>, bot_id: impl Into<String>) -> Self {
        let now = now_rfc3339();
        Self {
            session_id: session_id.into(),
            bot_id: bot_id.into(),
            kind: SessionKind::Chat,
            parent_session: None,
            fork_point: None,
            team_id: None,
            requested_by_session: None,
            role_in_team: None,
            created_at: now.clone(),
            updated_at: now,
            message_count: 0,
            start: 0,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct BotsFile {
    bots: Vec<BotEntry>,
}

/// 旧 thread_store 记录格式（仅用于迁移读取）。
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LegacyThreadRecord {
    History { messages: Vec<Message> },
}

#[derive(Clone)]
pub struct SessionStore {
    root: Arc<PathBuf>,
}

impl SessionStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Arc::new(root.into()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn session_dir(&self, sid: &str) -> Result<PathBuf, String> {
        validate_session_id(sid)?;
        Ok(self.root.join("sessions").join(sid))
    }

    // ───────────────────────── 会话消息 ─────────────────────────

    pub fn append_message(&self, sid: &str, msg: &Message) -> Result<(), String> {
        self.append_messages(sid, std::slice::from_ref(msg))
    }

    pub fn append_messages(&self, sid: &str, msgs: &[Message]) -> Result<(), String> {
        if msgs.is_empty() {
            return Ok(());
        }
        let dir = self.session_dir(sid)?;
        std::fs::create_dir_all(&dir)
            .map_err(|err| format!("create session dir {}: {err}", dir.display()))?;
        let path = dir.join("messages.jsonl");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|err| format!("open messages {}: {err}", path.display()))?;
        use std::io::Write as _;
        for msg in msgs {
            let line =
                serde_json::to_string(msg).map_err(|err| format!("serialize message: {err}"))?;
            writeln!(file, "{line}")
                .map_err(|err| format!("write messages {}: {err}", path.display()))?;
        }
        Ok(())
    }

    pub fn load_messages(&self, sid: &str) -> Result<Vec<Message>, String> {
        let path = self.session_dir(sid)?.join("messages.jsonl");
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for line in raw.lines().filter(|l| !l.trim().is_empty()) {
            let msg = serde_json::from_str::<Message>(line)
                .map_err(|err| format!("invalid message line {}: {err}", path.display()))?;
            out.push(msg);
        }
        Ok(out)
    }

    // ──────────────── 崩溃恢复 turn-scratch（§2.6 缺陷3 阶0）────────────────
    // 一轮进行中产生的 finalized message 逐条 append 到 turn-scratch.jsonl（rollout）。
    // 干净收尾后清空；崩溃后该文件非空 = 该 turn 半途夭折，启动时把它并回 messages.jsonl。
    // 与 messages.jsonl/压缩解耦：scratch 仅作崩溃恢复，正常路径不影响既有提交/压缩行为。

    fn scratch_path(&self, sid: &str) -> Result<PathBuf, String> {
        Ok(self.session_dir(sid)?.join("turn-scratch.jsonl"))
    }

    /// 追加一条本轮 finalized message 到 scratch（rollout 增量落盘）。
    pub fn append_scratch(&self, sid: &str, msg: &Message) -> Result<(), String> {
        let dir = self.session_dir(sid)?;
        std::fs::create_dir_all(&dir)
            .map_err(|err| format!("create session dir {}: {err}", dir.display()))?;
        let path = dir.join("turn-scratch.jsonl");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|err| format!("open scratch {}: {err}", path.display()))?;
        use std::io::Write as _;
        let line = serde_json::to_string(msg).map_err(|err| format!("serialize scratch: {err}"))?;
        writeln!(file, "{line}")
            .map_err(|err| format!("write scratch {}: {err}", path.display()))?;
        Ok(())
    }

    /// 读 scratch 全部消息（无文件=空）。
    pub fn read_scratch(&self, sid: &str) -> Result<Vec<Message>, String> {
        let Ok(path) = self.scratch_path(sid) else {
            return Ok(Vec::new());
        };
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for line in raw.lines().filter(|l| !l.trim().is_empty()) {
            // scratch 容损：坏行跳过（崩溃可能截断最后一行），尽力恢复其余。
            if let Ok(msg) = serde_json::from_str::<Message>(line) {
                out.push(msg);
            }
        }
        Ok(out)
    }

    /// 清空 scratch（干净收尾后调用）。文件不存在视为成功。
    pub fn clear_scratch(&self, sid: &str) -> Result<(), String> {
        let path = self.scratch_path(sid)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("remove scratch {}: {e}", path.display())),
        }
    }

    /// 启动恢复：若 scratch 非空（上次 turn 半途崩溃），把其消息并回 messages.jsonl 并清空。
    /// 返回恢复的消息条数（0=无需恢复）。幂等：恢复后 scratch 已清。
    pub fn recover_scratch(&self, sid: &str) -> Result<usize, String> {
        let pending = self.read_scratch(sid)?;
        if pending.is_empty() {
            // 可能是空文件残留，顺手清掉。
            let _ = self.clear_scratch(sid);
            return Ok(0);
        }
        self.append_messages(sid, &pending)?;
        self.clear_scratch(sid)?;
        Ok(pending.len())
    }

    // ───────────────────────── 会话元信息 ─────────────────────────

    pub fn write_meta(&self, sid: &str, meta: &SessionMeta) -> Result<(), String> {
        let dir = self.session_dir(sid)?;
        std::fs::create_dir_all(&dir)
            .map_err(|err| format!("create session dir {}: {err}", dir.display()))?;
        let json =
            serde_json::to_vec_pretty(meta).map_err(|err| format!("serialize meta: {err}"))?;
        atomic_write(&dir.join("meta.json"), &json)
    }

    pub fn read_meta(&self, sid: &str) -> Result<Option<SessionMeta>, String> {
        let path = self.session_dir(sid)?.join("meta.json");
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return Ok(None);
        };
        let meta = serde_json::from_str::<SessionMeta>(&raw)
            .map_err(|err| format!("invalid meta {}: {err}", path.display()))?;
        Ok(Some(meta))
    }

    /// turn 提交后更新 meta 的 `message_count` / `updated_at`（meta 不存在则不动）。
    pub fn bump_meta_after_turn(&self, sid: &str, message_count: usize) -> Result<(), String> {
        if let Some(mut meta) = self.read_meta(sid)? {
            meta.message_count = message_count;
            meta.updated_at = now_rfc3339();
            self.write_meta(sid, &meta)?;
        }
        Ok(())
    }

    /// turn 提交后 **upsert** meta（懒持久化）：meta 不存在则按 `bot_id` 新建 chat meta，
    /// 已存在则只更 `message_count`/`updated_at`（保留 fork/subagent 等 kind 与归属）。
    /// 让空会话（WS 连接即开但从不发言）不落盘，避免空壳累积。
    pub fn upsert_meta_after_turn(
        &self,
        sid: &str,
        bot_id: &str,
        message_count: usize,
    ) -> Result<(), String> {
        let mut meta = self
            .read_meta(sid)?
            .unwrap_or_else(|| SessionMeta::new_chat(sid, bot_id));
        meta.message_count = message_count;
        meta.updated_at = now_rfc3339();
        self.write_meta(sid, &meta)
    }

    pub fn list_metas(&self) -> Result<Vec<SessionMeta>, String> {
        let sessions_dir = self.root.join("sessions");
        let Ok(entries) = std::fs::read_dir(&sessions_dir) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|err| format!("read sessions dir: {err}"))?;
            if !entry.path().is_dir() {
                continue;
            }
            let Some(sid) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if let Some(meta) = self.read_meta(&sid)? {
                out.push(meta);
            }
        }
        out.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        Ok(out)
    }

    /// 删除会话的持久化目录 `sessions/<id>/`（含 meta/messages/todos）。幂等：不存在视为成功。
    pub fn delete_session(&self, sid: &str) -> Result<(), String> {
        let dir = self.session_dir(sid)?;
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .map_err(|err| format!("delete session {}: {err}", dir.display()))?;
        }
        Ok(())
    }

    // ───────────────────────── bot 注册表 ─────────────────────────

    pub fn load_bots(&self) -> Result<Vec<BotEntry>, String> {
        let path = self.root.join("bots.json");
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return Ok(Vec::new());
        };
        let file = serde_json::from_str::<BotsFile>(&raw)
            .map_err(|err| format!("invalid bots.json {}: {err}", path.display()))?;
        Ok(file.bots)
    }

    pub fn save_bots(&self, bots: &[BotEntry]) -> Result<(), String> {
        std::fs::create_dir_all(self.root.as_path())
            .map_err(|err| format!("create root {}: {err}", self.root.display()))?;
        let file = BotsFile {
            bots: bots.to_vec(),
        };
        let json =
            serde_json::to_vec_pretty(&file).map_err(|err| format!("serialize bots: {err}"))?;
        atomic_write(&self.root.join("bots.json"), &json)
    }

    // ───────────────────────── 旧数据迁移（方案 A）─────────────────────────

    /// 把现存 `<root>/threads/*.jsonl` 旧快照导入新结构。已迁移的会话目录跳过（幂等）。
    /// 旧 `threads/` 目录保留不删（回退安全）。`default_bot_id` 用于补建 meta 的归属。
    pub fn migrate_legacy_threads(&self, default_bot_id: &str) -> Result<usize, String> {
        let threads_dir = self.root.join("threads");
        let Ok(entries) = std::fs::read_dir(&threads_dir) else {
            return Ok(0);
        };
        let mut migrated = 0;
        for entry in entries {
            let entry = entry.map_err(|err| format!("read threads dir: {err}"))?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(sid) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if validate_session_id(sid).is_err() {
                continue;
            }
            // 幂等：已有 session 目录则跳过。
            if self.session_dir(sid)?.exists() {
                continue;
            }
            let messages = read_legacy_snapshot(&path)?;
            self.append_messages(sid, &messages)?;
            let mut meta = SessionMeta::new_chat(sid, default_bot_id);
            meta.message_count = messages.len();
            self.write_meta(sid, &meta)?;
            migrated += 1;
        }
        Ok(migrated)
    }
}

/// 读旧 thread_store 快照文件，取最新一条 History 记录的 messages。
fn read_legacy_snapshot(path: &Path) -> Result<Vec<Message>, String> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Ok(Vec::new());
    };
    let mut latest = Vec::new();
    for line in raw.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<LegacyThreadRecord>(line)
            .map_err(|err| format!("invalid legacy record {}: {err}", path.display()))?
        {
            LegacyThreadRecord::History { messages } => latest = messages,
        }
    }
    Ok(latest)
}

/// 原子写：写 `<path>.tmp` 再 rename（同目录 rename 原子）。
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|err| format!("write {}: {err}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .map_err(|err| format!("rename {} -> {}: {err}", tmp.display(), path.display()))
}

fn validate_session_id(sid: &str) -> Result<(), String> {
    if sid.is_empty() {
        return Err("session id is empty".into());
    }
    // `<sid>` 现在是目录组件（不再是 `<id>.jsonl` 文件名），必须挡掉 `.`/`..` 防穿越。
    if sid == "." || sid == ".." {
        return Err(format!("invalid session id: {sid}"));
    }
    if sid
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        Ok(())
    } else {
        Err(format!("invalid session id: {sid}"))
    }
}

/// 无外部依赖的 UTC RFC3339 时间戳（秒精度）。
fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Howard Hinnant 的 civil-from-days：天数（自 1970-01-01）→ (年, 月, 日)。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `SubsessionStore` 端口的 SessionStore 实现（§2.5b）：把 subagent run 落盘为
/// `kind=subagent`、`parent_session=父会话` 的第一级 subsession。装配处（webui-bin）
/// 构造并经 `AgentBuilder::subsession_store` 注入；agent-loop 仅依赖 base-types 的 trait。
pub struct SessionStoreSubsessions {
    store: SessionStore,
    /// subsession 归属 bot（当前子 agent 不承袭父 bot，统一记 default；§4.5 团队层再细化）。
    bot_id: String,
}

impl SessionStoreSubsessions {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            store: SessionStore::new(root),
            bot_id: crate::hub::DEFAULT_BOT_ID.to_string(),
        }
    }
}

impl SubsessionStore for SessionStoreSubsessions {
    fn record_subsession(&self, child: &str, parent: &str) -> Result<(), String> {
        let mut meta = SessionMeta::new_chat(child, &self.bot_id);
        meta.kind = SessionKind::Subagent;
        meta.parent_session = Some(parent.to_string());
        self.store.write_meta(child, &meta)
    }

    fn persist_subsession_messages(&self, child: &str, msgs: &[Message]) -> Result<(), String> {
        self.store.append_messages(child, msgs)?;
        // 对齐 message_count（record 时 meta 已建）。
        self.store.bump_meta_after_turn(child, msgs.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base_types::{ContentPart, Message};

    fn tmp_root() -> PathBuf {
        std::env::temp_dir().join(format!("botobot-sstore-{}", uuid::Uuid::new_v4()))
    }

    fn text_of(m: &Message) -> String {
        m.content
            .iter()
            .map(|p| match p {
                ContentPart::Text(t) => t.as_str(),
                ContentPart::ImageUrl(_) => "",
            })
            .collect()
    }

    #[test]
    fn append_then_load_roundtrip() {
        let root = tmp_root();
        let store = SessionStore::new(root.clone());
        store.append_message("s1", &Message::user("a")).unwrap();
        store
            .append_messages("s1", &[Message::assistant("b"), Message::user("c")])
            .unwrap();
        let msgs = store.load_messages("s1").unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(text_of(&msgs[0]), "a");
        assert_eq!(text_of(&msgs[2]), "c");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn scratch_recovery_merges_crashed_turn_then_clears() {
        let root = tmp_root();
        let store = SessionStore::new(root.clone());
        // 上一干净 turn 已落 messages.jsonl。
        store.append_message("s1", &Message::user("q1")).unwrap();
        store
            .append_message("s1", &Message::assistant("a1"))
            .unwrap();
        // 本轮进行中：finalized message 逐条进 scratch，然后“崩溃”（没走 clear）。
        store.append_scratch("s1", &Message::user("q2")).unwrap();
        store
            .append_scratch("s1", &Message::assistant("partial a2"))
            .unwrap();
        assert_eq!(store.read_scratch("s1").unwrap().len(), 2);

        // 重启恢复：scratch 并回 messages.jsonl 并清空。
        let recovered = store.recover_scratch("s1").unwrap();
        assert_eq!(recovered, 2);
        let msgs = store.load_messages("s1").unwrap();
        assert_eq!(msgs.len(), 4, "崩溃 turn 的 2 条应已并入");
        assert_eq!(text_of(&msgs[3]), "partial a2");
        assert!(
            store.read_scratch("s1").unwrap().is_empty(),
            "恢复后 scratch 应清空"
        );
        // 幂等：再恢复无变化。
        assert_eq!(store.recover_scratch("s1").unwrap(), 0);
        assert_eq!(store.load_messages("s1").unwrap().len(), 4);

        // clear 后无残留。
        store.append_scratch("s1", &Message::user("x")).unwrap();
        store.clear_scratch("s1").unwrap();
        assert_eq!(store.recover_scratch("s1").unwrap(), 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn meta_roundtrip_and_list() {
        let root = tmp_root();
        let store = SessionStore::new(root.clone());
        let mut meta = SessionMeta::new_chat("s1", "bot-x");
        meta.message_count = 2;
        meta.total_prompt_tokens = 42;
        store.write_meta("s1", &meta).unwrap();
        store
            .write_meta("s2", &SessionMeta::new_chat("s2", "bot-y"))
            .unwrap();

        let read = store.read_meta("s1").unwrap().unwrap();
        assert_eq!(read.bot_id, "bot-x");
        assert_eq!(read.message_count, 2);
        assert_eq!(read.total_prompt_tokens, 42);
        assert!(store.read_meta("missing").unwrap().is_none());

        let metas = store.list_metas().unwrap();
        assert_eq!(metas.len(), 2);
        assert_eq!(metas[0].session_id, "s1");
        // 原子写不留 .tmp
        assert!(!root.join("sessions/s1/meta.tmp").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn bots_roundtrip() {
        let root = tmp_root();
        let store = SessionStore::new(root.clone());
        assert!(store.load_bots().unwrap().is_empty());
        let bots = vec![BotEntry {
            id: "bot-default".into(),
            name: "botobot".into(),
            profile: "coder".into(),
            workdir: PathBuf::from("/tmp"),
            system: None,
        }];
        store.save_bots(&bots).unwrap();
        let loaded = store.load_bots().unwrap();
        assert_eq!(loaded, bots);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn migrate_expands_snapshot_and_idempotent() {
        let root = tmp_root();
        let store = SessionStore::new(root.clone());
        // 构造旧快照：threads/old.jsonl，含两条 History 记录（取最新一条 2 消息）
        let threads = root.join("threads");
        std::fs::create_dir_all(&threads).unwrap();
        let snap = "{\"type\":\"history\",\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}]}\n\
                    {\"type\":\"history\",\"messages\":[{\"role\":\"user\",\"content\":\"hi\"},{\"role\":\"assistant\",\"content\":\"yo\"}]}\n";
        std::fs::write(threads.join("old.jsonl"), snap).unwrap();

        let n = store.migrate_legacy_threads("bot-default").unwrap();
        assert_eq!(n, 1);
        let msgs = store.load_messages("old").unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(text_of(&msgs[1]), "yo");
        let meta = store.read_meta("old").unwrap().unwrap();
        assert_eq!(meta.message_count, 2);
        assert_eq!(meta.bot_id, "bot-default");

        // 幂等：再迁移一次不重复导入
        let n2 = store.migrate_legacy_threads("bot-default").unwrap();
        assert_eq!(n2, 0);
        assert_eq!(store.load_messages("old").unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn subsession_adapter_records_and_persists() {
        let root = tmp_root();
        let adapter = SessionStoreSubsessions::new(root.clone());
        adapter.record_subsession("child-1", "parent-1").unwrap();
        adapter
            .persist_subsession_messages(
                "child-1",
                &[Message::user("task"), Message::assistant("done")],
            )
            .unwrap();

        let store = SessionStore::new(root.clone());
        let meta = store.read_meta("child-1").unwrap().unwrap();
        assert_eq!(meta.kind, SessionKind::Subagent);
        assert_eq!(meta.parent_session.as_deref(), Some("parent-1"));
        assert_eq!(meta.message_count, 2);
        assert_eq!(store.load_messages("child-1").unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_path_traversal_ids() {
        let store = SessionStore::new(tmp_root());
        assert!(store.load_messages("..").is_err());
        assert!(store.append_message("..", &Message::user("x")).is_err());
        assert!(
            store
                .write_meta(".", &SessionMeta::new_chat(".", "b"))
                .is_err()
        );
    }

    #[test]
    fn civil_from_days_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_905), (2024, 7, 1)); // sanity
    }
}
