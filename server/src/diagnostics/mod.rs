//! Diagnostics foundation: stable error codes, event model, and redaction.
//!
//! Storage, API, evidence collection, and upload live in later modules.

pub mod code;
pub mod event;
pub mod evidence;
pub mod logs;
pub mod redact;
pub mod store;

pub use code::{DiagnosticCode, Severity};
pub use event::{fingerprint, DiagnosticContext, DiagnosticEvent};
pub use evidence::{collect_base_evidence, BaseEvidence};
pub use logs::{collect_log_tail, LogTail};
pub use redact::Redactor;
pub use store::{global, record, DiagnosticsConfig, DiagnosticsHub, ReportSummary};
