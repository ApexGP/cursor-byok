//! Compiles rules, skills, MCP metadata, and environment context.
mod globs;
mod rules;
mod skills;
mod turn;

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
};

use prost::Message;
use serde_json::Value;

use crate::{
    cursor::{
        protocol::proto::agent::v1 as pb, services::context_sync::RequestContextSynchronizer,
        tools::runtime::McpRoute,
    },
    model::{normalize_tool_name, ToolDefinition},
    store::BlobId,
    Error, Result,
};

pub(crate) use turn::{attached_context, ide_state, turn_files};

/// Tells the model that catalogued agent-decided rules still need a read.
const AGENT_RULES_INSTRUCTION: &str = "Rules below are not loaded. When a rule description matches the current task, read the file at its fullPath before continuing.";

const DYNAMIC_TOOLS_INSTRUCTION: &str = r#"You have access to tools through dynamic namespaces, e.g. MCP servers, using `GetMcpTools` and `CallMcpTool`.

## Dynamic Tool Discovery and Invocation

Use `GetMcpTools` to discover tool schemas, then `CallMcpTool` to invoke one tool. Aim to minimize round-trips: ideally one discovery call followed by one invocation.

If the user mentions a product or service represented by an available namespace, and the request likely depends on it, proactively inspect that namespace before answering. If you are unsure which namespace matches, search with a relevant pattern.

`GetMcpTools` supports these modes:

1. `{"server":"<id>"}`: returns schemas and full descriptions for every tool in that namespace.
2. `{"server":"<id>","toolName":"<name>"}`: returns one tool schema with its full description.
3. `{"pattern":"<regex>"}`: searches namespace names, tool names and tool descriptions. Matching ignores case.
4. `{"server":"<id>","pattern":"<regex>"}`: searches tool names and descriptions within one namespace. Matching ignores case.
5. No arguments: returns the full catalog.

Pattern-search and catalog results shorten long descriptions, with "... [truncated]" at each cut; namespace and single-tool lookups always return the complete description.

Always inspect a tool's schema before invoking it with `CallMcpTool`.

If the available dynamic tools do not fully support what the user asked you to do, complete the work you can with the current tool set. In your work summary, include what you were unable to do and why. Do not use browser automation to work around missing tools unless the user explicitly asks you to use the browser."#;

pub async fn hydrate(
    request: &pb::AgentRunRequest,
    context_sync: &RequestContextSynchronizer,
) -> Result<pb::RequestContext> {
    let mut context = request_context(request).cloned().unwrap_or_default();
    let Some(parts) = request
        .action
        .as_ref()
        .and_then(|action| action.request_context_parts.as_ref())
    else {
        if is_background_completion(request) {
            return context_sync
                .load(request.conversation_id.as_deref().unwrap_or_default())
                .await;
        }
        return Ok(context);
    };

    if let Some(current) = context_sync
        .refresh_if_missing(
            parts,
            request.conversation_id.as_deref().unwrap_or_default(),
        )
        .await?
    {
        context.rules = current.rules;
        context.non_file_rules = current.non_file_rules;
        context.cloud_rule = current.cloud_rule;
        context.agent_skills = current.agent_skills;
        context.skill_options = current.skill_options;
        context.custom_subagents = current.custom_subagents;
        context.tools = current.tools;
        context.mcp_instructions = current.mcp_instructions;
        context.mcp_file_system_options = current.mcp_file_system_options;
        context.mcp_meta_tool_options = current.mcp_meta_tool_options;
        return Ok(context);
    }

    if let Some(part) = decode_part::<pb::RequestContextRulesPart>(
        "rules",
        &parts.rules_blob_id,
        parts.rules_byte_length,
        context_sync,
    )
    .await?
    {
        context.rules = part.rules;
        context.non_file_rules = part.non_file_rules;
        context.cloud_rule = part.cloud_rule;
    }
    if let Some(part) = decode_part::<pb::RequestContextSkillsPart>(
        "skills",
        &parts.skills_blob_id,
        parts.skills_byte_length,
        context_sync,
    )
    .await?
    {
        context.agent_skills = part.agent_skills;
        context.skill_options = part.skill_options;
    }
    if let Some(part) = decode_part::<pb::RequestContextSubagentsPart>(
        "subagents",
        &parts.subagents_blob_id,
        parts.subagents_byte_length,
        context_sync,
    )
    .await?
    {
        context.custom_subagents = part.custom_subagents;
    }
    if let Some(part) = decode_part::<pb::RequestContextMcpsPart>(
        "MCP",
        &parts.mcps_blob_id,
        parts.mcps_byte_length,
        context_sync,
    )
    .await?
    {
        context.tools = part.tools;
        context.mcp_instructions = part.mcp_instructions;
        context.mcp_file_system_options = part.mcp_file_system_options;
        context.mcp_meta_tool_options = part.mcp_meta_tool_options;
    }
    Ok(context)
}

fn is_background_completion(request: &pb::AgentRunRequest) -> bool {
    matches!(
        request
            .action
            .as_ref()
            .and_then(|action| action.action.as_ref()),
        Some(pb::conversation_action::Action::BackgroundTaskCompletionAction(_))
    )
}

