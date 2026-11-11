//! Allowlisted, redacted database evidence for diagnostic reports.
//!
//! Every query uses explicit column names and every string is passed through
//! [`Redactor`] before it can reach a report. Query failures never abort report
//! generation: they are surfaced through `db_unavailable`/`error`.

use std::path::Path;

use serde::Serialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};

use crate::{
    diagnostics::{event::DiagnosticContext, redact::Redactor},
    model::{resolve_request_url, ModelType},
    store::{now_ms, Store},
};

use super::cursor_state::{collect_cursor_state_evidence, CursorStateEvidence};

/// Arrays in the diagnostic JSON are capped at this many items.
const EVIDENCE_LIMIT: usize = 20;
/// When no run id is supplied, only look back this far for a recent failure.
const RUN_LOOKBACK_MS: i64 = 24 * 60 * 60 * 1000;

const MIGRATION_COLUMNS: &str = "version, description, success";
const MODEL_COLUMNS: &str = "model_hash, model_type, base_url, use_full_url, openai_endpoint, \
    context_window_tokens, max_completion_tokens, anthropic_max_tokens, thinking_budget_tokens, \
    created_at_ms, updated_at_ms";
const RUN_COLUMNS: &str = "run_id, conversation_id, status, run_kind, subagent_kind, \
    provider_call_index, failure_category, failure_summary, created_at_ms, updated_at_ms";
const TOOL_ROUND_COLUMNS: &str =
    "round_id, status, version, next_completion_seq, created_at_ms, updated_at_ms";
const TOOL_CALL_COLUMNS: &str = "call_id, name, status, result_is_error, completed_at_ms";
const LLM_CALL_COLUMNS: &str = "call_id, run_id, provider_call_index, model_hash, provider_type, \
    request_type, model_id, status, finish_reason, created_at_ms, request_started_at_ms, \
    response_headers_at_ms, first_event_at_ms, first_text_at_ms, first_valid_response_at_ms, \
    finished_at_ms, queue_ms, ttfb_ms, ttft_ms, ttfr_ms, duration_ms, input_tokens, output_tokens, \
    total_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens, message_count, \
    tool_count, request_bytes, response_bytes, stream_event_count, http_status, error_kind, \
    error_message, reasoning_effort, fast";

/// Database evidence bundled into a diagnostic report.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseEvidence {
    pub cursor_byok: CursorByokEvidence,
    pub cursor_state: CursorStateEvidence,
}

/// Allowlisted evidence from `cursor-byok.db`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorByokEvidence {
    pub schema_version: Option<i64>,
    pub file: FileStats,
    pub migrations: Vec<Value>,
    pub settings: Value,
    pub models: Vec<Value>,
    pub run: Option<Value>,
    pub llm_calls: Vec<Value>,
    pub messages: Value,
    pub checkpoints: Value,
    pub db_unavailable: Option<String>,
}

/// SQLite file and page statistics. No database or WAL file is ever copied.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileStats {
    pub bytes: u64,
    pub wal_bytes: u64,
    pub shm_bytes: u64,
    pub page_count: Option<i64>,
    pub freelist_pages: Option<i64>,
    pub journal_mode: Option<String>,
}

/// Collects both database evidence sources. This function never fails.
pub async fn collect_database_evidence(
    store: &Store,
    context: &DiagnosticContext,
    state_db: &Path,
) -> DatabaseEvidence {
    let cursor_byok = collect_cursor_byok_evidence(store, context).await;
    let cursor_state = match collect_cursor_state_evidence(state_db).await {
        Ok(evidence) => evidence,
        Err(error) => CursorStateEvidence {
            exists: state_db.is_file(),
            error: Some(Redactor::new().redact(&error.to_string())),
            ..CursorStateEvidence::default()
        },
    };
    DatabaseEvidence {
        cursor_byok,
        cursor_state,
    }
}

