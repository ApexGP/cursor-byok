//! Repairs Cursor's persisted composer bookkeeping after an interrupted run.
//!
//! Cursor sets `chatGenerationUUID` when a run starts and clears it when the
//! run settles. A dropped stream or a shutdown mid-run leaves the sentinel
//! persisted next to a non-generating status, and Cursor's idle goal
//! reconciliation refuses to continue an active goal while the sentinel is
//! set. The sentinel only lives in Cursor's own store, so the repair runs
//! while Cursor is stopped, under the model-cache sync lock.
//!
//! Before writing, the database and its WAL/SHM sidecars are copied to a
//! dedicated `.composer.bak` snapshot, and the row updates are committed in one
//! transaction. A failed backup or update therefore leaves the original store
//! intact.

use std::{path::Path, time::Duration};

use base64::{engine::general_purpose::STANDARD, Engine};
use prost::Message;
use serde_json::Value;
use sqlx::{sqlite::SqliteConnectOptions, Connection, Row, SqliteConnection};

use crate::{cursor::protocol::proto::agent::v1 as pb, Result};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Goal read-back happens inside a Run, so it must never stall the turn.
const READ_BUSY_TIMEOUT: Duration = Duration::from_millis(250);
const COMPOSER_PREFIX: &str = "composerData:";
const BACKUP_EXTENSION: &str = "vscdb.cursor-byok.composer.bak";

/// What Cursor's composer cache says about the goal of a conversation.
#[derive(Debug)]
pub(crate) enum ClientGoal {
    /// The cache is missing, unreadable, or older than the goal we hold.
    Unknown,
    /// The composer applied a newer checkpoint and no longer carries a goal.
    Cleared,
    Present(pb::GoalState),
}

/// Reads the goal Cursor persisted for `conversation_id`. Cursor flushes goal
/// mutations (edit, pause, resume, clear) immediately, so this is how the Run
/// loop learns about them while a goal turn stays open. `goal_since_ms` is the
/// goal's creation time: a cache older than that has not seen this goal yet.
pub(crate) async fn read_goal(conversation_id: &str, goal_since_ms: u64) -> ClientGoal {
    let Ok(path) = super::account::state_db_path() else {
        return ClientGoal::Unknown;
    };
    read_goal_at(&path, conversation_id, goal_since_ms).await
}

pub(crate) async fn read_goal_at(
    path: &Path,
    conversation_id: &str,
    goal_since_ms: u64,
) -> ClientGoal {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .busy_timeout(READ_BUSY_TIMEOUT);
    let Ok(mut connection) = SqliteConnection::connect_with(&options).await else {
        return ClientGoal::Unknown;
    };
    let row = sqlx::query("SELECT CAST(value AS TEXT) AS value FROM cursorDiskKV WHERE key = ?")
        .bind(format!("{COMPOSER_PREFIX}{conversation_id}"))
        .fetch_optional(&mut connection)
        .await;
    let Ok(Some(row)) = row else {
        return ClientGoal::Unknown;
    };
    let Ok(text) = row.try_get::<String, _>("value") else {
        return ClientGoal::Unknown;
    };
    parse_goal(&text, goal_since_ms)
}

/// Decodes `composerData` JSON into the goal it caches.
fn parse_goal(text: &str, goal_since_ms: u64) -> ClientGoal {
    let Ok(composer) = serde_json::from_str::<Value>(text) else {
        return ClientGoal::Unknown;
    };
    let applied_at = composer
        .get("conversationCheckpointLastUpdatedAt")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    if applied_at == 0 || applied_at < goal_since_ms {
        return ClientGoal::Unknown;
    }
    let Some(state) = composer.get("conversationState").and_then(Value::as_str) else {
        return ClientGoal::Unknown;
    };
    let Some(encoded) = state.strip_prefix('~') else {
        return ClientGoal::Unknown;
    };
    let Ok(bytes) = STANDARD.decode(encoded) else {
        return ClientGoal::Unknown;
    };
    let Ok(state) = pb::ConversationStateStructure::decode(bytes.as_slice()) else {
        return ClientGoal::Unknown;
    };
    match state.goal_state {
        Some(goal) => ClientGoal::Present(goal),
        None => ClientGoal::Cleared,
    }
}

