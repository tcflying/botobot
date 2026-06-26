//! team-core：多 bot IM 协作层（生成性公理的「外部世界」之一）。
//!
//! **机械传话 + 存消息**——无语义记忆 / 不规划 / 不评判，心智全在 bot 层。
//! `Switchboard` 是 IM 平台容器，持 bots / projects / teams 三注册表并驱动协作。
//! TeamStore 只存 IM 协作层；执行历史（LLM/tool）仍归 bot-api 的 SessionStore。
//!
//! 依赖方向：本 crate 在 bot-api **之下**，不依赖 `SessionId` 类型（session id 以字符串绑定）。

mod bot;
mod conduct;
mod message;
mod orchestrator;
mod project;
mod store;
mod task;
mod team;

pub use bot::Bot;
pub use conduct::{ParticipantTracker, TeamOutcome, TerminalKind};
pub use message::{Author, Message};
pub use orchestrator::{SessionRunner, TaskPlanner, TeamOrchestrator};
pub use project::TeamProject;
pub use store::TeamStore;
pub use task::TeamTask;
pub use team::{RoleInTeam, RoutingPolicy, Team, TeamSessionLink, TeamStatus};

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 协作层错误。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TeamError {
    #[error("bot not found: {0}")]
    BotNotFound(String),
    #[error("project not found: {0}")]
    ProjectNotFound(String),
    #[error("team not found: {0}")]
    TeamNotFound(String),
    #[error("leader is not a member: {0}")]
    LeaderNotMember(String),
    #[error("delegate target is not a member: {0}")]
    DelegateNotMember(String),
    #[error("no bot available to open team")]
    NoBotAvailable,
    #[error("bot already exists: {0}")]
    BotExists(String),
    #[error("project already exists: {0}")]
    ProjectExists(String),
    #[error("team store io: {0}")]
    Io(String),
}

/// `/api/teams` 快照（bots 已由 `/api/bots` 暴露，不重复）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwitchboardSnapshot {
    pub teams: Vec<Team>,
    pub projects: Vec<TeamProject>,
}

/// IM 平台容器：机械传话，无心智。持 bots / projects / teams 三注册表。
#[derive(Debug, Default)]
pub struct Switchboard {
    bots: BTreeMap<String, Bot>,
    projects: BTreeMap<String, TeamProject>,
    teams: BTreeMap<String, Team>,
}

impl Switchboard {
    pub fn new() -> Self {
        Self::default()
    }

    // ───────────────────────── 注册表 ─────────────────────────

    pub fn add_bot(&mut self, bot: Bot) -> Result<(), TeamError> {
        if self.bots.contains_key(&bot.id) {
            return Err(TeamError::BotExists(bot.id));
        }
        self.bots.insert(bot.id.clone(), bot);
        Ok(())
    }

    pub fn add_project(&mut self, project: TeamProject) -> Result<(), TeamError> {
        if self.projects.contains_key(&project.id) {
            return Err(TeamError::ProjectExists(project.id));
        }
        self.projects.insert(project.id.clone(), project);
        Ok(())
    }

    pub fn bot(&self, id: &str) -> Option<&Bot> {
        self.bots.get(id)
    }
    pub fn project(&self, id: &str) -> Option<&TeamProject> {
        self.projects.get(id)
    }
    pub fn team(&self, id: &str) -> Option<&Team> {
        self.teams.get(id)
    }
    pub fn bots(&self) -> impl Iterator<Item = &Bot> {
        self.bots.values()
    }
    pub fn projects(&self) -> impl Iterator<Item = &TeamProject> {
        self.projects.values()
    }
    pub fn teams(&self) -> impl Iterator<Item = &Team> {
        self.teams.values()
    }

