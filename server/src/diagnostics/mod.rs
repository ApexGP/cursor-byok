//! Diagnostics foundation: stable error codes, event model, and redaction.
//!
//! Storage, API, evidence collection, and upload live in later modules.

pub mod code;
pub mod event;
pub mod redact;

pub use code::{DiagnosticCode, Severity};
pub use event::{fingerprint, DiagnosticContext, DiagnosticEvent};
pub use redact::Redactor;
