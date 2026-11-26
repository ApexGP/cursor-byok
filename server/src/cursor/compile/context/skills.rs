//! Renders Cursor's skill catalog.
//!
//! Cursor lists skills as paths grouped under aliased skill roots and keeps
//! descriptions out of the catalog except for its own built-in skills. A large
//! catalog is truncated to an initial list plus a note naming the directories
//! that still hold omitted skills.

use std::collections::HashMap;

use crate::cursor::protocol::proto::agent::v1 as pb;

use super::xml;

const INSTRUCTION: &str = "When users ask you to perform tasks, check if any of the available skills below can help complete the task more effectively. Skills provide specialized capabilities and domain knowledge. To use a skill, read the skill file at the provided absolute path using the Read tool, then follow the instructions within. When a skill is relevant, read and follow it IMMEDIATELY as your first action. NEVER just announce or mention a skill without actually reading and following it. Use the skills listed below. If a later task specifically requires discovering more skills, additional skills may exist in the directories shown in the skills section.";

const AVAILABLE_SKILLS_DESCRIPTION: &str = "Skills the agent can use. Use the Read tool with the provided absolute path to fetch full contents. A path that starts with an alias ($0, $1, ...) continues from the directory listed for that alias.";

/// Cursor caps the catalog at 2% of the model context window, measured with a
/// chars/4 token estimate; the fallback window matches Cursor's own default.
const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;

pub(super) fn section(
    context: &pb::RequestContext,
    context_window_tokens: Option<u64>,
) -> Option<String> {
    let skills = context
        .agent_skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation && skill.globs.is_empty())
        .collect::<Vec<&pb::AgentSkill>>();
    if skills.is_empty() {
        return None;
    }

    let mut aliases = HashMap::new();
    let mut roots = Vec::new();
    for skill in &skills {
        let Some((root, _)) = split_root(&skill.full_path) else {
            continue;
        };
        if !aliases.contains_key(root) {
            aliases.insert(root, roots.len());
            roots.push(root);
        }
    }

    let alias_capacity = roots
        .iter()
        .enumerate()
        .map(|(alias, root)| format!("${alias} = {root}\n").len())
        .sum::<usize>();
    let base_header = format!(
        "{INSTRUCTION}\n\n\n<available_skills description=\"{AVAILABLE_SKILLS_DESCRIPTION}\">\n\n"
    );

    let mut body = String::new();
    let mut omitted = Vec::new();
    let mut listed_roots = Vec::new();
    let mut full = false;
    let budget =
        catalog_budget(context_window_tokens).saturating_sub(base_header.len() + alias_capacity);
    for skill in &skills {
        if full {
            omitted.push(*skill);
            continue;
        }
        let line = entry_at(&skill.full_path, &skill.description, &aliases);
        if body.len() + line.len() + 2 > budget {
            full = true;
            omitted.push(*skill);
            continue;
        }
        if let Some((root, _)) = split_root(&skill.full_path) {
            if !listed_roots.contains(&root) {
                listed_roots.push(root);
            }
        }
        body.push_str(&line);
        body.push_str("\n\n");
    }

    let mut alias_block = String::new();
    for (alias, root) in roots.iter().enumerate() {
        if listed_roots.contains(root) {
            alias_block.push_str(&format!("${alias} = {root}\n"));
        }
    }
    let header = format!(
        "{INSTRUCTION}\n\n\n<available_skills description=\"{AVAILABLE_SKILLS_DESCRIPTION}\">\n{alias_block}\n"
    );
    if !omitted.is_empty() {
        body.push_str(&omitted_notice(&omitted));
    }

    Some(format!(
        "<agent_skills>\n{header}{body}\n</available_skills>\n</agent_skills>"
    ))
}

/// A turn-scoped skill keeps its absolute path; built-in descriptions stay
/// inline because they carry instructions the model needs before reading.
pub(super) fn entry(skill: &pb::AgentSkill) -> String {
    let description = builtin_root(&skill.full_path)
        .then_some(skill.description.trim())
        .filter(|description| !description.is_empty());
    match description {
        Some(description) => format!(
            "<agent_skill fullPath=\"{}\">{}</agent_skill>",
            xml(&skill.full_path),
            xml(description)
        ),
        None => format!("<agent_skill fullPath=\"{}\" />", xml(&skill.full_path)),
    }
}