    /// 从（持久化恢复的）部件重建 Switchboard。TeamStore 不存 bots（归 SessionStore/hub
    /// 注册表），故 load 时 bots 由调用方注入；这里允许传空。
    pub fn from_parts(bots: Vec<Bot>, projects: Vec<TeamProject>, teams: Vec<Team>) -> Self {
        Self {
            bots: bots.into_iter().map(|b| (b.id.clone(), b)).collect(),
            projects: projects.into_iter().map(|p| (p.id.clone(), p)).collect(),
            teams: teams.into_iter().map(|t| (t.id.clone(), t)).collect(),
        }
    }

    // ───────────────────────── 协作驱动 ─────────────────────────

    /// 开一个群。`members` 为空时按 fallback：project.default_bots → 仅一个 bot 自动 → 否则 NoBotAvailable。
    /// leader 必 ∈ 解析后的 members。team id 由 Switchboard 生成。
    pub fn open_team(
        &mut self,
        project_id: &str,
        members: Vec<String>,
        leader: String,
        task_description: impl Into<String>,
    ) -> Result<String, TeamError> {
        let project = self
            .projects
            .get(project_id)
            .ok_or_else(|| TeamError::ProjectNotFound(project_id.to_string()))?;

        // members fallback（Q11=C）
        let members = if !members.is_empty() {
            members
        } else if !project.default_bots.is_empty() {
            project.default_bots.clone()
        } else if self.bots.len() == 1 {
            vec![self.bots.keys().next().unwrap().clone()]
        } else {
            return Err(TeamError::NoBotAvailable);
        };

        // 全部 members 必须是已登记 bot
        for m in &members {
            if !self.bots.contains_key(m) {
                return Err(TeamError::BotNotFound(m.clone()));
            }
        }
        // leader 必 ∈ members
        if !members.iter().any(|m| m == &leader) {
            return Err(TeamError::LeaderNotMember(leader));
        }

        let team_id = format!("team-{}", uuid::Uuid::new_v4());
        let task = TeamTask {
            id: format!("task-{}", uuid::Uuid::new_v4()),
            project_id: project_id.to_string(),
            description: task_description.into(),
        };
        let team = Team {
            id: team_id.clone(),
            task,
            members,
            leader,
            routing: RoutingPolicy::LeaderMediated,
            status: TeamStatus::Active,
            messages: Vec::new(),
            session_links: Vec::new(),
        };
        self.teams.insert(team_id.clone(), team);
        Ok(team_id)
    }

    /// 追加一条群消息，返回其 seq（群内单调递增）。
    pub fn post_message(
        &mut self,
        team_id: &str,
        author: Author,
        content: impl Into<String>,
    ) -> Result<u64, TeamError> {
        let team = self
            .teams
            .get_mut(team_id)
            .ok_or_else(|| TeamError::TeamNotFound(team_id.to_string()))?;
        let seq = team.next_seq();
        team.messages.push(Message {
            seq,
            author,
            content: content.into(),
        });
        Ok(seq)
    }

    /// leader 委派校验：目标必须是该群成员（DelegateNotMember）。实际开 session 归 hub，
    /// hub 随后调 [`Switchboard::link_session`] 记录委派边。
    pub fn delegate(&self, team_id: &str, bot_id: &str) -> Result<(), TeamError> {
        let team = self
            .teams
            .get(team_id)
            .ok_or_else(|| TeamError::TeamNotFound(team_id.to_string()))?;
        if !team.is_member(bot_id) {
            return Err(TeamError::DelegateNotMember(bot_id.to_string()));
        }
        Ok(())
    }

    /// 记录 team ↔ bot session 委派边（hub 开/复用 session 后调用）。
    pub fn link_session(
        &mut self,
        team_id: &str,
        bot_id: impl Into<String>,
        session_id: impl Into<String>,
        role: RoleInTeam,
        requested_by_session: Option<String>,
    ) -> Result<(), TeamError> {
        let bot_id = bot_id.into();
        let team = self
            .teams
            .get_mut(team_id)
            .ok_or_else(|| TeamError::TeamNotFound(team_id.to_string()))?;
        if !team.is_member(&bot_id) {
            return Err(TeamError::DelegateNotMember(bot_id));
        }
        team.session_links.push(TeamSessionLink {
            bot_id,
            session_id: session_id.into(),
            role,
            requested_by_session,
        });
        Ok(())
    }

