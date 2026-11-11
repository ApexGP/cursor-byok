//! Diagnostics foundation: stable error codes, event model, and redaction.
//!
//! Storage, API, evidence collection, and upload live in later modules.

pub mod bundle;
pub mod capture;
pub mod code;
pub mod cursor_state;
pub mod db_evidence;
pub mod event;
pub mod evidence;
pub mod issue;
pub mod logs;
pub mod panic;
pub mod redact;
pub mod report;
pub mod store;
pub mod upload;

pub use bundle::{build_bundle, Bundle, BUNDLE_SCHEMA_VERSION, MAX_BUNDLE_BYTES};
pub use capture::{
    build_process_error_event, build_run_failure_event, build_sync_error_event,
    capture_process_error, capture_run_outcome, capture_sync_error, code_for_run_failure,
};
pub use code::{DiagnosticCode, Severity};
pub use cursor_state::{collect_cursor_state_evidence, CursorStateEvidence};
pub use db_evidence::{collect_database_evidence, CursorByokEvidence, DatabaseEvidence, FileStats};
pub use event::{fingerprint, DiagnosticContext, DiagnosticEvent};
pub use evidence::{collect_base_evidence, BaseEvidence};
pub use issue::{issue_url, render_issue, IssueDraft};
pub use logs::{collect_log_tail, LogTail};
pub use panic::{install_panic_hook, recover_crash_files};
pub use redact::Redactor;
pub use report::{build_report, diagnostic_json_for_issue, BundleLink, DiagnosticReport};
pub use store::{global, record, DiagnosticsConfig, DiagnosticsHub, ReportSummary};
pub use upload::{build_upload_and_link, ensure_upload_configured, upload_bundle, UploadedBundle};
