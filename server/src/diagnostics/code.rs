//! Defines the stable diagnostic error-code registry and severity levels.
//!
//! Codes are part of the public contract: once released, a code must never be
//! reused or given a different meaning.

use serde::{Deserialize, Serialize};

/// Stable diagnostic error codes emitted at terminal failure boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticCode {
    AppStartup,
    AppPanic,
    Database,
    CursorProcess,
    CursorState,
    Sync,
    SyncVerify,
    Run,
    RunSse,
    ProviderRetry,
    ProviderIdle,
    ToolRound,
    UiBoundary,
    UiUnhandled,
    Update,
}

impl DiagnosticCode {
    /// Returns the stable `CB-<AREA>-<NNN>` identifier.
    ///
    /// Codes are append-only: never change or reuse an existing mapping.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AppStartup => "CB-APP-001",
            Self::AppPanic => "CB-APP-002",
            Self::Database => "CB-DB-001",
            Self::CursorProcess => "CB-CURSOR-001",
            Self::CursorState => "CB-CURSOR-002",
            Self::Sync => "CB-SYNC-001",
            Self::SyncVerify => "CB-SYNC-002",
            Self::Run => "CB-RUN-001",
            Self::RunSse => "CB-RUN-002",
            Self::ProviderRetry => "CB-PROVIDER-001",
            Self::ProviderIdle => "CB-PROVIDER-002",
            Self::ToolRound => "CB-TOOL-001",
            Self::UiBoundary => "CB-UI-001",
            Self::UiUnhandled => "CB-UI-002",
            Self::Update => "CB-UPDATE-001",
        }
    }

    /// Returns the default severity documented for this code.
    pub fn default_severity(self) -> Severity {
        match self {
            Self::AppStartup | Self::AppPanic => Severity::Fatal,
            Self::CursorState | Self::Update => Severity::Warning,
            Self::Database
            | Self::CursorProcess
            | Self::Sync
            | Self::SyncVerify
            | Self::Run
            | Self::RunSse
            | Self::ProviderRetry
            | Self::ProviderIdle
            | Self::ToolRound
            | Self::UiBoundary
            | Self::UiUnhandled => Severity::Error,
        }
    }
}

/// Severity assigned to a diagnostic event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Warning,
    Error,
    Fatal,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_codes_are_stable_and_unique() {
        let codes = [
            DiagnosticCode::AppStartup,
            DiagnosticCode::AppPanic,
            DiagnosticCode::Database,
            DiagnosticCode::CursorProcess,
            DiagnosticCode::CursorState,
            DiagnosticCode::Sync,
            DiagnosticCode::SyncVerify,
            DiagnosticCode::Run,
            DiagnosticCode::RunSse,
            DiagnosticCode::ProviderRetry,
            DiagnosticCode::ProviderIdle,
            DiagnosticCode::ToolRound,
            DiagnosticCode::UiBoundary,
            DiagnosticCode::UiUnhandled,
            DiagnosticCode::Update,
        ];
        let mut seen = std::collections::HashSet::new();
        for code in codes {
            assert!(seen.insert(code.as_str()));
        }
        assert_eq!(DiagnosticCode::Sync.as_str(), "CB-SYNC-001");
        assert_eq!(DiagnosticCode::Run.as_str(), "CB-RUN-001");
    }

    #[test]
    fn default_severities_match_the_design_table() {
        assert_eq!(
            DiagnosticCode::AppStartup.default_severity(),
            Severity::Fatal
        );
        assert_eq!(DiagnosticCode::AppPanic.default_severity(), Severity::Fatal);
        assert_eq!(
            DiagnosticCode::CursorState.default_severity(),
            Severity::Warning
        );
        assert_eq!(DiagnosticCode::Update.default_severity(), Severity::Warning);
        assert_eq!(DiagnosticCode::Sync.default_severity(), Severity::Error);
        assert_eq!(DiagnosticCode::Run.default_severity(), Severity::Error);
    }
}
