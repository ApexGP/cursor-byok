//! Owns durable goal state transitions and continuation prompting.
pub mod continuation;
mod handle;

pub use handle::{
    goal_id, Continuation, GoalHandle, STATUS_ACTIVE, STATUS_CLEARED, STATUS_COMPLETE,
    STATUS_PAUSED,
};