/// Clears `chatGenerationUUID` from every composer Cursor already finished.
/// Returns the number of repaired composers.
pub(super) async fn repair_interrupted_generations(path: &Path) -> Result<usize> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .busy_timeout(BUSY_TIMEOUT);
    let mut connection = SqliteConnection::connect_with(&options).await?;
    let composer_table: Option<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'cursorDiskKV'",
    )
    .fetch_optional(&mut connection)
    .await?;
    if composer_table.is_none() {
        return Ok(0);
    }
    let rows =
        sqlx::query("SELECT key, CAST(value AS TEXT) AS value FROM cursorDiskKV WHERE key LIKE ?")
            .bind(format!("{COMPOSER_PREFIX}%"))
            .fetch_all(&mut connection)
            .await?;
    let mut pending = Vec::new();
    for row in rows {
        let key: String = row.try_get("key")?;
        let text: String = row.try_get("value")?;
        let Ok(mut composer) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let Some(composer) = composer.as_object_mut() else {
            continue;
        };
        if !generation_finished(composer) {
            continue;
        }
        if composer.remove("chatGenerationUUID").is_none() {
            continue;
        }
        pending.push((key, serde_json::to_string(&*composer)?));
    }
    if pending.is_empty() {
        return Ok(0);
    }
    super::state_db::backup(path, BACKUP_EXTENSION)?;
    let mut transaction = connection.begin().await?;
    for (key, value) in &pending {
        sqlx::query("UPDATE cursorDiskKV SET value = ? WHERE key = ?")
            .bind(value)
            .bind(key)
            .execute(&mut *transaction)
            .await?;
    }
    transaction.commit().await?;
    tracing::info!(
        repaired = pending.len(),
        "cleared stale composer generation sentinels left by interrupted runs"
    );
    Ok(pending.len())
}

