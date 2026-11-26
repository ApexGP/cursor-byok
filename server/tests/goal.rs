//! Verifies CreateGoal/UpdateGoal Tools and goal continuation turns.
#[path = "support/fake_provider.rs"]
mod fake_provider;
#[path = "support/fixtures.rs"]
mod fixtures;

use std::sync::Arc;

use cursor_server::{
    cursor::{
        goal::{goal_id, STATUS_ACTIVE, STATUS_PAUSED},
        prompting::{PromptAssets, PromptCompiler},
        protocol::{connect, proto::agent::v1 as pb},
        TransportCommand, TransportRegistry,
    },
    model::{ContentPart, ProjectedContent},
    provider::{FinishReason, ModelEvent},
};
use prost::Message;

fn assets() -> PromptAssets {
    PromptAssets::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("prompt/cursor")
            .as_path(),
    )
    .unwrap()
}

fn user_request(conversation_id: &str) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::RunRequest(
            pb::AgentRunRequest {
                action: Some(pb::ConversationAction {
                    action: Some(pb::conversation_action::Action::UserMessageAction(
                        pb::UserMessageAction {
                            user_message: Some(pb::UserMessage {
                                text: "Create a goal to ship the feature".into(),
                                message_id: "user-message".into(),
                                mode: pb::AgentMode::Agent as i32,
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                    )),
                    ..Default::default()
                }),
                conversation_id: Some(conversation_id.into()),
                requested_model: Some(pb::RequestedModel {
                    model_id: "test-model".into(),
                    ..Default::default()
                }),
                run_id: Some("goal-run".into()),
                ..Default::default()
            },
        )),
    }
}

fn goal_state(conversation_id: &str, idle: u32, continuations: u32) -> pb::GoalState {
    pb::GoalState {
        conversation_id: conversation_id.into(),
        goal_id: goal_id(conversation_id, "goal-call"),
        objective: "Ship the feature".into(),
        status: STATUS_ACTIVE,
        idle_continuations_without_tool_calls: idle,
        active_duration_ms: Some(0),
        last_accrued_at_ms: Some(1_000),
        continuation_count: continuations,
        agent_session_id: None,
        context_tokens_added: None,
    }
}

fn user_request_with_goal(conversation_id: &str, goal: pb::GoalState) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::RunRequest(
            pb::AgentRunRequest {
                action: Some(pb::ConversationAction {
                    action: Some(pb::conversation_action::Action::UserMessageAction(
                        pb::UserMessageAction {
                            user_message: Some(pb::UserMessage {
                                text: "Keep going".into(),
                                message_id: "user-message".into(),
                                mode: pb::AgentMode::Agent as i32,
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                    )),
                    ..Default::default()
                }),
                conversation_state: Some(pb::ConversationStateStructure {
                    mode: Some(pb::AgentMode::Agent as i32),
                    goal_state: Some(goal),
                    ..Default::default()
                }),
                conversation_id: Some(conversation_id.into()),
                requested_model: Some(pb::RequestedModel {
                    model_id: "test-model".into(),
                    ..Default::default()
                }),
                run_id: Some("goal-run".into()),
                ..Default::default()
            },
        )),
    }
}

fn text_turn(text: &str) -> Vec<ModelEvent> {
    vec![
        ModelEvent::Start {
            model_call_id: "model-1".into(),
        },
        ModelEvent::TextStart,
        ModelEvent::TextDelta(text.into()),
        ModelEvent::TextEnd,
        ModelEvent::Done(FinishReason::Stop),
    ]
}

fn continuation_request(conversation_id: &str, goal: pb::GoalState) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::RunRequest(
            pb::AgentRunRequest {
                action: Some(pb::ConversationAction {
                    action: Some(pb::conversation_action::Action::GoalContinuationAction(
                        pb::GoalContinuationAction {},
                    )),
                    ..Default::default()
                }),
                conversation_state: Some(pb::ConversationStateStructure {
                    mode: Some(pb::AgentMode::Agent as i32),
                    goal_state: Some(goal),
                    ..Default::default()
                }),
                conversation_id: Some(conversation_id.into()),
                requested_model: Some(pb::RequestedModel {
                    model_id: "test-model".into(),
                    ..Default::default()
                }),
                run_id: Some("goal-run".into()),
                ..Default::default()
            },
        )),
    }
}