fn entry_at(path: &str, description: &str, aliases: &HashMap<&str, usize>) -> String {
    let Some((root, tail)) = split_root(path) else {
        return format!("<agent_skill fullPath=\"{}\" />", xml(path));
    };
    let aliased = format!("${}{}", aliases[root], tail);
    if is_builtin(root) && !description.trim().is_empty() {
        format!(
            "<agent_skill fullPath=\"{}\">{}</agent_skill>",
            xml(&aliased),
            xml(description.trim())
        )
    } else {
        format!("<agent_skill fullPath=\"{}\" />", xml(&aliased))
    }
}

fn omitted_notice(omitted: &[&pb::AgentSkill]) -> String {
    let mut roots = Vec::new();
    for skill in omitted {
        let root = split_root(&skill.full_path)
            .map(|(root, _)| root)
            .unwrap_or(skill.full_path.as_str());
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    format!(
        "Additional skills omitted from this initial list ({}). Directories containing omitted skills: {}.",
        omitted.len(),
        roots.join(", ")
    )
}

/// Splits a skill path at its last `skills` or `skills-cursor` directory.
/// The tail keeps the platform separator so joined aliases keep the original
/// path style.
fn split_root(path: &str) -> Option<(&str, &str)> {
    let mut boundary = None;
    let mut start = 0;
    for (index, byte) in path.bytes().enumerate() {
        if byte == b'/' || byte == b'\\' {
            let segment = &path[start..index];
            if segment == "skills" || segment == "skills-cursor" {
                boundary = Some(index);
            }
            start = index + 1;
        }
    }
    let end = boundary?;
    let tail = path.get(end..)?;
    (!tail.trim_start_matches(['/', '\\']).is_empty()).then_some((&path[..end], tail))
}

fn builtin_root(path: &str) -> bool {
    split_root(path).is_some_and(|(root, _)| is_builtin(root))
}

fn is_builtin(root: &str) -> bool {
    root.rsplit(['/', '\\']).next() == Some("skills-cursor")
}

/// Two percent of the window in chars/4 units: Cursor renders at most
/// `0.02 * window` estimated tokens of skill catalog.
fn catalog_budget(context_window_tokens: Option<u64>) -> usize {
    let window = context_window_tokens.unwrap_or(DEFAULT_CONTEXT_WINDOW);
    let tokens = window.saturating_mul(2) / 100;
    usize::try_from(tokens.saturating_mul(4)).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(path: &str, description: &str) -> pb::AgentSkill {
        pb::AgentSkill {
            full_path: path.into(),
            description: description.into(),
            ..Default::default()
        }
    }

    fn context(skills: Vec<pb::AgentSkill>) -> pb::RequestContext {
        pb::RequestContext {
            agent_skills: skills,
            ..Default::default()
        }
    }

    #[test]
    fn builtin_skills_keep_descriptions_and_user_skills_keep_only_paths() {
        let text = section(
            &context(vec![
                skill(
                    "/home/u/.cursor/skills-cursor/automate/SKILL.md",
                    "built in",
                ),
                skill("/repo/.cursor/skills/tdd/SKILL.md", "user skill"),
            ]),
            None,
        )
        .unwrap();

        assert!(
            text.contains("<agent_skill fullPath=\"$0/automate/SKILL.md\">built in</agent_skill>"),
            "{text}"
        );
        assert!(
            text.contains("<agent_skill fullPath=\"$1/tdd/SKILL.md\" />"),
            "{text}"
        );
        assert!(!text.contains("user skill"), "{text}");
        assert!(
            text.contains("$0 = /home/u/.cursor/skills-cursor"),
            "{text}"
        );
        assert!(text.contains("$1 = /repo/.cursor/skills"), "{text}");
        assert!(
            text.contains("read the skill file at the provided absolute path"),
            "{text}"
        );
    }

    #[test]
    fn glob_and_manual_skills_stay_out_of_the_catalog() {
        let mut globbed = skill("/repo/.cursor/skills/ts/SKILL.md", "ts");
        globbed.globs = vec!["**/*.ts".into()];
        let mut manual = skill("/repo/.cursor/skills/manual/SKILL.md", "manual");
        manual.disable_model_invocation = true;

        let text = section(
            &context(vec![
                skill("/repo/.cursor/skills/general/SKILL.md", "general"),
                globbed,
                manual,
            ]),
            None,
        )
        .unwrap();

        assert!(text.contains("general/SKILL.md"), "{text}");
        assert!(!text.contains("ts/SKILL.md"), "{text}");
        assert!(!text.contains("manual/SKILL.md"), "{text}");
    }

    #[test]
    fn a_large_catalog_is_cut_into_an_initial_list_with_omitted_directories() {
        let skills = (0..500)
            .map(|index| {
                skill(
                    &format!("/repo/.cursor/skills/skill-{index:03}/SKILL.md"),
                    "user skill",
                )
            })
            .collect::<Vec<_>>();

        let text = section(&context(skills), None).unwrap();

        assert!(text.len() <= catalog_budget(None) + 400, "{}", text.len());
        assert!(
            text.contains("Additional skills omitted from this initial list ("),
            "{text}"
        );
        assert!(
            text.contains("Directories containing omitted skills: /repo/.cursor/skills."),
            "{text}"
        );
        let listed = text.matches("<agent_skill ").count();
        assert!(listed > 100 && listed < 500, "{listed}");
    }

    #[test]
    fn the_catalog_budget_follows_the_model_context_window() {
        let skills = (0..800)
            .map(|index| {
                skill(
                    &format!("/repo/.cursor/skills/skill-{index:03}/SKILL.md"),
                    "user skill",
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(catalog_budget(Some(256_000)), 20_480);
        assert_eq!(catalog_budget(None), 16_000);
        assert_eq!(catalog_budget(Some(1_000_000)), 80_000);

        let small = section(&context(skills.clone()), Some(256_000)).unwrap();
        let large = section(&context(skills), Some(1_000_000)).unwrap();
        assert!(
            large.matches("<agent_skill ").count() > small.matches("<agent_skill ").count(),
            "a larger window keeps more skills"
        );
    }

    #[test]
    fn a_complete_catalog_has_no_omission_note() {
        let text = section(
            &context(vec![skill("/repo/.cursor/skills/tdd/SKILL.md", "tdd")]),
            None,
        )
        .unwrap();
        assert!(!text.contains("Additional skills omitted"), "{text}");
    }

    #[test]
    fn paths_without_a_skills_directory_keep_the_full_path() {
        let text = section(&context(vec![skill("SKILL.md", "loose")]), None).unwrap();
        assert!(
            text.contains("<agent_skill fullPath=\"SKILL.md\" />"),
            "{text}"
        );
        assert!(!text.contains("$0 ="), "{text}");
    }

    #[test]
    fn windows_paths_keep_their_separators() {
        let text = section(
            &context(vec![skill(
                "C:\\Users\\u\\.cursor\\skills\\tdd\\SKILL.md",
                "tdd",
            )]),
            None,
        )
        .unwrap();
        assert!(
            text.contains("<agent_skill fullPath=\"$0\\tdd\\SKILL.md\" />"),
            "{text}"
        );
        assert!(
            text.contains("$0 = C:\\Users\\u\\.cursor\\skills"),
            "{text}"
        );
    }

    #[test]
    fn turn_scoped_builtin_skills_keep_descriptions() {
        let builtin = skill("/home/u/.cursor/skills-cursor/loop/SKILL.md", "loop skill");
        assert_eq!(
            entry(&builtin),
            "<agent_skill fullPath=\"/home/u/.cursor/skills-cursor/loop/SKILL.md\">loop skill</agent_skill>"
        );
        let user = skill("/repo/.cursor/skills/tdd/SKILL.md", "tdd skill");
        assert_eq!(
            entry(&user),
            "<agent_skill fullPath=\"/repo/.cursor/skills/tdd/SKILL.md\" />"
        );
    }
}
