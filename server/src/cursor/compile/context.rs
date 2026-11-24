//! Compiles rules, skills, MCP metadata, and environment context.
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use prost::Message;
use serde::Deserialize;
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

pub fn compile_context(context: &pb::RequestContext, today: &str) -> String {
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
    let skill_paths = context
        .agent_skills
        .iter()
        .map(|skill| skill.full_path.as_str())
        .filter(|path| !path.is_empty())
        .collect::<HashSet<_>>();
    let skill_contents = context
        .agent_skills
        .iter()
        .map(|skill| skill.content.as_str())
        .filter(|content| !content.is_empty())
        .collect::<HashSet<_>>();
    let mut rules = Vec::new();
    let mut catalogs = Vec::new();
    let mut catalogued = HashSet::new();
    for rule in context.rules.iter().chain(context.non_file_rules.iter()) {
        if is_skill(rule, &skill_paths, &skill_contents) {
            continue;
        }
        match rule_kind(rule) {
            RuleKind::Always => push_rule_content(&mut rules, rule),
            RuleKind::AgentFetched { description } => {
                if description.trim().is_empty() || rule.full_path.trim().is_empty() {
                    push_rule_content(&mut rules, rule);
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
            RuleKind::Globbed { .. } | RuleKind::Manual => {}
        }
    }
    rules.extend(
        context
            .cloud_rule
            .iter()
            .map(|rule| format!("<user_rule>\n{rule}\n</user_rule>")),
    );
    if !rules.is_empty() {
        sections.push(format!("<rules>\n{}\n</rules>", rules.join("\n")));
    }
    if !catalogs.is_empty() {
        sections.push(format!(
            "<agent_rules>\n{}\n</agent_rules>",
            catalogs.join("\n")
        ));
    }
    let skills = context
        .agent_skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation && skill.globs.is_empty())
        .map(skill_entry)
        .collect::<Vec<_>>();
    if !skills.is_empty() {
        sections.push(format!(
            "<agent_skills>\n<available_skills>\n{}\n</available_skills>\n</agent_skills>",
            skills.join("\n")
        ));
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
    {
        let servers = context
            .mcp_meta_tool_options
            .as_ref()
            .into_iter()
            .flat_map(|options| &options.mcp_descriptors)
            .filter_map(compile_mcp_descriptor)
            .collect::<Vec<_>>();
        if !servers.is_empty() {
            sections.push(format!(
                "<mcp_meta_tools>\nThe following MCP tools are available. Call a listed tool directly with CallMcpTool without calling GetMcpTools first. If a call returns an error, use it to correct the arguments or authentication and retry when appropriate.\n<mcp_meta_tool_servers>\n{}\n</mcp_meta_tool_servers>\n</mcp_meta_tools>",
                servers.join("\n")
            ));
        }
    }
    sections.join("\n\n")
}

fn compile_mcp_descriptor(server: &pb::McpDescriptor) -> Option<String> {
    if server.server_identifier.trim().is_empty() {
        return None;
    }
    let tools = server
        .tools
        .iter()
        .filter(|tool| !tool.tool_name.trim().is_empty())
        .map(|tool| {
            let mut lines = vec![format!("<mcp_tool name=\"{}\">", xml(&tool.tool_name))];
            if let Some(path) = tool
                .definition_path
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                lines.push(format!("<definition_path>{}</definition_path>", xml(path)));
            }
            if let Some(description) = tool
                .description
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                lines.push(format!("<description>{}</description>", xml(description)));
            }
            if let Some(schema) = mcp_input_schema(tool) {
                lines.push(format!("<input_schema>{}</input_schema>", xml(&schema)));
            }
            lines.push("</mcp_tool>".into());
            lines.join("\n")
        })
        .collect::<Vec<_>>();
    if tools.is_empty() {
        return None;
    }
    Some(format!(
        "<mcp_meta_tool_server name=\"{}\" identifier=\"{}\">\n<tools>\n{}\n</tools>\n</mcp_meta_tool_server>",
        xml(if server.server_name.trim().is_empty() {
            &server.server_identifier
        } else {
            &server.server_name
        }),
        xml(&server.server_identifier),
        tools.join("\n"),
    ))
}

