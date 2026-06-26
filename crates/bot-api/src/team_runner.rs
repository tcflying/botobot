//! §4.5 v2：[`team_core::SessionRunner`] 的 bot-api 实现——真跑 member session 一轮，
//! 把终结映射为 [`TerminalKind`]（接 `team_core::TeamOrchestrator`，让 leader 编排端到端）。
//!
//! 流程：`team_delegate`（开/取 member session + 记委派边）→ `subscribe` → `submit(UserMessage)`
//! → 等终结事件（`TurnComplete`→Done / `CancelComplete`→Cancelled / `AgentEvent::Error`→Failed）。
//! **必映射三类终结**（不挂起——见 `team_core::conduct` 防卡死纪律）。

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use base_types::{AgentEvent, Llm, LlmEvent, LlmOpts, Message};
use futures::StreamExt;
use team_core::{Author, SessionRunner, TaskPlanner, TeamOrchestrator, TeamOutcome, TerminalKind};

use crate::Hub;
use crate::protocol::{EventMsg, Op, Submission};

/// §4.5 v2：leader 主动编排端到端——派发各 member 跑一轮 → 聚合三类终结 → leader **自动把结果
/// 汇总贴回 team transcript**。返回 [`TeamOutcome`]。`leader` 作为汇总消息作者。
/// 真多 bot 压测属运行验证；编排+汇总逻辑用 test hub（单/少 bot）端到端验证。
pub async fn conduct_team(
    hub: &Hub,
    team_id: &str,
    members: &[String],
    leader: &str,
    task: &str,
) -> TeamOutcome {
    let orch = TeamOrchestrator::new(HubSessionRunner::new(hub.clone()));
    let out = orch.run_team(team_id, members, task).await;
    let summary = format!(
        "[团队编排完成] 成员 {} · 完成 {} · 失败 {} · 取消 {}",
        out.total(),
        out.done,
        out.failed,
        out.cancelled
    );
    let _ = hub.team_post(team_id, Author::Bot(leader.to_string()), summary);
    out
}

/// §4.5 **leader 主动编排**端到端：leader 的 LLM 先把 task 拆成 per-member 子任务（[`LlmTaskPlanner`]），
/// 各 member 跑**各自子任务**，再聚合 + leader 汇总。相比 [`conduct_team`] 的「同 task 广播」是真正分工。
/// `llm` = leader bot 的 LLM 句柄（装配层注入；多 bot 运行验证待真环境，规划+分发逻辑用 test hub 验证）。
pub async fn conduct_team_planned(
    hub: &Hub,
    team_id: &str,
    members: &[String],
    leader: &str,
    task: &str,
    llm: Arc<dyn Llm>,
) -> TeamOutcome {
    let orch = TeamOrchestrator::new(HubSessionRunner::new(hub.clone()));
    let planner = LlmTaskPlanner::new(llm);
    let out = orch
        .run_team_planned(team_id, members, task, &planner)
        .await;
    let summary = format!(
        "[团队编排完成·分工] 成员 {} · 完成 {} · 失败 {} · 取消 {}",
        out.total(),
        out.done,
        out.failed,
        out.cancelled
    );
    let _ = hub.team_post(team_id, Author::Bot(leader.to_string()), summary);
    out
}

/// §4.5 leader 规划器：用 leader 的 LLM 把总 task 拆成 per-member 子任务。
/// prompt 让 LLM 逐行回 `<member_id>: <子任务>`；只接受在册 member（防分给不存在的 bot）。
pub struct LlmTaskPlanner {
    llm: Arc<dyn Llm>,
}

impl LlmTaskPlanner {
    pub fn new(llm: Arc<dyn Llm>) -> Self {
        Self { llm }
    }
}

#[async_trait]
impl TaskPlanner for LlmTaskPlanner {
    async fn plan(
        &self,
        _team_id: &str,
        members: &[String],
        task: &str,
    ) -> HashMap<String, String> {
        let roster = members.join(", ");
        let sys = format!(
            "You are the team leader allocating work. Members (by id): {roster}.\n\
             Split the overall task into one focused subtask per member. Reply with ONE line per \
             member as `<member_id>: <subtask>`, nothing else. Use the exact member ids."
        );
        let msgs = vec![
            Message::system(sys),
            Message::user(format!("Overall task: {task}")),
        ];
        let Some(text) = collect_decision_text(self.llm.as_ref(), &msgs).await else {
            return HashMap::new(); // LLM 失败 → 空规划，编排器全员兜底原 task
        };
        parse_assignments(&text, members)
    }
}

