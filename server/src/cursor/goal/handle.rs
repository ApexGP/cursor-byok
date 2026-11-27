//! Owns durable goal state and its active-time accounting.
use std::sync::Arc;

use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use crate::cursor::protocol::proto::agent::v1 as pb;

const MAX_IDLE_CONTINUATIONS: u32 = 3;

pub const STATUS_ACTIVE: i32 = pb::GoalStatus::Active as i32;
pub const STATUS_PAUSED: i32 = pb::GoalStatus::Paused as i32;
pub const STATUS_COMPLETE: i32 = pb::GoalStatus::Complete as i32;
pub const STATUS_CLEARED: i32 = pb::GoalStatus::Cleared as i32;

/// Durable goal state shared by checkpoint building, Tool execution, the
/// continuation action handler, and the Run loop that keeps a goal turn open.
#[derive(Clone, Default)]
pub struct GoalHandle {
    state: Arc<Mutex<Option<pb::GoalState>>>,
    /// When this goal was created. Cursor's composer cache is only trusted for
    /// goal mutations once it has applied a checkpoint newer than this.
    created_at_ms: Arc<Mutex<Option<u64>>>,
}

impl std::fmt::Debug for GoalHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.lock();
        formatter
            .debug_struct("GoalHandle")
            .field(
                "goal_id",
                &state.as_ref().map(|state| state.goal_id.as_str()),
            )
            .field("status", &state.as_ref().map(|state| state.status))
            .finish()
    }
}

pub enum Continuation {
    Continue(pb::GoalState),
    Paused,
    Unavailable,
}

impl GoalHandle {
    pub fn new(state: Option<pb::GoalState>) -> Self {
        Self {
            state: Arc::new(Mutex::new(state)),
            created_at_ms: Arc::new(Mutex::new(None)),
        }
    }

    pub fn snapshot(&self) -> Option<pb::GoalState> {
        self.state.lock().clone()
    }

    /// Creation time of the goal currently held by this handle.
    pub fn created_at_ms(&self) -> Option<u64> {
        *self.created_at_ms.lock()
    }

    /// Applies the goal state Cursor persisted for this conversation. Cursor
    /// owns the objective and the status (the user can edit, pause, resume, or
    /// clear the goal while the turn is open); counters and active time stay
    /// with our continuation accounting. A different `goal_id` replaces the
    /// goal this turn was working on. Returns whether the goal is active.
    pub fn adopt(&self, client: &pb::GoalState, now_ms: u64) -> bool {
        let mut guard = self.state.lock();
        let Some(current) = guard.as_mut() else {
            return false;
        };
        if client.goal_id != current.goal_id {
            let mut replacement = client.clone();
            replacement.idle_continuations_without_tool_calls = 0;
            replacement.continuation_count = 0;
            replacement.active_duration_ms = Some(0);
            replacement.last_accrued_at_ms =
                (replacement.status == STATUS_ACTIVE).then_some(now_ms);
            tracing::info!(
                previous_goal_id = %current.goal_id,
                goal_id = %replacement.goal_id,
                "client replaced the goal; continuing with the new objective"
            );
            *guard = Some(replacement.clone());
            return replacement.status == STATUS_ACTIVE;
        }
        if client.status != current.status {
            if client.status == STATUS_ACTIVE {
                start_accrual(current, now_ms);
            } else {
                stop_accrual(current, now_ms);
            }
            current.status = client.status;
        }
        if !client.objective.is_empty() && client.objective != current.objective {
            current.objective = client.objective.clone();
            current.idle_continuations_without_tool_calls = 0;
            tracing::info!(
                goal_id = %current.goal_id,
                "goal objective changed; continuing with the new objective"
            );
        }
        current.status == STATUS_ACTIVE
    }

    pub fn status_argument(value: &str) -> Option<i32> {
        match value {
            "active" => Some(STATUS_ACTIVE),
            "complete" => Some(STATUS_COMPLETE),
            _ => None,
        }
    }