fn mcp_input_schema(tool: &pb::McpToolDescriptor) -> Option<String> {
    tool.input_schema_json
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            serde_json::from_str::<Value>(value)
                .map(|value| value.to_string())
                .unwrap_or_else(|_| value.to_string())
        })
        .or_else(|| {
            tool.input_schema
                .as_ref()
                .map(prost_value)
                .map(|value| value.to_string())
        })
}

pub fn meta_mcp_routes(context: &pb::RequestContext) -> HashMap<(String, String), McpRoute> {
    context
        .mcp_meta_tool_options
        .as_ref()
        .into_iter()
        .flat_map(|options| &options.mcp_descriptors)
        .filter(|server| !server.server_identifier.trim().is_empty())
        .flat_map(|server| {
            server.tools.iter().filter_map(move |tool| {
                if tool.tool_name.trim().is_empty() {
                    return None;
                }
                let provider_identifier = if server.server_name.trim().is_empty() {
                    server.server_identifier.clone()
                } else {
                    server.server_name.clone()
                };
                Some((
                    (server.server_identifier.clone(), tool.tool_name.clone()),
                    McpRoute {
                        name: format!("{}-{}", server.server_identifier, tool.tool_name),
                        provider_identifier,
                        tool_name: tool.tool_name.clone(),
                        description: tool.description.clone().unwrap_or_default(),
                    },
                ))
            })
        })
        .collect()
}

fn is_skill_rule(rule: &pb::CursorRule) -> bool {
    Path::new(&rule.full_path)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("SKILL.md"))
}

fn is_skill(
    rule: &pb::CursorRule,
    skill_paths: &HashSet<&str>,
    skill_contents: &HashSet<&str>,
) -> bool {
    is_skill_rule(rule)
        || skill_paths.contains(rule.full_path.as_str())
        || skill_contents.contains(rule.content.as_str())
}

fn push_rule_content(rules: &mut Vec<String>, rule: &pb::CursorRule) {
    if !rule.content.trim().is_empty() {
        rules.push(format!("<user_rule>\n{}\n</user_rule>", rule.content));
    }
}

fn skill_entry(skill: &pb::AgentSkill) -> String {
    format!(
        "<agent_skill fullPath=\"{}\">{}</agent_skill>",
        xml(&skill.full_path),
        xml(&skill.description),
    )
}

/// Cursor's four project-rule shapes. The protocol carries the same split in
/// `CursorRule.type`; local markdown rules have no metadata and stay always-on.
#[derive(Clone, Debug, PartialEq)]
enum RuleKind {
    Always,
    AgentFetched { description: String },
    Globbed { globs: Vec<String> },
    Manual,
}

fn rule_kind(rule: &pb::CursorRule) -> RuleKind {
    if rule.is_required == Some(true) {
        return RuleKind::Always;
    }
    if let Some(kind) = rule.r#type.as_ref().and_then(|kind| kind.r#type.as_ref()) {
        return match kind {
            pb::cursor_rule_type::Type::Global(_) => RuleKind::Always,
            pb::cursor_rule_type::Type::FileGlobbed(part) => {
                let globs = normalize_globs(&part.globs);
                if globs.is_empty() {
                    RuleKind::Always
                } else {
                    RuleKind::Globbed { globs }
                }
            }
            pb::cursor_rule_type::Type::AgentFetched(part) => RuleKind::AgentFetched {
                description: part.description.clone(),
            },
            pb::cursor_rule_type::Type::ManuallyAttached(_) => RuleKind::Manual,
        };
    }
    let frontmatter = rule_frontmatter(rule);
    if frontmatter.always_apply == Some(true) {
        return RuleKind::Always;
    }
    if let Some(globs) = frontmatter.globs.map(GlobsField::into_vec) {
        if !globs.is_empty() {
            return RuleKind::Globbed { globs };
        }
    }
    if let Some(description) = frontmatter
        .description
        .filter(|description| !description.trim().is_empty())
    {
        return RuleKind::AgentFetched { description };
    }
    if frontmatter.always_apply == Some(false) {
        return RuleKind::Manual;
    }
    RuleKind::Always
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RuleFrontmatter {
    #[serde(rename = "alwaysApply")]
    always_apply: Option<bool>,
    globs: Option<GlobsField>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum GlobsField {
    Single(String),
    Multiple(Vec<String>),
}

impl GlobsField {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::Single(value) => normalize_globs(&[value]),
            Self::Multiple(values) => normalize_globs(&values),
        }
    }
}