async fn drain(
    handle: &cursor_server::cursor::TransportHandle,
    mut output: tokio::sync::mpsc::UnboundedReceiver<bytes::Bytes>,
) -> Vec<pb::AgentServerMessage> {
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(user_request("conversation")),
        })
        .await
        .unwrap();
    let mut seqno = 1;
    let mut messages = Vec::new();
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), output.recv())
            .await
            .unwrap()
            .unwrap();
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            break;
        }
        let message = pb::AgentServerMessage::decode(payload).unwrap();
        if let Some(pb::agent_server_message::Message::KvServerMessage(kv)) = &message.message {
            handle
                .command(TransportCommand::Append {
                    seqno,
                    message: Box::new(kv_ack(kv.id)),
                })
                .await
                .unwrap();
            seqno += 1;
        }
        messages.push(message);
    }
    messages
}

fn kv_ack(id: u32) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::KvClientMessage(
            pb::KvClientMessage {
                id,
                message: Some(pb::kv_client_message::Message::SetBlobResult(
                    pb::SetBlobResult { error: None },
                )),
            },
        )),
    }
}

#[tokio::test]
async fn a_resumed_goal_keeps_its_counters_and_pauses_on_the_idle_streak() {
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    for _ in 0..3 {
        provider.push(text_turn("Working on it."));
    }
    let registry = TransportRegistry::new(
        store,
        Arc::new(provider.clone()),
        PromptCompiler::new(assets()),
    );
    let handle = registry
        .get_or_create("goal-restart-request")
        .await
        .unwrap();
    let output = handle.subscribe();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(user_request_with_goal(
                "conversation",
                goal_state("conversation", 1, 5),
            )),
        })
        .await
        .unwrap();
    let messages = drain(&handle, output).await;
    let goal = messages
        .iter()
        .filter_map(|message| match &message.message {
            Some(pb::agent_server_message::Message::ConversationCheckpointUpdate(state)) => {
                state.goal_state.clone()
            }
            _ => None,
        })
        .next_back()
        .expect("checkpoint must contain the resumed goal");

    assert_eq!(goal.status, STATUS_PAUSED);
    assert_eq!(goal.continuation_count, 7);
    assert_eq!(goal.idle_continuations_without_tool_calls, 3);
    assert_eq!(provider.requests().len(), 3);
}

#[tokio::test]
async fn create_goal_tool_persists_goal_state_and_renders_its_card() {
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    provider.push(vec![
        ModelEvent::Start {
            model_call_id: "model-1".into(),
        },
        ModelEvent::ToolCallStart {
            index: 0,
            call_id: "goal-call".into(),
            name: "CreateGoal".into(),
        },
        ModelEvent::ToolCallArgumentsDelta {
            index: 0,
            delta: "{\"objective\":\"Ship the feature\"}".into(),
        },
        ModelEvent::ToolCallEnd { index: 0 },
        ModelEvent::Done(FinishReason::ToolUse),
    ]);
    provider.push(vec![
        ModelEvent::Start {
            model_call_id: "model-2".into(),
        },
        ModelEvent::TextStart,
        ModelEvent::TextDelta("Goal created.".into()),
        ModelEvent::TextEnd,
        ModelEvent::Done(FinishReason::Stop),
    ]);
    // The goal keeps the turn open; three idle continuations pause it.
    for _ in 0..3 {
        provider.push(text_turn("Continuing."));
    }
    let registry = TransportRegistry::new(store, Arc::new(provider), PromptCompiler::new(assets()));
    let handle = registry.get_or_create("create-goal-request").await.unwrap();
    let output = handle.subscribe();
    let messages = drain(&handle, output).await;
    let mut first_goal = None;
    let mut rendered_success = false;
    for message in messages {
        match message.message {
            Some(pb::agent_server_message::Message::ConversationCheckpointUpdate(state)) => {
                if let Some(state) = state.goal_state {
                    first_goal.get_or_insert(state);
                }
            }
            Some(pb::agent_server_message::Message::InteractionUpdate(update)) => {
                if let Some(pb::interaction_update::Message::ToolCallCompleted(completed)) =
                    update.message
                {
                    if let Some(pb::tool_call::Tool::CreateGoalToolCall(tool)) =
                        completed.tool_call.and_then(|call| call.tool)
                    {
                        rendered_success = matches!(
                            tool.result.and_then(|result| result.result),
                            Some(pb::create_goal_result::Result::Success(_))
                        );
                    }
                }
            }
            _ => {}
        }
    }
    let goal = first_goal.expect("checkpoint must contain the created goal");
    assert_eq!(goal.objective, "Ship the feature");
    assert_eq!(goal.status, STATUS_ACTIVE);
    assert!(rendered_success, "CreateGoal must render a success card");
}