async fn collect_cursor_byok_evidence(
    store: &Store,
    context: &DiagnosticContext,
) -> CursorByokEvidence {
    let redactor = Redactor::new();
    let pool = &store.pool;
    let mut errors = Vec::new();

    let schema_version = match collect_schema_version(pool).await {
        Ok(version) => version,
        Err(error) => {
            push_error(&mut errors, "schema version", error);
            None
        }
    };
    let migrations = match collect_migrations(pool, &redactor).await {
        Ok(migrations) => migrations,
        Err(error) => {
            push_error(&mut errors, "migrations", error);
            Vec::new()
        }
    };
    let file = match collect_file_stats(pool, &redactor).await {
        Ok(file) => file,
        Err(error) => {
            push_error(&mut errors, "file stats", error);
            FileStats::default()
        }
    };
    let settings = match collect_settings(pool, &redactor).await {
        Ok(settings) => settings,
        Err(error) => {
            push_error(&mut errors, "settings", error);
            json!({})
        }
    };
    let models = match collect_models(pool, &redactor).await {
        Ok(models) => models,
        Err(error) => {
            push_error(&mut errors, "model configs", error);
            Vec::new()
        }
    };
    let selected_run = match collect_run(pool, context, &redactor).await {
        Ok(run) => run,
        Err(error) => {
            push_error(&mut errors, "run", error);
            None
        }
    };
    let conversation_id = selected_run
        .as_ref()
        .map(|run| run.conversation_id.clone())
        .or_else(|| context.conversation_id.clone());

    let mut run = selected_run.as_ref().map(|run| run.value.clone());
    if let Some(selected) = &selected_run {
        match collect_tool_rounds(pool, &selected.run_id, &redactor).await {
            Ok(rounds) => {
                if let Some(value) = run.as_mut() {
                    value["toolRounds"] = Value::Array(rounds);
                }
            }
            Err(error) => push_error(&mut errors, "tool rounds", error),
        }
    }

    let llm_calls = match collect_llm_calls(
        pool,
        selected_run.as_ref().map(|run| run.run_id.as_str()),
        &redactor,
    )
    .await
    {
        Ok(calls) => calls,
        Err(error) => {
            push_error(&mut errors, "llm calls", error);
            Vec::new()
        }
    };
    let messages = match collect_message_roles(pool, conversation_id.as_deref(), &redactor).await {
        Ok(messages) => messages,
        Err(error) => {
            push_error(&mut errors, "messages", error);
            json!({})
        }
    };
    let checkpoints = match collect_checkpoints(pool, conversation_id.as_deref()).await {
        Ok(checkpoints) => checkpoints,
        Err(error) => {
            push_error(&mut errors, "checkpoints", error);
            json!({})
        }
    };

    let db_unavailable = (!errors.is_empty()).then(|| redactor.redact(&errors.join("; ")));
    CursorByokEvidence {
        schema_version,
        file,
        migrations,
        settings,
        models,
        run,
        llm_calls,
        messages,
        checkpoints,
        db_unavailable,
    }
}

fn push_error(errors: &mut Vec<String>, label: &str, error: sqlx::Error) {
    errors.push(format!("{label}: {error}"));
}

async fn collect_schema_version(pool: &SqlitePool) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations WHERE success = 1")
        .fetch_one(pool)
        .await
}

async fn collect_migrations(
    pool: &SqlitePool,
    redactor: &Redactor,
) -> Result<Vec<Value>, sqlx::Error> {
    let rows = sqlx::query(&format!(
        "SELECT {MIGRATION_COLUMNS} FROM _sqlx_migrations ORDER BY version"
    ))
    .fetch_all(pool)
    .await?;
    let mut migrations = Vec::with_capacity(rows.len());
    for row in rows {
        migrations.push(json!({
            "version": row.try_get::<i64, _>("version")?,
            "description": redactor.redact(&row.try_get::<String, _>("description")?),
            "success": row.try_get::<i64, _>("success")? != 0,
        }));
    }
    Ok(migrations)
}

async fn collect_file_stats(
    pool: &SqlitePool,
    redactor: &Redactor,
) -> Result<FileStats, sqlx::Error> {
    let mut stats = FileStats::default();
    let rows = sqlx::query("PRAGMA database_list").fetch_all(pool).await?;
    if let Some(row) = rows.iter().find(|row| {
        row.try_get::<String, _>("name")
            .is_ok_and(|name| name == "main")
    }) {
        if let Ok(file) = row.try_get::<String, _>("file") {
            if !file.is_empty() {
                stats.bytes = tokio::fs::metadata(&file)
                    .await
                    .map(|metadata| metadata.len())
                    .unwrap_or(0);
                stats.wal_bytes = tokio::fs::metadata(format!("{file}-wal"))
                    .await
                    .map(|metadata| metadata.len())
                    .unwrap_or(0);
                stats.shm_bytes = tokio::fs::metadata(format!("{file}-shm"))
                    .await
                    .map(|metadata| metadata.len())
                    .unwrap_or(0);
            }
        }
    }
    stats.page_count = sqlx::query_scalar("PRAGMA page_count")
        .fetch_optional(pool)
        .await?;
    stats.freelist_pages = sqlx::query_scalar("PRAGMA freelist_count")
        .fetch_optional(pool)
        .await?;
    stats.journal_mode = sqlx::query_scalar::<_, String>("PRAGMA journal_mode")
        .fetch_optional(pool)
        .await?
        .map(|mode| redactor.redact(&mode));
    Ok(stats)
}

