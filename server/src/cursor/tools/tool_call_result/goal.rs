//! Converts CreateGoal and UpdateGoal Tool calls into goal state changes.
use serde_json::Value;

use crate::{
    cursor::{
        goal::GoalHandle,
        protocol::proto::agent::v1 as pb,
        tools::{codec, runtime::ExecContext},
    },
    model::{ToolCall, ToolResult},
    Error, Result,
};

use super::{now_ms, ToolCompletion};

pub(crate) fn create(call: &ToolCall, context: &ExecContext) -> Result<ToolCompletion> {
    let objective = call
        .arguments
        .get("objective")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|objective| !objective.is_empty());
    let Some(objective) = objective else {
        return Ok(failure(
            call,
            "CreateGoal objective must be a non-empty string",
        )?);
    };
    let state = context.goal.create(
        &context.conversation_id,
        context.agent_session_id.as_deref(),
        objective,
        &call.call_id,
        now_ms(),
    );
    let mut rendered = codec::render_tool_call(call, false)?;
    let Some(pb::tool_call::Tool::CreateGoalToolCall(tool)) = rendered.tool.as_mut() else {
        return Err(Error::Protocol(
            "CreateGoal has no Cursor representation".into(),
        ));
    };
    tool.result = Some(pb::CreateGoalResult {
        result: Some(pb::create_goal_result::Result::Success(
            pb::CreateGoalSuccess {},
        )),
    });
    let tool = rendered
        .tool
        .ok_or_else(|| Error::Protocol("CreateGoal has no Cursor representation".into()))?;
    Ok(ToolCompletion::new(
        call,
        now_ms(),
        ToolResult {
            call_id: call.call_id.clone(),
            content: serde_json::json!({ "success": { "goal_id": state.goal_id } }).to_string(),
            is_error: false,
            image: None,
        },
        tool,
    ))
}

pub(crate) fn update(call: &ToolCall, context: &ExecContext) -> Result<ToolCompletion> {
    let status = call
        .arguments
        .get("status")
        .and_then(Value::as_str)
        .and_then(GoalHandle::status_argument);
    let Some(status) = status else {
        return Ok(failure(
            call,
            "UpdateGoal status must be \"active\" or \"complete\"",
        )?);
    };
    let state = match context.goal.update_status(status, now_ms()) {
        Ok(state) => state,
        Err(error) => return Ok(failure(call, &error)?),
    };
    let mut rendered = codec::render_tool_call(call, false)?;
    let Some(pb::tool_call::Tool::UpdateGoalToolCall(tool)) = rendered.tool.as_mut() else {
        return Err(Error::Protocol(
            "UpdateGoal has no Cursor representation".into(),
        ));
    };
    tool.result = Some(pb::UpdateGoalResult {
        result: Some(pb::update_goal_result::Result::Success(
            pb::UpdateGoalSuccess {
                status: state.status,
            },
        )),
    });
    let tool = rendered
        .tool
        .ok_or_else(|| Error::Protocol("UpdateGoal has no Cursor representation".into()))?;
    Ok(ToolCompletion::new(
        call,
        now_ms(),
        ToolResult {
            call_id: call.call_id.clone(),
            content: serde_json::json!({ "success": { "status": GoalHandle::status_name(state.status) } })
                .to_string(),
            is_error: false,
            image: None,
        },
        tool,
    ))
}

fn failure(call: &ToolCall, error: &str) -> Result<ToolCompletion> {
    let mut rendered = codec::render_tool_call(call, false)?;
    match rendered.tool.as_mut() {
        Some(pb::tool_call::Tool::CreateGoalToolCall(tool)) => {
            tool.result = Some(pb::CreateGoalResult {
                result: Some(pb::create_goal_result::Result::Error(pb::GoalError {
                    error: error.into(),
                })),
            });
        }
        Some(pb::tool_call::Tool::UpdateGoalToolCall(tool)) => {
            tool.result = Some(pb::UpdateGoalResult {
                result: Some(pb::update_goal_result::Result::Error(pb::GoalError {
                    error: error.into(),
                })),
            });
        }
        _ => {
            return Err(Error::Protocol(format!(
                "{} has no Cursor goal representation",
                call.name
            )))
        }
    }
    let tool = rendered
        .tool
        .ok_or_else(|| Error::Protocol(format!("{} has no Cursor representation", call.name)))?;
    Ok(ToolCompletion::new(
        call,
        now_ms(),
        ToolResult {
            call_id: call.call_id.clone(),
            content: serde_json::json!({ "error": error }).to_string(),
            is_error: true,
            image: None,
        },
        tool,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cursor::goal::{STATUS_ACTIVE, STATUS_COMPLETE};
    use serde_json::json;

    fn context() -> ExecContext {
        ExecContext {
            conversation_id: "conversation-1".into(),
            goal: GoalHandle::default(),
            ..Default::default()
        }
    }

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            index: 0,
            call_id: "call-1".into(),
            model_call_id: "model-1".into(),
            name: name.into(),
            arguments_text: String::new(),
            arguments,
            argument_error: None,
        }
    }

    #[test]
    fn create_goal_updates_the_shared_goal_state() {
        let context = context();
        let completion = create(
            &call("CreateGoal", json!({ "objective": "Ship the feature" })),
            &context,
        )
        .unwrap();
        assert!(!completion.result().is_error);
        let state = context.goal.snapshot().unwrap();
        assert_eq!(state.objective, "Ship the feature");
        assert_eq!(state.status, STATUS_ACTIVE);
    }

    #[test]
    fn update_goal_requires_an_existing_goal() {
        let context = context();
        let completion = update(
            &call("UpdateGoal", json!({ "status": "complete" })),
            &context,
        )
        .unwrap();
        assert!(completion.result().is_error);
        assert!(context.goal.snapshot().is_none());
    }

    #[test]
    fn update_goal_marks_the_goal_complete() {
        let context = context();
        create(
            &call("CreateGoal", json!({ "objective": "Ship" })),
            &context,
        )
        .unwrap();
        let completion = update(
            &call("UpdateGoal", json!({ "status": "complete" })),
            &context,
        )
        .unwrap();
        assert!(!completion.result().is_error);
        assert_eq!(context.goal.snapshot().unwrap().status, STATUS_COMPLETE);
    }
}