/// Mirrors Cursor's own invariant: a run is over once the composer stopped
/// generating and no conversation action manager still owns the stream.
fn generation_finished(composer: &serde_json::Map<String, Value>) -> bool {
    composer.get("status").and_then(Value::as_str) != Some("generating")
        && composer
            .get("conversationActionManager")
            .is_none_or(Value::is_null)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn connect(path: &Path) -> SqliteConnection {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true);
        SqliteConnection::connect_with(&options).await.unwrap()
    }

    async fn seed(path: &Path, composers: &[(&str, Value)]) {
        let mut connection = connect(path).await;
        sqlx::query("CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)")
            .execute(&mut connection)
            .await
            .unwrap();
        for (key, composer) in composers {
            sqlx::query("INSERT INTO cursorDiskKV(key, value) VALUES(?, ?)")
                .bind(key)
                .bind(composer.to_string())
                .execute(&mut connection)
                .await
                .unwrap();
        }
    }

    async fn read(path: &Path, key: &str) -> Value {
        let mut connection = connect(path).await;
        let text: String =
            sqlx::query_scalar("SELECT CAST(value AS TEXT) FROM cursorDiskKV WHERE key = ?")
                .bind(key)
                .fetch_one(&mut connection)
                .await
                .unwrap();
        serde_json::from_str(&text).unwrap()
    }

    #[tokio::test]
    async fn clears_the_sentinel_only_for_finished_composers() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");
        seed(
            &path,
            &[
                (
                    "composerData:finished",
                    serde_json::json!({
                        "status": "completed",
                        "chatGenerationUUID": "run-1",
                        "title": "keep me"
                    }),
                ),
                (
                    "composerData:aborted",
                    serde_json::json!({"status": "aborted", "chatGenerationUUID": "run-2"}),
                ),
                (
                    "composerData:running",
                    serde_json::json!({"status": "generating", "chatGenerationUUID": "run-3"}),
                ),
                (
                    "composerData:managed",
                    serde_json::json!({
                        "status": "completed",
                        "chatGenerationUUID": "run-4",
                        "conversationActionManager": {"generationUUID": "run-4"}
                    }),
                ),
                (
                    "composerData:clean",
                    serde_json::json!({"status": "completed"}),
                ),
            ],
        )
        .await;

        assert_eq!(repair_interrupted_generations(&path).await.unwrap(), 2);

        for key in ["composerData:finished", "composerData:aborted"] {
            assert!(read(&path, key).await.get("chatGenerationUUID").is_none());
        }
        assert_eq!(
            read(&path, "composerData:running").await["chatGenerationUUID"],
            "run-3"
        );
        assert_eq!(
            read(&path, "composerData:managed").await["chatGenerationUUID"],
            "run-4"
        );
        assert!(read(&path, "composerData:clean")
            .await
            .get("chatGenerationUUID")
            .is_none());

        let backup = path.with_extension(BACKUP_EXTENSION);
        assert!(backup.exists(), "the pre-repair store must be backed up");
        let original = read(&backup, "composerData:finished").await;
        assert_eq!(original["chatGenerationUUID"], "run-1");
        assert_eq!(
            read(&path, "composerData:finished").await["title"],
            "keep me",
            "unknown composer fields survive the JSON round-trip"
        );
    }

    #[tokio::test]
    async fn skips_stores_without_the_composer_table() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");
        SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true),
        )
        .await
        .unwrap();

        assert_eq!(repair_interrupted_generations(&path).await.unwrap(), 0);
        assert!(!path.with_extension(BACKUP_EXTENSION).exists());
    }

    #[tokio::test]
    async fn a_clean_store_is_not_backed_up() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");
        seed(
            &path,
            &[(
                "composerData:clean",
                serde_json::json!({"status": "completed"}),
            )],
        )
        .await;

        assert_eq!(repair_interrupted_generations(&path).await.unwrap(), 0);
        assert!(!path.with_extension(BACKUP_EXTENSION).exists());
    }

    #[tokio::test]
    async fn a_failed_backup_leaves_the_store_untouched() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");
        seed(
            &path,
            &[(
                "composerData:finished",
                serde_json::json!({"status": "completed", "chatGenerationUUID": "run-1"}),
            )],
        )
        .await;
        std::fs::create_dir(path.with_extension(BACKUP_EXTENSION)).unwrap();

        assert!(repair_interrupted_generations(&path).await.is_err());
        assert_eq!(
            read(&path, "composerData:finished").await["chatGenerationUUID"],
            "run-1"
        );
    }

    fn composer(goal: Option<pb::GoalState>, applied_at: u64) -> Value {
        let state = pb::ConversationStateStructure {
            goal_state: goal,
            ..Default::default()
        };
        let encoded = STANDARD.encode(state.encode_to_vec());
        serde_json::json!({
            "status": "generating",
            "conversationState": format!("~{encoded}"),
            "conversationCheckpointLastUpdatedAt": applied_at,
        })
    }

    fn goal() -> pb::GoalState {
        pb::GoalState {
            conversation_id: "conversation".into(),
            goal_id: "60ace89a-3eab-525f-8875-7551b64829e3".into(),
            objective: "Ship the feature".into(),
            status: 1,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn reads_the_goal_cursor_persisted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");
        seed(
            &path,
            &[("composerData:conversation", composer(Some(goal()), 2_000))],
        )
        .await;

        match read_goal_at(&path, "conversation", 1_000).await {
            ClientGoal::Present(state) => assert_eq!(state.objective, "Ship the feature"),
            other => panic!("expected a goal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reports_a_cleared_goal_only_after_the_cache_caught_up() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");
        seed(
            &path,
            &[("composerData:conversation", composer(None, 2_000))],
        )
        .await;

        assert!(matches!(
            read_goal_at(&path, "conversation", 1_000).await,
            ClientGoal::Cleared
        ));
        // The goal is newer than anything Cursor applied: nothing to conclude.
        assert!(matches!(
            read_goal_at(&path, "conversation", 3_000).await,
            ClientGoal::Unknown
        ));
    }

    #[tokio::test]
    async fn unknown_when_the_composer_is_missing_or_unreadable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");
        seed(
            &path,
            &[
                ("composerData:other", composer(Some(goal()), 2_000)),
                (
                    "composerData:broken",
                    serde_json::json!({"conversationState": "not-base64"}),
                ),
            ],
        )
        .await;

        assert!(matches!(
            read_goal_at(&path, "conversation", 1_000).await,
            ClientGoal::Unknown
        ));
        assert!(matches!(
            read_goal_at(&path, "broken", 1_000).await,
            ClientGoal::Unknown
        ));
    }
}