async fn collect_settings(pool: &SqlitePool, redactor: &Redactor) -> Result<Value, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT setting_key, value_json FROM service_settings WHERE setting_key IN (
            'cursor_takeover_enabled', 'default_model', 'llm_detailed_logging',
            'commit_settings', 'outbound_proxy', 'cursor_tab'
         )",
    )
    .fetch_all(pool)
    .await?;

    let mut settings = Map::new();
    for row in rows {
        let key: String = row.try_get("setting_key")?;
        let raw: String = row.try_get("value_json")?;
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        match key.as_str() {
            "cursor_takeover_enabled" => {
                settings.insert(key, value.as_bool().map(Value::Bool).unwrap_or(Value::Null));
            }
            "llm_detailed_logging" => {
                settings.insert(key, value.as_bool().map(Value::Bool).unwrap_or(Value::Null));
            }
            "default_model" => {
                let value = value
                    .as_str()
                    .map(|model| Value::String(redactor.redact(model)))
                    .unwrap_or(Value::Null);
                settings.insert(key, value);
            }
            "commit_settings" => {
                let prompt = value
                    .get("prompt")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let prompt_hash = hex::encode(Sha256::digest(prompt.as_bytes()));
                settings.insert(
                    key,
                    json!({
                        "modelId": value
                            .get("model_id")
                            .and_then(Value::as_str)
                            .map(|model| redactor.redact(model)),
                        "promptLength": prompt.chars().count(),
                        "promptSha256": format!("sha256:{prompt_hash}"),
                        "promptLocale": value
                            .get("prompt_locale")
                            .and_then(Value::as_str)
                            .map(|locale| redactor.redact(locale)),
                    }),
                );
            }
            "outbound_proxy" => {
                settings.insert(
                    "proxy".to_string(),
                    json!({ "mode": value.get("mode").and_then(Value::as_str).map(|mode| redactor.redact(mode)) }),
                );
            }
            "cursor_tab" => {
                settings.insert(
                    "tab".to_string(),
                    json!({ "mode": value.get("mode").and_then(Value::as_str).map(|mode| redactor.redact(mode)) }),
                );
            }
            _ => {}
        }
    }
    Ok(Value::Object(settings))
}

async fn collect_models(pool: &SqlitePool, redactor: &Redactor) -> Result<Vec<Value>, sqlx::Error> {
    let rows = sqlx::query(&format!(
        "SELECT {MODEL_COLUMNS} FROM model_configs ORDER BY sort_order, display_name LIMIT ?"
    ))
    .bind(EVIDENCE_LIMIT as i64)
    .fetch_all(pool)
    .await?;

    let mut models = Vec::with_capacity(rows.len());
    for row in rows {
        let model_type: String = row.try_get("model_type")?;
        let base_url: String = row.try_get("base_url")?;
        let openai_endpoint: String = row.try_get("openai_endpoint")?;
        let use_full_url: i64 = row.try_get("use_full_url")?;
        let kind = if model_type == "anthropic" {
            ModelType::Anthropic
        } else {
            ModelType::OpenAi
        };
        let request_url = resolve_request_url(kind, &base_url, &openai_endpoint, use_full_url != 0)
            .ok()
            .and_then(|url| url::Url::parse(&url).ok())
            .map(|url| {
                json!({
                    "scheme": redactor.redact(url.scheme()),
                    "host": redactor.redact(url.host_str().unwrap_or_default()),
                    "path": redactor.redact(url.path()),
                })
            });

        let mut model = json!({
            "modelHash": redactor.redact(&row.try_get::<String, _>("model_hash")?),
            "modelType": redactor.redact(&model_type),
            "contextWindowTokens": row.try_get::<Option<i64>, _>("context_window_tokens")?,
            "maxCompletionTokens": row.try_get::<Option<i64>, _>("max_completion_tokens")?,
            "anthropicMaxTokens": row.try_get::<Option<i64>, _>("anthropic_max_tokens")?,
            "thinkingBudgetTokens": row.try_get::<Option<i64>, _>("thinking_budget_tokens")?,
            "createdAtMs": row.try_get::<i64, _>("created_at_ms")?,
            "updatedAtMs": row.try_get::<i64, _>("updated_at_ms")?,
        });
        if let Some(request_url) = request_url {
            model["requestUrl"] = request_url;
        }
        models.push(model);
    }
    Ok(models)
}

