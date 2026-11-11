//! Builds diagnostic events at terminal failure boundaries.

use crate::{
    diagnostics::{record, DiagnosticCode, DiagnosticContext, DiagnosticEvent},
    run::{RunFailure, RunOutcome},
    Error,
};

/// Maps a terminal Run failure to its stable diagnostic code.
pub fn code_for_run_failure(failure: &RunFailure) -> DiagnosticCode {
    match failure {
        RunFailure::Provider(_) => DiagnosticCode::ProviderRetry,
        RunFailure::Store(_) => DiagnosticCode::Database,
        RunFailure::Protocol(_) | RunFailure::Client(_) => DiagnosticCode::Run,
    }
}

/// Builds the event for a terminated Agent Run.
pub fn build_run_failure_event(
    run_id: &str,
    conversation_id: &str,
    request_id: Option<&str>,
    failure: &RunFailure,
) -> DiagnosticEvent {
    let message = failure_message(failure);
    let code = code_for_run_failure(failure);
    let mut context = DiagnosticContext::new("run", "run");
    context.run_id = Some(run_id.to_owned());
    context.conversation_id = Some(conversation_id.to_owned());
    context.request_id = request_id.map(str::to_owned);
    DiagnosticEvent::new(code, summary("Agent run failed", &message))
        .with_error(failure.category(), message)
        .with_context(context)
}

/// Builds a sync failure event without touching the hot path until failure.
pub fn build_sync_error_event(
    error: &Error,
    takeover_enabled: bool,
    cursor_running: bool,
) -> DiagnosticEvent {
    let mut context = DiagnosticContext::new("sync", "sync_model_cache");
    context.takeover_enabled = Some(takeover_enabled);
    context.cursor_running_at_sync = Some(cursor_running);
    DiagnosticEvent::new(
        DiagnosticCode::Sync,
        summary("Cursor model cache sync failed", &error.to_string()),
    )
    .with_error("sync", error.to_string())
    .with_context(context)
}

/// Builds a failure event for stopping/launching Cursor.
pub fn build_process_error_event(operation: &str, error: &Error) -> DiagnosticEvent {
    let context = DiagnosticContext::new("cursor", operation);
    DiagnosticEvent::new(
        DiagnosticCode::CursorProcess,
        summary("Cursor process control failed", &error.to_string()),
    )
    .with_error(operation, error.to_string())
    .with_context(context)
}

pub fn capture_run_outcome(
    run_id: &str,
    conversation_id: &str,
    request_id: Option<&str>,
    outcome: &RunOutcome,
) {
    if let RunOutcome::Failed(failure) = outcome {
        record(build_run_failure_event(
            run_id,
            conversation_id,
            request_id,
            failure,
        ));
    }
}

pub fn capture_sync_error(error: &Error, takeover_enabled: bool, cursor_running: bool) {
    record(build_sync_error_event(
        error,
        takeover_enabled,
        cursor_running,
    ));
}

pub fn capture_process_error(operation: &str, error: &Error) {
    record(build_process_error_event(operation, error));
}

fn failure_message(failure: &RunFailure) -> String {
    match failure {
        RunFailure::Protocol(message)
        | RunFailure::Provider(message)
        | RunFailure::Store(message)
        | RunFailure::Client(message) => message.clone(),
    }
}

fn summary(prefix: &str, message: &str) -> String {
    let first_line = message.lines().next().unwrap_or("").trim();
    let text = if first_line.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}: {first_line}")
    };
    text.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::RunFailure;

    #[test]
    fn run_failures_map_to_stable_codes() {
        assert_eq!(
            code_for_run_failure(&RunFailure::Provider("x".into())).as_str(),
            "CB-PROVIDER-001"
        );
        assert_eq!(
            code_for_run_failure(&RunFailure::Store("x".into())).as_str(),
            "CB-DB-001"
        );
        assert_eq!(
            code_for_run_failure(&RunFailure::Protocol("x".into())).as_str(),
            "CB-RUN-001"
        );
        assert_eq!(
            code_for_run_failure(&RunFailure::Client("x".into())).as_str(),
            "CB-RUN-001"
        );
    }

    #[test]
    fn run_event_carries_opaque_ids_and_no_body_fields() {
        let event = build_run_failure_event(
            "run-1",
            "conversation-1",
            Some("request-1"),
            &RunFailure::Provider("upstream failed".into()),
        );
        assert_eq!(event.code.as_str(), "CB-PROVIDER-001");
        assert_eq!(event.context.run_id.as_deref(), Some("run-1"));
        assert_eq!(
            event.context.conversation_id.as_deref(),
            Some("conversation-1")
        );
        assert_eq!(event.context.request_id.as_deref(), Some("request-1"));
        assert_eq!(event.error_kind.as_deref(), Some("provider"));
    }

    #[test]
    fn sync_and_process_events_carry_operation_context() {
        let sync = build_sync_error_event(&Error::Store("database is locked".into()), true, true);
        assert_eq!(sync.code.as_str(), "CB-SYNC-001");
        assert_eq!(sync.context.takeover_enabled, Some(true));
        assert_eq!(sync.context.cursor_running_at_sync, Some(true));

        let process = build_process_error_event(
            "restart_cursor",
            &Error::Config("Cursor did not exit".into()),
        );
        assert_eq!(process.code.as_str(), "CB-CURSOR-001");
        assert_eq!(process.context.operation, "restart_cursor");
    }
}