    pub fn status_name(status: i32) -> &'static str {
        match status {
            STATUS_ACTIVE => "active",
            STATUS_PAUSED => "paused",
            STATUS_COMPLETE => "complete",
            STATUS_CLEARED => "cleared",
            _ => "unknown",
        }
    }

    /// Creates the goal for a CreateGoal Tool call, or returns the existing
    /// goal when the same Tool call already created it.
    pub fn create(
        &self,
        conversation_id: &str,
        agent_session_id: Option<&str>,
        objective: &str,
        tool_call_id: &str,
        now_ms: u64,
    ) -> pb::GoalState {
        let goal_id = goal_id(conversation_id, tool_call_id);
        let agent_session_id = agent_session_id.filter(|session| !session.is_empty());
        let mut state = self.state.lock();
        if let Some(existing) = state.as_ref() {
            if existing.goal_id == goal_id
                && matches_identity(existing, conversation_id, agent_session_id)
            {
                return existing.clone();
            }
        }
        let goal = pb::GoalState {
            conversation_id: conversation_id.into(),
            goal_id,
            objective: objective.into(),
            status: STATUS_ACTIVE,
            idle_continuations_without_tool_calls: 0,
            active_duration_ms: Some(0),
            last_accrued_at_ms: Some(now_ms),
            continuation_count: 0,
            agent_session_id: agent_session_id.map(str::to_string),
            context_tokens_added: None,
        };
        *self.created_at_ms.lock() = Some(now_ms);
        *state = Some(goal.clone());
        goal
    }

    /// Applies an UpdateGoal status. Only `active` and `complete` are accepted.
    pub fn update_status(
        &self,
        status: i32,
        now_ms: u64,
    ) -> std::result::Result<pb::GoalState, String> {
        let mut guard = self.state.lock();
        let current = guard
            .as_ref()
            .ok_or_else(|| "UpdateGoal requires an existing goal".to_string())?;
        let mut next = current.clone();
        next.status = status;
        if status == STATUS_ACTIVE {
            next.idle_continuations_without_tool_calls = 0;
            if current.status != STATUS_ACTIVE {
                start_accrual(&mut next, now_ms);
            }
        } else if current.status == STATUS_ACTIVE {
            stop_accrual(&mut next, now_ms);
        }
        *guard = Some(next.clone());
        Ok(next)
    }

    /// Starts a GoalContinuationAction turn. This increments the idle streak
    /// and accrues active time before the continuation message is built.
    pub fn begin_continuation(
        &self,
        conversation_id: &str,
        agent_session_id: Option<&str>,
        now_ms: u64,
    ) -> Continuation {
        let mut guard = self.state.lock();
        let Some(current) = guard.as_ref() else {
            return Continuation::Unavailable;
        };
        if !is_valid(current, conversation_id, agent_session_id) {
            return Continuation::Unavailable;
        }
        let mut next = current.clone();
        if current.idle_continuations_without_tool_calls >= MAX_IDLE_CONTINUATIONS {
            next.status = STATUS_PAUSED;
            stop_accrual(&mut next, now_ms);
            *guard = Some(next);
            return Continuation::Paused;
        }
        next.idle_continuations_without_tool_calls += 1;
        next.continuation_count += 1;
        accrue(&mut next, now_ms);
        *guard = Some(next.clone());
        Continuation::Continue(next)
    }

    /// Clears the idle streak after the goal makes observable progress.
    pub fn note_tool_call(&self) {
        let mut guard = self.state.lock();
        let Some(state) = guard.as_mut() else {
            return;
        };
        if state.status == STATUS_ACTIVE {
            state.idle_continuations_without_tool_calls = 0;
        }
    }

    /// Marks the goal cleared after Cursor removed it from its composer cache.
    /// The cleared state is persisted with the next checkpoint, and
    /// `begin_continuation` stops returning `Continue` because the goal is no
    /// longer active.
    pub fn clear(&self, now_ms: u64) -> bool {
        let mut guard = self.state.lock();
        let Some(state) = guard.as_mut() else {
            return false;
        };
        if state.status == STATUS_CLEARED {
            return false;
        }
        state.status = STATUS_CLEARED;
        stop_accrual(state, now_ms);
        true
    }
}

pub fn goal_id(conversation_id: &str, tool_call_id: &str) -> String {
    let digest = Sha256::digest(format!("{conversation_id}\0{tool_call_id}"));
    let hex = hex::encode(digest);
    let variant = (u8::from_str_radix(&hex[16..17], 16).unwrap_or_default() & 3) | 8;
    format!(
        "{}-{}-5{}-{}{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        variant,
        &hex[17..20],
        &hex[20..32]
    )
}

fn is_valid(state: &pb::GoalState, conversation_id: &str, agent_session_id: Option<&str>) -> bool {
    state.status == STATUS_ACTIVE
        && is_uuid(&state.goal_id)
        && matches_identity(state, conversation_id, agent_session_id)
}

fn matches_identity(
    state: &pb::GoalState,
    conversation_id: &str,
    agent_session_id: Option<&str>,
) -> bool {
    match state.agent_session_id.as_deref() {
        None => state.conversation_id == conversation_id,
        Some(session) => {
            !session.is_empty()
                && agent_session_id.is_some_and(|current| !current.is_empty() && current == session)
        }
    }
}

