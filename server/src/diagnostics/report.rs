//! Assembles diagnostic reports and renders the schema JSON shipped in issues.
//!
//! The wire shape is built explicitly so enum variants and private context
//! fields (for example `conversation_id`) never leak verbatim.

use std::path::Path;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Serialize, Serializer};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::{
    diagnostics::{
        code::Severity,
        db_evidence::DatabaseEvidence,
        event::{DiagnosticContext, DiagnosticEvent},
        evidence::{collect_base_evidence, BaseEvidence},
        logs::{collect_log_tail, LogTail},
        redact::Redactor,
        store::DiagnosticsHub,
    },
    Error, Result,
};

/// Largest JSON body embedded into the GitHub issue template.
pub const ISSUE_DIAGNOSTIC_JSON_LIMIT: usize = 6000;
/// `logs.tail` lines embedded into issue JSON (the bundle keeps more).
const ISSUE_LOG_LINES: usize = 40;
/// Arrays are capped at this many items before per-report truncation runs.
const MAX_TIMELINE_ENTRIES: usize = 20;
/// Log tail budget for a freshly assembled report.
const LOG_TAIL_BUDGET_BYTES: usize = 16 * 1024;

/// A fully assembled, redacted diagnostic report.
#[derive(Debug, Clone)]
pub struct DiagnosticReport {
    pub schema_version: u32,
    pub report_id: String,
    /// Human summary used for the issue title and body; not part of the JSON.
    pub summary: String,
    pub error_code: String,
    pub severity: Severity,
    pub captured_at: DateTime<Utc>,
    pub app: BaseEvidence,
    pub error: Value,
    pub context: DiagnosticContext,
    pub timeline: Vec<Value>,
    pub logs: LogTail,
    pub database: DatabaseEvidence,
    pub redaction: Value,
}

/// A hydrated diagnostic bundle link attached to an issue after upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleLink {
    pub url: String,
    pub sha256: String,
    pub expires_at: DateTime<Utc>,
}

/// Assembles a report from the hub's stored event plus collected evidence.
pub async fn build_report(hub: &DiagnosticsHub, report_id: &str) -> Result<DiagnosticReport> {
    let log_dir = crate::config::managed_data_dir()
        .ok()
        .map(|dir| dir.join("logs"));
    let state_db = crate::local_app::cursor_state_db_path().unwrap_or_default();
    build_report_inner(hub, report_id, log_dir.as_deref(), &state_db).await
}

#[cfg(test)]
async fn build_report_with_sources(
    hub: &DiagnosticsHub,
    report_id: &str,
    log_dir: &Path,
    state_db: &Path,
) -> Result<DiagnosticReport> {
    build_report_inner(hub, report_id, Some(log_dir), state_db).await
}

async fn build_report_inner(
    hub: &DiagnosticsHub,
    report_id: &str,
    log_dir: Option<&Path>,
    state_db: &Path,
) -> Result<DiagnosticReport> {
    let event = hub
        .event(report_id)
        .await
        .ok_or_else(|| Error::Store(format!("diagnostic report not found: {report_id}")))?;
    let config = hub.config();
    let app = collect_base_evidence(&config.app_version);
    let logs = match log_dir {
        Some(dir) => {
            collect_log_tail(
                dir,
                event.captured_at,
                config.log_window,
                LOG_TAIL_BUDGET_BYTES,
            )
            .await
        }
        None => LogTail {
            lines: Vec::new(),
            truncated: false,
            error: Some(Redactor::new().redact("cannot resolve managed data directory")),
        },
    };
    let database = crate::diagnostics::db_evidence::collect_database_evidence(
        hub.database_store(),
        &event.context,
        state_db,
    )
    .await;
    let timeline = timeline_for_event(&event);

    Ok(DiagnosticReport {
        schema_version: 1,
        report_id: event.report_id.clone(),
        summary: event.summary.clone(),
        error_code: event.code.as_str().to_string(),
        severity: event.severity,
        captured_at: event.captured_at,
        app,
        error: event_error_value(&event),
        context: event.context,
        timeline,
        logs,
        database,
        redaction: redaction_value(),
    })
}

