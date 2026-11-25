//! Turn-scoped material: IDE files, selected context, and glob attachment.
use crate::cursor::protocol::proto::agent::v1 as pb;

use super::{globs, rules};

/// Workspace-relative paths available to this turn, used for glob attachment.
#[derive(Debug, Default)]
pub(crate) struct TurnFiles {
    relative: Vec<String>,
}

impl TurnFiles {
    fn is_empty(&self) -> bool {
        self.relative.is_empty()
    }

    fn push(&mut self, path: &str, relative_path: Option<&str>, workspaces: &[String]) {
        if let Some(relative) = turn_relative(path, relative_path, workspaces) {
            self.insert(relative);
        }
    }

    /// Folders enter the candidate list with a trailing slash so directory
    /// globs such as `docs/**` attach without pretending a folder is a file.
    fn push_folder(&mut self, path: &str, relative_path: Option<&str>, workspaces: &[String]) {
        if let Some(relative) = turn_relative(path, relative_path, workspaces) {
            self.insert(format!("{}/", relative.trim_end_matches('/')));
        }
    }

    fn insert(&mut self, relative: String) {
        if !self.relative.contains(&relative) {
            self.relative.push(relative);
        }
    }

    fn matches(&self, globs: &[String]) -> bool {
        globs::matches(globs, &self.relative)
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

/// Files and folders the turn exposes to the agent, relative to every
/// workspace root so globs match the same way in single- and multi-root
/// workspaces.
pub(crate) fn turn_files(context: &pb::RequestContext, user: &pb::UserMessage) -> TurnFiles {
    let workspaces = context
        .env
        .as_ref()
        .map(|env| env.workspace_paths.as_slice())
        .unwrap_or_default();
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
            files.push(&file.path, file.relative_path.as_deref(), workspaces);
        }
    }
    for file in &selected.files {
        files.push(&file.path, file.relative_path.as_deref(), workspaces);
    }
    for selection in &selected.code_selections {
        files.push(
            &selection.path,
            selection.relative_path.as_deref(),
            workspaces,
        );
    }
    // Folders participate as directory candidates: a rule scoped to
    // `docs/**` applies when the user points the agent at the `docs` subtree.
    for folder in &selected.folders {
        files.push_folder(&folder.path, folder.relative_path.as_deref(), workspaces);
    }
    files
}

fn turn_relative(path: &str, relative_path: Option<&str>, workspaces: &[String]) -> Option<String> {
    relative_path
        .filter(|value| !value.trim().is_empty())
        .and_then(normalize_relative)
        .or_else(|| workspace_relative(path, workspaces))
}

fn normalize_relative(path: &str) -> Option<String> {
    let path = path.trim().replace('\\', "/");
    let path = path
        .trim_start_matches("./")
        .trim_start_matches('/')
        .trim_end_matches('/');
    (!path.is_empty()).then(|| path.to_string())
}

/// Relativizes `path` against the first workspace root that has a directory
/// boundary match. Paths outside every root stay as they are so a shared
/// prefix such as `/repo` never claims `/repo2`.
fn workspace_relative(path: &str, workspaces: &[String]) -> Option<String> {
    let path = path.trim().replace('\\', "/");
    if path.is_empty() {
        return None;
    }
    for workspace in workspaces {
        let workspace = workspace.trim().replace('\\', "/");
        let workspace = workspace.trim_end_matches('/');
        if workspace.is_empty() {
            continue;
        }
        let Some(rest) = path.strip_prefix(workspace) else {
            continue;
        };
        if rest.is_empty() {
            return None;
        }
        let Some(rest) = rest.strip_prefix('/') else {
            continue;
        };
        return normalize_relative(rest);
    }
    normalize_relative(&path)
}

/// Turn-scoped rules and skills whose globs match the files of this turn.
pub(crate) fn attached_context(context: &pb::RequestContext, files: &TurnFiles) -> Option<String> {
    if files.is_empty() {
        return None;
    }
    let skills = rules::SkillIndex::new(context);
    let mut sections = Vec::new();
    let mut matched_rules = Vec::new();
    for rule in context.rules.iter().chain(context.non_file_rules.iter()) {
        if skills.covers(rule) {
            continue;
        }
        let rules::RuleKind::Globbed { globs } = rules::rule_kind(rule) else {
            continue;
        };
        if files.matches(&globs) {
            rules::push_rule_content(&mut matched_rules, rule);
        }
    }
    if !matched_rules.is_empty() {
        sections.push(format!("<rules>\n{}\n</rules>", matched_rules.join("\n")));
    }
    let matched_skills = context
        .agent_skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation && !skill.globs.is_empty())
        .filter(|skill| files.matches(&globs::normalize_globs(&skill.globs)))
        .map(rules::skill_entry)
        .collect::<Vec<_>>();
    if !matched_skills.is_empty() {
        sections.push(format!(
            "<agent_skills>\n<available_skills>\n{}\n</available_skills>\n</agent_skills>",
            matched_skills.join("\n")
        ));
    }
    (!sections.is_empty()).then(|| sections.join("\n\n"))
}

