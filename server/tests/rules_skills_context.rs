//! Verifies rules and skills follow Cursor's injection semantics.
#[path = "support/fake_provider.rs"]
mod fake_provider;
#[path = "support/fixtures.rs"]
mod fixtures;

use std::sync::Arc;

use cursor_server::{
    cursor::{
        prompting::{PromptAssets, PromptCompiler},
        protocol::connect,
        protocol::proto::agent::v1 as pb,
        TransportCommand, TransportRegistry,
    },
    model::{ContentPart, ModelRequest, ProjectedContent},
    provider::{FinishReason, ModelEvent},
};
use prost::Message;

#[tokio::test]
async fn rules_and_skills_follow_cursor_injection_semantics() {
    let (_store_dir, store) = fixtures::temp_store().await;
    let provider = fake_provider::FakeProvider::default();
    for call in ["call-1", "call-2"] {
        provider.push(vec![
            ModelEvent::Start {
                model_call_id: call.into(),
            },
            ModelEvent::TextStart,
            ModelEvent::TextDelta("ok".into()),
            ModelEvent::TextEnd,
            ModelEvent::Done(FinishReason::Stop),
        ]);
    }
    let assets = PromptAssets::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("prompt/cursor")
            .as_path(),
    )
    .unwrap();
    let registry = TransportRegistry::new(
        store,
        Arc::new(provider.clone()),
        PromptCompiler::new(assets),
    );

    let first = run_turn(
        &registry,
        "rules-request-1",
        turn("first", "rules-user-1", "docs/readme.md", None),
    )
    .await;
    run_turn(
        &registry,
        "rules-request-2",
        turn("second", "rules-user-2", "src/main.rs", Some(first)),
    )
    .await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);

    let first_context = only_request_context(&requests[0]);
    assert!(
        first_context.contains("<user_rule>\nALWAYS BODY\n</user_rule>"),
        "{first_context}"
    );
    assert!(
        first_context.contains(
            "<agent_rule fullPath=\"/repo/.cursor/rules/agent.mdc\" description=\"API conventions\"/>"
        ),
        "{first_context}"
    );
    assert!(!first_context.contains("AGENT BODY"), "{first_context}");
    assert!(!first_context.contains("RUST BODY"), "{first_context}");
    assert!(!first_context.contains("MANUAL BODY"), "{first_context}");
    assert!(first_context.contains("general skill"), "{first_context}");
    assert!(!first_context.contains("ts skill"), "{first_context}");
    assert!(
        !history_text(&requests[0]).contains("RUST BODY"),
        "no open file matches src/**/*.rs on the first turn"
    );

    let second_context = only_request_context(&requests[1]);
    assert_eq!(
        first_context, second_context,
        "open files must not change the stable request context"
    );
    let second_history = history_text(&requests[1]);
    assert!(
        second_history.contains("RUST BODY"),
        "matching glob rule is attached for this turn only: {second_history}"
    );
    assert!(
        !second_history.contains("ts skill"),
        "glob scoped skills stay out until a matching file shows up"
    );

    registry.shutdown().await;
}

fn only_request_context(request: &ModelRequest) -> String {
    let contexts = request
        .history
        .iter()
        .filter(|message| message.message_id.starts_with("request-context:"))
        .map(|message| {
            let ProjectedContent::Parts(parts) = &message.content else {
                panic!("request context message must be parts")
            };
            let [ContentPart::Text { text }] = parts.as_slice() else {
                panic!("request context message must be one text part")
            };
            text.clone()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        contexts.len(),
        1,
        "exactly one request-context message is projected"
    );
    contexts.into_iter().next().unwrap()
}

fn history_text(request: &ModelRequest) -> String {
    serde_json::to_string(&request.history).unwrap()
}

async fn run_turn(
    registry: &TransportRegistry,
    request_id: &str,
    request: pb::AgentClientMessage,
) -> pb::ConversationStateStructure {
    let handle = registry.get_or_create(request_id).await.unwrap();
    let mut output = handle.subscribe();
    let mut seqno = 0;
    handle
        .command(TransportCommand::Append {
            seqno,
            message: Box::new(request),
        })
        .await
        .unwrap();
    seqno += 1;
    let mut state = None;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), output.recv())
            .await
            .expect("run finishes within timeout")
            .expect("output stays open until EndStream");
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            return state.expect("run published no checkpoint");
        }
        match pb::AgentServerMessage::decode(payload).unwrap().message {
            Some(pb::agent_server_message::Message::ConversationCheckpointUpdate(update)) => {
                state = Some(update)
            }
            Some(pb::agent_server_message::Message::KvServerMessage(kv)) => {
                handle
                    .command(TransportCommand::Append {
                        seqno,
                        message: Box::new(set_blob_result(kv.id)),
                    })
                    .await
                    .unwrap();
                seqno += 1;
            }
            _ => {}
        }
    }
}