/// Builds the initial timeline from the captured event, capped newest-first.
///
/// Only the redacted summary is used; run/request/conversation identifiers are
/// never copied into the timeline.
fn timeline_for_event(event: &DiagnosticEvent) -> Vec<Value> {
    let mut timeline = vec![json!({
        "at": timestamp(event.captured_at),
        "event": "captured",
        "detail": &event.summary,
    })];
    if timeline.len() > MAX_TIMELINE_ENTRIES {
        timeline.drain(..timeline.len() - MAX_TIMELINE_ENTRIES);
    }
    timeline
}

fn event_error_value(event: &DiagnosticEvent) -> Value {
    json!({
        "kind": &event.error_kind,
        "message": &event.error_message,
        "chain": &event.error_chain,
        "fingerprint": &event.fingerprint,
    })
}

fn context_value(context: &DiagnosticContext) -> Value {
    let conversation_id_hash = context.conversation_id.as_deref().map(conversation_id_hash);
    json!({
        "area": &context.area,
        "operation": &context.operation,
        "runId": &context.run_id,
        "requestId": &context.request_id,
        "conversationIdHash": conversation_id_hash,
        "modelHash": &context.model_hash,
        "providerType": &context.provider_type,
        "httpStatus": context.http_status,
        "takeoverEnabled": context.takeover_enabled,
        "cursorRunningAtSync": context.cursor_running_at_sync,
    })
}

/// Serializes the issue schema JSON, truncating deterministically to `max_chars`.
pub fn diagnostic_json_for_issue(report: &DiagnosticReport, max_chars: usize) -> Result<String> {
    issue_json(report, None, max_chars)
}

/// Builds the issue JSON, optionally embedding an uploaded bundle link.
pub(crate) fn issue_json(
    report: &DiagnosticReport,
    bundle: Option<&BundleLink>,
    max_chars: usize,
) -> Result<String> {
    let mut value = report_to_value(report, bundle);
    cap_log_lines(&mut value, ISSUE_LOG_LINES);
    let mut json = serialize(&value)?;
    if json.len() <= max_chars {
        return Ok(json);
    }

    loop {
        let changed = shrink_logs(&mut value)
            || shrink_timeline(&mut value)
            || shrink_llm_calls(&mut value)
            || shrink_catalog_ids(&mut value);
        if !changed {
            break;
        }
        json = serialize(&value)?;
        if json.len() <= max_chars {
            return Ok(json);
        }
    }

    compact_report_value(&mut value);
    json = serialize(&value)?;
    if json.len() <= max_chars {
        return Ok(json);
    }

    let minimal = json!({
        "schemaVersion": report.schema_version,
        "reportId": &report.report_id,
        "errorCode": &report.error_code,
        "severity": severity_str(report.severity),
        "capturedAt": timestamp(report.captured_at),
        "logs": { "tail": [], "truncated": true },
        "truncated": true,
    });
    json = serde_json::to_string(&minimal)?;
    if json.len() <= max_chars {
        return Ok(json);
    }

    let floor = serde_json::to_string(&json!({
        "schemaVersion": report.schema_version,
        "truncated": true,
    }))?;
    if floor.len() <= max_chars {
        return Ok(floor);
    }
    Err(Error::Config(format!(
        "diagnostic JSON size limit is too small: {max_chars}"
    )))
}

fn serialize(value: &Value) -> Result<String> {
    Ok(serde_json::to_string_pretty(value)?)
}

fn severity_str(severity: Severity) -> &'static str {
    match severity {
        Severity::Warning => "warning",
        Severity::Error => "error",
        Severity::Fatal => "fatal",
    }
}

