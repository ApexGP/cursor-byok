//! Builds Cursor usage and context breakdown data.
use std::collections::HashSet;

use crate::{
    cursor::protocol::proto::agent::v1 as pb,
    model::{CanonicalMessage, ContentPart, MessageContent, Origin, ToolDefinition},
    Result,
};

const CATEGORIES: [(&str, &str); 8] = [
    ("system_prompt", "System prompt"),
    ("tools", "Tool definitions"),
    ("rules", "Rules"),
    ("skills", "Skills"),
    ("mcp", "MCP & dynamic tools"),
    ("subagents", "Subagent definitions"),
    ("summarized_conversation", "Summarized conversation"),
    ("conversation", "Conversation"),
];
const EASTER_EGG_CATEGORY: (&str, &str) = ("leookun", "@leookun stole 1 token 😂");

const SYSTEM: usize = 0;
const TOOLS: usize = 1;
const RULES: usize = 2;
const SKILLS: usize = 3;
const MCP: usize = 4;
const SUBAGENTS: usize = 5;
const SUMMARY: usize = 6;
const CONVERSATION: usize = 7;

#[derive(Clone, Copy, Default)]
struct Measure {
    characters: u64,
    token_units: u64,
}

impl Measure {
    fn add(&mut self, text: &str) {
        self.characters += text.encode_utf16().count() as u64;
        let mut units = 0_u64;
        for character in text.chars() {
            let width = character.len_utf16() as u64;
            units += if character.is_ascii() {
                width * 273
            } else {
                width * 550
            };
        }
        self.token_units += units;
    }

    fn estimated_tokens(self) -> u64 {
        self.token_units.div_ceil(1_000)
    }
}

pub(crate) fn breakdown(
    used_tokens: u32,
    max_tokens: u32,
    baseline: Option<&pb::PromptTokenBreakdownSnapshot>,
    instructions: &str,
    tools: &[ToolDefinition],
    dynamic_tools: &HashSet<String>,
    messages: &[CanonicalMessage],
) -> Result<pb::PromptTokenBreakdownSnapshot> {
    let mut measures = [Measure::default(); 8];
    measures[SYSTEM].add(instructions);
    for tool in tools {
        let encoded = serde_json::to_string(tool)?;
        if dynamic_tools.contains(&tool.name) {
            measures[MCP].add(&encoded);
        } else {
            measures[TOOLS].add(&encoded);
        }
    }
    for message in messages {
        measure_message(message, &mut measures)?;
    }

    let mut estimates = [0_u64; 8];
    for index in 0..CONVERSATION {
        estimates[index] = measures[index].estimated_tokens();
    }
    if measures[SUMMARY].characters != 0 {
        estimates[SUMMARY] = measures[SUMMARY].estimated_tokens();
    } else if let Some(summary) = baseline.and_then(|snapshot| {
        snapshot
            .categories
            .iter()
            .find(|category| category.id == CATEGORIES[SUMMARY].0)
    }) {
        measures[SUMMARY].characters = summary.character_count.unwrap_or(0) as u64;
        estimates[SUMMARY] = summary.estimated_tokens as u64;
    }
    let easter_egg_tokens = 1_u64;
    let categorized_tokens = used_tokens as u64;
    fit_special_estimates(&mut estimates, categorized_tokens);
    estimates[CONVERSATION] =
        categorized_tokens.saturating_sub(estimates[..CONVERSATION].iter().sum::<u64>());

    let mut categories = CATEGORIES
        .iter()
        .enumerate()
        .map(|(index, (id, label))| pb::PromptTokenBreakdownCategory {
            id: (*id).into(),
            label: (*label).into(),
            estimated_tokens: estimates[index].min(u32::MAX as u64) as u32,
            character_count: (measures[index].characters != 0)
                .then_some(measures[index].characters.min(u32::MAX as u64) as u32),
        })
        .collect::<Vec<_>>();
    categories.push(pb::PromptTokenBreakdownCategory {
        id: EASTER_EGG_CATEGORY.0.into(),
        label: EASTER_EGG_CATEGORY.1.into(),
        estimated_tokens: easter_egg_tokens as u32,
        character_count: None,
    });
    Ok(pb::PromptTokenBreakdownSnapshot {
        total_used_tokens: used_tokens,
        max_tokens,
        categories,
    })
}

