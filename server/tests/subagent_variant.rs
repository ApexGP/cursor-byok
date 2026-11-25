//! Verifies a parent-selected subagent variant reaches the child run without
//! leaking to other models or root runs.
#[path = "support/fake_provider.rs"]
mod fake_provider;
#[path = "support/fixtures.rs"]
mod fixtures;

use std::{sync::Arc, time::Duration};

use cursor_server::{
    cursor::{
        prompting::{PromptAssets, PromptCompiler},
        protocol::connect,
        protocol::proto::agent::v1 as pb,
        TransportCommand, TransportHandle, TransportParent, TransportRegistry,
    },
    model::{ModelConfigInput, ModelLatency, ModelType, OPENAI_CHAT_ENDPOINT},
    provider::{FinishReason, ModelEvent},
};
use prost::Message;
use tokio::task::JoinHandle;

fn model_input(model_id: &str, context: u64, effort: &str) -> ModelConfigInput {
    ModelConfigInput {
        sort_order: 0,
        display_name: format!("DS {model_id}"),
        group_name: Some("byok".into()),
        model_type: ModelType::OpenAi,
        base_url: "https://example.com/v1".into(),
        use_full_url: false,
        api_key: "upstream".into(),
        tooltip_data: "test".into(),
        model_id: model_id.into(),
        reasoning_effort: Some(effort.into()),
        openai_endpoint: OPENAI_CHAT_ENDPOINT.into(),
        openai_extra_params_enabled: false,
        openai_extra_params: serde_json::json!({}),
        custom_headers_enabled: false,
        custom_headers: serde_json::json!({}),
        anthropic_extra_params_enabled: false,
        anthropic_extra_params: serde_json::json!({}),
        context_window_tokens: Some(context),
        max_completion_tokens: None,
        anthropic_max_tokens: None,
        anthropic_thinking_effort: None,
        thinking_budget_tokens: None,
    }
}

fn task_events(call_id: &str) -> Vec<ModelEvent> {
    vec![
        ModelEvent::Start {
            model_call_id: "parent-model-call".into(),
        },
        ModelEvent::ToolCallStart {
            index: 0,
            call_id: call_id.into(),
            name: "Task".into(),
        },
        ModelEvent::ToolCallArgumentsDelta {
            index: 0,
            delta: r#"{"prompt":"do work","description":"work","subagent_type":"generalPurpose"}"#
                .into(),
        },
        ModelEvent::ToolCallEnd { index: 0 },
        ModelEvent::Done(FinishReason::ToolUse),
    ]
}

fn text_events(text: &str) -> Vec<ModelEvent> {
    vec![
        ModelEvent::Start {
            model_call_id: format!("model-{text}"),
        },
        ModelEvent::TextStart,
        ModelEvent::TextDelta(text.into()),
        ModelEvent::TextEnd,
        ModelEvent::Done(FinishReason::Stop),
    ]
}