fn timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn report_to_value(report: &DiagnosticReport, bundle: Option<&BundleLink>) -> Value {
    let mut logs = Map::new();
    logs.insert("tail".to_string(), json!(&report.logs.lines));
    logs.insert("truncated".to_string(), json!(report.logs.truncated));
    if let Some(error) = &report.logs.error {
        logs.insert("error".to_string(), json!(error));
    }

    let mut map = Map::new();
    map.insert("schemaVersion".to_string(), json!(report.schema_version));
    map.insert("reportId".to_string(), json!(&report.report_id));
    map.insert("errorCode".to_string(), json!(&report.error_code));
    map.insert("severity".to_string(), json!(severity_str(report.severity)));
    map.insert(
        "capturedAt".to_string(),
        json!(timestamp(report.captured_at)),
    );
    map.insert(
        "app".to_string(),
        json!({
            "version": &report.app.app_version,
            "platform": &report.app.platform,
            "arch": &report.app.arch,
            "channel": "release",
        }),
    );
    map.insert(
        "cursor".to_string(),
        json!({
            "version": &report.app.cursor_version,
            "installKind": "unknown",
        }),
    );
    map.insert("error".to_string(), report.error.clone());
    map.insert("context".to_string(), context_value(&report.context));

    let mut timeline = report.timeline.clone();
    let timeline_truncated = timeline.len() > MAX_TIMELINE_ENTRIES;
    if timeline_truncated {
        timeline.drain(..timeline.len() - MAX_TIMELINE_ENTRIES);
    }
    map.insert("timeline".to_string(), Value::Array(timeline));
    if timeline_truncated {
        map.insert("timelineTruncated".to_string(), json!(true));
    }
    map.insert("logs".to_string(), Value::Object(logs));
    map.insert(
        "database".to_string(),
        serde_json::to_value(&report.database).unwrap_or(Value::Null),
    );
    map.insert("redaction".to_string(), report.redaction.clone());
    if let Some(bundle) = bundle {
        map.insert(
            "bundle".to_string(),
            json!({
                "url": &bundle.url,
                "sha256": &bundle.sha256,
                "expiresAt": timestamp(bundle.expires_at),
            }),
        );
    }
    Value::Object(map)
}

/// Keeps only the newest `max` log lines, marking the tail truncated.
fn cap_log_lines(value: &mut Value, max: usize) {
    let len = value
        .pointer("/logs/tail")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    if len <= max {
        return;
    }
    if let Some(lines) = value
        .pointer_mut("/logs/tail")
        .and_then(Value::as_array_mut)
    {
        lines.drain(..len - max);
    }
    set_flag(value, "/logs", "truncated");
}

fn shrink_logs(value: &mut Value) -> bool {
    let len = value
        .pointer("/logs/tail")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    if len == 0 {
        return false;
    }
    if let Some(lines) = value
        .pointer_mut("/logs/tail")
        .and_then(Value::as_array_mut)
    {
        lines.drain(..len - len / 2);
    }
    set_flag(value, "/logs", "truncated");
    true
}

fn shrink_timeline(value: &mut Value) -> bool {
    let len = value
        .get("timeline")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    if len == 0 {
        return false;
    }
    if let Some(items) = value.get_mut("timeline").and_then(Value::as_array_mut) {
        items.drain(..len - len / 2);
    }
    set_flag(value, "", "timelineTruncated");
    true
}

fn shrink_llm_calls(value: &mut Value) -> bool {
    let len = value
        .pointer("/database/cursorByok/llmCalls")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    if len == 0 {
        return false;
    }
    if let Some(calls) = value
        .pointer_mut("/database/cursorByok/llmCalls")
        .and_then(Value::as_array_mut)
    {
        calls.drain(..len - len / 2);
    }
    set_flag(value, "/database/cursorByok", "llmCallsTruncated");
    true
}

fn shrink_catalog_ids(value: &mut Value) -> bool {
    for key in [
        "catalogIds",
        "syncedIds",
        "overrideEnabled",
        "overrideDisabled",
        "userAddedModels",
    ] {
        let pointer = format!("/database/cursorState/{key}");
        let len = value
            .pointer(&pointer)
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        if len == 0 {
            continue;
        }
        if let Some(items) = value.pointer_mut(&pointer).and_then(Value::as_array_mut) {
            items.drain(..len - len / 2);
        }
        set_flag(value, "/database/cursorState", "truncated");
        return true;
    }
    false
}

fn set_flag(value: &mut Value, pointer: &str, key: &str) {
    let target = if pointer.is_empty() {
        Some(value)
    } else {
        value.pointer_mut(pointer)
    };
    if let Some(object) = target.and_then(Value::as_object_mut) {
        object.insert(key.to_string(), json!(true));
    }
}