fn is_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    bytes.iter().enumerate().all(|(index, byte)| match index {
        8 | 13 | 18 | 23 => *byte == b'-',
        _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(byte),
    })
}

fn accrue(state: &mut pb::GoalState, now_ms: u64) {
    if let Some(last) = state.last_accrued_at_ms {
        let elapsed = now_ms.saturating_sub(last);
        state.active_duration_ms = Some(
            state
                .active_duration_ms
                .unwrap_or_default()
                .saturating_add(elapsed),
        );
    }
    state.last_accrued_at_ms = Some(now_ms);
}

fn start_accrual(state: &mut pb::GoalState, now_ms: u64) {
    state.active_duration_ms = Some(state.active_duration_ms.unwrap_or_default());
    state.last_accrued_at_ms = Some(now_ms);
}

fn stop_accrual(state: &mut pb::GoalState, now_ms: u64) {
    if let Some(last) = state.last_accrued_at_ms {
        let elapsed = now_ms.saturating_sub(last);
        state.active_duration_ms = Some(
            state
                .active_duration_ms
                .unwrap_or_default()
                .saturating_add(elapsed),
        );
    }
    state.last_accrued_at_ms = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONVERSATION: &str = "conversation-1";

    fn created() -> GoalHandle {
        let goal = GoalHandle::default();
        goal.create(CONVERSATION, None, "Ship the feature", "call-1", 1_000);
        goal
    }

    #[test]
    fn goal_ids_are_stable_and_uuid_formatted() {
        let first = goal_id(CONVERSATION, "call-1");
        assert_eq!(first, goal_id(CONVERSATION, "call-1"));
        assert_ne!(first, goal_id(CONVERSATION, "call-2"));
        assert!(is_uuid(&first));
        assert_eq!(&first[14..15], "5");
    }

    #[test]
    fn create_goal_starts_active_time() {
        let state = created().snapshot().unwrap();
        assert_eq!(state.status, STATUS_ACTIVE);
        assert_eq!(state.idle_continuations_without_tool_calls, 0);
        assert_eq!(state.active_duration_ms, Some(0));
        assert_eq!(state.last_accrued_at_ms, Some(1_000));
    }

    #[test]
    fn continuation_increments_counters_and_accrues_time() {
        let goal = created();
        let Continuation::Continue(state) = goal.begin_continuation(CONVERSATION, None, 1_500)
        else {
            panic!("expected continuation");
        };
        assert_eq!(state.continuation_count, 1);
        assert_eq!(state.idle_continuations_without_tool_calls, 1);
        assert_eq!(state.active_duration_ms, Some(500));
        assert_eq!(state.last_accrued_at_ms, Some(1_500));
    }

    #[test]
    fn tool_activity_clears_the_idle_streak() {
        let goal = created();
        goal.begin_continuation(CONVERSATION, None, 1_500);
        goal.note_tool_call();
        assert_eq!(
            goal.snapshot()
                .unwrap()
                .idle_continuations_without_tool_calls,
            0
        );
    }

    #[test]
    fn continuations_are_unbounded_while_tool_calls_arrive() {
        let goal = created();
        for round in 0..100 {
            assert!(matches!(
                goal.begin_continuation(CONVERSATION, None, 1_000 + round),
                Continuation::Continue(_)
            ));
            goal.note_tool_call();
        }
        let state = goal.snapshot().unwrap();
        assert_eq!(state.status, STATUS_ACTIVE);
        assert_eq!(state.continuation_count, 100);
        assert_eq!(state.idle_continuations_without_tool_calls, 0);
    }

    #[test]
    fn clearing_the_goal_stops_continuation() {
        let goal = created();
        goal.begin_continuation(CONVERSATION, None, 1_500);
        assert!(goal.clear(1_600));
        let state = goal.snapshot().unwrap();
        assert_eq!(state.status, STATUS_CLEARED);
        assert_eq!(state.last_accrued_at_ms, None);
        assert!(matches!(
            goal.begin_continuation(CONVERSATION, None, 1_700),
            Continuation::Unavailable
        ));
        assert!(!goal.clear(1_800), "clearing twice is a no-op");
    }

    #[test]
    fn three_idle_continuations_pause_the_goal() {
        let goal = created();
        for count in 1..=3 {
            assert!(matches!(
                goal.begin_continuation(CONVERSATION, None, 1_000 + count as u64),
                Continuation::Continue(_)
            ));
        }
        assert!(matches!(
            goal.begin_continuation(CONVERSATION, None, 2_000),
            Continuation::Paused
        ));
        let state = goal.snapshot().unwrap();
        assert_eq!(state.status, STATUS_PAUSED);
        assert_eq!(state.continuation_count, 3);
        assert_eq!(state.last_accrued_at_ms, None);
    }

    #[test]
    fn create_goal_reuses_a_finished_goal_with_the_same_tool_call() {
        let goal = created();
        goal.update_status(STATUS_COMPLETE, 1_700).unwrap();
        let state = goal.create(CONVERSATION, None, "Different objective", "call-1", 2_000);
        assert_eq!(state.status, STATUS_COMPLETE);
        assert_eq!(state.objective, "Ship the feature");
    }

    #[test]
    fn update_complete_finalizes_active_time() {
        let goal = created();
        let state = goal.update_status(STATUS_COMPLETE, 1_700).unwrap();
        assert_eq!(state.status, STATUS_COMPLETE);
        assert_eq!(state.active_duration_ms, Some(700));
        assert_eq!(state.last_accrued_at_ms, None);
    }

    #[test]
    fn update_active_restarts_a_paused_goal() {
        let goal = created();
        goal.update_status(STATUS_COMPLETE, 1_700).unwrap();
        let state = goal.update_status(STATUS_ACTIVE, 2_000).unwrap();
        assert_eq!(state.status, STATUS_ACTIVE);
        assert_eq!(state.active_duration_ms, Some(700));
        assert_eq!(state.last_accrued_at_ms, Some(2_000));
    }

    #[test]
    fn adopting_a_client_edit_replaces_the_objective_and_clears_the_idle_streak() {
        let goal = created();
        goal.begin_continuation(CONVERSATION, None, 1_500);
        let mut client = goal.snapshot().unwrap();
        client.objective = "Ship the smaller feature".into();
        assert!(goal.adopt(&client, 2_000));
        let state = goal.snapshot().unwrap();
        assert_eq!(state.objective, "Ship the smaller feature");
        assert_eq!(state.idle_continuations_without_tool_calls, 0);
        // Continuation accounting stays ours, not the client's.
        assert_eq!(state.continuation_count, 1);
    }

    #[test]
    fn adopting_a_paused_client_goal_stops_continuation() {
        let goal = created();
        let mut client = goal.snapshot().unwrap();
        client.status = STATUS_PAUSED;
        assert!(!goal.adopt(&client, 1_500));
        let state = goal.snapshot().unwrap();
        assert_eq!(state.status, STATUS_PAUSED);
        assert_eq!(state.last_accrued_at_ms, None);
        assert!(matches!(
            goal.begin_continuation(CONVERSATION, None, 1_600),
            Continuation::Unavailable
        ));
    }

    #[test]
    fn adopting_a_replacement_goal_resets_continuation_counters() {
        let goal = created();
        goal.begin_continuation(CONVERSATION, None, 1_500);
        let mut client = goal.snapshot().unwrap();
        client.goal_id = "replacement-goal".into();
        client.objective = "Ship the new goal".into();
        client.continuation_count = 7;
        client.idle_continuations_without_tool_calls = 2;
        client.active_duration_ms = Some(9_999);
        assert!(goal.adopt(&client, 2_000));
        let state = goal.snapshot().unwrap();
        assert_eq!(state.goal_id, "replacement-goal");
        assert_eq!(state.objective, "Ship the new goal");
        assert_eq!(state.continuation_count, 0);
        assert_eq!(state.idle_continuations_without_tool_calls, 0);
        assert_eq!(state.active_duration_ms, Some(0));
        assert_eq!(state.last_accrued_at_ms, Some(2_000));
    }

    #[test]
    fn creation_time_is_recorded_for_new_goals_only() {
        let goal = GoalHandle::default();
        assert_eq!(goal.created_at_ms(), None);
        goal.create(CONVERSATION, None, "Ship", "call-1", 1_000);
        assert_eq!(goal.created_at_ms(), Some(1_000));
        // Reusing the same CreateGoal call keeps the original creation time.
        goal.create(CONVERSATION, None, "Ship", "call-1", 5_000);
        assert_eq!(goal.created_at_ms(), Some(1_000));
        // A different Tool call is a different goal.
        goal.create(CONVERSATION, None, "Ship", "call-2", 9_000);
        assert_eq!(goal.created_at_ms(), Some(9_000));
    }

    #[test]
    fn continuation_requires_the_same_agent_session() {
        let goal = GoalHandle::default();
        goal.create(CONVERSATION, Some("session-a"), "Ship", "call-1", 1_000);
        assert!(matches!(
            goal.begin_continuation(CONVERSATION, Some("session-b"), 1_100),
            Continuation::Unavailable
        ));
        assert!(matches!(
            goal.begin_continuation(CONVERSATION, Some("session-a"), 1_100),
            Continuation::Continue(_)
        ));
    }
}
