//! Shared builders for request-context unit tests.
use crate::cursor::protocol::proto::agent::v1 as pb;

pub(super) fn rule(content: &str) -> pb::CursorRule {
    pb::CursorRule {
        content: content.into(),
        ..Default::default()
    }
}

pub(super) fn typed_rule(content: &str, kind: pb::cursor_rule_type::Type) -> pb::CursorRule {
    pb::CursorRule {
        full_path: "/repo/.cursor/rules/rule.mdc".into(),
        content: content.into(),
        r#type: Some(pb::CursorRuleType { r#type: Some(kind) }),
        ..Default::default()
    }
}

pub(super) fn frontmatter_rule(content: &str, frontmatter: &str) -> pb::CursorRule {
    pb::CursorRule {
        full_path: "/repo/.cursor/rules/rule.mdc".into(),
        content: content.into(),
        frontmatter: frontmatter.into(),
        ..Default::default()
    }
}

pub(super) fn skill(
    path: &str,
    description: &str,
    content: &str,
    globs: &[&str],
) -> pb::AgentSkill {
    pb::AgentSkill {
        full_path: path.into(),
        description: description.into(),
        content: content.into(),
        globs: globs.iter().map(|glob| (*glob).to_string()).collect(),
        ..Default::default()
    }
}

pub(super) fn mcp_descriptor(
    identifier: &str,
    name: &str,
    tools: &[&str],
    instructions: Option<&str>,
) -> pb::McpDescriptor {
    pb::McpDescriptor {
        server_identifier: identifier.into(),
        server_name: name.into(),
        server_use_instructions: instructions.map(str::to_string),
        tools: tools
            .iter()
            .map(|tool| pb::McpToolDescriptor {
                tool_name: (*tool).into(),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

pub(super) fn mcp_tool(
    name: &str,
    provider_identifier: &str,
    tool_name: &str,
) -> pb::McpToolDefinition {
    pb::McpToolDefinition {
        name: name.into(),
        provider_identifier: provider_identifier.into(),
        tool_name: tool_name.into(),
        description: format!("{tool_name} description"),
        input_schema_json: Some(r#"{"type":"object","properties":{}}"#.into()),
        ..Default::default()
    }
}
