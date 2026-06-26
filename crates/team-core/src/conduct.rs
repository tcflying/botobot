//! §4.5 v2 编排安全核心：参与者完成追踪。
//!
//! **必记踩坑（照搬前身 mission-tech `MissionConductor` 教训）**：后台聚合器**必须等三类终结
//! 事件之一**（`Done`=TurnDone / `Failed`=SubmissionFailed / `Cancelled`=SubmissionCancelled），
//! **不能只等 Done**——否则失败/取消的 participant 永不递减计数，team 永卡 `Running/Active`。
//!
//! 本模块是 v2 编排的**可单测纯逻辑核心**：登记参与者 → 每个参与者任一终结事件递减 → 全部终结即
//! 产出 [`TeamOutcome`]。实际 session 运行 / 结果回写 transcript 的接线（同步 SessionRunner 端口
//! 或异步后台编排器，用户 2026-06-22 拍板方案 C）属后续步骤。

use std::collections::{HashMap, HashSet};

/// 一个 participant 的终结事件类型（三类之一——缺一会卡死，见模块注）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalKind {
    /// 正常跑完一轮（TurnDone）。
    Done,
    /// 提交失败（SubmissionFailed）。
    Failed,
    /// 被取消（SubmissionCancelled）。
    Cancelled,
}

/// 一个 team 全部参与者终结后的聚合结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamOutcome {
    pub team_id: String,
    pub done: usize,
    pub failed: usize,
    pub cancelled: usize,
}

impl TeamOutcome {
    pub fn total(&self) -> usize {
        self.done + self.failed + self.cancelled
    }
}

/// 参与者完成追踪器：per-team 记「仍未终结的参与者集合」+ 累计三类终结计数。
/// 任一参与者任一终结事件递减；集合空 → 该 team 完成，返回 [`TeamOutcome`]。
#[derive(Debug, Default)]
pub struct ParticipantTracker {
    pending: HashMap<String, HashSet<String>>,
    counts: HashMap<String, (usize, usize, usize)>, // team_id → (done, failed, cancelled)
}

impl ParticipantTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一个 team 及其参与者集合（覆盖式；空集合等于无需等待）。
    pub fn start(
        &mut self,
        team_id: impl Into<String>,
        participants: impl IntoIterator<Item = String>,
    ) {
        let id = team_id.into();
        self.pending
            .insert(id.clone(), participants.into_iter().collect());
        self.counts.entry(id).or_insert((0, 0, 0));
    }

    /// 某 team 是否仍在等待（有未终结参与者）。
    pub fn is_active(&self, team_id: &str) -> bool {
        self.pending.get(team_id).is_some_and(|s| !s.is_empty())
    }

    /// 仍未终结的参与者数。
    pub fn pending_count(&self, team_id: &str) -> usize {
        self.pending.get(team_id).map_or(0, |s| s.len())
    }

    /// 记录一个参与者的终结事件（**三类都递减**——这是防卡死的关键）。
    /// 幂等：未知 team / 未知或已终结的 participant 不改变状态、返回 `None`。
    /// 当该 team 最后一个参与者终结时，返回 `Some(TeamOutcome)`（调用方据此把 team 置 Done）。
    pub fn on_terminal(
        &mut self,
        team_id: &str,
        participant: &str,
        kind: TerminalKind,
    ) -> Option<TeamOutcome> {
        let set = self.pending.get_mut(team_id)?;
        if !set.remove(participant) {
            return None; // 未知/重复终结 → 不重复计数
        }
        let c = self.counts.entry(team_id.to_string()).or_insert((0, 0, 0));
        match kind {
            TerminalKind::Done => c.0 += 1,
            TerminalKind::Failed => c.1 += 1,
            TerminalKind::Cancelled => c.2 += 1,
        }
        if set.is_empty() {
            let (done, failed, cancelled) = *c;
            self.pending.remove(team_id);
            self.counts.remove(team_id);
            Some(TeamOutcome {
                team_id: team_id.to_string(),
                done,
                failed,
                cancelled,
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completes_only_after_all_participants_terminal() {
        let mut t = ParticipantTracker::new();
        t.start("team1", ["a".into(), "b".into(), "c".into()]);
        assert!(t.is_active("team1"));
        assert_eq!(t.pending_count("team1"), 3);
        assert!(t.on_terminal("team1", "a", TerminalKind::Done).is_none());
        assert!(t.on_terminal("team1", "b", TerminalKind::Done).is_none());
        assert!(t.is_active("team1"));
        let out = t
            .on_terminal("team1", "c", TerminalKind::Done)
            .expect("最后一个终结应产出 outcome");
        assert_eq!(out.done, 3);
        assert_eq!(out.total(), 3);
        assert!(!t.is_active("team1"));
    }

    // 核心教训：失败/取消的 participant 也必须递减，否则永卡。
    #[test]
    fn failed_and_cancelled_also_decrement_so_team_never_hangs() {
        let mut t = ParticipantTracker::new();
        t.start("team2", ["a".into(), "b".into(), "c".into()]);
        assert!(t.on_terminal("team2", "a", TerminalKind::Failed).is_none());
        assert!(
            t.on_terminal("team2", "b", TerminalKind::Cancelled)
                .is_none()
        );
        // 只剩 c；若只等 Done，混合终结会卡死——这里 c 一 Done 即完成。
        let out = t
            .on_terminal("team2", "c", TerminalKind::Done)
            .expect("混合终结也应完成");
        assert_eq!(out.done, 1);
        assert_eq!(out.failed, 1);
        assert_eq!(out.cancelled, 1);
        assert_eq!(out.total(), 3);
    }

    #[test]
    fn duplicate_and_unknown_terminals_are_idempotent() {
        let mut t = ParticipantTracker::new();
        t.start("team3", ["a".into(), "b".into()]);
        assert!(t.on_terminal("team3", "a", TerminalKind::Done).is_none());
        // 重复终结 a → 不递减、不计数。
        assert!(t.on_terminal("team3", "a", TerminalKind::Failed).is_none());
        assert_eq!(t.pending_count("team3"), 1);
        // 未知 team / participant → None。
        assert!(t.on_terminal("nope", "x", TerminalKind::Done).is_none());
        assert!(t.on_terminal("team3", "zzz", TerminalKind::Done).is_none());
        // b 终结 → 完成，done 计数只含 a + b（重复 a 的 Failed 未计）。
        let out = t.on_terminal("team3", "b", TerminalKind::Done).unwrap();
        assert_eq!(out.done, 2);
        assert_eq!(out.failed, 0);
    }

    #[test]
    fn empty_participants_team_is_not_active() {
        let mut t = ParticipantTracker::new();
        t.start("team4", []);
        assert!(!t.is_active("team4"));
        assert_eq!(t.pending_count("team4"), 0);
    }
}
