//! Projects Cursor project rules into injection kinds.
use std::{collections::HashSet, path::Path};

use serde::Deserialize;

use crate::cursor::protocol::proto::agent::v1 as pb;

use super::globs::normalize_globs;

/// Cursor's four project-rule shapes. The protocol carries the same split in
/// `CursorRule.type`; local markdown rules have no metadata and stay always-on.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum RuleKind {
    Always,
    AgentFetched { description: String },
    Globbed { globs: Vec<String> },
    Manual,
}

pub(super) fn rule_kind(rule: &pb::CursorRule) -> RuleKind {
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

fn is_skill_rule(rule: &pb::CursorRule) -> bool {
    Path::new(&rule.full_path)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("SKILL.md"))
}

/// Skills that the same request also delivers as rules, so a `SKILL.md` is
/// projected once instead of twice.
pub(super) struct SkillIndex<'a> {
    paths: HashSet<&'a str>,
    contents: HashSet<&'a str>,
}

impl<'a> SkillIndex<'a> {
    pub(super) fn new(context: &'a pb::RequestContext) -> Self {
        Self {
            paths: context
                .agent_skills
                .iter()
                .map(|skill| skill.full_path.as_str())
                .filter(|path| !path.is_empty())
                .collect(),
            contents: context
                .agent_skills
                .iter()
                .map(|skill| skill.content.as_str())
                .filter(|content| !content.is_empty())
                .collect(),
        }
    }

    pub(super) fn covers(&self, rule: &pb::CursorRule) -> bool {
        is_skill_rule(rule)
            || self.paths.contains(rule.full_path.as_str())
            || self.contents.contains(rule.content.as_str())
    }
}

pub(super) fn push_rule_content(rules: &mut Vec<String>, rule: &pb::CursorRule) {
    if !rule.content.trim().is_empty() {
        rules.push(format!("<user_rule>\n{}\n</user_rule>", rule.content));
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{frontmatter_rule, rule, typed_rule};
    use super::*;

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
        assert_eq!(
            rule_kind(&frontmatter_rule(
                "body",
                "alwaysApply: false\nglobs: docs/**",
            )),
            RuleKind::Globbed {
                globs: vec!["docs/**".into()],
            },
            "a bare YAML block without delimiters is accepted"
        );
        assert_eq!(
            rule_kind(&frontmatter_rule(
                "body",
                "---\nalwaysApply: false\nglobs: src/**\n---\ntrailing text",
            )),
            RuleKind::Globbed {
                globs: vec!["src/**".into()],
            },
            "content after the closing delimiter is ignored"
        );
    }

    #[test]
    fn rule_kind_keeps_brace_alternates_in_frontmatter() {
        assert_eq!(
            rule_kind(&frontmatter_rule(
                "body",
                "---\nalwaysApply: false\nglobs: '**/*.{ts,tsx}'\n---",
            )),
            RuleKind::Globbed {
                globs: vec!["**/*.{ts,tsx}".into()],
            }
        );
        assert_eq!(
            rule_kind(&frontmatter_rule(
                "body",
                "---\nalwaysApply: false\nglobs: ['{src,app}/**/*.ts', 'docs/**']\n---",
            )),
            RuleKind::Globbed {
                globs: vec!["{src,app}/**/*.ts".into(), "docs/**".into()],
            }
        );
        assert_eq!(
            rule_kind(&typed_rule(
                "body",
                pb::cursor_rule_type::Type::FileGlobbed(pb::CursorRuleTypeFileGlobs {
                    globs: vec!["**/*.{ts,tsx}, documents/**".into()],
                }),
            )),
            RuleKind::Globbed {
                globs: vec!["**/*.{ts,tsx}".into(), "documents/**".into()],
            }
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
    fn rule_kind_tolerates_malformed_frontmatter() {
        for frontmatter in [
            "---\nalwaysApply: maybe\n---",
            "---\nalwaysApply: 1\n---",
            "---\nglobs: 42\n---",
            "---\nglobs: [1, 2]\n---",
            "---\ndescription: [a, b]\n---",
            "---\nglobs: {a: b}\n---",
            "---\ndescription: {nested: value}\n---",
            "---",
            "---\nglobs: [unclosed\n---",
            "not: yaml: at: all",
            "",
        ] {
            assert_eq!(
                rule_kind(&frontmatter_rule("body", frontmatter)),
                RuleKind::Always,
                "malformed frontmatter must fall back to always: {frontmatter:?}"
            );
        }
    }

    #[test]
    fn rule_kind_treats_empty_frontmatter_fields_as_unset() {
        for frontmatter in [
            "---\nalwaysApply: false\nglobs: []\n---",
            "---\nalwaysApply: false\nglobs: ''\n---",
            "---\nalwaysApply: false\nglobs: '   '\n---",
            "---\nalwaysApply: false\ndescription: '   '\n---",
        ] {
            assert_eq!(
                rule_kind(&frontmatter_rule("body", frontmatter)),
                RuleKind::Manual,
                "empty fields do not turn into matches: {frontmatter:?}"
            );
        }
    }

    #[test]
    fn rule_kind_reads_crlf_frontmatter() {
        assert_eq!(
            rule_kind(&frontmatter_rule(
                "body",
                "---\r\nalwaysApply: false\r\nglobs: src/**/*.tsx\r\n---\r\n",
            )),
            RuleKind::Globbed {
                globs: vec!["src/**/*.tsx".into()],
            }
        );
    }

    #[test]
    fn rule_kind_normalizes_malformed_protocol_globs() {
        assert_eq!(
            rule_kind(&typed_rule(
                "body",
                pb::cursor_rule_type::Type::FileGlobbed(pb::CursorRuleTypeFileGlobs {
                    globs: vec![String::new(), "   ".into(), ",".into()],
                }),
            )),
            RuleKind::Always,
            "a globbed rule with no usable pattern must not disappear"
        );
        assert_eq!(
            rule_kind(&typed_rule(
                "body",
                pb::cursor_rule_type::Type::FileGlobbed(pb::CursorRuleTypeFileGlobs {
                    globs: vec![" src/**/*.rs , ".into()],
                }),
            )),
            RuleKind::Globbed {
                globs: vec!["src/**/*.rs".into()],
            }
        );
    }

    #[test]
    fn required_rules_win_over_malformed_metadata() {
        let mut rule = frontmatter_rule("body", "---\nglobs: [unclosed\n---");
        rule.is_required = Some(true);
        assert_eq!(rule_kind(&rule), RuleKind::Always);
    }
}