async fn decode_part<T: Message + Default>(
    name: &str,
    raw_id: &[u8],
    expected_length: u32,
    context_sync: &RequestContextSynchronizer,
) -> Result<Option<T>> {
    if raw_id.is_empty() {
        if expected_length != 0 {
            return Err(Error::Protocol(format!(
                "{name} context has a byte length but no BlobID"
            )));
        }
        return Ok(None);
    }
    let id = BlobId::from_bytes(raw_id)?;
    let data = context_sync.get(&id).await?.ok_or_else(|| {
        Error::Protocol(format!(
            "{name} context Blob is missing: {}",
            id.to_base64()
        ))
    })?;
    if data.len() != expected_length as usize {
        return Err(Error::Protocol(format!(
            "{name} context Blob length mismatch: expected {expected_length}, got {}",
            data.len()
        )));
    }
    T::decode(data.as_slice())
        .map(Some)
        .map_err(|error| Error::Protocol(format!("invalid {name} context Blob: {error}")))
}

/// 把本地 md 规则目录(rules 服务的存储)合并进请求上下文,
/// 使 BYOK 运行在 IDE 未携带这些规则时也能消费它们。
/// 与 IDE 已发规则按内容去重;读取失败只告警,不影响运行。
pub fn merge_local_rules(context: &mut pb::RequestContext, rules_dir: &Path) {
    let records = match crate::cursor::services::knowledge::RuleStore::open(rules_dir.into())
        .and_then(|store| store.list())
    {
        Ok(records) => records,
        Err(error) => {
            tracing::warn!(%error, "cannot read local rules; continuing without them");
            return;
        }
    };
    let existing = context
        .rules
        .iter()
        .chain(context.non_file_rules.iter())
        .map(|rule| rule.content.trim().to_owned())
        .chain(context.cloud_rule.iter().map(|rule| rule.trim().to_owned()))
        .collect::<HashSet<_>>();
    for record in records {
        if record.knowledge.trim().is_empty() || existing.contains(record.knowledge.trim()) {
            continue;
        }
        context.non_file_rules.push(pb::CursorRule {
            content: record.knowledge,
            ..Default::default()
        });
    }
}

pub fn request_context(request: &pb::AgentRunRequest) -> Option<&pb::RequestContext> {
    let action = request.action.as_ref()?;
    action
        .request_context_parts
        .as_ref()
        .and_then(|parts| parts.dynamic_context.as_ref())
        .or_else(|| match action.action.as_ref()? {
            pb::conversation_action::Action::UserMessageAction(action) => {
                action.request_context.as_ref()
            }
            pb::conversation_action::Action::ExecutePlanAction(action) => {
                action.request_context.as_ref()
            }
            _ => None,
        })
}

pub fn compile_context(
    context: &pb::RequestContext,
    today: &str,
    context_window_tokens: Option<u64>,
) -> String {
    let mut sections = Vec::new();
    let mut transcripts = None;
    if let Some(env) = &context.env {
        let workspace = env
            .workspace_paths
            .first()
            .map(String::as_str)
            .unwrap_or("");
        let repo = context.git_repos.iter().find(|repo| repo.path == workspace);
        sections.push(format!(
            "<user_info>\nOS Version: {}\n\nShell: {}\n\nWorkspace Path: {}\n\nIs directory a git repo: {}\n\nTerminals folder: {}\n\nToday's date: {}\n\nNote: Prefer using absolute paths over relative paths as tool call args when possible.\n</user_info>",
            env.os_version,
            env.shell,
            workspace,
            repo.map(|repo| format!("Yes, at {}", repo.path)).unwrap_or_else(|| "No".into()),
            env.terminals_folder,
            today,
        ));
        if !env.agent_transcripts_folder.is_empty() {
            transcripts = Some(format!(
                "<agent_transcripts>\nAgent transcripts (past chats) live in {}. They have names like <uuid>.jsonl, cite parent chat transcripts to the user as [<title for chat <=6 words>\n](<uuid excluding .jsonl>). Don't discuss the folder structure.\n</agent_transcripts>",
                env.agent_transcripts_folder
            ));
        }
    }
    sections.extend(context.git_repos.iter().map(|repo| {
        format!(
            "<git_status>\nThis is the git status at the start of the conversation. Note that this status is a snapshot in time, and will not update during the conversation.\n\n\nGit repo: {}\n\n```\n{}\n```\n</git_status>",
            repo.path, repo.status
        )
    }));
    sections.extend(transcripts);
    let skill_index = rules::SkillIndex::new(context);
    let mut rule_bodies = Vec::new();
    let mut catalogs = Vec::new();
    let mut catalogued = HashSet::new();
    for rule in context.rules.iter().chain(context.non_file_rules.iter()) {
        if skill_index.covers(rule) {
            continue;
        }
        match rules::rule_kind(rule) {
            rules::RuleKind::Always => rules::push_rule_content(&mut rule_bodies, rule),
            rules::RuleKind::AgentFetched { description } => {
                if description.trim().is_empty() || rule.full_path.trim().is_empty() {
                    rules::push_rule_content(&mut rule_bodies, rule);
                    continue;
                }
                if catalogued.insert(rule.full_path.as_str()) {
                    catalogs.push(format!(
                        "<agent_rule fullPath=\"{}\" description=\"{}\"/>",
                        xml(&rule.full_path),
                        xml(description.trim()),
                    ));
                }
            }
            rules::RuleKind::Globbed { .. } | rules::RuleKind::Manual => {}
        }
    }
    rule_bodies.extend(
        context
            .cloud_rule
            .iter()
            .map(|rule| format!("<user_rule>\n{rule}\n</user_rule>")),
    );
    if !rule_bodies.is_empty() {
        sections.push(format!("<rules>\n{}\n</rules>", rule_bodies.join("\n")));
    }
    if !catalogs.is_empty() {
        sections.push(format!(
            "<agent_rules>\n{}\n{}\n</agent_rules>",
            AGENT_RULES_INSTRUCTION,
            catalogs.join("\n")
        ));
    }
    if let Some(section) = skills::section(context, context_window_tokens) {
        sections.push(section);
    }
    let subagents = context
        .custom_subagents
        .iter()
        .map(|agent| {
            format!(
                "<subagent name=\"{}\">{}</subagent>",
                xml(&agent.name),
                agent.description
            )
        })
        .collect::<Vec<_>>();
    if !subagents.is_empty() {
        sections.push(format!(
            "<subagents>\n{}\n</subagents>",
            subagents.join("\n")
        ));
    }
    if let Some(options) = active_meta_tool_options(context) {
        let namespaces = options
            .mcp_descriptors
            .iter()
            .filter_map(|server| compile_dynamic_namespace(server, context))
            .collect::<Vec<_>>();
        if !namespaces.is_empty() {
            sections.push(format!(
                "<dynamic_tools>\n{DYNAMIC_TOOLS_INSTRUCTION}\n\nAvailable dynamic tool namespaces:\n\n<dynamic_tool_namespaces>\n{}\n</dynamic_tool_namespaces>\n\n\nIf an MCP-backed namespace requires authentication, call `mcp_auth` through `CallMcpTool` for that namespace, then inspect it again and retry if appropriate. Do not authenticate namespaces preemptively or repeatedly.\n</dynamic_tools>",
                namespaces.join("\n")
            ));
        }
    }
    sections.join("\n\n")
}

