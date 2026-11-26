//! Repairs Cursor's persisted composer bookkeeping after an interrupted run.
//!
//! Cursor sets `chatGenerationUUID` when a run starts and clears it when the
//! run settles. A dropped stream or a shutdown mid-run leaves the sentinel
//! persisted next to a non-generating status, and Cursor's idle goal
//! reconciliation refuses to continue an active goal while the sentinel is
//! set. The sentinel only lives in Cursor's own store, so the repair runs
//! while Cursor is stopped, before it loads its composers.

use std::{path::Path, time::Duration};

use serde_json::Value;
use sqlx::{sqlite::SqliteConnectOptions, Connection, Row, SqliteConnection};

use crate::Result;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const COMPOSER_PREFIX: &str = "composerData:";

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
    let rows = sqlx::query("SELECT key, CAST(value AS TEXT) AS value FROM cursorDiskKV WHERE key LIKE ?")
        .bind(format!("{COMPOSER_PREFIX}%"))
        .fetch_all(&mut connection)
        .await?;
    let mut repaired = 0;
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
        sqlx::query("UPDATE cursorDiskKV SET value = ? WHERE key = ?")
            .bind(serde_json::to_string(&*composer)?)
            .bind(key)
            .execute(&mut connection)
            .await?;
        repaired += 1;
    }
    if repaired > 0 {
        tracing::info!(
            repaired,
            "cleared stale composer generation sentinels left by interrupted runs"
        );
    }
    Ok(repaired)
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
        sqlx::query(
            "CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)",
        )
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
        let text: String = sqlx::query_scalar("SELECT CAST(value AS TEXT) FROM cursorDiskKV WHERE key = ?")
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
                    serde_json::json!({"status": "completed", "chatGenerationUUID": "run-1"}),
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
                ("composerData:clean", serde_json::json!({"status": "completed"})),
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
    }
}