/// Last-resort reduction before falling back to a minimal document.
fn compact_report_value(value: &mut Value) {
    let mut logs_truncated = false;
    if let Some(lines) = value
        .pointer_mut("/logs/tail")
        .and_then(Value::as_array_mut)
    {
        if lines.len() > 3 {
            lines.drain(..lines.len() - 3);
            logs_truncated = true;
        }
    }
    if logs_truncated {
        set_flag(value, "/logs", "truncated");
    }

    let had_timeline = value
        .get("timeline")
        .and_then(Value::as_array)
        .is_some_and(|items| !items.is_empty());
    if had_timeline {
        if let Some(items) = value.get_mut("timeline").and_then(Value::as_array_mut) {
            items.clear();
        }
        set_flag(value, "", "timelineTruncated");
    }

    let had_llm_calls = value
        .pointer("/database/cursorByok/llmCalls")
        .and_then(Value::as_array)
        .is_some_and(|calls| !calls.is_empty());
    if had_llm_calls {
        if let Some(calls) = value
            .pointer_mut("/database/cursorByok/llmCalls")
            .and_then(Value::as_array_mut)
        {
            calls.clear();
        }
        set_flag(value, "/database/cursorByok", "llmCallsTruncated");
    }

    if let Some(db) = value
        .pointer_mut("/database/cursorByok")
        .and_then(Value::as_object_mut)
    {
        for key in ["models", "migrations"] {
            if let Some(items) = db.get_mut(key).and_then(Value::as_array_mut) {
                if items.len() > 3 {
                    items.drain(..items.len() - 3);
                }
            }
        }
    }

    let mut state_truncated = false;
    for key in [
        "catalogIds",
        "syncedIds",
        "overrideEnabled",
        "overrideDisabled",
        "userAddedModels",
    ] {
        let pointer = format!("/database/cursorState/{key}");
        if let Some(items) = value.pointer_mut(&pointer).and_then(Value::as_array_mut) {
            if !items.is_empty() {
                items.clear();
                state_truncated = true;
            }
        }
    }
    if state_truncated {
        set_flag(value, "/database/cursorState", "truncated");
    }

    clip_json_strings(value, 300);
}

fn clip_json_strings(value: &mut Value, max_bytes: usize) {
    match value {
        Value::String(text) => *text = clip_bytes(text, max_bytes),
        Value::Array(items) => {
            for item in items {
                clip_json_strings(item, max_bytes);
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                clip_json_strings(item, max_bytes);
            }
        }
        _ => {}
    }
}

/// Clips a string to at most `max_bytes`, respecting UTF-8 boundaries.
pub(crate) fn clip_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut clipped = text[..end].to_string();
    clipped.push('…');
    clipped
}

impl Serialize for DiagnosticReport {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        report_to_value(self, None).serialize(serializer)
    }
}

/// Fixed redaction statement embedded in every report.
pub(crate) fn redaction_value() -> Value {
    serde_json::json!({
        "applied": [
            "api_key",
            "authorization",
            "bearer",
            "email",
            "user_path",
            "prompt_body"
        ],
        "omitted": [
            "messages.content",
            "llm_call_requests.body",
            "state.vscdb.auth"
        ],
        "notes": ["diagnostic bundle is redacted before upload"]
    })
}

