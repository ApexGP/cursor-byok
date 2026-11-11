//! Basic, non-secret environment evidence for diagnostic reports.

/// Environment facts that never require filesystem or network access.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseEvidence {
    pub app_version: String,
    pub platform: String,
    pub arch: String,
    pub cursor_version: Option<String>,
}

/// Collects the base environment evidence.
///
/// `app_version` is passed by the caller because the desktop version differs
/// from the server crate version. Cursor's version is unknown here and stays
/// `None` for now.
pub fn collect_base_evidence(app_version: &str) -> BaseEvidence {
    BaseEvidence {
        app_version: app_version.to_string(),
        platform: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        cursor_version: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_base_evidence_uses_caller_version_and_build_target() {
        let evidence = collect_base_evidence("1.2.3");
        assert_eq!(evidence.app_version, "1.2.3");
        assert_eq!(evidence.platform, std::env::consts::OS);
        assert_eq!(evidence.arch, std::env::consts::ARCH);
        assert_eq!(evidence.cursor_version, None);
    }
}