struct SelectedRun {
    run_id: String,
    conversation_id: String,
    value: Value,
}

async fn collect_run(
    pool: &SqlitePool,
    context: &DiagnosticContext,
    redactor: &Redactor,
) -> Result<Option<SelectedRun>, sqlx::Error> {
    let row = if let Some(run_id) = context.run_id.as_deref() {
        sqlx::query(&format!("SELECT {RUN_COLUMNS} FROM runs WHERE run_id = ?"))
            .bind(run_id)
            .fetch_optional(pool)
            .await?
    } else if let Some(conversation_id) = context.conversation_id.as_deref() {
        sqlx::query(&format!(
            "SELECT {RUN_COLUMNS} FROM runs WHERE conversation_id = ? AND status = 'failed' \
             ORDER BY updated_at_ms DESC LIMIT 1"
        ))
        .bind(conversation_id)
        .fetch_optional(pool)
        .await?
    } else {
        sqlx::query(&format!(
            "SELECT {RUN_COLUMNS} FROM runs WHERE status = 'failed' AND updated_at_ms >= ? \
             ORDER BY updated_at_ms DESC LIMIT 1"
        ))
        .bind(now_ms() - RUN_LOOKBACK_MS)
        .fetch_optional(pool)
        .await?
    };

    let Some(row) = row else {
        return Ok(None);
    };
    let run_id: String = row.try_get("run_id")?;
    let conversation_id: String = row.try_get("conversation_id")?;
    let value = json!({
        "runId": redactor.redact(&run_id),
        "status": redactor.redact(&row.try_get::<String, _>("status")?),
        "runKind": redactor.redact(&row.try_get::<String, _>("run_kind")?),
        "subagentKind": row
            .try_get::<Option<String>, _>("subagent_kind")?
            .map(|kind| redactor.redact(&kind)),
        "providerCallIndex": row.try_get::<i64, _>("provider_call_index")?,
        "failureCategory": row
            .try_get::<Option<String>, _>("failure_category")?
            .map(|category| redactor.redact(&category)),
        "failureSummary": row
            .try_get::<Option<String>, _>("failure_summary")?
            .map(|summary| redactor.redact(&summary)),
        "createdAtMs": row.try_get::<i64, _>("created_at_ms")?,
        "updatedAtMs": row.try_get::<i64, _>("updated_at_ms")?,
    });
    Ok(Some(SelectedRun {
        run_id,
        conversation_id,
        value,
    }))
}