fn measure_message(message: &CanonicalMessage, measures: &mut [Measure; 8]) -> Result<()> {
    match &message.content {
        MessageContent::Parts { parts } => {
            for part in parts {
                if let ContentPart::Text { text } = part {
                    if is_section_message(message) {
                        measure_sections(text, measures);
                    } else {
                        measures[CONVERSATION].add(text);
                    }
                }
            }
        }
        MessageContent::Assistant {
            text,
            thinking,
            tool_calls,
            ..
        } => {
            measures[CONVERSATION].add(text);
            measures[CONVERSATION].add(thinking);
            measures[CONVERSATION].add(&serde_json::to_string(tool_calls)?);
        }
        MessageContent::ToolResult(result) => {
            measures[CONVERSATION].add(&serde_json::to_string(result)?);
        }
    }
    Ok(())
}

/// Messages whose text carries independently attributable sections: the
/// per-turn runtime message and the append-only request context.
fn is_section_message(message: &CanonicalMessage) -> bool {
    message.origin == Origin::Runtime || message.message_id.starts_with("request-context:")
}

fn measure_sections(text: &str, measures: &mut [Measure; 8]) {
    let mut ranges = Vec::new();
    collect_ranges(text, "rules", RULES, &mut ranges);
    collect_ranges(text, "rule", RULES, &mut ranges);
    collect_ranges(text, "agent_rules", RULES, &mut ranges);
    collect_ranges(text, "agent_skills", SKILLS, &mut ranges);
    collect_ranges(text, "skill", SKILLS, &mut ranges);
    collect_ranges(text, "subagents", SUBAGENTS, &mut ranges);
    collect_ranges(text, "dynamic_tools", MCP, &mut ranges);
    collect_ranges(text, "conversation_summary", SUMMARY, &mut ranges);
    ranges.sort_by_key(|range| range.0);

    let mut cursor = 0;
    for (start, end, category) in ranges {
        if start < cursor {
            continue;
        }
        measures[CONVERSATION].add(&text[cursor..start]);
        measures[category].add(&text[start..end]);
        cursor = end;
    }
    measures[CONVERSATION].add(&text[cursor..]);
}

fn collect_ranges(text: &str, tag: &str, category: usize, output: &mut Vec<(usize, usize, usize)>) {
    let opening = format!("<{tag}");
    let closing = format!("</{tag}>");
    let mut cursor = 0;
    while let Some(relative_start) = text[cursor..].find(&opening) {
        let start = cursor + relative_start;
        let Some(open_end) = text[start..].find('>').map(|offset| start + offset + 1) else {
            break;
        };
        let Some(relative_end) = text[open_end..].find(&closing) else {
            break;
        };
        let end = open_end + relative_end + closing.len();
        output.push((start, end, category));
        cursor = end;
    }
}