fn user_request(
    conversation_id: &str,
    run_id: &str,
    message_id: &str,
    model_id: &str,
) -> pb::AgentRunRequest {
    pb::AgentRunRequest {
        action: Some(pb::ConversationAction {
            action: Some(pb::conversation_action::Action::UserMessageAction(
                pb::UserMessageAction {
                    user_message: Some(pb::UserMessage {
                        text: "do the task".into(),
                        message_id: message_id.into(),
                        mode: pb::AgentMode::Agent as i32,
                        ..Default::default()
                    }),
                    request_context: Some(pb::RequestContext::default()),
                    ..Default::default()
                },
            )),
            ..Default::default()
        }),
        conversation_id: Some(conversation_id.into()),
        run_id: Some(run_id.into()),
        requested_model: Some(pb::RequestedModel {
            model_id: model_id.into(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn parameter(id: &str, value: &str) -> pb::requested_model::ModelParameterValue {
    pb::requested_model::ModelParameterValue {
        id: id.into(),
        value: value.into(),
    }
}

async fn append_run(handle: &TransportHandle, seqno: i64, request: pb::AgentRunRequest) -> i64 {
    handle
        .command(TransportCommand::Append {
            seqno,
            message: Box::new(pb::AgentClientMessage {
                message: Some(pb::agent_client_message::Message::RunRequest(request)),
            }),
        })
        .await
        .unwrap();
    seqno + 1
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

/// Reads frames forever, acking KV messages so prepared runs never stall.
fn spawn_acker(handle: TransportHandle) -> JoinHandle<()> {
    let mut output = handle.subscribe();
    tokio::spawn(async move {
        let mut seqno = 1i64;
        loop {
            let Ok(frame) = tokio::time::timeout(Duration::from_secs(10), output.recv()).await
            else {
                continue;
            };
            let Some(frame) = frame else { break };
            let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
            if flags & connect::END_STREAM_FLAG != 0 {
                continue;
            }
            let Ok(server) = pb::AgentServerMessage::decode(payload) else {
                continue;
            };
            if let Some(pb::agent_server_message::Message::KvServerMessage(kv)) = server.message {
                let _ = handle
                    .command(TransportCommand::Append {
                        seqno,
                        message: Box::new(kv_ack(kv.id)),
                    })
                    .await;
                seqno += 1;
            }
        }
    })
}

async fn wait_requests(provider: &fake_provider::FakeProvider, expected: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while provider.requests().len() < expected {
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected {expected} provider requests, saw {}",
            provider.requests().len()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn child_run_uses_parent_selected_variant_and_does_not_leak() {
    let (_directory, store) = fixtures::temp_store().await;
    let primary = store
        .create_model(&model_input("upstream-model", 256_000, "high"))
        .await
        .unwrap();
    let other = store
        .create_model(&model_input("other-upstream", 128_000, "low"))
        .await
        .unwrap();
    let primary_hash = primary.model_hash.clone();
    let other_hash = other.model_hash.clone();

    let provider = fake_provider::FakeProvider::default();
    provider.push(task_events("task-call-1"));
    provider.push(text_events("child done"));
    provider.push(text_events("follow up done"));
    provider.push(text_events("other done"));
    provider.push(text_events("root done"));

    let assets = PromptAssets::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("prompt/cursor")
            .as_path(),
    )
    .unwrap();
    let registry = TransportRegistry::new(
        store.clone(),
        Arc::new(provider.clone()),
        PromptCompiler::new(assets),
    );

    // --- Parent run selects 1M/max/fast through the subagent override. ---
    let parent = registry.get_or_create("parent-request").await.unwrap();
    let mut parent_output = parent.subscribe();
    let mut parent_seqno = 1i64;
    let mut request = user_request("parent-conv", "parent-run", "parent-user", &primary_hash);
    request.subagent_model_overrides = vec![pb::SubagentModelOverride {
        subagent_type: "generalPurpose".into(),
        selection: Some(pb::subagent_model_override::Selection::Model(
            pb::RequestedModel {
                model_id: primary_hash.clone(),
                parameters: vec![
                    parameter("context", "1000000"),
                    parameter("effort", "max"),
                    parameter("fast", "true"),
                ],
                ..Default::default()
            },
        )),
    }];
    append_run(&parent, 0, request).await;

    let mut dispatched = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !dispatched {
        assert!(
            tokio::time::Instant::now() < deadline,
            "parent Task never dispatched"
        );
        let frame = tokio::time::timeout(Duration::from_secs(5), parent_output.recv())
            .await
            .unwrap()
            .unwrap();
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            continue;
        }
        let Ok(server) = pb::AgentServerMessage::decode(payload) else {
            continue;
        };
        match server.message {
            Some(pb::agent_server_message::Message::KvServerMessage(kv)) => {
                parent
                    .command(TransportCommand::Append {
                        seqno: parent_seqno,
                        message: Box::new(kv_ack(kv.id)),
                    })
                    .await
                    .unwrap();
                parent_seqno += 1;
            }
            Some(pb::agent_server_message::Message::ExecServerMessage(exec)) => {
                eprintln!("parent exec: {exec:?}");
                dispatched = true;
            }
            _ => {}
        }
    }

    // --- Child run with the parent tool call id and an EMPTY message id. ---
    let child = registry.get_or_create("child-request").await.unwrap();
    child
        .set_parent(TransportParent {
            request_id: "parent-request".into(),
            tool_call_id: "task-call-1".into(),
        })
        .unwrap();
    let child_acker = spawn_acker(child.clone());
    append_run(
        &child,
        0,
        user_request("child-conv", "child-run", "", &primary_hash),
    )
    .await;
    wait_requests(&provider, 2).await;
    let first_child = provider.requests()[1].model.clone();
    eprintln!("first child model: {first_child:?}");
    assert_eq!(first_child.model_id, primary_hash);
    assert_eq!(first_child.context_window_tokens, Some(1_000_000));
    assert_eq!(first_child.reasoning.effort.as_deref(), Some("max"));
    assert!(first_child.reasoning.enabled);
    assert_eq!(first_child.latency, ModelLatency::Fast);

    // --- A later subagent run on another transport keeps the variant. ---
    let follow_up_handle = registry
        .get_or_create("child-followup-request")
        .await
        .unwrap();
    follow_up_handle
        .set_parent(TransportParent {
            request_id: "parent-request".into(),
            tool_call_id: "task-call-1".into(),
        })
        .unwrap();
    let follow_up_acker = spawn_acker(follow_up_handle.clone());
    append_run(
        &follow_up_handle,
        0,
        user_request(
            "child-followup-conv",
            "child-run-2",
            "child-user-2",
            &primary_hash,
        ),
    )
    .await;
    wait_requests(&provider, 3).await;
    let follow_up = provider.requests()[2].model.clone();
    eprintln!("follow-up child model: {follow_up:?}");
    assert_eq!(follow_up.context_window_tokens, Some(1_000_000));
    assert_eq!(follow_up.reasoning.effort.as_deref(), Some("max"));
    assert_eq!(follow_up.latency, ModelLatency::Fast);

    // --- A child that selects a DIFFERENT model must not inherit the variant. ---
    let mismatch = registry.get_or_create("mismatch-request").await.unwrap();
    mismatch
        .set_parent(TransportParent {
            request_id: "parent-request".into(),
            tool_call_id: "task-call-1".into(),
        })
        .unwrap();
    let mismatch_acker = spawn_acker(mismatch.clone());
    append_run(
        &mismatch,
        0,
        user_request(
            "mismatch-conv",
            "mismatch-run",
            "mismatch-user",
            &other_hash,
        ),
    )
    .await;
    wait_requests(&provider, 4).await;
    let mismatch_model = provider.requests()[3].model.clone();
    eprintln!("mismatch child model: {mismatch_model:?}");
    assert_eq!(mismatch_model.model_id, other_hash);
    assert_eq!(mismatch_model.context_window_tokens, Some(128_000));
    assert_eq!(mismatch_model.reasoning.effort.as_deref(), Some("low"));
    assert_eq!(mismatch_model.latency, ModelLatency::Standard);

    // --- A root run (no parent headers) must not inherit any variant. ---
    let root = registry.get_or_create("root-request").await.unwrap();
    let root_acker = spawn_acker(root.clone());
    append_run(
        &root,
        0,
        user_request("root-conv", "root-run", "root-user", &primary_hash),
    )
    .await;
    wait_requests(&provider, 5).await;
    let root_model = provider.requests()[4].model.clone();
    eprintln!("root model: {root_model:?}");
    assert_eq!(root_model.context_window_tokens, Some(256_000));
    assert_eq!(root_model.reasoning.effort.as_deref(), Some("high"));
    assert_eq!(root_model.latency, ModelLatency::Standard);

    child_acker.abort();
    follow_up_acker.abort();
    mismatch_acker.abort();
    root_acker.abort();
}
