//! §4.5 v2 leader 主动编排（方案 C：SessionRunner 端口 + 异步编排）。
//!
//! 把「只记意图」的 delegate 升级为真编排：leader 把 task 派给各 member → 各自跑一轮 session →
//! 三类终结事件（[`TerminalKind`]）经 [`ParticipantTracker`] 聚合 → 全终结产出 [`TeamOutcome`]。
//!
//! **端口化**：`team-core` 不依赖 session 运行（那归 bot-api）；编排只认 [`SessionRunner`] trait，
//! 实际跑 member session 的 impl 在装配层注入。**编排逻辑可单测**（mock runner 喂终结事件）；
//! 真多 bot 运行验证属 impl 接线（需 bot-api session driver）。

use async_trait::async_trait;

use crate::conduct::{ParticipantTracker, TeamOutcome, TerminalKind};

/// 跑一个 member 的一轮 session 的端口（impl 在 bot-api 装配层）。
#[async_trait]
pub trait SessionRunner: Send + Sync {
    /// 为 `bot_id` 在 `team_id` 上下文跑 `task`，返回该参与者的终结事件类型。
    /// **必须**对失败/取消也返回对应 [`TerminalKind`]（不能 panic/挂起——否则 team 永卡，见 conduct 注）。
    async fn run_member(&self, team_id: &str, bot_id: &str, task: &str) -> TerminalKind;
}

/// §4.5 leader **主动编排**端口：把总 task 拆成 per-member 子任务（impl 在 bot-api，用 leader bot 的 LLM）。
/// `team-core` 不依赖 LLM，故规划经此 trait 解耦——编排逻辑可用 stub planner 单测。
#[async_trait]
pub trait TaskPlanner: Send + Sync {
    /// 给定总任务与成员名单，返回 `member_id → 子任务`。**未包含**的 member 由编排器用原 task 兜底
    /// （planner 漏分/失败不致某 member 空跑）。
    async fn plan(
        &self,
        team_id: &str,
        members: &[String],
        task: &str,
    ) -> std::collections::HashMap<String, String>;
}

/// leader 主动编排器：把 task 并行派给 members，聚合三类终结事件，产出结果。
pub struct TeamOrchestrator<R: SessionRunner> {
    runner: R,
}

impl<R: SessionRunner> TeamOrchestrator<R> {
    pub fn new(runner: R) -> Self {
        Self { runner }
    }

    /// 编排一个 team：**并行**派发所有 member（`join_all` 并发跑一轮）→ 终结事件喂 tracker →
    /// 全终结返回 [`TeamOutcome`]。tracker 对终结顺序无关，故并发安全（结果聚合在 join 后串行做）。
    pub async fn run_team(&self, team_id: &str, members: &[String], task: &str) -> TeamOutcome {
        let mut tracker = ParticipantTracker::new();
        tracker.start(team_id, members.iter().cloned());
        let mut outcome = TeamOutcome {
            team_id: team_id.to_string(),
            done: 0,
            failed: 0,
            cancelled: 0,
        };
        // 并行：所有 member 并发跑（&self.runner 并发 &self 调用安全）。
        let futs = members.iter().map(|m| {
            let m = m.clone();
            async move {
                let kind = self.runner.run_member(team_id, &m, task).await;
                (m, kind)
            }
        });
        for (m, kind) in futures::future::join_all(futs).await {
            if let Some(o) = tracker.on_terminal(team_id, &m, kind) {
                outcome = o; // 最后一个终结产出最终聚合
            }
        }
        outcome
    }

