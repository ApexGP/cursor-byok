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
    let provider = provider_with(&["call-1", "call-2", "call-3"]);
    let registry = TransportRegistry::new(store, Arc::new(provider.clone()), prompt_compiler());

    let first = run_turn(
        &registry,
        "rules-request-1",
        turn("first", "rules-user-1", "docs/readme.md", None),
    )
    .await;
    let second = run_turn(
        &registry,
        "rules-request-2",
        turn("second", "rules-user-2", "src/main.rs", Some(first)),
    )
    .await;
    run_turn(
        &registry,
        "rules-request-3",
        turn("third", "rules-user-3", "app/web.tsx", Some(second)),
    )
    .await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 3);

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
    assert!(!first_context.contains("WEB BODY"), "{first_context}");
    assert!(!first_context.contains("MANUAL BODY"), "{first_context}");
    assert!(
        first_context.contains("<user_rule>\nBROKEN FRONTMATTER BODY\n</user_rule>"),
        "malformed frontmatter falls back to always-apply: {first_context}"
    );
    assert!(
        !first_context.contains("MALFORMED GLOB BODY"),
        "an invalid glob must not inject its body: {first_context}"
    );
    assert!(first_context.contains("general skill"), "{first_context}");
    assert!(!first_context.contains("ts skill"), "{first_context}");
    assert!(!first_context.contains("web skill"), "{first_context}");
    assert!(
        first_context.contains("read the file at its fullPath before continuing"),
        "the catalog must tell the agent the rules are not loaded: {first_context}"
    );
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
    assert!(
        !second_history.contains("WEB BODY"),
        "brace globs must not attach before a matching file shows up: {second_history}"
    );

    let third_history = history_text(&requests[2]);
    assert!(
        third_history.contains("WEB BODY"),
        "brace glob rules attach for a matching file: {third_history}"
    );
    assert!(
        third_history.contains("web skill"),
        "brace glob skills attach for a matching file: {third_history}"
    );
    assert!(
        third_history.contains("RUST BODY"),
        "earlier turn material stays in the append-only history"
    );
    assert!(
        !third_history.contains("MALFORMED GLOB BODY"),
        "an invalid glob never attaches, even when files are open: {third_history}"
    );

    registry.shutdown().await;
}

#[tokio::test]
async fn glob_rules_attach_from_every_workspace_root() {
    let (_store_dir, store) = fixtures::temp_store().await;
    let provider = provider_with(&["call-1"]);
    let registry = TransportRegistry::new(store, Arc::new(provider.clone()), prompt_compiler());

    run_turn(
        &registry,
        "multi-root-request",
        turn_in(
            "second root",
            "multi-root-user",
            "/repo2/src/main.rs",
            None,
            &["/repo", "/repo2"],
            None,
        ),
    )
    .await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let history = history_text(&requests[0]);
    assert!(
        history.contains("RUST BODY"),
        "an open file in the second workspace root attaches the glob rule: {history}"
    );

    registry.shutdown().await;
}

fn prompt_compiler() -> PromptCompiler {
    let assets = PromptAssets::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("prompt/cursor")
            .as_path(),
    )
    .unwrap();
    PromptCompiler::new(assets)
}

fn provider_with(calls: &[&str]) -> fake_provider::FakeProvider {
    let provider = fake_provider::FakeProvider::default();
    for call in calls {
        provider.push(vec![
            ModelEvent::Start {
                model_call_id: (*call).into(),
            },
            ModelEvent::TextStart,
            ModelEvent::TextDelta("ok".into()),
            ModelEvent::TextEnd,
            ModelEvent::Done(FinishReason::Stop),
        ]);
    }
    provider
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
    turn_in(
        text,
        message_id,
        &format!("/repo/{relative_path}"),
        Some(relative_path),
        &["/repo"],
        state,
    )
}

fn turn_in(
    text: &str,
    message_id: &str,
    path: &str,
    relative_path: Option<&str>,
    workspaces: &[&str],
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
                                                        path: path.into(),
                                                        relative_path: relative_path
                                                            .map(str::to_string),
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
                        dynamic_context: Some(request_context(workspaces)),
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

fn request_context(workspaces: &[&str]) -> pb::RequestContext {
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
                "WEB BODY",
                "web.mdc",
                pb::cursor_rule_type::Type::FileGlobbed(pb::CursorRuleTypeFileGlobs {
                    globs: vec!["**/*.{tsx,jsx}".into()],
                }),
            ),
            typed_rule(
                "MALFORMED GLOB BODY",
                "malformed-glob.mdc",
                pb::cursor_rule_type::Type::FileGlobbed(pb::CursorRuleTypeFileGlobs {
                    globs: vec!["src/[unclosed".into()],
                }),
            ),
            pb::CursorRule {
                full_path: "/repo/.cursor/rules/broken-frontmatter.mdc".into(),
                content: "BROKEN FRONTMATTER BODY".into(),
                frontmatter: "---\nglobs: [unclosed\n---".into(),
                ..Default::default()
            },
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
            skill("web-only", "web skill", &["**/*.{tsx,jsx}"]),
        ],
        env: Some(pb::RequestContextEnv {
            workspace_paths: workspaces.iter().map(|path| (*path).to_string()).collect(),
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