/// 解析 `<member_id>: <subtask>` 行，只保留在册 member（去空白；同 member 后行覆盖）。
fn parse_assignments(text: &str, members: &[String]) -> HashMap<String, String> {
    let valid: std::collections::HashSet<&str> = members.iter().map(|s| s.as_str()).collect();
    let mut out = HashMap::new();
    for line in text.lines() {
        let Some((id, sub)) = line.split_once(':') else {
            continue;
        };
        let (id, sub) = (id.trim(), sub.trim());
        if valid.contains(id) && !sub.is_empty() {
            out.insert(id.to_string(), sub.to_string());
        }
    }
    out
}

async fn collect_decision_text(llm: &dyn Llm, msgs: &[Message]) -> Option<String> {
    let mut stream = llm.infer(msgs, &[], &LlmOpts::default()).await.ok()?;
    let mut last = None;
    while let Some(ev) = stream.next().await {
        if let Ok(LlmEvent::Done(d)) = ev {
            last = Some(d.text);
        }
    }
    last
}

/// 用 [`Hub`] 真跑 member session 的 SessionRunner。
pub struct HubSessionRunner {
    hub: Hub,
}

impl HubSessionRunner {
    pub fn new(hub: Hub) -> Self {
        Self { hub }
    }
}

#[async_trait]
impl SessionRunner for HubSessionRunner {
    async fn run_member(&self, team_id: &str, bot_id: &str, task: &str) -> TerminalKind {
        // 开/取 member session（失败=该参与者直接 Failed，不阻塞 team）。
        let sid = match self.hub.team_delegate(team_id, bot_id) {
            Ok(s) => s,
            Err(_) => return TerminalKind::Failed,
        };
        let mut rx = match self.hub.subscribe(&sid) {
            Some(r) => r,
            None => return TerminalKind::Failed,
        };
        let sub = Submission::new(
            sid.clone(),
            Op::UserMessage {
                text: task.to_string(),
                images: Vec::new(),
                thinking: None,
                web_search: None,
                code_execution: None,
                force_recall: false,
            },
        );
        if self.hub.submit(sub).await.is_err() {
            return TerminalKind::Failed;
        }
        // 等终结：TurnComplete→Done / CancelComplete→Cancelled / Agent Error→Failed。
        loop {
            match rx.recv().await {
                Ok(ev) => match ev.msg {
                    EventMsg::TurnComplete => return TerminalKind::Done,
                    EventMsg::CancelComplete => return TerminalKind::Cancelled,
                    EventMsg::Agent(AgentEvent::Error { .. }) => return TerminalKind::Failed,
                    _ => continue,
                },
                // 通道关闭/滞后 → 视为失败（不挂起）。
                Err(_) => return TerminalKind::Failed,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Hub;
    use base_types::{Decision, LlmEvent, LlmOpts, LlmResult, Message, ToolSpec};
    use std::sync::Arc;
    use team_core::TeamOrchestrator;

    struct OneShotLlm;
    #[async_trait]
    impl base_types::Llm for OneShotLlm {
        async fn infer(
            &self,
            _m: &[Message],
            _t: &[ToolSpec],
            _o: &LlmOpts,
        ) -> LlmResult<base_types::LlmStream> {
            let d = Decision {
                text: "done".into(),
                finish_reason: Some("stop".into()),
                ..Default::default()
            };
            let evs: Vec<LlmResult<LlmEvent>> = vec![Ok(LlmEvent::Done(d))];
            Ok(Box::pin(futures::stream::iter(evs)))
        }
    }

    #[tokio::test]
    async fn orchestrates_real_member_turns_to_done() {
        let dir = std::env::temp_dir().join(format!("botobot-trun-{}", uuid::Uuid::new_v4()));
        let wd = std::env::temp_dir().join(format!("trun-wd-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&wd).unwrap();
        let hub = Hub::with_config(
            agent_loop::Agent::builder()
                .llm(Arc::new(OneShotLlm))
                .build(),
            crate::HubConfig {
                store_root: Some(dir.clone()),
                ..Default::default()
            },
        );
        let a = hub.create_bot("leader", &wd).unwrap();
        let b = hub.create_bot("member", &wd).unwrap();
        hub.create_team_project("p1", "P", &wd, vec![]).unwrap();
        let tid = hub
            .open_team("p1", vec![a.id.clone(), b.id.clone()], a.id.clone(), "task")
            .unwrap();

        // 经 TeamOrchestrator + HubSessionRunner 真跑 member b 一轮 → Done。
        let orch = TeamOrchestrator::new(HubSessionRunner::new(hub.clone()));
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            orch.run_team(&tid, std::slice::from_ref(&b.id), "do it"),
        )
        .await
        .expect("编排不应超时");
        assert_eq!(out.done, 1, "member 一轮应 Done: {out:?}");
        assert_eq!(out.total(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // §4.5 leader 规划解析：只接受在册 member 的 `id: subtask` 行，去空白，乱行忽略。
    #[test]
    fn parse_assignments_keeps_valid_members_only() {
        let members = vec!["a".to_string(), "b".to_string()];
        let text = "a: research the topic\nb:  write the draft \nzzz: not a member\ngarbage line\n";
        let plan = super::parse_assignments(text, &members);
        assert_eq!(plan.get("a").unwrap(), "research the topic");
        assert_eq!(plan.get("b").unwrap(), "write the draft", "去首尾空白");
        assert!(!plan.contains_key("zzz"), "非在册 member 忽略");
        assert_eq!(plan.len(), 2);
    }

    // §4.5 leader 主动编排端到端：planner LLM 拆任务 → member 跑 → Done + leader「分工」汇总。
    #[tokio::test]
    async fn conduct_team_planned_runs_and_posts_summary() {
        // 规划用 LLM（返回 assignment 文本）；member turn 用 Hub 的 OneShotLlm。
        struct TextLlm(String);
        #[async_trait]
        impl base_types::Llm for TextLlm {
            async fn infer(
                &self,
                _m: &[Message],
                _t: &[ToolSpec],
                _o: &LlmOpts,
            ) -> LlmResult<base_types::LlmStream> {
                let d = Decision {
                    text: self.0.clone(),
                    finish_reason: Some("stop".into()),
                    ..Default::default()
                };
                Ok(Box::pin(futures::stream::iter(vec![Ok(LlmEvent::Done(d))])))
            }
        }

        let dir = std::env::temp_dir().join(format!("botobot-tplan-{}", uuid::Uuid::new_v4()));
        let wd = std::env::temp_dir().join(format!("tplan-wd-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&wd).unwrap();
        let hub = Hub::with_config(
            agent_loop::Agent::builder()
                .llm(Arc::new(OneShotLlm))
                .build(),
            crate::HubConfig {
                store_root: Some(dir.clone()),
                ..Default::default()
            },
        );
        let a = hub.create_bot("leader", &wd).unwrap();
        let b = hub.create_bot("member", &wd).unwrap();
        hub.create_team_project("p1", "P", &wd, vec![]).unwrap();
        let tid = hub
            .open_team("p1", vec![a.id.clone(), b.id.clone()], a.id.clone(), "task")
            .unwrap();

        // planner 给 b 分一个子任务（用真 id）。
        let planner_llm: Arc<dyn base_types::Llm> =
            Arc::new(TextLlm(format!("{}: research", b.id)));
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            super::conduct_team_planned(
                &hub,
                &tid,
                std::slice::from_ref(&b.id),
                &a.id,
                "do it",
                planner_llm,
            ),
        )
        .await
        .expect("不超时");
        assert_eq!(out.done, 1, "member 跑完应 Done: {out:?}");

        let snap = hub.teams_snapshot();
        let team = snap.teams.iter().find(|t| t.id == tid).unwrap();
        assert!(
            team.messages
                .iter()
                .any(|m| m.content.contains("分工") && m.author == Author::Bot(a.id.clone())),
            "leader 应贴分工汇总: {:?}",
            team.messages.iter().map(|m| &m.content).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn conduct_team_posts_leader_summary() {
        let dir = std::env::temp_dir().join(format!("botobot-tsum-{}", uuid::Uuid::new_v4()));
        let wd = std::env::temp_dir().join(format!("tsum-wd-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&wd).unwrap();
        let hub = Hub::with_config(
            agent_loop::Agent::builder()
                .llm(Arc::new(OneShotLlm))
                .build(),
            crate::HubConfig {
                store_root: Some(dir.clone()),
                ..Default::default()
            },
        );
        let a = hub.create_bot("leader", &wd).unwrap();
        let b = hub.create_bot("member", &wd).unwrap();
        hub.create_team_project("p1", "P", &wd, vec![]).unwrap();
        let tid = hub
            .open_team("p1", vec![a.id.clone(), b.id.clone()], a.id.clone(), "task")
            .unwrap();

        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            super::conduct_team(&hub, &tid, std::slice::from_ref(&b.id), &a.id, "do it"),
        )
        .await
        .expect("不超时");
        assert_eq!(out.done, 1);
        // leader 汇总已贴回 team transcript。
        let snap = hub.teams_snapshot();
        let team = snap.teams.iter().find(|t| t.id == tid).unwrap();
        assert!(
            team.messages.iter().any(
                |m| m.content.contains("团队编排完成") && m.author == Author::Bot(a.id.clone())
            ),
            "leader 应贴汇总消息: {:?}",
            team.messages.iter().map(|m| &m.content).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