#[tokio::test]
async fn goal_continuation_dispatches_a_simulated_user_turn() {
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    for _ in 0..3 {
        provider.push(text_turn("Working on it."));
    }
    let registry = TransportRegistry::new(
        store,
        Arc::new(provider.clone()),
        PromptCompiler::new(assets()),
    );
    let handle = registry
        .get_or_create("goal-continuation-request")
        .await
        .unwrap();
    let output = handle.subscribe();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(continuation_request(
                "conversation",
                goal_state("conversation", 0, 0),
            )),
        })
        .await
        .unwrap();
    let mut output = output;
    let mut goals = Vec::new();
    let mut seqno = 1;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(20), output.recv())
            .await
            .unwrap()
            .unwrap();
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            break;
        }
        let message = pb::AgentServerMessage::decode(payload).unwrap();
        if let Some(pb::agent_server_message::Message::KvServerMessage(kv)) = &message.message {
            handle
                .command(TransportCommand::Append {
                    seqno,
                    message: Box::new(kv_ack(kv.id)),
                })
                .await
                .unwrap();
            seqno += 1;
        }
        if let Some(pb::agent_server_message::Message::ConversationCheckpointUpdate(state)) =
            message.message
        {
            if let Some(state) = state.goal_state {
                goals.push(state);
            }
        }
    }
    let requests = provider.requests();
    let continuation_text = requests
        .first()
        .expect("continuation must call the provider")
        .history
        .iter()
        .flat_map(|message| match &message.content {
            ProjectedContent::Parts(parts) => parts.clone(),
            _ => Vec::new(),
        })
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text),
            ContentPart::Image { .. } => None,
        })
        .find(|text| text.contains("<system_notification source=\"goal\">"))
        .expect("provider must see the goal continuation prompt");
    assert!(continuation_text.contains("<objective>\nShip the feature\n</objective>"));
    let first = goals
        .iter()
        .find(|goal| goal.continuation_count == 1)
        .expect("checkpoint must contain the continuation state");
    assert_eq!(first.idle_continuations_without_tool_calls, 1);
    assert_eq!(
        goals.last().expect("final goal state").status,
        STATUS_PAUSED,
        "the loop keeps continuing until the idle streak pauses the goal"
    );
}

