//! Redacts credentials, user paths, and email addresses from diagnostic text.

use std::sync::OnceLock;

use regex::Regex;

/// Stateless, deterministic redactor.
///
/// Compiled patterns are shared through a process-wide cache so hot paths only
/// pay for the replacements, not for regex compilation.
pub struct Redactor;

impl Redactor {
    /// Creates a redactor. Pattern compilation happens once per process.
    pub fn new() -> Self {
        Self
    }

    /// Returns a redacted copy of `value`.
    ///
    /// The operation is deterministic and idempotent for already-redacted input.
    pub fn redact(&self, value: &str) -> String {
        let patterns = patterns();
        let mut output = patterns
            .authorization
            .replace_all(value, "[REDACTED]")
            .into_owned();
        output = patterns
            .key_value
            .replace_all(&output, "[REDACTED]")
            .into_owned();
        output = patterns.jwt.replace_all(&output, "[REDACTED]").into_owned();
        output = patterns
            .provider_key
            .replace_all(&output, "[REDACTED]")
            .into_owned();
        output = patterns
            .windows_user_path
            .replace_all(&output, "%USERPROFILE%")
            .into_owned();
        output = patterns
            .macos_user_path
            .replace_all(&output, "~")
            .into_owned();
        patterns.email.replace_all(&output, "[email]").into_owned()
    }
}

impl Default for Redactor {
    fn default() -> Self {
        Self::new()
    }
}

struct Patterns {
    authorization: Regex,
    key_value: Regex,
    jwt: Regex,
    provider_key: Regex,
    windows_user_path: Regex,
    macos_user_path: Regex,
    email: Regex,
}

fn patterns() -> &'static Patterns {
    static PATTERNS: OnceLock<Patterns> = OnceLock::new();
    PATTERNS.get_or_init(|| Patterns {
        authorization: Regex::new(
            r"(?i)(?:\bauthorization\s*[:=]\s*(?:bearer\s+)?|\bbearer\s+)[^\s,;]+",
        )
        .expect("valid authorization regex"),
        key_value: Regex::new(
            r"(?i)\b(?:api[_-]?key|access[_-]?token|refresh[_-]?token|client[_-]?secret|key|token|password|passwd|secret|jwt)\s*[:=]\s*[^\s,;]+",
        )
        .expect("valid key/value regex"),
        jwt: Regex::new(r"\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+")
            .expect("valid jwt regex"),
        provider_key: Regex::new(
            r"(?i)\b(?:(?:sk-|xai-|gsk_)[a-z0-9_-]{6,}|aiza[a-z0-9_-]{10,})",
        )
        .expect("valid provider key regex"),
        windows_user_path: Regex::new(r"(?i)\b[a-z]:[\\/]+users[\\/]+[^\\/\s]+")
            .expect("valid windows path regex"),
        macos_user_path: Regex::new(r"(?i)/users/[^/\s]+").expect("valid macos path regex"),
        email: Regex::new(r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}")
            .expect("valid email regex"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_common_credentials_and_user_paths() {
        let input = concat!(
            "Authorization: Bearer sk-live-SENTINEL ",
            "api_key=abc123 token=def456 ",
            "jwt=eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.signature ",
            r"path C:\Users\alice\AppData\Local ",
            "email report@example.com"
        );
        let output = Redactor::new().redact(input);
        for secret in [
            "SENTINEL",
            "abc123",
            "def456",
            "eyJhbGci",
            "alice",
            "report@example.com",
        ] {
            assert!(!output.contains(secret), "leaked {secret}: {output}");
        }
        assert!(output.contains("[REDACTED]"));
        assert!(output.contains("%USERPROFILE%"));
    }

    #[test]
    fn redacts_url_query_and_macos_paths() {
        let output = Redactor::new()
            .redact("https://example.test/v1?api_key=SENTINEL&x=1 /Users/alice/Library");
        assert!(!output.contains("SENTINEL"), "leaked secret: {output}");
        assert!(!output.contains("alice"), "leaked user: {output}");
        assert!(output.contains("[REDACTED]"));
        assert!(output.contains('~'));
    }

    #[test]
    fn redacts_standalone_provider_keys_and_jwts() {
        let output = Redactor::new().redact(
            "sk-live-standalone xai-key-123456 gsk_abcdefgh AIzaSyABCDEFGHIJK eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.sig",
        );
        for secret in [
            "sk-live-standalone",
            "xai-key-123456",
            "gsk_abcdefgh",
            "AIzaSyABCDEFGHIJK",
            "eyJhbGciOiJIUzI1NiJ9",
        ] {
            assert!(!output.contains(secret), "leaked {secret}: {output}");
        }
    }
}
