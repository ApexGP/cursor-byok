//! Read-only, allowlisted extraction from Cursor's `state.vscdb`.

use std::{
    path::Path,
    time::{Duration, SystemTime},
};

use serde::Serialize;
use serde_json::Value;
use sqlx::{sqlite::SqliteConnectOptions, Connection, SqliteConnection};

use crate::{diagnostics::redact::Redactor, Result};

const APP_USER_KEY: &str =
    "src.vs.platform.reactivestorage.browser.reactiveStorageServiceImpl.persistentStorage.applicationUser";
/// Arrays are capped to keep the report bounded.
const MAX_IDS: usize = 20;

/// Allowlisted Cursor model-selection state. Never contains credentials.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CursorStateEvidence {
    pub exists: bool,
    pub backup_age_ms: Option<i64>,
    pub catalog_ids: Vec<String>,
    pub synced_ids: Vec<String>,
    pub override_enabled: Vec<String>,
    pub override_disabled: Vec<String>,
    pub user_added_models: Vec<String>,
    pub truncated: bool,
    pub error: Option<String>,
}

pub async fn collect_cursor_state_evidence(path: &Path) -> Result<CursorStateEvidence> {
    let redactor = Redactor::new();
    let mut evidence = CursorStateEvidence {
        exists: path.is_file(),
        backup_age_ms: backup_age_ms(path),
        ..CursorStateEvidence::default()
    };
    if !evidence.exists {
        evidence.error = Some("state.vscdb not found".to_string());
        return Ok(evidence);
    }

    let options = SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .create_if_missing(false)
        .busy_timeout(Duration::from_secs(1))
        .pragma("query_only", "true");
    let mut connection = match SqliteConnection::connect_with(&options).await {
        Ok(connection) => connection,
        Err(error) => {
            evidence.error = Some(redactor.redact(&error.to_string()));
            return Ok(evidence);
        }
    };

    let value = match sqlx::query_scalar::<_, Option<String>>(
        "SELECT CAST(value AS TEXT) FROM ItemTable WHERE key = ?",
    )
    .bind(APP_USER_KEY)
    .fetch_optional(&mut connection)
    .await
    {
        Ok(value) => value.flatten(),
        Err(error) => {
            evidence.error = Some(redactor.redact(&error.to_string()));
            return Ok(evidence);
        }
    };
    let Some(value) = value else {
        evidence.error = Some("applicationUser state not found".to_string());
        return Ok(evidence);
    };
    let Ok(root) = serde_json::from_str::<Value>(&value) else {
        evidence.error = Some("applicationUser state is not valid JSON".to_string());
        return Ok(evidence);
    };

    let mut truncated = false;
    evidence.catalog_ids = extract_names(
        root.get("availableDefaultModels2"),
        &redactor,
        &mut truncated,
    );
    evidence.synced_ids =
        extract_strings(root.get("cursorByokModelIds"), &redactor, &mut truncated);
    let ai_settings = root.get("aiSettings");
    evidence.override_enabled = extract_strings(
        ai_settings.and_then(|settings| settings.get("modelOverrideEnabled")),
        &redactor,
        &mut truncated,
    );
    evidence.override_disabled = extract_strings(
        ai_settings.and_then(|settings| settings.get("modelOverrideDisabled")),
        &redactor,
        &mut truncated,
    );
    evidence.user_added_models = extract_strings(
        ai_settings.and_then(|settings| settings.get("userAddedModels")),
        &redactor,
        &mut truncated,
    );
    evidence.truncated = truncated;
    Ok(evidence)
}

fn extract_names(value: Option<&Value>, redactor: &Redactor, truncated: &mut bool) -> Vec<String> {
    let Some(items) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    cap(
        items
            .iter()
            .filter_map(|item| item.get("name").and_then(Value::as_str))
            .map(|name| redactor.redact(name)),
        truncated,
    )
}

fn extract_strings(
    value: Option<&Value>,
    redactor: &Redactor,
    truncated: &mut bool,
) -> Vec<String> {
    let Some(items) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    cap(
        items
            .iter()
            .filter_map(Value::as_str)
            .map(|item| redactor.redact(item)),
        truncated,
    )
}

fn cap(values: impl Iterator<Item = String>, truncated: &mut bool) -> Vec<String> {
    let mut values = values.collect::<Vec<_>>();
    if values.len() > MAX_IDS {
        values.truncate(MAX_IDS);
        *truncated = true;
    }
    values
}