fn fit_special_estimates(estimates: &mut [u64; 8], total: u64) {
    let special_total = estimates[..CONVERSATION].iter().sum::<u64>();
    if special_total <= total || special_total == 0 {
        return;
    }
    let original = *estimates;
    let mut assigned = 0;
    for index in 0..CONVERSATION {
        estimates[index] = original[index].saturating_mul(total) / special_total;
        assigned += estimates[index];
    }
    let mut remainder = total - assigned;
    let mut order = (0..CONVERSATION).collect::<Vec<_>>();
    order.sort_by_key(|index| {
        std::cmp::Reverse(original[*index].saturating_mul(total) % special_total)
    });
    for index in order {
        if remainder == 0 {
            break;
        }
        estimates[index] += 1;
        remainder -= 1;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::model::{CanonicalMessage, Origin, Role, ToolDefinition};

    fn category<'a>(
        snapshot: &'a pb::PromptTokenBreakdownSnapshot,
        id: &str,
    ) -> &'a pb::PromptTokenBreakdownCategory {
        snapshot
            .categories
            .iter()
            .find(|category| category.id == id)
            .unwrap_or_else(|| panic!("missing category {id}"))
    }

    fn characters(snapshot: &pb::PromptTokenBreakdownSnapshot, id: &str) -> Option<u32> {
        category(snapshot, id).character_count
    }

    fn tokens(snapshot: &pb::PromptTokenBreakdownSnapshot, id: &str) -> u32 {
        category(snapshot, id).estimated_tokens
    }

    const RULES: &str = "<rules><user_rule>RULE</user_rule></rules>";
    const AGENT_RULES: &str =
        "<agent_rules>not loaded<agent_rule fullPath=\"x\" description=\"y\"/></agent_rules>";
    const SKILLS: &str = "<agent_skills><available_skills><agent_skill fullPath=\"s\">desc</agent_skill></available_skills></agent_skills>";
    const SUBAGENTS: &str = "<subagents><subagent name=\"a\">d</subagent></subagents>";
    const MCP: &str =
        "<dynamic_tools><namespace name=\"i\" tools=\"a, b\" source=\"mcp\" /></dynamic_tools>";
    const ENV: &str = "<user_info>env</user_info>";

    #[test]
    fn request_context_sections_are_attributed_to_their_categories() {
        let text = [ENV, RULES, AGENT_RULES, SKILLS, SUBAGENTS, MCP].join("\n\n");
        let separators = 2 * 5;
        let message =
            CanonicalMessage::text("request-context:test", Role::User, Origin::Prompt, text);

        let snapshot =
            breakdown(20_000, 500_000, None, "", &[], &HashSet::new(), &[message]).unwrap();

        assert_eq!(
            characters(&snapshot, "rules"),
            Some((RULES.len() + AGENT_RULES.len()) as u32),
            "the agent-decided rule catalog belongs to Rules"
        );
        assert_eq!(characters(&snapshot, "skills"), Some(SKILLS.len() as u32));
        assert_eq!(
            characters(&snapshot, "subagents"),
            Some(SUBAGENTS.len() as u32)
        );
        assert_eq!(characters(&snapshot, "mcp"), Some(MCP.len() as u32));
        assert_eq!(
            characters(&snapshot, "conversation"),
            Some((ENV.len() + separators) as u32),
            "environment blocks and section separators stay in Conversation"
        );
        assert!(tokens(&snapshot, "rules") > 0);
        assert!(tokens(&snapshot, "skills") > 0);
        assert!(tokens(&snapshot, "subagents") > 0);
        assert!(tokens(&snapshot, "mcp") > 0);

        let special = (0..CONVERSATION)
            .map(|index| tokens(&snapshot, CATEGORIES[index].0))
            .sum::<u32>();
        assert_eq!(tokens(&snapshot, "conversation"), 20_000 - special);
    }

    #[test]
    fn runtime_sections_keep_being_attributed() {
        let message = CanonicalMessage::text("runtime:event", Role::User, Origin::Runtime, RULES);

        let snapshot =
            breakdown(5_000, 500_000, None, "", &[], &HashSet::new(), &[message]).unwrap();

        assert_eq!(characters(&snapshot, "rules"), Some(RULES.len() as u32));
        assert_eq!(characters(&snapshot, "conversation"), None);
    }

    #[test]
    fn plain_user_text_is_conversation_even_with_section_tags() {
        let message = CanonicalMessage::text("user", Role::User, Origin::User, RULES);

        let snapshot =
            breakdown(5_000, 500_000, None, "", &[], &HashSet::new(), &[message]).unwrap();

        assert_eq!(characters(&snapshot, "rules"), None);
        assert_eq!(
            characters(&snapshot, "conversation"),
            Some(RULES.len() as u32)
        );
    }

    #[test]
    fn dynamic_mcp_tools_are_separated_from_static_tools() {
        let dynamic = ToolDefinition {
            name: "mcp__browser".into(),
            description: "drive the browser".into(),
            parameters: serde_json::json!({"type": "object"}),
        };
        let dynamic_chars = serde_json::to_string(&dynamic)
            .unwrap()
            .encode_utf16()
            .count() as u32;
        let static_tool = ToolDefinition {
            name: "Read".into(),
            description: "read a file".into(),
            parameters: serde_json::json!({"type": "object"}),
        };
        let static_chars = serde_json::to_string(&static_tool)
            .unwrap()
            .encode_utf16()
            .count() as u32;
        let dynamic_names = HashSet::from(["mcp__browser".to_string()]);

        let snapshot = breakdown(
            1_000,
            100_000,
            None,
            "sys",
            &[dynamic, static_tool],
            &dynamic_names,
            &[],
        )
        .unwrap();

        assert_eq!(characters(&snapshot, "system_prompt"), Some(3));
        assert_eq!(characters(&snapshot, "mcp"), Some(dynamic_chars));
        assert_eq!(characters(&snapshot, "tools"), Some(static_chars));
    }

    fn request_context(text: impl Into<String>) -> CanonicalMessage {
        CanonicalMessage::text("request-context:test", Role::User, Origin::Prompt, text)
    }

    #[test]
    fn malformed_sections_fall_back_to_conversation() {
        for text in [
            "<rules><user_rule>UNCLOSED",
            "</rules>stray closing tag",
            "<rules>one",
            "<agent_skills",
            "<dynamic_tools><namespace name=\"i\" tools=\"a\"",
        ] {
            let snapshot = breakdown(
                1_000,
                100_000,
                None,
                "",
                &[],
                &HashSet::new(),
                &[request_context(text)],
            )
            .unwrap();
            assert_eq!(
                characters(&snapshot, "conversation"),
                Some(text.encode_utf16().count() as u32),
                "unclosed or stray tags stay in Conversation: {text:?}"
            );
            assert_eq!(characters(&snapshot, "rules"), None, "{text:?}");
            assert_eq!(characters(&snapshot, "skills"), None, "{text:?}");
            assert_eq!(characters(&snapshot, "mcp"), None, "{text:?}");
        }
    }

    #[test]
    fn nested_sections_count_once_for_the_outer_category() {
        let text = "<rules><agent_skills>nested</agent_skills></rules>";
        let snapshot = breakdown(
            1_000,
            100_000,
            None,
            "",
            &[],
            &HashSet::new(),
            &[request_context(text)],
        )
        .unwrap();

        assert_eq!(
            characters(&snapshot, "rules"),
            Some(text.encode_utf16().count() as u32)
        );
        assert_eq!(
            characters(&snapshot, "skills"),
            None,
            "overlapping ranges do not double count"
        );
    }

    #[test]
    fn sections_with_attributes_are_recognized() {
        let text = "<agent_skills source=\"x\">desc</agent_skills>";
        let snapshot = breakdown(
            1_000,
            100_000,
            None,
            "",
            &[],
            &HashSet::new(),
            &[request_context(text)],
        )
        .unwrap();

        assert_eq!(
            characters(&snapshot, "skills"),
            Some(text.encode_utf16().count() as u32)
        );
    }

    #[test]
    fn empty_sections_still_belong_to_their_category() {
        let text = "<rules></rules>";
        let snapshot = breakdown(
            1_000,
            100_000,
            None,
            "",
            &[],
            &HashSet::new(),
            &[request_context(text)],
        )
        .unwrap();

        assert_eq!(
            characters(&snapshot, "rules"),
            Some(text.encode_utf16().count() as u32)
        );
        assert!(tokens(&snapshot, "rules") > 0);
    }

    #[test]
    fn every_text_part_is_measured_independently() {
        let message = CanonicalMessage::parts(
            "request-context:test",
            Role::User,
            Origin::Prompt,
            vec![
                ContentPart::Text {
                    text: "<rules>A</rules>".into(),
                },
                ContentPart::Text {
                    text: "plain".into(),
                },
            ],
        );
        let snapshot =
            breakdown(1_000, 100_000, None, "", &[], &HashSet::new(), &[message]).unwrap();

        assert_eq!(characters(&snapshot, "rules"), Some(16));
        assert_eq!(characters(&snapshot, "conversation"), Some(5));
    }

    #[test]
    fn assistant_text_is_conversation_even_with_section_tags() {
        let message = CanonicalMessage {
            message_id: "assistant".into(),
            role: Role::Assistant,
            origin: Origin::Assistant,
            content: MessageContent::Assistant {
                text: "<rules>assistant text</rules>".into(),
                thinking: String::new(),
                tool_round_id: None,
                replay_state: None,
                tool_calls: Vec::new(),
            },
            runtime_event_id: None,
        };
        let text = "<rules>assistant text</rules>";
        let snapshot =
            breakdown(1_000, 100_000, None, "", &[], &HashSet::new(), &[message]).unwrap();

        assert_eq!(characters(&snapshot, "rules"), None);
        assert_eq!(
            characters(&snapshot, "conversation"),
            Some((text.encode_utf16().count() + 2) as u32),
            "assistant text plus the serialized empty tool-call list"
        );
    }

    #[test]
    fn unicode_sections_are_measured_by_utf16_width() {
        let text = "<rules>规则</rules>";
        let snapshot = breakdown(
            1_000,
            100_000,
            None,
            "",
            &[],
            &HashSet::new(),
            &[request_context(text)],
        )
        .unwrap();

        assert_eq!(
            characters(&snapshot, "rules"),
            Some(text.encode_utf16().count() as u32)
        );
        assert!(tokens(&snapshot, "rules") > 0);
    }
}