    /// §4.5 **leader 主动编排**：先让 `planner` 把总 task 拆成 per-member 子任务，各 member 跑**各自子任务**
    /// （未分配则跑原 task 兜底），再聚合。相比 [`Self::run_team`] 的「同一 task 广播」，这是真正的分工。
    /// 规划顺序无关聚合（并发跑、终结聚合在 join 后串行），故 tracker 安全。
    pub async fn run_team_planned<P: TaskPlanner>(
        &self,
        team_id: &str,
        members: &[String],
        task: &str,
        planner: &P,
    ) -> TeamOutcome {
        let plan = planner.plan(team_id, members, task).await;
        let mut tracker = ParticipantTracker::new();
        tracker.start(team_id, members.iter().cloned());
        let mut outcome = TeamOutcome {
            team_id: team_id.to_string(),
            done: 0,
            failed: 0,
            cancelled: 0,
        };
        let futs = members.iter().map(|m| {
            // 子任务兜底：planner 未分配该 member → 用原 task（不让其空跑）。
            let subtask = plan.get(m).cloned().unwrap_or_else(|| task.to_string());
            let m = m.clone();
            async move {
                let kind = self.runner.run_member(team_id, &m, &subtask).await;
                (m, kind)
            }
        });
        for (m, kind) in futures::future::join_all(futs).await {
            if let Some(o) = tracker.on_terminal(team_id, &m, kind) {
                outcome = o;
            }
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// mock：按预设脚本返回终结事件（含失败/取消，验证不卡死）。
    struct ScriptRunner {
        script: Mutex<std::collections::HashMap<String, TerminalKind>>,
    }
    #[async_trait]
    impl SessionRunner for ScriptRunner {
        async fn run_member(&self, _team: &str, bot_id: &str, _task: &str) -> TerminalKind {
            self.script
                .lock()
                .unwrap()
                .get(bot_id)
                .copied()
                .unwrap_or(TerminalKind::Done)
        }
    }

    #[tokio::test]
    async fn orchestrates_mixed_terminals_to_completion() {
        let mut script = std::collections::HashMap::new();
        script.insert("a".to_string(), TerminalKind::Done);
        script.insert("b".to_string(), TerminalKind::Failed);
        script.insert("c".to_string(), TerminalKind::Cancelled);
        let orch = TeamOrchestrator::new(ScriptRunner {
            script: Mutex::new(script),
        });
        let members = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let out = orch.run_team("team1", &members, "do work").await;
        // 混合终结也完成（不卡死），计数正确。
        assert_eq!(out.total(), 3);
        assert_eq!(out.done, 1);
        assert_eq!(out.failed, 1);
        assert_eq!(out.cancelled, 1);
    }

    #[tokio::test]
    async fn all_done_outcome() {
        let orch = TeamOrchestrator::new(ScriptRunner {
            script: Mutex::new(Default::default()),
        });
        let members = vec!["x".to_string(), "y".to_string()];
        let out = orch.run_team("t", &members, "task").await;
        assert_eq!(out.done, 2);
        assert_eq!(out.failed, 0);
    }

    // §4.5 leader 主动编排：planner 分子任务，各 member 跑各自子任务；未分配的兜底原 task。
    #[tokio::test]
    async fn planned_orchestration_dispatches_per_member_subtasks() {
        use std::collections::HashMap;

        // 记录每个 member 实际收到的 task（验证分工生效）。
        struct RecordingRunner {
            seen: Mutex<HashMap<String, String>>,
        }
        #[async_trait]
        impl SessionRunner for RecordingRunner {
            async fn run_member(&self, _team: &str, bot_id: &str, task: &str) -> TerminalKind {
                self.seen
                    .lock()
                    .unwrap()
                    .insert(bot_id.to_string(), task.to_string());
                TerminalKind::Done
            }
        }
        // planner 给 a/b 分子任务，c 不分（应兜底原 task）。
        struct StubPlanner;
        #[async_trait]
        impl TaskPlanner for StubPlanner {
            async fn plan(&self, _t: &str, _m: &[String], _task: &str) -> HashMap<String, String> {
                HashMap::from([
                    ("a".to_string(), "research".to_string()),
                    ("b".to_string(), "write".to_string()),
                ])
            }
        }

        let runner = RecordingRunner {
            seen: Mutex::new(HashMap::new()),
        };
        let orch = TeamOrchestrator::new(runner);
        let members = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let out = orch
            .run_team_planned("t", &members, "BIG TASK", &StubPlanner)
            .await;
        assert_eq!(out.done, 3, "全员完成");

        let seen = orch.runner.seen.lock().unwrap();
        assert_eq!(seen.get("a").unwrap(), "research", "a 应跑分配的子任务");
        assert_eq!(seen.get("b").unwrap(), "write");
        assert_eq!(seen.get("c").unwrap(), "BIG TASK", "未分配的 c 兜底原 task");
    }
}