fn rule_frontmatter(rule: &pb::CursorRule) -> RuleFrontmatter {
    let raw = rule.frontmatter.trim();
    if raw.is_empty() {
        return RuleFrontmatter::default();
    }
    serde_yaml::from_str(&frontmatter_block(raw)).unwrap_or_default()
}

/// Accepts both a bare YAML block and a full `---` delimited frontmatter block.
fn frontmatter_block(raw: &str) -> String {
    let mut lines = raw.lines();
    if lines.clone().next().map(str::trim) == Some("---") {
        lines.next();
    }
    lines
        .take_while(|line| line.trim() != "---")
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_globs(values: &[String]) -> Vec<String> {
    values
        .iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

/// Workspace-relative paths available to this turn, used for glob attachment.
#[derive(Debug, Default)]
pub(crate) struct TurnFiles {
    relative: Vec<String>,
}

impl TurnFiles {
    fn is_empty(&self) -> bool {
        self.relative.is_empty()
    }

    fn push(&mut self, path: &str, workspace: &str) {
        if let Some(path) = workspace_relative(path, workspace) {
            if !self.relative.contains(&path) {
                self.relative.push(path);
            }
        }
    }
}

pub(crate) fn ide_state(user: &pb::UserMessage) -> Option<&pb::invocation_context::IdeState> {
    match user
        .selected_context
        .as_ref()?
        .invocation_context
        .as_ref()?
        .data
        .as_ref()?
    {
        pb::invocation_context::Data::IdeState(state) => Some(state),
        _ => None,
    }
}

pub(crate) fn turn_files(user: &pb::UserMessage, workspace: &str) -> TurnFiles {
    let mut files = TurnFiles::default();
    let Some(selected) = user.selected_context.as_ref() else {
        return files;
    };
    if let Some(state) = ide_state(user) {
        for file in state
            .visible_files
            .iter()
            .chain(state.recently_viewed_files.iter())
        {
            files.push(
                file.relative_path.as_deref().unwrap_or(&file.path),
                workspace,
            );
        }
    }
    for file in &selected.files {
        files.push(
            file.relative_path.as_deref().unwrap_or(&file.path),
            workspace,
        );
    }
    for selection in &selected.code_selections {
        files.push(&selection.path, workspace);
    }
    for folder in &selected.folders {
        files.push(
            folder.relative_path.as_deref().unwrap_or(&folder.path),
            workspace,
        );
    }
    files
}

/// Turn-scoped rules and skills whose globs match the files of this turn.
pub(crate) fn attached_context(context: &pb::RequestContext, files: &TurnFiles) -> Option<String> {
    if files.is_empty() {
        return None;
    }
    let skill_paths = context
        .agent_skills
        .iter()
        .map(|skill| skill.full_path.as_str())
        .filter(|path| !path.is_empty())
        .collect::<HashSet<_>>();
    let skill_contents = context
        .agent_skills
        .iter()
        .map(|skill| skill.content.as_str())
        .filter(|content| !content.is_empty())
        .collect::<HashSet<_>>();
    let mut sections = Vec::new();
    let mut rules = Vec::new();
    for rule in context.rules.iter().chain(context.non_file_rules.iter()) {
        if is_skill(rule, &skill_paths, &skill_contents) {
            continue;
        }
        let RuleKind::Globbed { globs } = rule_kind(rule) else {
            continue;
        };
        if glob_matches(&globs, files) {
            push_rule_content(&mut rules, rule);
        }
    }
    if !rules.is_empty() {
        sections.push(format!("<rules>\n{}\n</rules>", rules.join("\n")));
    }
    let skills = context
        .agent_skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation && !skill.globs.is_empty())
        .filter(|skill| glob_matches(&normalize_globs(&skill.globs), files))
        .map(skill_entry)
        .collect::<Vec<_>>();
    if !skills.is_empty() {
        sections.push(format!(
            "<agent_skills>\n<available_skills>\n{}\n</available_skills>\n</agent_skills>",
            skills.join("\n")
        ));
    }
    (!sections.is_empty()).then(|| sections.join("\n\n"))
}

fn glob_matches(globs: &[String], files: &TurnFiles) -> bool {
    let Some(set) = glob_set(globs) else {
        return false;
    };
    files.relative.iter().any(|file| set.is_match(file))
}

fn glob_set(globs: &[String]) -> Option<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    let mut added = false;
    for pattern in normalize_globs(globs) {
        // `literal_separator` keeps `*` inside one path segment, matching the
        // rule glob semantics Cursor documents (`*.ts` is root-level only).
        if let Ok(glob) = GlobBuilder::new(&pattern).literal_separator(true).build() {
            builder.add(glob);
            added = true;
        }
    }
    added.then(|| builder.build().ok()).flatten()
}

