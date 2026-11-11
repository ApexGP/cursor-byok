//! Defines the diagnostic event/context model and fingerprinting.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::diagnostics::code::{DiagnosticCode, Severity};

/// Allowlisted, non-sensitive context attached to a diagnostic event.
///
/// Only these fields may leave the process. Request/response bodies, message
/// contents, headers, and credentials must never be added here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticContext {
    pub area: String,
    pub operation: String,
    pub run_id: Option<String>,
    pub request_id: Option<String>,
    pub conversation_id: Option<String>,
    pub model_hash: Option<String>,
    pub provider_type: Option<String>,
    pub http_status: Option<u16>,
    pub takeover_enabled: Option<bool>,
    pub cursor_running_at_sync: Option<bool>,
}

impl DiagnosticContext {
    /// Creates a context with the required area/operation and empty optional fields.
    pub fn new(area: impl Into<String>, operation: impl Into<String>) -> Self {
        Self {
            area: area.into(),
            operation: operation.into(),
            ..Self::default()
        }
    }
}

/// A captured diagnostic event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticEvent {
    pub report_id: String,
    pub code: DiagnosticCode,
    pub severity: Severity,
    pub captured_at: DateTime<Utc>,
    pub summary: String,
    pub error_kind: Option<String>,
    pub error_message: Option<String>,
    pub error_chain: Vec<String>,
    pub fingerprint: String,
    pub context: DiagnosticContext,
}

impl DiagnosticEvent {
    /// Creates an event with a generated report id and the code's default severity.
    pub fn new(code: DiagnosticCode, summary: impl Into<String>) -> Self {
        let summary = summary.into();
        let captured_at = Utc::now();
        Self {
            report_id: new_report_id(captured_at),
            code,
            severity: code.default_severity(),
            captured_at,
            fingerprint: fingerprint(code, &summary),
            summary,
            error_kind: None,
            error_message: None,
            error_chain: Vec::new(),
            context: DiagnosticContext::default(),
        }
    }

    /// Attaches error kind/message and refreshes the fingerprint from the message.
    pub fn with_error(self, kind: impl Into<String>, message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            fingerprint: fingerprint(self.code, &message),
            error_kind: Some(kind.into()),
            error_message: Some(message),
            ..self
        }
    }

    /// Replaces the allowlisted context.
    pub fn with_context(self, context: DiagnosticContext) -> Self {
        Self { context, ..self }
    }
}

/// Builds the `r-YYYYMMDD-<32 hex>` report identifier used by the upload contract.
fn new_report_id(now: DateTime<Utc>) -> String {
    format!("r-{}-{}", now.format("%Y%m%d"), Uuid::new_v4().simple())
}

/// Computes a deterministic `sha256:` fingerprint from a code and message.
///
/// The message is normalized (trimmed, lowercased, whitespace collapsed) so
/// cosmetic differences do not create distinct fingerprints.
pub fn fingerprint(code: DiagnosticCode, message: &str) -> String {
    let normalized = message
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let mut hasher = Sha256::new();
    hasher.update(code.as_str().as_bytes());
    hasher.update(b"\n");
    hasher.update(normalized.as_bytes());
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_id_has_the_expected_shape() {
        let event = DiagnosticEvent::new(DiagnosticCode::Sync, "database is locked");
        let id = event.report_id.as_str();
        assert!(id.starts_with("r-"), "unexpected report id: {id}");
        assert_eq!(id.len(), 2 + 8 + 1 + 32, "unexpected report id: {id}");
        let (date, suffix) = id[2..].split_once('-').expect("date separator");
        assert_eq!(date.len(), 8);
        assert!(date.chars().all(|c| c.is_ascii_digit()));
        assert_eq!(suffix.len(), 32);
        assert!(
            suffix
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "unexpected suffix: {suffix}"
        );
    }

    #[test]
    fn fingerprint_is_deterministic_and_normalized() {
        let first = fingerprint(DiagnosticCode::Sync, " Database  is\tlocked ");
        let second = fingerprint(DiagnosticCode::Sync, "database is locked");
        assert_eq!(first, second);
        assert_ne!(
            first,
            fingerprint(DiagnosticCode::Run, "database is locked")
        );
        assert!(first.starts_with("sha256:"));
        assert_eq!(first.len(), "sha256:".len() + 64);
    }

    #[test]
    fn event_defaults_and_builder_methods() {
        let context = DiagnosticContext::new("sync", "sync_model_cache");
        let event = DiagnosticEvent::new(DiagnosticCode::Sync, "database is locked")
            .with_error("sqlite", "database is locked")
            .with_context(context.clone());
        assert_eq!(event.severity, Severity::Error);
        assert_eq!(event.error_kind.as_deref(), Some("sqlite"));
        assert_eq!(event.error_message.as_deref(), Some("database is locked"));
        assert_eq!(event.context, context);
        assert!(event.error_chain.is_empty());
        assert!(event.report_id.starts_with("r-"));
    }
}