    /// 改 leader：新 leader 必 ∈ members。
    pub fn set_leader(
        &mut self,
        team_id: &str,
        new_leader: impl Into<String>,
    ) -> Result<(), TeamError> {
        let new_leader = new_leader.into();
        let team = self
            .teams
            .get_mut(team_id)
            .ok_or_else(|| TeamError::TeamNotFound(team_id.to_string()))?;
        if !team.is_member(&new_leader) {
            return Err(TeamError::LeaderNotMember(new_leader));
        }
        team.leader = new_leader;
        Ok(())
    }

    /// 设置群状态（Active/Done/Cancelled）。
    pub fn set_status(&mut self, team_id: &str, status: TeamStatus) -> Result<(), TeamError> {
        let team = self
            .teams
            .get_mut(team_id)
            .ok_or_else(|| TeamError::TeamNotFound(team_id.to_string()))?;
        team.status = status;
        Ok(())
    }

    /// `/api/teams` 快照。
    pub fn snapshot(&self) -> SwitchboardSnapshot {
        SwitchboardSnapshot {
            teams: self.teams.values().cloned().collect(),
            projects: self.projects.values().cloned().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn bot(id: &str) -> Bot {
        Bot {
            id: id.into(),
            name: id.into(),
            role: "coder".into(),
            home: None,
        }
    }
    fn project(id: &str, default_bots: Vec<String>) -> TeamProject {
        TeamProject {
            id: id.into(),
            name: id.into(),
            root_dir: PathBuf::from("/tmp"),
            default_bots,
        }
    }

    fn switchboard_with(bots: &[&str], proj: TeamProject) -> Switchboard {
        let mut o = Switchboard::new();
        for b in bots {
            o.add_bot(bot(b)).unwrap();
        }
        o.add_project(proj).unwrap();
        o
    }

    #[test]
    fn open_team_with_explicit_members() {
        let mut o = switchboard_with(&["a", "b"], project("p1", vec![]));
        let tid = o
            .open_team("p1", vec!["a".into(), "b".into()], "a".into(), "do x")
            .unwrap();
        let t = o.team(&tid).unwrap();
        assert_eq!(t.members.len(), 2);
        assert_eq!(t.leader, "a");
        assert_eq!(t.status, TeamStatus::Active);
        assert_eq!(t.routing, RoutingPolicy::LeaderMediated);
        assert_eq!(t.task.description, "do x");
    }

    #[test]
    fn open_team_falls_back_to_default_bots() {
        let mut o = switchboard_with(&["a", "b"], project("p1", vec!["a".into(), "b".into()]));
        let tid = o.open_team("p1", vec![], "b".into(), "x").unwrap();
        assert_eq!(o.team(&tid).unwrap().members, vec!["a", "b"]);
    }

    #[test]
    fn open_team_falls_back_to_single_bot() {
        let mut o = switchboard_with(&["solo"], project("p1", vec![]));
        let tid = o.open_team("p1", vec![], "solo".into(), "x").unwrap();
        assert_eq!(o.team(&tid).unwrap().members, vec!["solo"]);
    }

    #[test]
    fn open_team_no_bot_available() {
        let mut o = switchboard_with(&["a", "b"], project("p1", vec![]));
        assert_eq!(
            o.open_team("p1", vec![], "a".into(), "x"),
            Err(TeamError::NoBotAvailable)
        );
    }

    #[test]
    fn open_team_rejects_unknown_member_and_non_member_leader() {
        let mut o = switchboard_with(&["a"], project("p1", vec![]));
        assert_eq!(
            o.open_team("p1", vec!["ghost".into()], "ghost".into(), "x"),
            Err(TeamError::BotNotFound("ghost".into()))
        );
        assert_eq!(
            o.open_team("p1", vec!["a".into()], "b".into(), "x"),
            Err(TeamError::LeaderNotMember("b".into()))
        );
    }

    #[test]
    fn open_team_unknown_project() {
        let mut o = Switchboard::new();
        o.add_bot(bot("a")).unwrap();
        assert_eq!(
            o.open_team("nope", vec!["a".into()], "a".into(), "x"),
            Err(TeamError::ProjectNotFound("nope".into()))
        );
    }

    #[test]
    fn post_message_increments_seq() {
        let mut o = switchboard_with(&["a"], project("p1", vec![]));
        let tid = o
            .open_team("p1", vec!["a".into()], "a".into(), "x")
            .unwrap();
        assert_eq!(o.post_message(&tid, Author::User, "hi").unwrap(), 0);
        assert_eq!(
            o.post_message(&tid, Author::Bot("a".into()), "yo").unwrap(),
            1
        );
        assert_eq!(o.team(&tid).unwrap().messages.len(), 2);
    }

    #[test]
    fn delegate_must_be_member() {
        let mut o = switchboard_with(&["a", "b", "c"], project("p1", vec![]));
        let tid = o
            .open_team("p1", vec!["a".into(), "b".into()], "a".into(), "x")
            .unwrap();
        assert!(o.delegate(&tid, "b").is_ok());
        assert_eq!(
            o.delegate(&tid, "c"),
            Err(TeamError::DelegateNotMember("c".into()))
        );
    }

    #[test]
    fn link_session_records_delegation_edge() {
        let mut o = switchboard_with(&["a", "b"], project("p1", vec![]));
        let tid = o
            .open_team("p1", vec!["a".into(), "b".into()], "a".into(), "x")
            .unwrap();
        o.link_session(
            &tid,
            "b",
            "sess-b",
            RoleInTeam::Member,
            Some("sess-a".into()),
        )
        .unwrap();
        let link = &o.team(&tid).unwrap().session_links[0];
        assert_eq!(link.bot_id, "b");
        assert_eq!(link.session_id, "sess-b");
        assert_eq!(link.role, RoleInTeam::Member);
        assert_eq!(link.requested_by_session.as_deref(), Some("sess-a"));
    }

    #[test]
    fn set_leader_requires_membership() {
        let mut o = switchboard_with(&["a", "b", "c"], project("p1", vec![]));
        let tid = o
            .open_team("p1", vec!["a".into(), "b".into()], "a".into(), "x")
            .unwrap();
        o.set_leader(&tid, "b").unwrap();
        assert_eq!(o.team(&tid).unwrap().leader, "b");
        assert_eq!(
            o.set_leader(&tid, "c"),
            Err(TeamError::LeaderNotMember("c".into()))
        );
    }

    #[test]
    fn duplicate_bot_and_project_rejected() {
        let mut o = switchboard_with(&["a"], project("p1", vec![]));
        assert_eq!(o.add_bot(bot("a")), Err(TeamError::BotExists("a".into())));
        assert_eq!(
            o.add_project(project("p1", vec![])),
            Err(TeamError::ProjectExists("p1".into()))
        );
    }

    #[test]
    fn snapshot_roundtrips_json() {
        let mut o = switchboard_with(&["a"], project("p1", vec![]));
        let tid = o
            .open_team("p1", vec!["a".into()], "a".into(), "x")
            .unwrap();
        o.post_message(&tid, Author::User, "hi").unwrap();
        let snap = o.snapshot();
        assert_eq!(snap.teams.len(), 1);
        assert_eq!(snap.projects.len(), 1);
        // JSONL 持久化前置：快照可序列化往返（Step 2 落 JSONL）
        let json = serde_json::to_string(&snap).unwrap();
        let back: SwitchboardSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.teams[0].id, tid);
    }
}
