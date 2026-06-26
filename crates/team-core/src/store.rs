//! TeamStore：IM 协作层的可选 JSONL 持久化（**轨迹非记忆**，命名避开 "memory"）。
//!
//! 分层（C 决策）：Switchboard 默认纯内存；TeamStore 是可选落盘层，**只存协作层**
//! （projects + teams + IM transcript），不存 bots（归 SessionStore/hub 注册表）、
//! 不存执行历史（LLM/tool 归 SessionStore）。
//!
//! 布局（复用 thread_store 的 append-only 思路）：
//! ```text
//! <root>/
//!   projects.json                 项目注册表（原子写）
//!   teams/<team_id>/
//!     team.json                   Team 元信息（不含 messages，原子写）
//!     messages.jsonl              IM transcript，一行一条 Message（append-only）
//! ```

use std::path::{Path, PathBuf};

use crate::{Message, Switchboard, Team, TeamError, TeamProject};

#[derive(Clone)]
pub struct TeamStore {
    root: PathBuf,
}

impl TeamStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn team_dir(&self, team_id: &str) -> Result<PathBuf, TeamError> {
        validate_id(team_id)?;
        Ok(self.root.join("teams").join(team_id))
    }

    // ───────────────────────── 全量保存 / 加载 ─────────────────────────

    /// 全量保存：projects.json + 每个 team 的 team.json + messages.jsonl（messages 全量重写）。
    pub fn persist(&self, switchboard: &Switchboard) -> Result<(), TeamError> {
        std::fs::create_dir_all(&self.root).map_err(io)?;
        let projects: Vec<&TeamProject> = switchboard.projects().collect();
        let json = serde_json::to_vec_pretty(&projects).map_err(ser)?;
        atomic_write(&self.root.join("projects.json"), &json)?;

        for team in switchboard.teams() {
            self.save_team(team)?;
        }
        Ok(())
    }

    /// 保存单个 team（team.json 不含 messages；messages.jsonl 全量重写）。
    pub fn save_team(&self, team: &Team) -> Result<(), TeamError> {
        let dir = self.team_dir(&team.id)?;
        std::fs::create_dir_all(&dir).map_err(io)?;

        // team.json：清空 messages（轨迹另存 jsonl）
        let mut meta = team.clone();
        meta.messages = Vec::new();
        let json = serde_json::to_vec_pretty(&meta).map_err(ser)?;
        atomic_write(&dir.join("team.json"), &json)?;

        // messages.jsonl：全量重写
        let mut buf = String::new();
        for m in &team.messages {
            buf.push_str(&serde_json::to_string(m).map_err(ser)?);
            buf.push('\n');
        }
        std::fs::write(dir.join("messages.jsonl"), buf).map_err(io)?;
        Ok(())
    }

    /// 增量追加一条 IM 消息（hub 每次 post 调用，避免全量重写）。
    pub fn append_team_message(&self, team_id: &str, msg: &Message) -> Result<(), TeamError> {
        let dir = self.team_dir(team_id)?;
        std::fs::create_dir_all(&dir).map_err(io)?;
        let line = serde_json::to_string(msg).map_err(ser)?;
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("messages.jsonl"))
            .map_err(io)?;
        writeln!(f, "{line}").map_err(io)
    }

    /// 从磁盘重建 Switchboard（bots 为空，由调用方注入；projects + teams 从盘载入）。
    pub fn load(&self) -> Result<Switchboard, TeamError> {
        let projects = self.load_projects()?;
        let teams = self.load_teams()?;
        Ok(Switchboard::from_parts(Vec::new(), projects, teams))
    }

    fn load_projects(&self) -> Result<Vec<TeamProject>, TeamError> {
        let path = self.root.join("projects.json");
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return Ok(Vec::new());
        };
        serde_json::from_str(&raw).map_err(ser)
    }

    fn load_teams(&self) -> Result<Vec<Team>, TeamError> {
        let teams_dir = self.root.join("teams");
        let Ok(entries) = std::fs::read_dir(&teams_dir) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for entry in entries {
            let entry = entry.map_err(io)?;
            if !entry.path().is_dir() {
                continue;
            }
            let dir = entry.path();
            let Ok(raw) = std::fs::read_to_string(dir.join("team.json")) else {
                continue;
            };
            let mut team: Team = serde_json::from_str(&raw).map_err(ser)?;
            team.messages = load_messages(&dir.join("messages.jsonl"))?;
            out.push(team);
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }
}