/// Returns the age of the newest `state.vscdb.cursor-byok.bak*` sibling, in ms.
fn backup_age_ms(path: &Path) -> Option<i64> {
    let name = path.file_name()?.to_string_lossy();
    let parent = path.parent()?;
    let entries = std::fs::read_dir(parent).ok()?;
    let mut newest = None;
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if !file_name.starts_with(name.as_ref()) || !file_name.contains(".cursor-byok.bak") {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified()) else {
            continue;
        };
        let age = SystemTime::now()
            .duration_since(modified)
            .map(|elapsed| elapsed.as_millis() as i64)
            .unwrap_or(0);
        newest = Some(newest.map_or(age, |current: i64| current.min(age)));
    }
    newest
}

#[cfg(test)]
mod tests {
    use std::{path::Path, time::Duration};

    use serde_json::json;
    use sqlx::{Connection, SqliteConnection};

    use super::*;

    const APP_USER_KEY: &str =
        "src.vs.platform.reactivestorage.browser.reactiveStorageServiceImpl.persistentStorage.applicationUser";
    const SECRET: &str = "AUTH-SENTINEL";

    async fn seed_cursor_state(path: &Path) {
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .busy_timeout(Duration::from_secs(1));
        let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)",
        )
        .execute(&mut connection)
        .await
        .unwrap();
        let root = json!({
            "availableDefaultModels2": [
                { "name": "local-model" },
                { "name": "second-model" }
            ],
            "cursorByokModelIds": ["local-model"],
            "aiSettings": {
                "modelOverrideEnabled": ["local-model"],
                "modelOverrideDisabled": ["disabled-model"],
                "userAddedModels": ["user-model"]
            },
            "cursorCreds": { "accessToken": SECRET },
            "email": SECRET
        });
        for (key, value) in [
            (APP_USER_KEY, root.to_string()),
            ("cursorAuth/accessToken", SECRET.to_string()),
        ] {
            sqlx::query("INSERT OR REPLACE INTO ItemTable(key, value) VALUES(?, ?)")
                .bind(key)
                .bind(value)
                .execute(&mut connection)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn cursor_state_extracts_only_catalog_and_visibility_keys() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.vscdb");
        seed_cursor_state(&path).await;

        let evidence = collect_cursor_state_evidence(&path).await.unwrap();
        let json = serde_json::to_string(&evidence).unwrap();

        assert!(evidence.exists);
        assert_eq!(
            evidence.catalog_ids,
            vec!["local-model".to_string(), "second-model".to_string()]
        );
        assert_eq!(evidence.synced_ids, vec!["local-model".to_string()]);
        assert_eq!(evidence.override_enabled, vec!["local-model".to_string()]);
        assert_eq!(
            evidence.override_disabled,
            vec!["disabled-model".to_string()]
        );
        assert_eq!(evidence.user_added_models, vec!["user-model".to_string()]);
        assert!(evidence.error.is_none());
        assert!(json.contains("local-model"));
        for secret in [SECRET, "cursorCreds", "cursorAuth", "accessToken"] {
            assert!(!json.contains(secret), "leaked {secret}: {json}");
        }
    }

    #[tokio::test]
    async fn cursor_state_reports_backup_age_without_copying_the_database() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.vscdb");
        seed_cursor_state(&path).await;
        let backup = path.with_extension("vscdb.cursor-byok.bak");
        std::fs::write(&backup, b"backup").unwrap();

        let evidence = collect_cursor_state_evidence(&path).await.unwrap();

        assert!(evidence.backup_age_ms.is_some_and(|age| age >= 0));
    }

    #[tokio::test]
    async fn missing_cursor_state_is_structured_and_not_created() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("missing").join("state.vscdb");

        let evidence = collect_cursor_state_evidence(&path).await.unwrap();

        assert!(!evidence.exists);
        assert!(evidence.error.is_some());
        assert!(!path.exists(), "diagnostics must not create state.vscdb");
    }

    #[tokio::test]
    async fn cursor_state_caps_arrays_and_marks_truncation() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.vscdb");
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)",
        )
        .execute(&mut connection)
        .await
        .unwrap();
        let ids = (0..25)
            .map(|index| format!("model-{index}"))
            .collect::<Vec<_>>();
        let root = json!({
            "availableDefaultModels2": ids.iter().map(|id| json!({ "name": id })).collect::<Vec<_>>(),
            "cursorByokModelIds": ids,
            "aiSettings": {
                "modelOverrideEnabled": [],
                "modelOverrideDisabled": [],
                "userAddedModels": []
            }
        });
        sqlx::query("INSERT OR REPLACE INTO ItemTable(key, value) VALUES(?, ?)")
            .bind(APP_USER_KEY)
            .bind(root.to_string())
            .execute(&mut connection)
            .await
            .unwrap();

        let evidence = collect_cursor_state_evidence(&path).await.unwrap();

        assert_eq!(evidence.catalog_ids.len(), 20);
        assert_eq!(evidence.synced_ids.len(), 20);
        assert!(evidence.truncated);
    }
}