fn active_meta_tool_options(context: &pb::RequestContext) -> Option<&pb::McpMetaToolOptions> {
    context
        .mcp_meta_tool_options
        .as_ref()
        .filter(|options| options.enabled)
}

pub fn meta_tools_enabled(context: &pb::RequestContext) -> bool {
    active_meta_tool_options(context).is_some()
}

fn compile_dynamic_namespace(
    server: &pb::McpDescriptor,
    context: &pb::RequestContext,
) -> Option<String> {
    let identifier = server.server_identifier.trim();
    if identifier.is_empty() {
        return None;
    }
    let mut seen = HashSet::new();
    let tools = server
        .tools
        .iter()
        .filter_map(|tool| {
            let name = tool.tool_name.trim();
            (!name.is_empty() && seen.insert(name)).then(|| xml(name))
        })
        .collect::<Vec<_>>();
    let instructions = server
        .server_use_instructions
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            context
                .mcp_instructions
                .iter()
                .find(|entry| entry.server_identifier.trim() == identifier)
                .map(|entry| entry.instructions.trim())
                .filter(|value| !value.is_empty())
        });
    if tools.is_empty() && instructions.is_none() {
        return None;
    }
    let mut output = format!(
        "<namespace name=\"{}\" tools=\"{}\"",
        xml(identifier),
        tools.join(", ")
    );
    if let Some(instructions) = instructions {
        output.push_str(&format!(
            " namespaceUseInstructions=\"{}\"",
            xml(instructions)
        ));
    }
    output.push_str(" source=\"mcp\" />");
    Some(output)
}

