//! Glob normalization and matching for glob-scoped rules and skills.
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

/// Splits comma-separated glob lists while keeping `{a,b}` alternates intact.
pub(super) fn normalize_globs(values: &[String]) -> Vec<String> {
    values.iter().flat_map(|value| split(value)).collect()
}

fn split(value: &str) -> Vec<String> {
    let mut patterns = Vec::new();
    let mut current = String::new();
    let mut braces = 0usize;
    for character in value.chars() {
        match character {
            '{' => {
                braces += 1;
                current.push(character);
            }
            '}' => {
                braces = braces.saturating_sub(1);
                current.push(character);
            }
            ',' if braces == 0 => push(&mut patterns, &mut current),
            _ => current.push(character),
        }
    }
    push(&mut patterns, &mut current);
    patterns
}

fn push(patterns: &mut Vec<String>, current: &mut String) {
    let pattern = current.trim();
    if !pattern.is_empty() {
        patterns.push(pattern.to_string());
    }
    current.clear();
}

/// Whether any path matches any of the globs.
pub(super) fn matches(globs: &[String], paths: &[String]) -> bool {
    let Some(set) = build(globs) else {
        return false;
    };
    paths.iter().any(|path| set.is_match(path))
}

fn build(globs: &[String]) -> Option<GlobSet> {
    let mut set = GlobSetBuilder::new();
    let mut added = false;
    for pattern in normalize_globs(globs) {
        // `literal_separator` keeps `*` inside one path segment, matching the
        // rule glob semantics Cursor documents (`*.ts` is root-level only).
        // Windows paths and IDE globs are case-insensitive.
        let mut glob = GlobBuilder::new(&pattern);
        glob.literal_separator(true).case_insensitive(cfg!(windows));
        if let Ok(glob) = glob.build() {
            set.add(glob);
            added = true;
        }
    }
    added.then(|| set.build().ok()).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn globs(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn paths(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn normalize_globs_splits_only_outside_alternates() {
        assert_eq!(
            normalize_globs(&globs(&["src/**/*.tsx, docs/**/*.md"])),
            globs(&["src/**/*.tsx", "docs/**/*.md"])
        );
        assert_eq!(
            normalize_globs(&globs(&["**/*.{ts,tsx}"])),
            globs(&["**/*.{ts,tsx}"]),
            "brace alternates must stay one pattern"
        );
        assert_eq!(
            normalize_globs(&globs(&["**/*.{ts,tsx}, src/*.rs"])),
            globs(&["**/*.{ts,tsx}", "src/*.rs"])
        );
        assert_eq!(
            normalize_globs(&globs(&["{{src,app}/**/*.ts, tests/*.rs}"])),
            globs(&["{{src,app}/**/*.ts, tests/*.rs}"]),
            "nested braces stay one pattern"
        );
        assert_eq!(
            normalize_globs(&globs(&["  src/*.rs ,, ", ""])),
            globs(&["src/*.rs"])
        );
    }

    #[test]
    fn glob_matching_uses_workspace_relative_paths() {
        let files = paths(&["src/app/main.rs", "docs/readme.md"]);
        assert!(matches(&globs(&["src/**/*.rs"]), &files));
        assert!(matches(&globs(&["*.rs, docs/**/*.md"]), &files));
        assert!(!matches(&globs(&["**/*.tsx"]), &files));
        assert!(
            !matches(&globs(&["src/*.rs"]), &files),
            "literal_separator keeps * inside one segment"
        );
        assert!(!matches(&globs(&["src/**/*.rs"]), &paths(&[])));
    }

    #[test]
    fn brace_globs_match_alternate_extensions() {
        let ts = globs(&["**/*.{ts,tsx}"]);
        assert!(matches(&ts, &paths(&["app/web.tsx"])));
        assert!(matches(&ts, &paths(&["app/web.ts"])));
        assert!(!matches(&ts, &paths(&["app/web.js"])));

        let mixed = globs(&["**/*.{ts,tsx}, docs/**/*.md"]);
        assert!(matches(&mixed, &paths(&["docs/readme.md"])));
        assert!(!matches(&mixed, &paths(&["docs/readme.txt"])));
    }

    #[test]
    fn invalid_patterns_do_not_hide_valid_ones() {
        assert!(!matches(
            &globs(&["src/[unclosed"]),
            &paths(&["src/main.rs"])
        ));
        assert!(matches(
            &globs(&["src/[unclosed", "src/**/*.rs"]),
            &paths(&["src/main.rs"])
        ));
    }

    #[cfg(windows)]
    #[test]
    fn glob_matching_ignores_case_on_windows() {
        assert!(matches(&globs(&["src/**/*.rs"]), &paths(&["Src/Main.rs"])));
    }
}