async fn collect_tool_rounds(
    pool: &SqlitePool,
    run_id: &str,
    redactor: &Redactor,
) -> Result<Vec<Value>, sqlx::Error> {
    let rows = sqlx::query(&format!(
        "SELECT {TOOL_ROUND_COLUMNS} FROM tool_rounds WHERE run_id = ? \
         ORDER BY created_at_ms LIMIT ?"
    ))
    .bind(run_id)
    .bind(EVIDENCE_LIMIT as i64)
    .fetch_all(pool)
    .await?;

    let mut rounds = Vec::with_capacity(rows.len());
    for row in rows {
        let round_id: String = row.try_get("round_id")?;
        let calls = sqlx::query(&format!(
            "SELECT {TOOL_CALL_COLUMNS} FROM tool_round_calls WHERE round_id = ? \
             ORDER BY call_index LIMIT ?"
        ))
        .bind(&round_id)
        .bind(EVIDENCE_LIMIT as i64)
        .fetch_all(pool)
        .await?;
        let calls = calls
            .into_iter()
            .map(|call| {
                Ok(json!({
                    "callId": redactor.redact(&call.try_get::<String, _>("call_id")?),
                    "name": redactor.redact(&call.try_get::<String, _>("name")?),
                    "status": redactor.redact(&call.try_get::<String, _>("status")?),
                    "resultIsError": call
                        .try_get::<Option<i64>, _>("result_is_error")?
                        .map(|value| value != 0),
                    "completedAtMs": call.try_get::<Option<i64>, _>("completed_at_ms")?,
                }))
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?;
        rounds.push(json!({
            "roundId": redactor.redact(&round_id),
            "status": redactor.redact(&row.try_get::<String, _>("status")?),
            "version": row.try_get::<i64, _>("version")?,
            "nextCompletionSeq": row.try_get::<i64, _>("next_completion_seq")?,
            "createdAtMs": row.try_get::<i64, _>("created_at_ms")?,
            "updatedAtMs": row.try_get::<i64, _>("updated_at_ms")?,
            "calls": calls,
        }));
    }
    Ok(rounds)
}

async fn collect_llm_calls(
    pool: &SqlitePool,
    run_id: Option<&str>,
    redactor: &Redactor,
) -> Result<Vec<Value>, sqlx::Error> {
    let rows = if let Some(run_id) = run_id {
        sqlx::query(&format!(
            "SELECT {LLM_CALL_COLUMNS} FROM llm_calls WHERE run_id = ? \
             ORDER BY provider_call_index, created_at_ms LIMIT ?"
        ))
        .bind(run_id)
        .bind(EVIDENCE_LIMIT as i64)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query(&format!(
            "SELECT {LLM_CALL_COLUMNS} FROM llm_calls \
             WHERE status = 'failed' AND created_at_ms >= ? \
             ORDER BY created_at_ms DESC LIMIT ?"
        ))
        .bind(now_ms() - RUN_LOOKBACK_MS)
        .bind(EVIDENCE_LIMIT as i64)
        .fetch_all(pool)
        .await?
    };

    let mut calls = Vec::with_capacity(rows.len());
    for row in rows {
        calls.push(json!({
            "callId": redactor.redact(&row.try_get::<String, _>("call_id")?),
            "runId": redactor.redact(&row.try_get::<String, _>("run_id")?),
            "providerCallIndex": row.try_get::<i64, _>("provider_call_index")?,
            "modelHash": row
                .try_get::<Option<String>, _>("model_hash")?
                .map(|hash| redactor.redact(&hash)),
            "providerType": redactor.redact(&row.try_get::<String, _>("provider_type")?),
            "requestType": redactor.redact(&row.try_get::<String, _>("request_type")?),
            "modelId": redactor.redact(&row.try_get::<String, _>("model_id")?),
            "status": redactor.redact(&row.try_get::<String, _>("status")?),
            "finishReason": row
                .try_get::<Option<String>, _>("finish_reason")?
                .map(|reason| redactor.redact(&reason)),
            "createdAtMs": row.try_get::<i64, _>("created_at_ms")?,
            "requestStartedAtMs": row.try_get::<Option<i64>, _>("request_started_at_ms")?,
            "responseHeadersAtMs": row.try_get::<Option<i64>, _>("response_headers_at_ms")?,
            "firstEventAtMs": row.try_get::<Option<i64>, _>("first_event_at_ms")?,
            "firstTextAtMs": row.try_get::<Option<i64>, _>("first_text_at_ms")?,
            "firstValidResponseAtMs": row.try_get::<Option<i64>, _>("first_valid_response_at_ms")?,
            "finishedAtMs": row.try_get::<Option<i64>, _>("finished_at_ms")?,
            "queueMs": row.try_get::<Option<i64>, _>("queue_ms")?,
            "ttfbMs": row.try_get::<Option<i64>, _>("ttfb_ms")?,
            "ttftMs": row.try_get::<Option<i64>, _>("ttft_ms")?,
            "ttfrMs": row.try_get::<Option<i64>, _>("ttfr_ms")?,
            "durationMs": row.try_get::<Option<i64>, _>("duration_ms")?,
            "inputTokens": row.try_get::<Option<i64>, _>("input_tokens")?,
            "outputTokens": row.try_get::<Option<i64>, _>("output_tokens")?,
            "totalTokens": row.try_get::<Option<i64>, _>("total_tokens")?,
            "cacheReadTokens": row.try_get::<Option<i64>, _>("cache_read_tokens")?,
            "cacheWriteTokens": row.try_get::<Option<i64>, _>("cache_write_tokens")?,
            "reasoningTokens": row.try_get::<Option<i64>, _>("reasoning_tokens")?,
            "messageCount": row.try_get::<i64, _>("message_count")?,
            "toolCount": row.try_get::<i64, _>("tool_count")?,
            "requestBytes": row.try_get::<Option<i64>, _>("request_bytes")?,
            "responseBytes": row.try_get::<i64, _>("response_bytes")?,
            "streamEventCount": row.try_get::<i64, _>("stream_event_count")?,
            "httpStatus": row.try_get::<Option<i64>, _>("http_status")?,
            "errorKind": row
                .try_get::<Option<String>, _>("error_kind")?
                .map(|kind| redactor.redact(&kind)),
            "errorMessage": row
                .try_get::<Option<String>, _>("error_message")?
                .map(|message| redactor.redact(&message)),
            "reasoningEffort": row
                .try_get::<Option<String>, _>("reasoning_effort")?
                .map(|effort| redactor.redact(&effort)),
            "fast": row.try_get::<i64, _>("fast")? != 0,
        }));
    }
    Ok(calls)
}

async fn collect_message_roles(
    pool: &SqlitePool,
    conversation_id: Option<&str>,
    redactor: &Redactor,
) -> Result<Value, sqlx::Error> {
    let Some(conversation_id) = conversation_id else {
        return Ok(json!({}));
    };
    let rows = sqlx::query(
        "SELECT role, COUNT(*) AS count FROM messages WHERE conversation_id = ? GROUP BY role",
    )
    .bind(conversation_id)
    .fetch_all(pool)
    .await?;
    let mut counts = Map::new();
    for row in rows {
        let role = redactor.redact(&row.try_get::<String, _>("role")?);
        counts.insert(role, json!(row.try_get::<i64, _>("count")?));
    }
    Ok(Value::Object(counts))
}

async fn collect_checkpoints(
    pool: &SqlitePool,
    conversation_id: Option<&str>,
) -> Result<Value, sqlx::Error> {
    let Some(conversation_id) = conversation_id else {
        return Ok(json!({}));
    };
    let row = sqlx::query(
        "SELECT COUNT(*) AS count, MIN(created_at_ms) AS min_created_at_ms, \
         MAX(created_at_ms) AS max_created_at_ms FROM conversation_checkpoints \
         WHERE conversation_id = ?",
    )
    .bind(conversation_id)
    .fetch_one(pool)
    .await?;
    Ok(json!({
        "count": row.try_get::<i64, _>("count")?,
        "minCreatedAtMs": row.try_get::<Option<i64>, _>("min_created_at_ms")?,
        "maxCreatedAtMs": row.try_get::<Option<i64>, _>("max_created_at_ms")?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::DiagnosticContext;

    fn sync_context(run_id: &str) -> DiagnosticContext {
        let mut context = DiagnosticContext::new("sync", "sync_model_cache");
        context.run_id = Some(run_id.to_string());
        context.conversation_id = Some("conv-1".to_string());
        context
    }

    async fn seeded_store_with_secrets(temp: &tempfile::TempDir) -> Store {
        let path = temp.path().join("cursor-byok.db");
        let store = Store::connect(&format!("sqlite:{}", path.display()))
            .await
            .unwrap();

        for statement in [
            "INSERT INTO conversations(conversation_id, current_checkpoint_id, active_run_id, updated_at_ms)
             VALUES ('conv-1', 2, NULL, 1763000000000)",
            "INSERT INTO conversation_checkpoints(checkpoint_id, conversation_id, parent_checkpoint_id, state_digest, created_at_ms)
             VALUES (1, 'conv-1', NULL, randomblob(32), 1763000000000)",
            "INSERT INTO conversation_checkpoints(checkpoint_id, conversation_id, parent_checkpoint_id, state_digest, created_at_ms)
             VALUES (2, 'conv-1', 1, randomblob(32), 1763000001000)",
            "INSERT INTO runs(run_id, conversation_id, base_checkpoint_id, head_checkpoint_id, run_kind,
                subagent_kind, status, provider_call_index, failure_category, failure_summary, created_at_ms, updated_at_ms)
             VALUES ('run-1', 'conv-1', 1, 2, 'chat', NULL, 'failed', 2, 'provider',
                'upstream failed at /Users/alice/project', 1763000000000, 1763000002000)",
            "INSERT INTO tool_rounds(round_id, run_id, base_checkpoint_id, assistant_json, status, version,
                next_completion_seq, created_at_ms, updated_at_ms)
             VALUES ('round-1', 'run-1', 1, 'ASSISTANT-SENTINEL', 'failed', 3, 7,
                1763000000000, 1763000001000)",
            "INSERT INTO tool_round_calls(round_id, call_index, call_id, model_call_id, name, arguments_json,
                status, completion_seq, result_content, result_is_error, completed_at_ms)
             VALUES ('round-1', 0, 'call-1', 'model-call-1', 'shell', 'ARG-SENTINEL', 'completed', 1,
                'RESULT-SENTINEL', 1, 1763000001000)",
            "INSERT INTO llm_calls(call_id, run_id, conversation_id, provider_call_index, model_hash,
                provider_type, provider_url, request_type, request_url, model_id, display_name, status,
                finish_reason, created_at_ms, request_started_at_ms, finished_at_ms, queue_ms, ttfb_ms,
                ttft_ms, duration_ms, input_tokens, output_tokens, total_tokens, message_count, tool_count,
                request_bytes, response_bytes, stream_event_count, http_status, error_kind, error_message,
                detailed, reasoning_effort, fast)
             VALUES ('llm-1', 'run-1', 'conv-1', 2, 'model-1', 'openai', 'https://api.example.test',
                'chat', 'https://api.example.test/v1/chat/completions', 'test-model', 'Test Model',
                'failed', 'stop', 1763000000000, 1763000000100, 1763000002000, 3, 120, 150, 1900,
                100, 20, 120, 5, 1, 1024, 4096, 42, 429, 'provider', 'upstream rejected sk-SENTINEL',
                1, 'high', 1)",
            "INSERT INTO llm_call_requests(call_id, headers_json, body_json, byte_count)
             VALUES ('llm-1', '{\"authorization\":\"Bearer REQUEST-BODY-SENTINEL\"}',
                '{\"messages\":[{\"content\":\"REQUEST-BODY-SENTINEL\"}]}', 64)",
            "INSERT INTO messages(conversation_id, message_id, role, origin, payload_json, created_at_ms)
             VALUES ('conv-1', 'msg-1', 'user', 'user', 'MESSAGE-SENTINEL', 1763000000000)",
            "INSERT INTO messages(conversation_id, message_id, role, origin, payload_json, created_at_ms)
             VALUES ('conv-1', 'msg-2', 'user', 'user', 'MESSAGE-SENTINEL', 1763000000100)",
            "INSERT INTO messages(conversation_id, message_id, role, origin, payload_json, created_at_ms)
             VALUES ('conv-1', 'msg-3', 'assistant', 'assistant', 'MESSAGE-SENTINEL', 1763000000200)",
            "INSERT OR REPLACE INTO service_settings(setting_key, value_json, updated_at_ms) VALUES
                ('cursor_takeover_enabled', 'true', 1763000000000)",
            "INSERT OR REPLACE INTO service_settings(setting_key, value_json, updated_at_ms) VALUES
                ('default_model', '\"model-1\"', 1763000000000)",
            "INSERT OR REPLACE INTO service_settings(setting_key, value_json, updated_at_ms) VALUES
                ('llm_detailed_logging', 'false', 1763000000000)",
            "INSERT OR REPLACE INTO service_settings(setting_key, value_json, updated_at_ms) VALUES
                ('commit_settings', '{\"model_id\":\"model-1\",\"prompt\":\"MESSAGE-SENTINEL\",\"prompt_locale\":\"en-US\"}', 1763000000000)",
            "INSERT OR REPLACE INTO service_settings(setting_key, value_json, updated_at_ms) VALUES
                ('outbound_proxy', '{\"mode\":\"custom\",\"address\":\"http://user:PROXY-PASS-SENTINEL@PROXY-SENTINEL:8080\",\"auth_enabled\":true,\"username\":\"PROXY-USER-SENTINEL\",\"password\":\"PROXY-PASS-SENTINEL\"}', 1763000000000)",
            "INSERT OR REPLACE INTO service_settings(setting_key, value_json, updated_at_ms) VALUES
                ('cursor_tab', '{\"mode\":\"custom\",\"address\":\"https://TAB-SENTINEL.example\"}', 1763000000000)",
        ] {
            sqlx::query(statement).execute(&store.pool).await.unwrap();
        }

        sqlx::query(
            "INSERT INTO model_configs(model_hash, sort_order, display_name, model_type, base_url,
                use_full_url, api_key, tooltip_data, model_id, openai_endpoint, openai_extra_params_json,
                custom_headers_enabled, custom_headers_json, anthropic_extra_params_json,
                context_window_tokens, max_completion_tokens, created_at_ms, updated_at_ms)
             VALUES ('model-1', 1, 'Test Model', 'openai', 'https://api.example.test/v1?key=QUERY-SENTINEL',
                0, 'sk-SENTINEL', 'tooltip', 'test-model', '/v1/chat/completions',
                '{\"body\":\"EXTRA-PARAM-SENTINEL\"}', 1,
                '{\"x-api-key\":\"HEADER-SENTINEL\"}', '{}', 128000, 4096,
                1763000000000, 1763000000000)",
        )
        .execute(&store.pool)
        .await
        .unwrap();

        store
    }

    #[tokio::test]
    async fn evidence_never_contains_api_keys_or_message_bodies() {
        let temp = tempfile::tempdir().unwrap();
        let store = seeded_store_with_secrets(&temp).await;
        let state_db = temp.path().join("state.vscdb");

        let evidence = collect_database_evidence(&store, &sync_context("run-1"), &state_db).await;
        let json = serde_json::to_string(&evidence).unwrap();

        for secret in [
            "sk-SENTINEL",
            "MESSAGE-SENTINEL",
            "REQUEST-BODY-SENTINEL",
            "QUERY-SENTINEL",
            "HEADER-SENTINEL",
            "EXTRA-PARAM-SENTINEL",
            "ASSISTANT-SENTINEL",
            "ARG-SENTINEL",
            "RESULT-SENTINEL",
            "PROXY-SENTINEL",
            "PROXY-USER-SENTINEL",
            "PROXY-PASS-SENTINEL",
            "TAB-SENTINEL",
            "alice",
        ] {
            assert!(!json.contains(secret), "leaked {secret}: {json}");
        }
        assert!(
            !evidence.cursor_byok.llm_calls.is_empty(),
            "allowed llm calls must be present"
        );
    }

    #[tokio::test]
    async fn allowed_fields_are_present_after_redaction() {
        let temp = tempfile::tempdir().unwrap();
        let store = seeded_store_with_secrets(&temp).await;
        let state_db = temp.path().join("state.vscdb");

        let evidence = collect_database_evidence(&store, &sync_context("run-1"), &state_db).await;
        let json = serde_json::to_string(&evidence).unwrap();

        for allowed in [
            "\"schemaVersion\":11",
            "\"journalMode\"",
            "\"runId\":\"run-1\"",
            "\"status\":\"failed\"",
            "\"failureCategory\":\"provider\"",
            "\"roundId\":\"round-1\"",
            "\"nextCompletionSeq\":7",
            "\"callId\":\"call-1\"",
            "\"resultIsError\":true",
            "\"modelId\":\"test-model\"",
            "\"httpStatus\":429",
            "\"modelHash\":\"model-1\"",
            "\"requestUrl\":{\"host\":\"api.example.test\"",
            "\"scheme\":\"https\"",
            "\"path\":\"/v1/chat/completions\"",
            "\"contextWindowTokens\":128000",
            "\"cursor_takeover_enabled\":true",
            "\"default_model\":\"model-1\"",
            "\"commit_settings\":{\"modelId\":\"model-1\",\"promptLength\":16",
            "\"promptSha256\":\"sha256:",
            "\"proxy\":{\"mode\":\"custom\"}",
            "\"tab\":{\"mode\":\"custom\"}",
            "\"user\":2",
            "\"assistant\":1",
            "\"checkpoints\":{\"count\":2",
        ] {
            assert!(json.contains(allowed), "missing {allowed}: {json}");
        }
        assert!(evidence.cursor_byok.db_unavailable.is_none());
    }

    #[tokio::test]
    async fn unavailable_database_is_structured_and_not_fatal() {
        let temp = tempfile::tempdir().unwrap();
        let store = seeded_store_with_secrets(&temp).await;
        store.pool.close().await;
        let state_db = temp.path().join("missing").join("state.vscdb");

        let evidence = collect_database_evidence(&store, &sync_context("run-1"), &state_db).await;

        assert!(evidence.cursor_byok.db_unavailable.is_some());
        assert!(!evidence.cursor_state.exists);
        assert!(evidence.cursor_state.error.is_some());
    }

    #[test]
    fn file_stats_default_is_serializable_without_database() {
        let json = serde_json::to_string(&FileStats::default()).unwrap();
        assert!(json.contains("\"bytes\":0"));
        assert!(json.contains("\"journalMode\":null"));
    }
}