impl TurnFiles {
    #[cfg(test)]
    fn from_relative(paths: &[&str]) -> Self {
        Self {
            relative: paths.iter().map(|path| (*path).to_string()).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{skill, typed_rule};
    use super::*;

    fn context(workspaces: &[&str]) -> pb::RequestContext {
        pb::RequestContext {
            env: Some(pb::RequestContextEnv {
                workspace_paths: workspaces.iter().map(|path| (*path).to_string()).collect(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn workspaces(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn ide_file(
        path: &str,
        relative_path: Option<&str>,
    ) -> pb::invocation_context::ide_state::File {
        pb::invocation_context::ide_state::File {
            path: path.into(),
            relative_path: relative_path.map(str::to_string),
            ..Default::default()
        }
    }

    fn user(
        selected: pb::SelectedContext,
        ide: Option<pb::invocation_context::IdeState>,
    ) -> pb::UserMessage {
        pb::UserMessage {
            text: "question".into(),
            message_id: "message".into(),
            selected_context: Some(pb::SelectedContext {
                invocation_context: ide.map(|state| pb::InvocationContext {
                    data: Some(pb::invocation_context::Data::IdeState(state)),
                }),
                ..selected
            }),
            ..Default::default()
        }
    }

    #[test]
    fn workspace_relative_strips_the_workspace_root() {
        assert_eq!(
            workspace_relative("/repo/src/main.rs", &workspaces(&["/repo"])),
            Some("src/main.rs".into())
        );
        assert_eq!(
            workspace_relative("C:\\repo\\src\\main.rs", &workspaces(&["C:/repo/"])),
            Some("src/main.rs".into())
        );
        assert_eq!(
            workspace_relative("./src/main.rs", &[]),
            Some("src/main.rs".into())
        );
        assert_eq!(workspace_relative("   ", &workspaces(&["/repo"])), None);
        assert_eq!(workspace_relative("/repo", &workspaces(&["/repo"])), None);
    }

    #[test]
    fn workspace_relative_requires_a_directory_boundary() {
        assert_eq!(
            workspace_relative("/repo-other/src/main.rs", &workspaces(&["/repo"])),
            Some("repo-other/src/main.rs".into()),
            "a shared prefix without a separator is not inside the workspace"
        );
        assert_eq!(
            workspace_relative("/repo2/src/main.rs", &workspaces(&["/repo"])),
            Some("repo2/src/main.rs".into())
        );
    }

    #[test]
    fn workspace_relative_tries_every_root() {
        assert_eq!(
            workspace_relative("/repo2/src/main.rs", &workspaces(&["/repo", "/repo2"])),
            Some("src/main.rs".into())
        );
    }

    #[test]
    fn turn_files_collect_every_ide_source() {
        let user = user(
            pb::SelectedContext {
                files: vec![pb::SelectedFile {
                    path: "/repo/app/selected.ts".into(),
                    relative_path: Some("app/selected.ts".into()),
                    ..Default::default()
                }],
                code_selections: vec![pb::SelectedCodeSelection {
                    path: "/repo/src/lib.rs".into(),
                    relative_path: None,
                    ..Default::default()
                }],
                folders: vec![pb::SelectedFolder {
                    path: "/repo/docs".into(),
                    relative_path: None,
                    ..Default::default()
                }],
                ..Default::default()
            },
            Some(pb::invocation_context::IdeState {
                visible_files: vec![ide_file("/repo/src/main.rs", Some("src/main.rs"))],
                recently_viewed_files: vec![
                    ide_file("/repo/tests/t.rs", None),
                    ide_file("/repo/src/main.rs", Some("src/main.rs")),
                ],
                ..Default::default()
            }),
        );

        let files = turn_files(&context(&["/repo"]), &user);
        assert_eq!(
            files.relative,
            [
                "src/main.rs",
                "tests/t.rs",
                "app/selected.ts",
                "src/lib.rs",
                "docs/"
            ],
            "visible files, recently viewed files, selected files, code selections, and folders are deduped in order"
        );
    }

    #[test]
    fn turn_files_prefers_the_workspace_relative_path() {
        let user = user(
            pb::SelectedContext {
                files: vec![pb::SelectedFile {
                    path: "/elsewhere/src/main.rs".into(),
                    relative_path: Some("src/main.rs".into()),
                    ..Default::default()
                }],
                ..Default::default()
            },
            None,
        );
        let files = turn_files(&context(&["/repo"]), &user);
        assert_eq!(files.relative, ["src/main.rs"]);
    }

    #[test]
    fn turn_files_relativizes_against_every_workspace_root() {
        let user = user(
            pb::SelectedContext {
                code_selections: vec![pb::SelectedCodeSelection {
                    path: "/repo2/src/main.rs".into(),
                    relative_path: None,
                    ..Default::default()
                }],
                ..Default::default()
            },
            None,
        );
        let files = turn_files(&context(&["/repo", "/repo2"]), &user);
        assert_eq!(files.relative, ["src/main.rs"]);
    }

    #[test]
    fn globs_never_attach_files_outside_the_workspace() {
        let user = user(
            pb::SelectedContext {
                code_selections: vec![pb::SelectedCodeSelection {
                    path: "/repo-other/src/main.rs".into(),
                    relative_path: None,
                    ..Default::default()
                }],
                ..Default::default()
            },
            None,
        );
        let files = turn_files(&context(&["/repo"]), &user);
        assert_eq!(files.relative, ["repo-other/src/main.rs"]);
        assert!(!files.matches(&["src/**".into()]));
        assert!(files.matches(&["repo-other/**".into()]));
    }

    #[test]
    fn folders_are_glob_candidates() {
        let user = user(
            pb::SelectedContext {
                folders: vec![pb::SelectedFolder {
                    path: "/repo/docs".into(),
                    relative_path: Some("docs".into()),
                    ..Default::default()
                }],
                ..Default::default()
            },
            None,
        );
        let files = turn_files(&context(&["/repo"]), &user);
        assert_eq!(files.relative, ["docs/"]);
        assert!(files.matches(&["docs/**".into()]));
        assert!(
            !files.matches(&["docs/*.rs".into()]),
            "a folder is not a file match"
        );
    }

    #[test]
    fn attached_context_matches_brace_globs() {
        let mut context = context(&["/repo"]);
        context.rules = vec![typed_rule(
            "BRACE BODY",
            pb::cursor_rule_type::Type::FileGlobbed(pb::CursorRuleTypeFileGlobs {
                globs: vec!["**/*.{ts,tsx}".into()],
            }),
        )];

        let attached =
            attached_context(&context, &TurnFiles::from_relative(&["app/web.tsx"])).unwrap();
        assert!(
            attached.contains("<user_rule>\nBRACE BODY\n</user_rule>"),
            "{attached}"
        );
        assert!(
            attached_context(&context, &TurnFiles::from_relative(&["app/web.js"])).is_none(),
            "brace alternates must not match other extensions"
        );
    }

    #[test]
    fn attached_context_includes_only_matching_globs() {
        let mut context = context(&["/repo"]);
        context.rules = vec![typed_rule(
            "RUST BODY",
            pb::cursor_rule_type::Type::FileGlobbed(pb::CursorRuleTypeFileGlobs {
                globs: vec!["src/**/*.rs".into()],
            }),
        )];
        context.agent_skills = vec![skill(
            "scoped/SKILL.md",
            "ts skill",
            "TS BODY",
            &["**/*.ts"],
        )];

        assert!(
            attached_context(&context, &TurnFiles::from_relative(&["docs/readme.md"])).is_none(),
            "no glob matches means nothing is attached"
        );
        let rust = attached_context(&context, &TurnFiles::from_relative(&["src/main.rs"])).unwrap();
        assert!(
            rust.contains("<user_rule>\nRUST BODY\n</user_rule>"),
            "{rust}"
        );
        assert!(!rust.contains("ts skill"), "{rust}");
        let typescript =
            attached_context(&context, &TurnFiles::from_relative(&["app/web.ts"])).unwrap();
        assert!(typescript.contains("ts skill"), "{typescript}");
        assert!(!typescript.contains("RUST BODY"), "{typescript}");
    }

    #[test]
    fn workspace_relative_handles_malformed_paths() {
        let repo = workspaces(&["/repo"]);
        assert_eq!(workspace_relative("", &repo), None);
        assert_eq!(workspace_relative("   ", &repo), None);
        assert_eq!(workspace_relative("/repo", &repo), None);
        assert_eq!(
            workspace_relative("/repo/", &repo),
            None,
            "a workspace root with a trailing separator is not a file"
        );
        assert_eq!(workspace_relative("/repo//", &repo), None);
        assert_eq!(
            workspace_relative("\\repo\\\\src\\main.rs", &repo),
            Some("src/main.rs".into()),
            "mixed and repeated separators normalize at the boundary"
        );
        assert_eq!(
            workspace_relative("/repo/src//main.rs", &repo),
            Some("src//main.rs".into()),
            "internal repeated separators are preserved"
        );
        assert_eq!(
            workspace_relative("/项目/src/主.rs", &workspaces(&["/项目"])),
            Some("src/主.rs".into())
        );
        assert_eq!(
            workspace_relative("src/main.rs", &workspaces(&["", "/repo"])),
            Some("src/main.rs".into()),
            "empty workspace entries are skipped"
        );
    }

    #[test]
    fn workspace_relative_uses_the_first_matching_root() {
        assert_eq!(
            workspace_relative(
                "/repo/nested/src/main.rs",
                &workspaces(&["/repo", "/repo/nested"])
            ),
            Some("nested/src/main.rs".into()),
            "root order decides when roots are nested"
        );
    }

    #[test]
    fn turn_files_ignore_empty_and_duplicate_paths() {
        let user = user(
            pb::SelectedContext {
                files: vec![
                    pb::SelectedFile {
                        path: String::new(),
                        relative_path: None,
                        ..Default::default()
                    },
                    pb::SelectedFile {
                        path: "/repo/src/a.rs".into(),
                        relative_path: Some("   ".into()),
                        ..Default::default()
                    },
                ],
                folders: vec![
                    pb::SelectedFolder {
                        path: String::new(),
                        relative_path: None,
                        ..Default::default()
                    },
                    pb::SelectedFolder {
                        path: "/repo/docs".into(),
                        relative_path: None,
                        ..Default::default()
                    },
                    pb::SelectedFolder {
                        path: "/repo/docs".into(),
                        relative_path: Some("docs/".into()),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            Some(pb::invocation_context::IdeState {
                visible_files: vec![ide_file("", None)],
                ..Default::default()
            }),
        );

        let files = turn_files(&context(&["/repo"]), &user);
        assert_eq!(files.relative, ["src/a.rs", "docs/"]);
    }

    #[test]
    fn turn_files_match_malformed_globs_to_nothing() {
        let user = user(
            pb::SelectedContext {
                files: vec![pb::SelectedFile {
                    path: "/repo/src/main.rs".into(),
                    relative_path: Some("src/main.rs".into()),
                    ..Default::default()
                }],
                ..Default::default()
            },
            None,
        );
        let files = turn_files(&context(&["/repo"]), &user);
        assert!(!files.matches(&["src/[unclosed".into()]));
        assert!(!files.matches(&["{unclosed".into()]));
        assert!(files.matches(&["src/[unclosed".into(), "src/**/*.rs".into()]));
    }

    #[test]
    fn ide_state_requires_an_ide_payload() {
        let user = pb::UserMessage {
            text: "question".into(),
            message_id: "message".into(),
            selected_context: Some(pb::SelectedContext {
                invocation_context: Some(pb::InvocationContext {
                    data: Some(pb::invocation_context::Data::BlobId(vec![1, 2, 3])),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(ide_state(&user).is_none());
        assert!(turn_files(&context(&["/repo"]), &user).is_empty());
    }

    #[test]
    fn attached_context_projects_skill_backed_rules_once() {
        let mut context = context(&["/repo"]);
        context.rules = vec![pb::CursorRule {
            full_path: "/repo/.agents/skills/tdd/SKILL.md".into(),
            content: "SKILL BODY".into(),
            r#type: Some(pb::CursorRuleType {
                r#type: Some(pb::cursor_rule_type::Type::FileGlobbed(
                    pb::CursorRuleTypeFileGlobs {
                        globs: vec!["src/**/*.rs".into()],
                    },
                )),
            }),
            ..Default::default()
        }];
        context.agent_skills = vec![skill(
            "/repo/.agents/skills/tdd/SKILL.md",
            "tdd skill",
            "SKILL BODY",
            &["src/**/*.rs"],
        )];

        let attached =
            attached_context(&context, &TurnFiles::from_relative(&["src/main.rs"])).unwrap();
        assert!(attached.contains("tdd skill"), "{attached}");
        assert!(
            !attached.contains("<user_rule>"),
            "the skill entry is the single projection: {attached}"
        );
    }

    #[test]
    fn turn_files_without_selected_context_is_empty() {
        let user = pb::UserMessage {
            text: "question".into(),
            message_id: "message".into(),
            ..Default::default()
        };
        assert!(turn_files(&context(&["/repo"]), &user).is_empty());
        assert!(turn_files(&pb::RequestContext::default(), &user).is_empty());
    }
}