fn set_blob_result(id: u32) -> pb::AgentClientMessage {
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

fn turn(
    text: &str,
    message_id: &str,
    relative_path: &str,
    state: Option<pb::ConversationStateStructure>,
) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::RunRequest(
            pb::AgentRunRequest {
                action: Some(pb::ConversationAction {
                    action: Some(pb::conversation_action::Action::UserMessageAction(
                        pb::UserMessageAction {
                            user_message: Some(pb::UserMessage {
                                text: text.into(),
                                message_id: message_id.into(),
                                mode: pb::AgentMode::Agent as i32,
                                selected_context: Some(pb::SelectedContext {
                                    invocation_context: Some(pb::InvocationContext {
                                        data: Some(pb::invocation_context::Data::IdeState(
                                            pb::invocation_context::IdeState {
                                                visible_files: vec![
                                                    pb::invocation_context::ide_state::File {
                                                        path: format!("/repo/{relative_path}"),
                                                        relative_path: Some(relative_path.into()),
                                                        total_lines: 10,
                                                        ..Default::default()
                                                    },
                                                ],
                                                ..Default::default()
                                            },
                                        )),
                                    }),
                                    ..Default::default()
                                }),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                    )),
                    request_context_parts: Some(pb::RequestContextPartReferences {
                        dynamic_context: Some(request_context()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                conversation_id: Some("rules-conversation".into()),
                run_id: Some(message_id.into()),
                requested_model: Some(pb::RequestedModel {
                    model_id: "test-model".into(),
                    ..Default::default()
                }),
                conversation_state: state,
                ..Default::default()
            },
        )),
    }
}

fn request_context() -> pb::RequestContext {
    pb::RequestContext {
        rules: vec![
            typed_rule(
                "ALWAYS BODY",
                "always.mdc",
                pb::cursor_rule_type::Type::Global(pb::CursorRuleTypeGlobal::default()),
            ),
            typed_rule(
                "AGENT BODY",
                "agent.mdc",
                pb::cursor_rule_type::Type::AgentFetched(pb::CursorRuleTypeAgentFetched {
                    description: "API conventions".into(),
                }),
            ),
            typed_rule(
                "RUST BODY",
                "rust.mdc",
                pb::cursor_rule_type::Type::FileGlobbed(pb::CursorRuleTypeFileGlobs {
                    globs: vec!["src/**/*.rs".into()],
                }),
            ),
            typed_rule(
                "MANUAL BODY",
                "manual.mdc",
                pb::cursor_rule_type::Type::ManuallyAttached(
                    pb::CursorRuleTypeManuallyAttached::default(),
                ),
            ),
        ],
        agent_skills: vec![
            skill("general", "general skill", &[]),
            skill("ts-only", "ts skill", &["**/*.ts"]),
        ],
        env: Some(pb::RequestContextEnv {
            workspace_paths: vec!["/repo".into()],
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn typed_rule(content: &str, file: &str, kind: pb::cursor_rule_type::Type) -> pb::CursorRule {
    pb::CursorRule {
        full_path: format!("/repo/.cursor/rules/{file}"),
        content: content.into(),
        r#type: Some(pb::CursorRuleType { r#type: Some(kind) }),
        ..Default::default()
    }
}

fn skill(name: &str, description: &str, globs: &[&str]) -> pb::AgentSkill {
    pb::AgentSkill {
        full_path: format!("/repo/.agents/skills/{name}/SKILL.md"),
        description: description.into(),
        content: format!("{name} body"),
        globs: globs.iter().map(|glob| (*glob).to_string()).collect(),
        ..Default::default()
    }
}