fn workspace_relative(path: &str, workspace: &str) -> Option<String> {
    let path = path.trim().replace('\\', "/");
    if path.is_empty() {
        return None;
    }
    let workspace = workspace.trim().replace('\\', "/");
    let workspace = workspace.trim_end_matches('/');
    let relative = if !workspace.is_empty() && path.starts_with(workspace) {
        path[workspace.len()..].trim_start_matches('/')
    } else {
        path.as_str()
    };
    let relative = relative
        .trim_start_matches("./")
        .trim_start_matches('/')
        .trim_end_matches('/');
    (!relative.is_empty()).then(|| relative.to_string())
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
    let direct = request
        .mcp_tools
        .iter()
        .flat_map(|tools| tools.mcp_tools.iter());
    let contextual = context.tools.iter();
    let mut output = BTreeMap::new();
    for wire in direct.chain(contextual) {
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

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(content: &str) -> pb::CursorRule {
        pb::CursorRule {
            content: content.into(),
            ..Default::default()
        }
    }

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

    fn typed_rule(content: &str, kind: pb::cursor_rule_type::Type) -> pb::CursorRule {
        pb::CursorRule {
            full_path: "/repo/.cursor/rules/rule.mdc".into(),
            content: content.into(),
            r#type: Some(pb::CursorRuleType { r#type: Some(kind) }),
            ..Default::default()
        }
    }

    fn frontmatter_rule(content: &str, frontmatter: &str) -> pb::CursorRule {
        pb::CursorRule {
            full_path: "/repo/.cursor/rules/rule.mdc".into(),
            content: content.into(),
            frontmatter: frontmatter.into(),
            ..Default::default()
        }
    }

    fn skill(path: &str, description: &str, content: &str, globs: &[&str]) -> pb::AgentSkill {
        pb::AgentSkill {
            full_path: path.into(),
            description: description.into(),
            content: content.into(),
            globs: globs.iter().map(|glob| (*glob).to_string()).collect(),
            ..Default::default()
        }
    }

    fn turn_files(paths: &[&str]) -> TurnFiles {
        let mut files = TurnFiles::default();
        for path in paths {
            files.push(path, "/repo");
        }
        files
    }

    #[test]
    fn rule_kind_follows_the_protocol_type() {
        assert_eq!(
            rule_kind(&typed_rule(
                "always",
                pb::cursor_rule_type::Type::Global(pb::CursorRuleTypeGlobal::default()),
            )),
            RuleKind::Always
        );
        assert_eq!(
            rule_kind(&typed_rule(
                "globbed",
                pb::cursor_rule_type::Type::FileGlobbed(pb::CursorRuleTypeFileGlobs {
                    globs: vec!["src/**/*.rs".into()],
                }),
            )),
            RuleKind::Globbed {
                globs: vec!["src/**/*.rs".into()],
            }
        );
        assert_eq!(
            rule_kind(&typed_rule(
                "described",
                pb::cursor_rule_type::Type::AgentFetched(pb::CursorRuleTypeAgentFetched {
                    description: "RPC conventions".into(),
                }),
            )),
            RuleKind::AgentFetched {
                description: "RPC conventions".into(),
            }
        );
        assert_eq!(
            rule_kind(&typed_rule(
                "manual",
                pb::cursor_rule_type::Type::ManuallyAttached(
                    pb::CursorRuleTypeManuallyAttached::default(),
                ),
            )),
            RuleKind::Manual
        );
        assert_eq!(
            rule_kind(&typed_rule(
                "empty globs",
                pb::cursor_rule_type::Type::FileGlobbed(pb::CursorRuleTypeFileGlobs::default()),
            )),
            RuleKind::Always,
            "a file_globbed rule without patterns must not be dropped"
        );
    }

    #[test]
    fn rule_kind_falls_back_to_frontmatter() {
        assert_eq!(
            rule_kind(&frontmatter_rule(
                "body",
                "---\nalwaysApply: true\nglobs: src/**\n---",
            )),
            RuleKind::Always,
            "alwaysApply wins over globs"
        );
        assert_eq!(
            rule_kind(&frontmatter_rule(
                "body",
                "---\nalwaysApply: false\nglobs: src/**/*.tsx, docs/**/*.md\n---",
            )),
            RuleKind::Globbed {
                globs: vec!["src/**/*.tsx".into(), "docs/**/*.md".into()],
            }
        );
        assert_eq!(
            rule_kind(&frontmatter_rule(
                "body",
                "---\nalwaysApply: false\ndescription: RPC conventions\n---",
            )),
            RuleKind::AgentFetched {
                description: "RPC conventions".into(),
            }
        );
        assert_eq!(
            rule_kind(&frontmatter_rule("body", "---\nalwaysApply: false\n---")),
            RuleKind::Manual
        );
    }

    #[test]
    fn rule_kind_defaults_to_always_without_metadata() {
        assert_eq!(rule_kind(&rule("local markdown")), RuleKind::Always);
        let mut required = rule("team rule");
        required.is_required = Some(true);
        assert_eq!(rule_kind(&required), RuleKind::Always);
    }

    #[test]
    fn workspace_relative_strips_the_workspace_root() {
        assert_eq!(
            workspace_relative("/repo/src/main.rs", "/repo"),
            Some("src/main.rs".into())
        );
        assert_eq!(
            workspace_relative("C:\\repo\\src\\main.rs", "C:/repo/"),
            Some("src/main.rs".into())
        );
        assert_eq!(
            workspace_relative("./src/main.rs", ""),
            Some("src/main.rs".into())
        );
        assert_eq!(workspace_relative("   ", "/repo"), None);
    }

    #[test]
    fn glob_matching_uses_workspace_relative_paths() {
        let files = turn_files(&["src/app/main.rs", "docs/readme.md"]);
        assert!(glob_matches(&["src/**/*.rs".into()], &files));
        assert!(glob_matches(&["*.rs, docs/**/*.md".into()], &files));
        assert!(!glob_matches(&["**/*.tsx".into()], &files));
        assert!(!glob_matches(&["src/*.rs".into()], &files));
        assert!(!glob_matches(
            &["src/**/*.rs".into()],
            &TurnFiles::default()
        ));
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

        let text = compile_context(&context, "2026-11-24");
        assert!(text.contains("<user_rule>\nALWAYS BODY\n</user_rule>"));
        assert!(text.contains(
            "<agent_rule fullPath=\"/repo/.cursor/rules/rule.mdc\" description=\"API conventions\"/>"
        ));
        assert!(!text.contains("AGENT BODY"), "{text}");
        assert!(!text.contains("RUST BODY"), "{text}");
        assert!(!text.contains("MANUAL BODY"), "{text}");
        assert!(text.contains("general skill"));
        assert!(!text.contains("ts skill"), "{text}");
    }

    #[test]
    fn attached_context_includes_only_matching_globs() {
        let context = pb::RequestContext {
            rules: vec![typed_rule(
                "RUST BODY",
                pb::cursor_rule_type::Type::FileGlobbed(pb::CursorRuleTypeFileGlobs {
                    globs: vec!["src/**/*.rs".into()],
                }),
            )],
            agent_skills: vec![skill(
                "scoped/SKILL.md",
                "ts skill",
                "TS BODY",
                &["**/*.ts"],
            )],
            ..Default::default()
        };

        assert!(
            attached_context(&context, &turn_files(&["docs/readme.md"])).is_none(),
            "no glob matches means nothing is attached"
        );
        let rust = attached_context(&context, &turn_files(&["src/main.rs"])).unwrap();
        assert!(
            rust.contains("<user_rule>\nRUST BODY\n</user_rule>"),
            "{rust}"
        );
        assert!(!rust.contains("ts skill"), "{rust}");
        let typescript = attached_context(&context, &turn_files(&["app/web.ts"])).unwrap();
        assert!(typescript.contains("ts skill"), "{typescript}");
        assert!(!typescript.contains("RUST BODY"), "{typescript}");
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

        let text = compile_context(&context, "2026-11-24");
        assert!(text.contains("tdd skill"));
        assert!(!text.contains("<user_rule>"), "{text}");
    }
}