/// Hashes a conversation id, keeping only the first 16 hex characters.
pub(crate) fn conversation_id_hash(conversation_id: &str) -> String {
    let digest = Sha256::digest(conversation_id.as_bytes());
    hex::encode(digest)[..16].to_string()
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::diagnostics::{
        code::DiagnosticCode, cursor_state::CursorStateEvidence, db_evidence::CursorByokEvidence,
        event::DiagnosticContext,
    };

    pub(crate) fn at(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    pub(crate) fn empty_database() -> DatabaseEvidence {
        DatabaseEvidence {
            cursor_byok: CursorByokEvidence::default(),
            cursor_state: CursorStateEvidence::default(),
        }
    }

    pub(crate) fn test_report(code: DiagnosticCode) -> DiagnosticReport {
        DiagnosticReport {
            schema_version: 1,
            report_id: "r-20261111-0123456789abcdef0123456789abcdef".to_string(),
            summary: "模型目录同步失败".to_string(),
            error_code: code.as_str().to_string(),
            severity: code.default_severity(),
            captured_at: at("2026-11-11T12:34:56Z"),
            app: BaseEvidence {
                app_version: "1.0.6".to_string(),
                platform: "windows".to_string(),
                arch: "x86_64".to_string(),
                cursor_version: Some("0.48.6".to_string()),
            },
            error: serde_json::json!({
                "kind": "sqlite",
                "message": "database is locked",
                "chain": ["open db", "lock timeout"],
                "fingerprint": "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            }),
            context: DiagnosticContext {
                area: "sync".to_string(),
                operation: "sync_model_cache".to_string(),
                run_id: None,
                request_id: None,
                conversation_id: Some("conv-sentinel".to_string()),
                model_hash: None,
                provider_type: None,
                http_status: None,
                takeover_enabled: Some(true),
                cursor_running_at_sync: Some(true),
            },
            timeline: Vec::new(),
            logs: LogTail::default(),
            database: empty_database(),
            redaction: redaction_value(),
        }
    }

    pub(crate) fn test_report_with_logs(count: usize) -> DiagnosticReport {
        let mut report = test_report(DiagnosticCode::Sync);
        report.logs = LogTail {
            lines: (0..count)
                .map(|index| {
                    format!(
                        "2026-11-11T12:34:56.000000Z  INFO log line {index} {}",
                        "x".repeat(40)
                    )
                })
                .collect(),
            truncated: false,
            error: None,
        };
        report
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::{
        diagnostics::code::DiagnosticCode, diagnostics::event::DiagnosticEvent,
        diagnostics::store::DiagnosticsConfig, store::Store,
    };
    use std::time::Duration;

    fn test_config(temp: &tempfile::TempDir, app_version: &str) -> DiagnosticsConfig {
        DiagnosticsConfig {
            enabled: true,
            upload_url: None,
            github_repository: "ApexGP/cursor-byok".to_string(),
            retention_days: 7,
            max_events: 200,
            log_window: Duration::from_secs(60),
            data_dir: temp.path().join("diagnostics"),
            app_version: app_version.to_string(),
        }
    }

    #[tokio::test]
    async fn build_report_rejects_unknown_report() {
        let temp = tempfile::tempdir().unwrap();
        let hub = DiagnosticsHub::new(
            test_config(&temp, "1.0.6"),
            Store::connect("sqlite::memory:").await.unwrap(),
        )
        .await
        .unwrap();
        assert!(build_report(&hub, "r-missing").await.is_err());
    }

    #[tokio::test]
    async fn build_report_merges_event_environment_and_logs() {
        let temp = tempfile::tempdir().unwrap();
        let logs_dir = temp.path().join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        let log_file = format!("cursor-byok.{}.log", Utc::now().format("%Y-%m-%d"));
        std::fs::write(logs_dir.join(log_file), "plain captured line\n").unwrap();

        let hub = DiagnosticsHub::new(
            test_config(&temp, "9.9.9"),
            Store::connect("sqlite::memory:").await.unwrap(),
        )
        .await
        .unwrap();
        let mut event = DiagnosticEvent::new(DiagnosticCode::Sync, "database is locked")
            .with_error("sqlite", "database is locked");
        event.context = DiagnosticContext::new("sync", "sync_model_cache");
        hub.record(event);
        let report_id = hub.reports().await[0].report_id.clone();

        let state_db = temp.path().join("missing-state.vscdb");
        let report = build_report_with_sources(&hub, &report_id, &logs_dir, &state_db)
            .await
            .unwrap();

        assert_eq!(report.schema_version, 1);
        assert_eq!(report.report_id, report_id);
        assert_eq!(report.error_code, "CB-SYNC-001");
        assert_eq!(report.severity, Severity::Error);
        assert_eq!(report.app.app_version, "9.9.9");
        assert_eq!(report.context.area, "sync");
        assert_eq!(report.logs.error, None);
        assert!(report
            .logs
            .lines
            .iter()
            .any(|line| line.contains("captured line")));
        assert_eq!(report.timeline.len(), 1);
        assert_eq!(report.timeline[0]["event"], "captured");
        assert!(report.timeline[0]["detail"]
            .as_str()
            .unwrap()
            .contains("database is locked"));
        assert!(!report.database.cursor_state.exists);
    }

    #[test]
    fn conversation_id_is_hashed_and_never_serialized() {
        let json = diagnostic_json_for_issue(&test_report(DiagnosticCode::Sync), 6000).unwrap();
        assert!(!json.contains("conv-sentinel"));
        let parsed: Value = serde_json::from_str(&json).unwrap();
        let hash = parsed["context"]["conversationIdHash"].as_str().unwrap();
        assert_eq!(hash.len(), 16);
        assert_eq!(hash, conversation_id_hash("conv-sentinel"));
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(parsed["context"].get("conversationId").is_none());
        assert_eq!(parsed["errorCode"], "CB-SYNC-001");
        assert_eq!(parsed["severity"], "error");
        assert_eq!(parsed["app"]["channel"], "release");
        assert_eq!(parsed["cursor"]["installKind"], "unknown");
    }

    #[test]
    fn log_tail_is_capped_and_marked_truncated() {
        let report = test_report_with_logs(400);
        let json = diagnostic_json_for_issue(&report, ISSUE_DIAGNOSTIC_JSON_LIMIT).unwrap();
        let parsed: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["logs"]["truncated"], true);
        assert!(
            parsed["logs"]["tail"].as_array().unwrap().len() <= ISSUE_LOG_LINES,
            "too many log lines"
        );
    }

    #[test]
    fn truncation_preserves_valid_json_and_the_size_limit() {
        let report = test_report_with_logs(400);
        let json = diagnostic_json_for_issue(&report, 512).unwrap();
        assert!(json.len() <= 512, "json is {} bytes", json.len());
        let parsed: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["schemaVersion"], 1);
        assert_eq!(parsed["logs"]["truncated"], true);
    }

    #[test]
    fn truncation_marks_timeline_and_llm_calls() {
        let mut report = test_report_with_logs(100);
        report.timeline = (0..40)
            .map(|index| {
                serde_json::json!({"at": "2026-11-11T12:34:56Z", "event": format!("step-{index}")})
            })
            .collect();
        let mut database = empty_database();
        database.cursor_byok.llm_calls = (0..50)
            .map(
                |index| serde_json::json!({"callId": format!("call-{index}"), "durationMs": index}),
            )
            .collect();
        database.cursor_state.catalog_ids = (0..30).map(|index| format!("id-{index}")).collect();
        report.database = database;

        let mut value = report_to_value(&report, None);
        compact_report_value(&mut value);
        assert_eq!(value["logs"]["truncated"], true);
        assert_eq!(value["timelineTruncated"], true);
        assert_eq!(value["database"]["cursorByok"]["llmCallsTruncated"], true);
        assert_eq!(value["database"]["cursorState"]["truncated"], true);
        // Compaction must still produce valid JSON.
        serde_json::from_str::<Value>(&serde_json::to_string(&value).unwrap()).unwrap();
    }

    #[test]
    fn compact_does_not_mark_untouched_sections_as_truncated() {
        let mut value = report_to_value(&test_report(DiagnosticCode::Sync), None);
        compact_report_value(&mut value);
        assert_ne!(value["logs"]["truncated"], true);
        assert_ne!(value["database"]["cursorState"]["truncated"], true);
        assert!(value.get("timelineTruncated").is_none());
        assert!(value["database"]["cursorByok"]
            .get("llmCallsTruncated")
            .is_none());
    }

    #[test]
    fn timeline_is_capped_at_twenty_and_marked_truncated() {
        let mut report = test_report(DiagnosticCode::Sync);
        report.timeline = (0..25)
            .map(|index| {
                serde_json::json!({"at": "2026-11-11T12:34:56Z", "event": format!("e{index}")})
            })
            .collect();
        let json = diagnostic_json_for_issue(&report, ISSUE_DIAGNOSTIC_JSON_LIMIT).unwrap();
        let parsed: Value = serde_json::from_str(&json).unwrap();
        let timeline = parsed["timeline"].as_array().unwrap();
        assert_eq!(timeline.len(), MAX_TIMELINE_ENTRIES);
        assert_eq!(timeline.last().unwrap()["event"], "e24");
        assert_eq!(parsed["timelineTruncated"], true);
    }

    #[test]
    fn short_timeline_is_not_marked_truncated() {
        let report = test_report_with_logs(1);
        let json = diagnostic_json_for_issue(&report, ISSUE_DIAGNOSTIC_JSON_LIMIT).unwrap();
        let parsed: Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("timelineTruncated").is_none());
    }

    #[test]
    fn serialization_uses_the_issue_schema() {
        let value = serde_json::to_value(test_report(DiagnosticCode::Run)).unwrap();
        assert_eq!(value["schemaVersion"], 1);
        assert_eq!(value["errorCode"], "CB-RUN-001");
        assert!(value["context"].get("conversationIdHash").is_some());
        assert!(value["context"].get("conversationId").is_none());
    }
}