fn load_messages(path: &Path) -> Result<Vec<Message>, TeamError> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for line in raw.lines().filter(|l| !l.trim().is_empty()) {
        out.push(serde_json::from_str(line).map_err(ser)?);
    }
    Ok(out)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), TeamError> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)
}

fn validate_id(id: &str) -> Result<(), TeamError> {
    if id.is_empty() || id == "." || id == ".." {
        return Err(TeamError::Io(format!("invalid id: {id}")));
    }
    if id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        Ok(())
    } else {
        Err(TeamError::Io(format!("invalid id: {id}")))
    }
}

fn io(e: std::io::Error) -> TeamError {
    TeamError::Io(e.to_string())
}
fn ser(e: serde_json::Error) -> TeamError {
    TeamError::Io(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Author, Bot, RoleInTeam, TeamProject};
    use std::path::PathBuf;

    fn tmp_root() -> PathBuf {
        std::env::temp_dir().join(format!("botobot-teamstore-{}", uuid::Uuid::new_v4()))
    }

    fn seeded_switchboard() -> Switchboard {
        let mut o = Switchboard::new();
        o.add_bot(Bot {
            id: "a".into(),
            name: "a".into(),
            role: "coder".into(),
            home: None,
        })
        .unwrap();
        o.add_bot(Bot {
            id: "b".into(),
            name: "b".into(),
            role: "coder".into(),
            home: None,
        })
        .unwrap();
        o.add_project(TeamProject {
            id: "p1".into(),
            name: "p1".into(),
            root_dir: PathBuf::from("/tmp"),
            default_bots: vec!["a".into()],
        })
        .unwrap();
        o
    }

    #[test]
    fn persist_then_load_roundtrips_projects_and_teams() {
        let root = tmp_root();
        let mut o = seeded_switchboard();
        let tid = o
            .open_team("p1", vec!["a".into(), "b".into()], "a".into(), "do x")
            .unwrap();
        o.post_message(&tid, Author::User, "hi").unwrap();
        o.post_message(&tid, Author::Bot("a".into()), "yo").unwrap();
        o.link_session(
            &tid,
            "b",
            "sess-b",
            RoleInTeam::Member,
            Some("sess-a".into()),
        )
        .unwrap();

        let store = TeamStore::new(root.clone());
        store.persist(&o).unwrap();

        let loaded = store.load().unwrap();
        // bots 不落 team store
        assert_eq!(loaded.bots().count(), 0);
        assert_eq!(loaded.projects().count(), 1);
        let t = loaded.team(&tid).unwrap();
        assert_eq!(t.leader, "a");
        assert_eq!(t.members, vec!["a", "b"]);
        assert_eq!(t.messages.len(), 2);
        assert_eq!(t.messages[0].seq, 0);
        assert_eq!(t.messages[1].content, "yo");
        assert_eq!(t.session_links.len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn append_message_then_load() {
        let root = tmp_root();
        let mut o = seeded_switchboard();
        let tid = o
            .open_team("p1", vec!["a".into()], "a".into(), "x")
            .unwrap();
        let store = TeamStore::new(root.clone());
        store.save_team(o.team(&tid).unwrap()).unwrap();
        store
            .append_team_message(
                &tid,
                &Message {
                    seq: 0,
                    author: Author::User,
                    content: "first".into(),
                },
            )
            .unwrap();
        store
            .append_team_message(
                &tid,
                &Message {
                    seq: 1,
                    author: Author::Bot("a".into()),
                    content: "second".into(),
                },
            )
            .unwrap();

        let loaded = store.load().unwrap();
        let t = loaded.team(&tid).unwrap();
        assert_eq!(t.messages.len(), 2);
        assert_eq!(t.messages[1].content, "second");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn load_empty_root_is_empty_switchboard() {
        let store = TeamStore::new(tmp_root());
        let o = store.load().unwrap();
        assert_eq!(o.teams().count(), 0);
        assert_eq!(o.projects().count(), 0);
    }

    #[test]
    fn no_tmp_residue_after_persist() {
        let root = tmp_root();
        let mut o = seeded_switchboard();
        o.open_team("p1", vec!["a".into()], "a".into(), "x")
            .unwrap();
        let store = TeamStore::new(root.clone());
        store.persist(&o).unwrap();
        assert!(!root.join("projects.tmp").exists());
        let _ = std::fs::remove_dir_all(root);
    }
}