pub fn meta_mcp_routes(context: &pb::RequestContext) -> HashMap<(String, String), McpRoute> {
    context
        .mcp_meta_tool_options
        .as_ref()
        .into_iter()
        .flat_map(|options| &options.mcp_descriptors)
        .flat_map(|server| {
            let identifier = server.server_identifier.trim();
            if identifier.is_empty() {
                return Vec::new();
            }
            let provider_identifier = if server.server_name.trim().is_empty() {
                identifier.to_string()
            } else {
                server.server_name.trim().to_string()
            };
            server
                .tools
                .iter()
                .filter_map(|tool| {
                    let tool_name = tool.tool_name.trim();
                    if tool_name.is_empty() {
                        return None;
                    }
                    Some((
                        (identifier.to_string(), tool_name.to_string()),
                        McpRoute {
                            name: format!("{identifier}-{tool_name}"),
                            provider_identifier: provider_identifier.clone(),
                            tool_name: tool_name.to_string(),
                            description: tool.description.clone().unwrap_or_default(),
                        },
                    ))
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn selected_context(user: &pb::UserMessage) -> Option<String> {
    let selected = user.selected_context.as_ref()?;
    let mut sections = selected.extra_context.clone();
    sections.extend(
        selected
            .files
            .iter()
            .map(|file| format!("<file path=\"{}\">\n{}\n</file>", file.path, file.content)),
    );
    sections.extend(
        selected
            .code_selections
            .iter()
            .map(|value| format!("<code path=\"{}\">\n{}\n</code>", value.path, value.content)),
    );
    sections.extend(selected.terminals.iter().map(|value| {
        format!(
            "<terminal title=\"{}\">\n{}\n</terminal>",
            value.title.as_deref().unwrap_or_default(),
            value.content
        )
    }));
    sections.extend(selected.terminal_selections.iter().map(|value| {
        format!(
            "<terminal_selection title=\"{}\">\n{}\n</terminal_selection>",
            value.title.as_deref().unwrap_or_default(),
            value.content
        )
    }));
    sections.extend(selected.cursor_rules.iter().filter_map(|value| {
        value.rule.as_ref().map(|rule| {
            format!(
                "<rule path=\"{}\">\n{}\n</rule>",
                rule.full_path, rule.content
            )
        })
    }));
    sections.extend(selected.cursor_commands.iter().map(|value| {
        format!(
            "<command name=\"{}\">\n{}\n</command>",
            value.name, value.content
        )
    }));
    sections.extend(selected.selected_skills.iter().map(|value| {
        format!(
            "<skill path=\"{}\">\n{}\n{}\n</skill>",
            value.full_path, value.description, value.content
        )
    }));
    sections.extend(selected.external_links.iter().map(|value| {
        format!(
            "External link: {}{}",
            value.url,
            value
                .pdf_content
                .as_deref()
                .map(|content| format!("\n{content}"))
                .unwrap_or_default()
        )
    }));
    Some(sections.join("\n\n"))
}

pub fn dynamic_mcp(
    request: &pb::AgentRunRequest,
    context: &pb::RequestContext,
) -> Result<BTreeMap<String, (pb::McpToolDefinition, ToolDefinition)>> {
    // In meta mode Cursor has already decided which servers need their full
    // schemas in the request (for example browser/control servers) and keeps
    // the rest behind the dynamic catalog. Trust that selection instead of
    // merging the full request-level list back in.
    let meta_mode = active_meta_tool_options(context).is_some();
    let mut wires = Vec::new();
    if !meta_mode {
        wires.extend(
            request
                .mcp_tools
                .iter()
                .flat_map(|tools| tools.mcp_tools.iter()),
        );
    }
    wires.extend(context.tools.iter());
    let mut output = BTreeMap::new();
    for wire in wires {
        if wire.name.is_empty() {
            return Err(Error::Protocol(
                "MCP tool definition is missing name".into(),
            ));
        }
        let parameters = match wire.input_schema_json.as_deref() {
            Some(json) if !json.trim().is_empty() => serde_json::from_str(json)?,
            _ => prost_value(wire.input_schema.as_ref().ok_or_else(|| {
                Error::Protocol(format!("MCP tool {} is missing input schema", wire.name))
            })?),
        };
        let parameters = normalize_mcp_parameters(&wire.name, parameters)?;
        let name = normalize_tool_name(&wire.name);
        let definition = ToolDefinition {
            name: name.clone(),
            description: wire.description.clone(),
            parameters,
        };
        if output
            .insert(name.clone(), (wire.clone(), definition))
            .is_some()
        {
            return Err(Error::Protocol(format!(
                "duplicate MCP tool name after normalization: {name}"
            )));
        }
    }
    Ok(output)
}

fn normalize_mcp_parameters(tool_name: &str, mut parameters: Value) -> Result<Value> {
    let schema = parameters
        .as_object_mut()
        .ok_or_else(|| invalid_mcp_parameters(tool_name))?;
    match schema.get("type") {
        Some(Value::String(schema_type)) if schema_type == "object" => return Ok(parameters),
        Some(_) => return Err(invalid_mcp_parameters(tool_name)),
        None => {}
    }
    let object_only_union = ["anyOf", "oneOf"].into_iter().any(|keyword| {
        schema
            .get(keyword)
            .and_then(Value::as_array)
            .is_some_and(|branches| {
                !branches.is_empty()
                    && branches.iter().all(|branch| {
                        branch
                            .as_object()
                            .and_then(|branch| branch.get("type"))
                            .and_then(Value::as_str)
                            == Some("object")
                    })
            })
    });
    if !object_only_union {
        return Err(invalid_mcp_parameters(tool_name));
    }
    // OpenAI-compatible function schemas (and the corresponding schema
    // validators used by other providers) require the root schema to declare
    // an object type. Cursor's app-control MCP sometimes sends an object-only
    // `anyOf`/`oneOf` schema without that root annotation. Preserve the union
    // while adding the annotation to the model-facing copy.
    schema.insert("type".into(), Value::String("object".into()));
    Ok(parameters)
}

fn invalid_mcp_parameters(tool_name: &str) -> Error {
    Error::Protocol(format!(
        "MCP tool {tool_name} input schema must describe an object"
    ))
}

fn prost_value(value: &prost_types::Value) -> Value {
    use prost_types::value::Kind;
    match value.kind.as_ref() {
        None | Some(Kind::NullValue(_)) => Value::Null,
        Some(Kind::NumberValue(value)) => serde_json::Number::from_f64(*value)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        Some(Kind::StringValue(value)) => Value::String(value.clone()),
        Some(Kind::BoolValue(value)) => Value::Bool(*value),
        Some(Kind::StructValue(value)) => Value::Object(
            value
                .fields
                .iter()
                .map(|(key, value)| (key.clone(), prost_value(value)))
                .collect(),
        ),
        Some(Kind::ListValue(value)) => {
            Value::Array(value.values.iter().map(prost_value).collect())
        }
    }
}

#[cfg(test)]
mod testing;

#[cfg(test)]
mod tests {
    use super::testing::{mcp_descriptor, mcp_tool, rule, skill, typed_rule};
    use super::*;

    #[test]
    fn merge_local_rules_appends_and_dedupes_by_content() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("a.md"), "shared rule").unwrap();
        std::fs::write(directory.path().join("b.md"), "local only rule").unwrap();
        std::fs::write(directory.path().join("c.md"), "   \n").unwrap();

        let mut context = pb::RequestContext {
            non_file_rules: vec![rule("  shared rule  ")],
            ..Default::default()
        };
        merge_local_rules(&mut context, directory.path());

        let contents = context
            .non_file_rules
            .iter()
            .map(|rule| rule.content.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            contents,
            ["  shared rule  ", "local only rule"],
            "IDE-sent duplicate is kept once and blank local rules are skipped"
        );
    }

    #[test]
    fn merge_local_rules_survives_a_missing_directory() {
        let directory = tempfile::tempdir().unwrap();
        let mut context = pb::RequestContext::default();
        merge_local_rules(&mut context, &directory.path().join("nested/rules"));
        assert!(context.non_file_rules.is_empty());
    }

    #[test]
    fn merge_local_rules_survives_malformed_files() {
        let broken = tempfile::tempdir().unwrap();
        std::fs::write(broken.path().join("bad.md"), [0xff, 0xfe, 0x00]).unwrap();
        let mut context = pb::RequestContext {
            non_file_rules: vec![rule("ide rule")],
            ..Default::default()
        };
        merge_local_rules(&mut context, broken.path());
        assert_eq!(
            context
                .non_file_rules
                .iter()
                .map(|rule| rule.content.as_str())
                .collect::<Vec<_>>(),
            ["ide rule"],
            "a non-UTF-8 file aborts the local merge without dropping IDE rules"
        );

        let mixed = tempfile::tempdir().unwrap();
        std::fs::write(mixed.path().join("bad name.md"), "skipped").unwrap();
        std::fs::write(mixed.path().join("good.md"), "local rule").unwrap();
        let mut context = pb::RequestContext::default();
        merge_local_rules(&mut context, mixed.path());
        assert_eq!(
            context
                .non_file_rules
                .iter()
                .map(|rule| rule.content.as_str())
                .collect::<Vec<_>>(),
            ["local rule"],
            "an invalid file name is skipped without hiding valid rules"
        );
    }

    #[test]
    fn compile_context_separates_always_catalog_and_globbed_rules() {
        let context = pb::RequestContext {
            rules: vec![
                typed_rule(
                    "ALWAYS BODY",
                    pb::cursor_rule_type::Type::Global(pb::CursorRuleTypeGlobal::default()),
                ),
                typed_rule(
                    "AGENT BODY",
                    pb::cursor_rule_type::Type::AgentFetched(pb::CursorRuleTypeAgentFetched {
                        description: "API conventions".into(),
                    }),
                ),
                typed_rule(
                    "RUST BODY",
                    pb::cursor_rule_type::Type::FileGlobbed(pb::CursorRuleTypeFileGlobs {
                        globs: vec!["src/**/*.rs".into()],
                    }),
                ),
                typed_rule(
                    "MANUAL BODY",
                    pb::cursor_rule_type::Type::ManuallyAttached(
                        pb::CursorRuleTypeManuallyAttached::default(),
                    ),
                ),
            ],
            agent_skills: vec![
                skill("SKILL.md", "general skill", "SKILL BODY", &[]),
                skill("scoped/SKILL.md", "ts skill", "TS BODY", &["**/*.ts"]),
            ],
            ..Default::default()
        };

        let text = compile_context(&context, "2026-11-24", None);
        assert!(text.contains("<user_rule>\nALWAYS BODY\n</user_rule>"));
        assert!(text.contains(
            "<agent_rule fullPath=\"/repo/.cursor/rules/rule.mdc\" description=\"API conventions\"/>"
        ));
        assert!(text.contains(AGENT_RULES_INSTRUCTION));
        assert!(!text.contains("AGENT BODY"), "{text}");
        assert!(!text.contains("RUST BODY"), "{text}");
        assert!(!text.contains("MANUAL BODY"), "{text}");
        assert!(text.contains("<agent_skill fullPath=\"SKILL.md\" />"));
        assert!(!text.contains("fullPath=\"scoped/SKILL.md\""), "{text}");
    }

    #[test]
    fn compile_context_explains_agent_decided_rules() {
        let context = pb::RequestContext {
            rules: vec![typed_rule(
                "AGENT BODY",
                pb::cursor_rule_type::Type::AgentFetched(pb::CursorRuleTypeAgentFetched {
                    description: "API conventions".into(),
                }),
            )],
            ..Default::default()
        };

        let text = compile_context(&context, "2026-11-24", None);
        assert!(
            text.contains("<agent_rules>\nRules below are not loaded."),
            "{text}"
        );
        assert!(
            text.contains("read the file at its fullPath before continuing"),
            "{text}"
        );
    }

    #[test]
    fn compile_context_omits_the_agent_rule_catalog_without_agent_rules() {
        let context = pb::RequestContext {
            rules: vec![typed_rule(
                "ALWAYS BODY",
                pb::cursor_rule_type::Type::Global(pb::CursorRuleTypeGlobal::default()),
            )],
            ..Default::default()
        };

        let text = compile_context(&context, "2026-11-24", None);
        assert!(!text.contains("<agent_rules>"), "{text}");
    }

    #[test]
    fn skill_backed_rules_are_projected_once() {
        let context = pb::RequestContext {
            rules: vec![pb::CursorRule {
                full_path: "/repo/.agents/skills/tdd/SKILL.md".into(),
                content: "SKILL BODY".into(),
                ..Default::default()
            }],
            agent_skills: vec![skill(
                "/repo/.agents/skills/tdd/SKILL.md",
                "tdd skill",
                "SKILL BODY",
                &[],
            )],
            ..Default::default()
        };

        let text = compile_context(&context, "2026-11-24", None);
        assert!(
            text.contains("<agent_skill fullPath=\"$0/tdd/SKILL.md\" />"),
            "{text}"
        );
        assert!(!text.contains("<user_rule>"), "{text}");
    }

    #[test]
    fn rules_sharing_skill_content_are_projected_once() {
        let context = pb::RequestContext {
            rules: vec![pb::CursorRule {
                full_path: "/repo/.cursor/rules/duplicate.mdc".into(),
                content: "SKILL BODY".into(),
                ..Default::default()
            }],
            agent_skills: vec![skill(
                "/repo/.agents/skills/tdd/SKILL.md",
                "tdd skill",
                "SKILL BODY",
                &[],
            )],
            ..Default::default()
        };

        let text = compile_context(&context, "2026-11-24", None);
        assert!(
            text.contains("<agent_skill fullPath=\"$0/tdd/SKILL.md\" />"),
            "{text}"
        );
        assert!(
            !text.contains("<user_rule>"),
            "identical content is projected through the skill entry only: {text}"
        );
    }

    #[test]
    fn compile_context_escapes_rule_and_skill_attributes() {
        let mut rule = typed_rule(
            "AGENT BODY",
            pb::cursor_rule_type::Type::AgentFetched(pb::CursorRuleTypeAgentFetched {
                description: "x & y \"z\" <q>".into(),
            }),
        );
        rule.full_path = "/repo/a&b\"c<d>.mdc".into();
        let context = pb::RequestContext {
            rules: vec![rule],
            agent_skills: vec![skill(
                "/repo/s&k\"<1>/SKILL.md",
                "d & \"q\" <x>",
                "SKILL",
                &[],
            )],
            ..Default::default()
        };

        let text = compile_context(&context, "2026-11-24", None);
        assert!(
            text.contains("fullPath=\"/repo/a&amp;b&quot;c&lt;d&gt;.mdc\""),
            "{text}"
        );
        assert!(
            text.contains("description=\"x &amp; y &quot;z&quot; &lt;q&gt;\""),
            "{text}"
        );
        assert!(
            text.contains("fullPath=\"/repo/s&amp;k&quot;&lt;1&gt;/SKILL.md\""),
            "{text}"
        );
        assert!(
            text.contains("fullPath=\"/repo/s&amp;k&quot;&lt;1&gt;/SKILL.md\" />"),
            "user skills keep their path without a description: {text}"
        );
    }

    #[test]
    fn malformed_agent_rules_stay_inline() {
        let mut empty_description = typed_rule(
            "EMPTY DESCRIPTION",
            pb::cursor_rule_type::Type::AgentFetched(pb::CursorRuleTypeAgentFetched {
                description: "   ".into(),
            }),
        );
        empty_description.full_path = "/repo/empty.mdc".into();
        let mut missing_path = typed_rule(
            "MISSING PATH",
            pb::cursor_rule_type::Type::AgentFetched(pb::CursorRuleTypeAgentFetched {
                description: "has description".into(),
            }),
        );
        missing_path.full_path.clear();
        let context = pb::RequestContext {
            rules: vec![empty_description, missing_path],
            ..Default::default()
        };

        let text = compile_context(&context, "2026-11-24", None);
        assert!(
            text.contains("<user_rule>\nEMPTY DESCRIPTION\n</user_rule>"),
            "{text}"
        );
        assert!(
            text.contains("<user_rule>\nMISSING PATH\n</user_rule>"),
            "{text}"
        );
        assert!(!text.contains("<agent_rules>"), "{text}");
    }

    #[test]
    fn agent_decided_rules_are_catalogued_once_per_path() {
        let first = typed_rule(
            "FIRST BODY",
            pb::cursor_rule_type::Type::AgentFetched(pb::CursorRuleTypeAgentFetched {
                description: "first description".into(),
            }),
        );
        let mut second = typed_rule(
            "SECOND BODY",
            pb::cursor_rule_type::Type::AgentFetched(pb::CursorRuleTypeAgentFetched {
                description: "second description".into(),
            }),
        );
        second.full_path = first.full_path.clone();
        let context = pb::RequestContext {
            rules: vec![first, second],
            ..Default::default()
        };

        let text = compile_context(&context, "2026-11-24", None);
        assert_eq!(text.matches("<agent_rule ").count(), 1, "{text}");
        assert!(text.contains("description=\"first description\""), "{text}");
        assert!(!text.contains("second description"), "{text}");
    }

    #[test]
    fn blank_rule_bodies_are_skipped() {
        let context = pb::RequestContext {
            rules: vec![rule("   "), rule("\n\t")],
            ..Default::default()
        };

        let text = compile_context(&context, "2026-11-24", None);
        assert!(!text.contains("<rules>"), "{text}");
        assert!(!text.contains("<user_rule>"), "{text}");
    }

    #[test]
    fn compile_context_orders_rules_before_skills() {
        let context = pb::RequestContext {
            rules: vec![typed_rule(
                "ALWAYS BODY",
                pb::cursor_rule_type::Type::Global(pb::CursorRuleTypeGlobal::default()),
            )],
            agent_skills: vec![skill("s/SKILL.md", "a skill", "SKILL", &[])],
            ..Default::default()
        };

        let text = compile_context(&context, "2026-11-24", None);
        let rules = text.find("<rules>").expect("rules section");
        let skills = text.find("<agent_skills>").expect("skills section");
        assert!(rules < skills, "{text}");
    }

    #[test]
    fn compile_context_renders_dynamic_tool_namespaces() {
        let context = pb::RequestContext {
            mcp_meta_tool_options: Some(pb::McpMetaToolOptions {
                enabled: true,
                mcp_descriptors: vec![
                    mcp_descriptor(
                        "user-rea",
                        "REA",
                        &["search", "read"],
                        Some("REA instructions"),
                    ),
                    mcp_descriptor("user-eip", "EIP", &["lookup"], None),
                ],
            }),
            mcp_instructions: vec![pb::McpInstructions {
                server_identifier: "user-eip".into(),
                instructions: "EIP fallback".into(),
                ..Default::default()
            }],
            ..Default::default()
        };

        let text = compile_context(&context, "2026-11-24", None);
        assert!(text.contains("<dynamic_tools>"), "{text}");
        assert!(
            text.contains(
                "<namespace name=\"user-rea\" tools=\"search, read\" namespaceUseInstructions=\"REA instructions\" source=\"mcp\" />"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "<namespace name=\"user-eip\" tools=\"lookup\" namespaceUseInstructions=\"EIP fallback\" source=\"mcp\" />"
            ),
            "{text}"
        );
        assert!(
            text.contains("Use `GetMcpTools` to discover tool schemas"),
            "{text}"
        );
        assert!(text.contains("invoking it with `CallMcpTool`"), "{text}");
        assert!(!text.contains("<mcp_meta_tool"), "{text}");
        assert!(!text.contains("<input_schema>"), "{text}");
    }

    #[test]
    fn compile_context_renders_dynamic_catalog_for_a_single_tool() {
        let context = pb::RequestContext {
            mcp_meta_tool_options: Some(pb::McpMetaToolOptions {
                enabled: true,
                mcp_descriptors: vec![mcp_descriptor("solo", "Solo", &["only"], None)],
            }),
            ..Default::default()
        };

        let text = compile_context(&context, "2026-11-24", None);
        assert!(
            text.contains("<namespace name=\"solo\" tools=\"only\" source=\"mcp\" />"),
            "{text}"
        );
    }

    #[test]
    fn compile_context_catalogs_large_namespaces_without_schemas() {
        let tools = (0..250)
            .map(|index| format!("tool_{index}"))
            .collect::<Vec<_>>();
        let names = tools.iter().map(String::as_str).collect::<Vec<_>>();
        let context = pb::RequestContext {
            mcp_meta_tool_options: Some(pb::McpMetaToolOptions {
                enabled: true,
                mcp_descriptors: vec![mcp_descriptor("bulk", "Bulk", &names, None)],
            }),
            ..Default::default()
        };

        let text = compile_context(&context, "2026-11-24", None);
        assert_eq!(text.matches("<namespace ").count(), 1, "{text}");
        assert!(text.contains("tool_0, tool_1,"), "{text}");
        assert!(text.contains("tool_249"), "{text}");
        assert!(!text.contains("input_schema"), "{text}");
    }

    #[test]
    fn compile_context_skips_malformed_dynamic_namespaces() {
        let context = pb::RequestContext {
            mcp_meta_tool_options: Some(pb::McpMetaToolOptions {
                enabled: true,
                mcp_descriptors: vec![
                    mcp_descriptor("", "Missing identifier", &["orphan"], None),
                    mcp_descriptor("", "Missing identifier", &["orphan"], Some("ignored")),
                    mcp_descriptor(
                        "user-rea",
                        "REA",
                        &["", "ok", "ok", "a&\"<b>"],
                        Some("x & y \"z\" <q>"),
                    ),
                    mcp_descriptor("user-eip", "EIP", &[], None),
                    mcp_descriptor("resources", "Resources", &[], Some("resources only")),
                ],
            }),
            ..Default::default()
        };

        let text = compile_context(&context, "2026-11-24", None);
        assert!(!text.contains("orphan"), "{text}");
        assert!(
            text.contains(
                "<namespace name=\"user-rea\" tools=\"ok, a&amp;&quot;&lt;b&gt;\" namespaceUseInstructions=\"x &amp; y &quot;z&quot; &lt;q&gt;\" source=\"mcp\" />"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "<namespace name=\"resources\" tools=\"\" namespaceUseInstructions=\"resources only\" source=\"mcp\" />"
            ),
            "{text}"
        );
        assert!(!text.contains("name=\"user-eip\""), "{text}");
    }

    #[test]
    fn compile_context_omits_disabled_or_empty_dynamic_catalogs() {
        let disabled = pb::RequestContext {
            mcp_meta_tool_options: Some(pb::McpMetaToolOptions {
                enabled: false,
                mcp_descriptors: vec![mcp_descriptor("server", "Server", &["tool"], None)],
            }),
            ..Default::default()
        };
        assert!(!compile_context(&disabled, "2026-11-24", None).contains("<dynamic_tools>"));

        let empty = pb::RequestContext {
            mcp_meta_tool_options: Some(pb::McpMetaToolOptions {
                enabled: true,
                mcp_descriptors: vec![],
            }),
            ..Default::default()
        };
        assert!(!compile_context(&empty, "2026-11-24", None).contains("<dynamic_tools>"));
    }

    #[test]
    fn dynamic_mcp_meta_mode_uses_only_client_selected_tools() {
        let request = pb::AgentRunRequest {
            mcp_tools: Some(pb::McpTools {
                mcp_tools: vec![
                    mcp_tool("user-rea-search", "REA", "search"),
                    mcp_tool("user-eip-lookup", "EIP", "lookup"),
                ],
            }),
            ..Default::default()
        };
        let context = pb::RequestContext {
            tools: vec![mcp_tool("user-rea-search", "REA", "search")],
            mcp_meta_tool_options: Some(pb::McpMetaToolOptions {
                enabled: true,
                mcp_descriptors: vec![
                    mcp_descriptor("user-rea", "REA", &["search"], None),
                    mcp_descriptor("user-eip", "EIP", &["lookup"], None),
                ],
            }),
            ..Default::default()
        };

        let tools = dynamic_mcp(&request, &context).unwrap();
        assert_eq!(
            tools.keys().cloned().collect::<Vec<_>>(),
            ["user-rea-search"],
            "meta mode must trust Cursor's request-context tool selection"
        );
        assert_eq!(
            tools["user-rea-search"].1.parameters,
            serde_json::json!({"type": "object", "properties": {}})
        );
    }

    #[test]
    fn dynamic_mcp_meta_mode_allows_no_direct_tools() {
        let request = pb::AgentRunRequest {
            mcp_tools: Some(pb::McpTools {
                mcp_tools: vec![mcp_tool("user-rea-search", "REA", "search")],
            }),
            ..Default::default()
        };
        let context = pb::RequestContext {
            mcp_meta_tool_options: Some(pb::McpMetaToolOptions {
                enabled: true,
                mcp_descriptors: vec![mcp_descriptor("user-rea", "REA", &["search"], None)],
            }),
            ..Default::default()
        };

        assert!(dynamic_mcp(&request, &context).unwrap().is_empty());
    }

    #[test]
    fn dynamic_mcp_full_mode_merges_request_and_context_tools() {
        let request = pb::AgentRunRequest {
            mcp_tools: Some(pb::McpTools {
                mcp_tools: vec![mcp_tool("request-tool", "Request", "tool")],
            }),
            ..Default::default()
        };
        let context = pb::RequestContext {
            tools: vec![mcp_tool("context-tool", "Context", "tool")],
            ..Default::default()
        };

        let tools = dynamic_mcp(&request, &context).unwrap();
        assert_eq!(
            tools.keys().cloned().collect::<Vec<_>>(),
            ["context-tool", "request-tool"]
        );
    }

    #[test]
    fn dynamic_mcp_rejects_malformed_tool_data() {
        let request = pb::AgentRunRequest {
            mcp_tools: Some(pb::McpTools {
                mcp_tools: vec![mcp_tool("", "Server", "tool")],
            }),
            ..Default::default()
        };
        assert!(dynamic_mcp(&request, &pb::RequestContext::default())
            .unwrap_err()
            .to_string()
            .contains("missing name"));

        let request = pb::AgentRunRequest {
            mcp_tools: Some(pb::McpTools {
                mcp_tools: vec![pb::McpToolDefinition {
                    name: "bad-schema".into(),
                    input_schema_json: Some("[]".into()),
                    ..Default::default()
                }],
            }),
            ..Default::default()
        };
        assert!(dynamic_mcp(&request, &pb::RequestContext::default())
            .unwrap_err()
            .to_string()
            .contains("must describe an object"));

        let request = pb::AgentRunRequest {
            mcp_tools: Some(pb::McpTools {
                mcp_tools: vec![
                    mcp_tool("same name", "First", "tool"),
                    mcp_tool("same_name", "Second", "tool"),
                ],
            }),
            ..Default::default()
        };
        assert!(dynamic_mcp(&request, &pb::RequestContext::default())
            .unwrap_err()
            .to_string()
            .contains("duplicate MCP tool name"));
    }

    #[test]
    fn meta_mcp_routes_skip_blank_descriptors() {
        let context = pb::RequestContext {
            mcp_meta_tool_options: Some(pb::McpMetaToolOptions {
                enabled: true,
                mcp_descriptors: vec![
                    mcp_descriptor("", "No identifier", &["a"], None),
                    mcp_descriptor(" server ", " Server ", &["", " ok "], Some("instructions")),
                ],
            }),
            ..Default::default()
        };

        let routes = meta_mcp_routes(&context);
        assert_eq!(
            routes.keys().cloned().collect::<Vec<_>>(),
            [("server".to_string(), "ok".to_string())]
        );
        let route = &routes[&("server".to_string(), "ok".to_string())];
        assert_eq!(route.name, "server-ok");
        assert_eq!(route.provider_identifier, "Server");
        assert_eq!(route.tool_name, "ok");
        assert!(route.description.is_empty());
    }
}