#[tokio::test]
async fn three_idle_continuations_pause_the_goal_without_calling_the_model() {
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    let registry = TransportRegistry::new(
        store,
        Arc::new(provider.clone()),
        PromptCompiler::new(assets()),
    );
    let handle = registry.get_or_create("goal-pause-request").await.unwrap();
    let output = handle.subscribe();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(continuation_request(
                "conversation",
                goal_state("conversation", 3, 3),
            )),
        })
        .await
        .unwrap();
    let mut output = output;
    let mut goal = None;
    let mut seqno = 1;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), output.recv())
            .await
            .unwrap()
            .unwrap();
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            break;
        }
        let message = pb::AgentServerMessage::decode(payload).unwrap();
        if let Some(pb::agent_server_message::Message::KvServerMessage(kv)) = &message.message {
            handle
                .command(TransportCommand::Append {
                    seqno,
                    message: Box::new(kv_ack(kv.id)),
                })
                .await
                .unwrap();
            seqno += 1;
        }
        if let Some(pb::agent_server_message::Message::ConversationCheckpointUpdate(state)) =
            message.message
        {
            if let Some(state) = state.goal_state {
                goal = Some(state);
            }
        }
    }
    assert!(
        provider.requests().is_empty(),
        "anti-spin pause must not call the model"
    );
    assert_eq!(
        goal.expect("checkpoint must contain the paused goal")
            .status,
        STATUS_PAUSED
    );
}

#[tokio::test]
async fn an_active_goal_continues_without_a_client_action() {
    let (_directory, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    provider.push(text_turn("Working on it."));
    provider.push(text_turn("Still working on it."));
    let registry = TransportRegistry::new(
        store,
        Arc::new(provider.clone()),
        PromptCompiler::new(assets()),
    );
    let handle = registry.get_or_create("goal-loop-request").await.unwrap();
    let mut output = handle.subscribe();
    // One user turn carrying an active goal two continuations into its idle
    // streak: the loop continues once, then pauses on the third idle turn.
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(user_request_with_goal(
                "conversation",
                goal_state("conversation", 2, 0),
            )),
        })
        .await
        .unwrap();
    let mut goal = None;
    let mut turns_ended = 0;
    let mut seqno = 1;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(20), output.recv())
            .await
            .unwrap()
            .unwrap();
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            break;
        }
        let message = pb::AgentServerMessage::decode(payload).unwrap();
        if let Some(pb::agent_server_message::Message::KvServerMessage(kv)) = &message.message {
            handle
                .command(TransportCommand::Append {
                    seqno,
                    message: Box::new(kv_ack(kv.id)),
                })
                .await
                .unwrap();
            seqno += 1;
        }
        if let Some(pb::agent_server_message::Message::InteractionUpdate(update)) = &message.message
        {
            if matches!(
                update.message,
                Some(pb::interaction_update::Message::TurnEnded(_))
            ) {
                turns_ended += 1;
            }
        }
        if let Some(pb::agent_server_message::Message::ConversationCheckpointUpdate(state)) =
            message.message
        {
            if let Some(state) = state.goal_state {
                goal = Some(state);
            }
        }
    }

    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        2,
        "the goal must call the model again without a client continuation action"
    );
    let continuation_prompt = requests[1]
        .history
        .iter()
        .flat_map(|message| match &message.content {
            ProjectedContent::Parts(parts) => parts.clone(),
            _ => Vec::new(),
        })
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text),
            ContentPart::Image { .. } => None,
        })
        .find(|text| text.contains("<system_notification source=\"goal\">"))
        .expect("the second turn must be the goal continuation");
    assert!(continuation_prompt.contains("<objective>\nShip the feature\n</objective>"));
    let second_history = requests[1]
        .history
        .iter()
        .flat_map(|message| match &message.content {
            ProjectedContent::Parts(parts) => parts
                .iter()
                .filter_map(|part| match part {
                    ContentPart::Text { text } => Some(text.clone()),
                    ContentPart::Image { .. } => None,
                })
                .collect::<Vec<_>>(),
            ProjectedContent::Assistant { text, .. } => vec![text.clone()],
            ProjectedContent::ToolResult(_) => Vec::new(),
        })
        .collect::<Vec<_>>();
    assert!(
        second_history
            .iter()
            .any(|text| text.contains("Working on it.")),
        "the continuation must keep the previous turn's messages: {second_history:?}"
    );
    assert_eq!(
        turns_ended, 2,
        "each goal turn still reports its own turn end"
    );
    let goal = goal.expect("checkpoint must contain the goal state");
    assert_eq!(goal.status, STATUS_PAUSED);
    assert_eq!(goal.continuation_count, 1);
}
