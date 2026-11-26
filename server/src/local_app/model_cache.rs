//! Keeps Cursor's persisted model catalog in sync with configured BYOK models.
//!
//! Cursor's agent-host module refreshes the catalog directly from the official
//! backend, bypassing the local MITM proxy. That refresh overwrites
//! `availableDefaultModels2` with the official-only list, so we merge local
//! models back into the persisted catalog. User visibility choices
//! (`aiSettings.modelOverrideEnabled` / `modelOverrideDisabled`) are owned by
//! Cursor and are never rewritten. Cursor keeps `applicationUser` in memory, so
//! the sync only runs while Cursor is stopped; restart paths stop Cursor, sync,
//! then launch it again.

use std::{path::Path, time::Duration};

use serde_json::{json, Map, Value};
use sqlx::{sqlite::SqliteConnectOptions, Connection, Row, SqliteConnection};

use crate::{
    cursor::services::{analytics, model_catalog::cursor_catalog_entries},
    model::ModelConfig,
    store::Store,
    Error, Result,
};

use super::account;

const APP_USER_KEY: &str =
    "src.vs.platform.reactivestorage.browser.reactiveStorageServiceImpl.persistentStorage.applicationUser";
const ACCESS_TOKEN_KEY: &str = "cursorAuth/accessToken";
const CURSOR_SERVER_CONFIG_KEY: &str = "cursorai/serverConfig";
const STATSIG_BOOTSTRAP_KEY: &str = "workbench.experiments.statsigBootstrap";
const SYNCED_IDS_KEY: &str = "cursorByokModelIds";
const BACKUP_EXTENSION: &str = "vscdb.cursor-byok.bak";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Serializes the read-modify-write of Cursor's single applicationUser JSON.
/// Concurrent model CRUD would otherwise race on stale model snapshots and
/// drop entries.
static SYNC_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub async fn sync_with(store: &Store, service_url: Option<&str>) -> Result<CursorCatalogSync> {
    let _guard = SYNC_LOCK.lock().await;
    if super::process::cursor_running().await? {
        // Cursor keeps applicationUser in memory and periodically flushes it.
        // Writing while it runs loses the race and lets Cursor overwrite user
        // model visibility choices; restart_cursor stops it before syncing.
        tracing::info!(
            "Cursor is running; skipping applicationUser state sync to preserve model preferences"
        );
        return Ok(CursorCatalogSync::default());
    }
    let path = account::state_db_path()?;
    if !path.exists() {
        return Ok(CursorCatalogSync::default());
    }
    if let Err(error) = super::composer_state::repair_interrupted_generations(&path).await {
        tracing::warn!(%error, "failed to repair interrupted composer generations");
    }
    let models = store.models().await?;
    sync_at_with_service(&path, &models, service_url).await
}

#[cfg(test)]
pub(crate) async fn sync_at(path: &Path, models: &[ModelConfig]) -> Result<CursorCatalogSync> {
    sync_at_with_service(path, models, None).await
}

pub(crate) async fn sync_at_with_service(
    path: &Path,
    models: &[ModelConfig],
    service_url: Option<&str>,
) -> Result<CursorCatalogSync> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .busy_timeout(BUSY_TIMEOUT);
    let mut connection = SqliteConnection::connect_with(&options).await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)",
    )
    .execute(&mut connection)
    .await?;
    let row = sqlx::query("SELECT CAST(value AS TEXT) AS value FROM ItemTable WHERE key = ?")
        .bind(APP_USER_KEY)
        .fetch_optional(&mut connection)
        .await?;
    let Some(row) = row else {
        return Ok(CursorCatalogSync::default());
    };
    let text: String = row.try_get("value")?;
    let mut root: Value = serde_json::from_str(&text)
        .map_err(|error| Error::Config(format!("parse Cursor applicationUser state: {error}")))?;
    let original = root.clone();
    let root_object = root
        .as_object_mut()
        .ok_or_else(|| Error::Config("Cursor applicationUser state is not an object".into()))?;

    let entries = cursor_catalog_entries(models);
    let new_ids = entries
        .iter()
        .filter_map(|entry| entry.get("name").and_then(Value::as_str))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let previous_ids = root_object
        .get(SYNCED_IDS_KEY)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    // Only clean up entries for models that disappeared from the configuration.
    let stale_ids = previous_ids
        .iter()
        .filter(|id| !new_ids.iter().any(|new_id| new_id == *id))
        .cloned()
        .collect::<Vec<_>>();

    merge_catalog(
        root_object
            .entry("availableDefaultModels2")
            .or_insert_with(|| Value::Array(Vec::new())),
        &previous_ids,
        &new_ids,
        entries,
    )?;
    let settings = root_object.entry("aiSettings").or_insert_with(|| json!({}));
    let settings = settings
        .as_object_mut()
        .ok_or_else(|| Error::Config("Cursor aiSettings is not an object".into()))?;
    // `userAddedModels` makes Cursor synthesize a second, minimal
    // `isUserAdded=true` catalog row. Clean up only IDs written by older builds.
    // `modelOverrideEnabled` / `modelOverrideDisabled` are the user's choices;
    // never rewrite them here, or a concurrent Cursor flush loses the selection.
    remove_ids(settings, "userAddedModels", &stale_ids);
    remove_ids(settings, "userAddedModels", &new_ids);
    if let Some(service_url) = service_url {
        patch_cursor_creds(root_object, service_url);
    }
    if previous_ids != new_ids {
        root_object.insert(SYNCED_IDS_KEY.into(), string_array(&new_ids));
    }

    if root == original {
        tracing::info!(
            model_count = new_ids.len(),
            "Cursor state cache already in sync; skipping write"
        );
    } else {
        let updated = serde_json::to_string(&root)?;
        super::state_db::backup(path, BACKUP_EXTENSION)?;
        let mut transaction = connection.begin().await?;
        sqlx::query("INSERT OR REPLACE INTO ItemTable(key, value) VALUES(?, ?)")
            .bind(APP_USER_KEY)
            .bind(updated)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        tracing::info!(
            model_count = new_ids.len(),
            previous_count = previous_ids.len(),
            "synced local models into Cursor state cache"
        );
    }
    if let Err(error) = patch_statsig_bootstrap(path).await {
        tracing::warn!(%error, "failed to patch Cursor Statsig cache");
    }
    if let Some(service_url) = service_url {
        if let Err(error) = patch_server_config_cache(path, service_url).await {
            tracing::warn!(%error, "failed to patch Cursor server-config cache");
        }
    }
    Ok(CursorCatalogSync {
        model_ids: new_ids,
        previous_ids,
    })
}

/// Cursor loads `workbench.experiments.statsigBootstrap` before the local
/// `BootstrapStatsig` response is applied, so the agent-host, WebSocket, and
/// MCP meta gates must also be patched (or seeded when absent) in this
/// persisted cache with a `user.userID` matching the active JWT subject.
async fn patch_statsig_bootstrap(path: &Path) -> Result<()> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .busy_timeout(BUSY_TIMEOUT);
    let mut connection = SqliteConnection::connect_with(&options).await?;
    let token_row = sqlx::query("SELECT CAST(value AS TEXT) AS value FROM ItemTable WHERE key = ?")
        .bind(ACCESS_TOKEN_KEY)
        .fetch_optional(&mut connection)
        .await?;
    let auth_subject = token_row
        .and_then(|row| row.try_get::<String, _>("value").ok())
        .and_then(|token| account::jwt_subject(&token));
    let row = sqlx::query("SELECT CAST(value AS TEXT) AS value FROM ItemTable WHERE key = ?")
        .bind(STATSIG_BOOTSTRAP_KEY)
        .fetch_optional(&mut connection)
        .await?;
    let seeded = row.is_none();
    let root = match row {
        Some(row) => {
            let text: String = row.try_get("value")?;
            let mut parsed: Value = serde_json::from_str(&text)
                .map_err(|error| Error::Config(format!("parse Cursor Statsig cache: {error}")))?;
            analytics::reconcile_local_statsig_user(&mut parsed, auth_subject.as_deref());
            analytics::patch_statsig_gates(&mut parsed)?;
            parsed
        }
        None => analytics::local_statsig_bootstrap(auth_subject.as_deref())?,
    };
    let updated = serde_json::to_string(&root)?;
    sqlx::query("INSERT OR REPLACE INTO ItemTable(key, value) VALUES(?, ?)")
        .bind(STATSIG_BOOTSTRAP_KEY)
        .bind(updated)
        .execute(&mut connection)
        .await?;
    tracing::info!(
        seeded,
        auth_subject = auth_subject.as_deref().unwrap_or(account::SUBJECT),
        "patched Cursor Statsig cache (agent host + websocket disabled, MCP meta enabled)"
    );
    Ok(())
}

/// Cursor applies the persisted `cursorai/serverConfig` before the local
/// `GetServerConfig` response. Rewrite its agent URLs so the agent host starts
/// against the local service instead of the official agent backend.
async fn patch_server_config_cache(path: &Path, service_url: &str) -> Result<()> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .busy_timeout(BUSY_TIMEOUT);
    let mut connection = SqliteConnection::connect_with(&options).await?;
    let row = sqlx::query("SELECT CAST(value AS TEXT) AS value FROM ItemTable WHERE key = ?")
        .bind(CURSOR_SERVER_CONFIG_KEY)
        .fetch_optional(&mut connection)
        .await?;
    let Some(row) = row else {
        return Ok(());
    };
    let text: String = row.try_get("value")?;
    let mut root: Value = serde_json::from_str(&text)
        .map_err(|error| Error::Config(format!("parse Cursor server-config cache: {error}")))?;
    let Some(agent) = root
        .get_mut("agentUrlConfig")
        .and_then(Value::as_object_mut)
    else {
        return Ok(());
    };
    agent.insert("agentUrl".into(), service_url.into());
    agent.insert("agentnUrl".into(), service_url.into());
    agent.insert("localLoopInferenceUrl".into(), "".into());
    let updated = serde_json::to_string(&root)?;
    sqlx::query("INSERT OR REPLACE INTO ItemTable(key, value) VALUES(?, ?)")
        .bind(CURSOR_SERVER_CONFIG_KEY)
        .bind(updated)
        .execute(&mut connection)
        .await?;
    tracing::info!(
        service_url,
        "pinned Cursor server-config cache agent URLs to the local service"
    );
    Ok(())
}

#[derive(Debug, Default, serde::Serialize)]
pub(crate) struct CursorCatalogSync {
    pub model_ids: Vec<String>,
    pub previous_ids: Vec<String>,
}

fn merge_catalog(
    catalog: &mut Value,
    previous_ids: &[String],
    new_ids: &[String],
    entries: Vec<Value>,
) -> Result<()> {
    let catalog = catalog
        .as_array_mut()
        .ok_or_else(|| Error::Config("Cursor availableDefaultModels2 is not an array".into()))?;
    catalog.retain(|entry| {
        let Some(name) = entry.get("name").and_then(Value::as_str) else {
            return true;
        };
        !previous_ids.iter().any(|id| id == name) && !new_ids.iter().any(|id| id == name)
    });
    catalog.extend(entries);
    Ok(())
}

/// Cursor's extension-host transport bypasses `http.proxy`. With HTTP/2
/// disabled it selects `cursorCreds.backendUrl` for Run/Bidi, so pin that URL to
/// the local service as a fallback for an already-running instance. The primary
/// path is launching Cursor with `--test-backend-url`; this state write also
/// helps when the process predates the takeover. Keep the official account
/// label and agent URLs untouched so official authentication keeps working.
fn patch_cursor_creds(root: &mut Map<String, Value>, service_url: &str) {
    let Some(creds) = root
        .entry("cursorCreds")
        .or_insert_with(|| json!({}))
        .as_object_mut()
    else {
        return;
    };
    if creds.get("backendUrl").and_then(Value::as_str) == Some(service_url) {
        return;
    }
    creds.insert("backendUrl".into(), service_url.into());
    tracing::info!(service_url, "pinned Cursor backendUrl to the local service");
}

fn remove_ids(settings: &mut Map<String, Value>, key: &str, ids: &[String]) {
    if let Some(values) = settings.get_mut(key).and_then(Value::as_array_mut) {
        values.retain(|value| {
            value
                .as_str()
                .is_none_or(|value| !ids.iter().any(|id| id == value))
        });
    }
}

fn string_array(values: &[String]) -> Value {
    Value::Array(values.iter().cloned().map(Value::String).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelConfig, ModelType, OPENAI_CHAT_ENDPOINT};

    fn model() -> ModelConfig {
        ModelConfig {
            model_hash: "local-model".into(),
            sort_order: 0,
            display_name: "Local Model".into(),
            group_name: Some("DeepSeek".into()),
            model_type: ModelType::OpenAi,
            base_url: "https://provider.example/v1/chat/completions".into(),
            use_full_url: true,
            api_key: "provider-secret".into(),
            tooltip_data: "Local Model".into(),
            model_id: "upstream-model".into(),
            reasoning_effort: None,
            openai_endpoint: OPENAI_CHAT_ENDPOINT.into(),
            openai_extra_params_enabled: false,
            openai_extra_params: serde_json::json!({}),
            custom_headers_enabled: false,
            custom_headers: serde_json::json!({}),
            anthropic_extra_params_enabled: false,
            anthropic_extra_params: serde_json::json!({}),
            context_window_tokens: Some(1_000_000),
            max_completion_tokens: None,
            anthropic_max_tokens: None,
            anthropic_thinking_effort: None,
            thinking_budget_tokens: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    async fn seed(path: &Path, application_user: Value) {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true);
        let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)",
        )
        .execute(&mut connection)
        .await
        .unwrap();
        sqlx::query("INSERT OR REPLACE INTO ItemTable(key, value) VALUES(?, ?)")
            .bind(APP_USER_KEY)
            .bind(application_user.to_string())
            .execute(&mut connection)
            .await
            .unwrap();
    }

    async fn read(path: &Path) -> Value {
        serde_json::from_str(&read_raw(path).await).unwrap()
    }

    async fn read_raw(path: &Path) -> String {
        let mut connection = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
            .await
            .unwrap();
        sqlx::query_scalar("SELECT CAST(value AS TEXT) FROM ItemTable WHERE key = ?")
            .bind(APP_USER_KEY)
            .fetch_one(&mut connection)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn merges_local_models_and_removes_stale_entries() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");
        seed(
            &path,
            json!({
                "availableDefaultModels2": [
                    {"name": "official", "clientDisplayName": "Official"},
                    {"name": "stale", "clientDisplayName": "Stale"},
                ],
                "aiSettings": {
                    "modelOverrideEnabled": ["official", "stale"],
                    "modelOverrideDisabled": ["stale"],
                    "userAddedModels": ["stale"],
                },
                "cursorByokModelIds": ["stale"],
            }),
        )
        .await;

        sync_at(&path, &[model()]).await.unwrap();

        let state = read(&path).await;
        let catalog = state["availableDefaultModels2"].as_array().unwrap();
        let names = catalog
            .iter()
            .filter_map(|entry| entry["name"].as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"official"));
        assert!(!names.contains(&"stale"));
        assert!(names.contains(&"local-model"));
        let local = catalog
            .iter()
            .find(|entry| entry["name"] == "local-model")
            .expect("local model entry");
        assert_eq!(local["clientDisplayName"], "Local Model");
        assert_eq!(local["contextTokenLimit"], 1_000_000);
        assert!(!local["parameterDefinitions"].as_array().unwrap().is_empty());
        assert!(!local["variants"].as_array().unwrap().is_empty());

        let settings = &state["aiSettings"];
        // Visibility is Cursor-owned: the sync must not add, remove, or reorder
        // these entries, even for local models.
        assert_eq!(
            settings["modelOverrideEnabled"],
            json!(["official", "stale"])
        );
        assert_eq!(settings["modelOverrideDisabled"], json!(["stale"]));
        assert!(!settings["userAddedModels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "local-model" || value == "stale"));
        assert_eq!(state[SYNCED_IDS_KEY], json!(["local-model"]));
        assert!(path.with_extension(BACKUP_EXTENSION).exists());
    }

    #[tokio::test]
    async fn preserves_local_models_the_user_turned_off() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");
        seed(
            &path,
            json!({
                "availableDefaultModels2": [
                    {"name": "local-model", "clientDisplayName": "Local Model", "defaultOn": true},
                ],
                "aiSettings": {
                    "modelOverrideEnabled": ["local-model"],
                    "modelOverrideDisabled": ["local-model"],
                },
                "cursorByokModelIds": ["local-model"],
            }),
        )
        .await;

        sync_at(&path, &[model()]).await.unwrap();

        let state = read(&path).await;
        let settings = &state["aiSettings"];
        assert_eq!(settings["modelOverrideEnabled"], json!(["local-model"]));
        assert_eq!(settings["modelOverrideDisabled"], json!(["local-model"]));
        assert!(state["availableDefaultModels2"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == "local-model"));
    }

    #[tokio::test]
    async fn skips_the_write_when_the_state_is_already_current() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");
        let entry = crate::cursor::services::model_catalog::cursor_catalog_entries(&[model()])
            .into_iter()
            .next()
            .expect("catalog entry");
        seed(
            &path,
            json!({
                "availableDefaultModels2": [entry],
                "aiSettings": {
                    "modelOverrideEnabled": [],
                    "modelOverrideDisabled": ["official-hidden"],
                },
                "cursorByokModelIds": ["local-model"],
            }),
        )
        .await;
        let before = read_raw(&path).await;

        sync_at(&path, &[model()]).await.unwrap();

        assert_eq!(read_raw(&path).await, before);
        assert!(!path.with_extension(BACKUP_EXTENSION).exists());
    }

    #[tokio::test]
    async fn disables_agent_host_in_persisted_statsig_cache() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");
        seed(
            &path,
            json!({
                "availableDefaultModels2": [],
                "aiSettings": {},
            }),
        )
        .await;
        let host_key = analytics::djb2("cursor_agent_host");
        let websocket_key = analytics::djb2("nal_websocket_client");
        let retries_key = analytics::djb2("nal_agent_retries");
        let meta_mcp_key = analytics::djb2("meta_mcp_tool");
        let meta_slim_key = analytics::djb2("mcp_meta_tool_slim_descriptors");
        let input_schema_json_key = analytics::djb2("mcp_input_schema_json");
        let statsig = json!({
            "hash_used": "djb2",
            "user": { "userID": "auth0|user_official" },
            "feature_gates": {
                host_key.clone(): { "name": host_key, "value": true },
                websocket_key.clone(): { "name": websocket_key, "value": true },
                retries_key.clone(): { "name": retries_key, "value": false },
            },
            "has_updates": false,
        });
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
        sqlx::query("INSERT OR REPLACE INTO ItemTable(key, value) VALUES(?, ?)")
            .bind(STATSIG_BOOTSTRAP_KEY)
            .bind(statsig.to_string())
            .execute(&mut connection)
            .await
            .unwrap();
        drop(connection);

        sync_at(&path, &[model()]).await.unwrap();

        let mut connection = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
            .await
            .unwrap();
        let value: String =
            sqlx::query_scalar("SELECT CAST(value AS TEXT) FROM ItemTable WHERE key = ?")
                .bind(STATSIG_BOOTSTRAP_KEY)
                .fetch_one(&mut connection)
                .await
                .unwrap();
        let statsig: Value = serde_json::from_str(&value).unwrap();
        assert_eq!(statsig["user"]["userID"], "auth0|user_official");
        assert_eq!(statsig["feature_gates"][host_key]["value"], false);
        assert_eq!(statsig["feature_gates"][websocket_key]["value"], false);
        assert_eq!(statsig["feature_gates"][retries_key]["value"], true);
        assert_eq!(statsig["feature_gates"][meta_mcp_key]["value"], true);
        assert_eq!(statsig["feature_gates"][meta_slim_key]["value"], true);
        assert_eq!(
            statsig["feature_gates"][input_schema_json_key]["value"],
            true
        );
        assert_eq!(statsig["has_updates"], true);
    }

    #[tokio::test]
    async fn repairs_legacy_local_ultra_user_and_seeds_missing_statsig_cache() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");
        seed(
            &path,
            json!({
                "availableDefaultModels2": [],
                "aiSettings": {},
            }),
        )
        .await;
        let token = account::local_token().unwrap();
        let legacy = json!({
            "hash_used": "none",
            "user": {
                "userID": "local_ultra",
                "customIDs": { "localUserID": "local_ultra" }
            },
            "sdkParams": { "stableID": "local_ultra" },
            "feature_gates": {},
            "has_updates": true,
        });
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
        sqlx::query("INSERT OR REPLACE INTO ItemTable(key, value) VALUES(?, ?)")
            .bind(ACCESS_TOKEN_KEY)
            .bind(&token)
            .execute(&mut connection)
            .await
            .unwrap();
        sqlx::query("INSERT OR REPLACE INTO ItemTable(key, value) VALUES(?, ?)")
            .bind(STATSIG_BOOTSTRAP_KEY)
            .bind(legacy.to_string())
            .execute(&mut connection)
            .await
            .unwrap();
        drop(connection);

        sync_at(&path, &[model()]).await.unwrap();

        let mut connection = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
            .await
            .unwrap();
        let value: String =
            sqlx::query_scalar("SELECT CAST(value AS TEXT) FROM ItemTable WHERE key = ?")
                .bind(STATSIG_BOOTSTRAP_KEY)
                .fetch_one(&mut connection)
                .await
                .unwrap();
        let statsig: Value = serde_json::from_str(&value).unwrap();
        assert_eq!(statsig["user"]["userID"], account::SUBJECT);
        assert_eq!(
            statsig["user"]["customIDs"]["localUserID"],
            account::SUBJECT
        );
        assert_eq!(statsig["sdkParams"]["stableID"], account::SUBJECT);
        assert_eq!(statsig["feature_gates"]["meta_mcp_tool"]["value"], true);
        assert_eq!(
            statsig["feature_gates"]["mcp_meta_tool_slim_descriptors"]["value"],
            true
        );

        // Removing the row seeds a fresh local bootstrap on the next sync.
        sqlx::query("DELETE FROM ItemTable WHERE key = ?")
            .bind(STATSIG_BOOTSTRAP_KEY)
            .execute(&mut connection)
            .await
            .unwrap();
        drop(connection);

        sync_at(&path, &[model()]).await.unwrap();

        let mut connection = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
            .await
            .unwrap();
        let seeded: String =
            sqlx::query_scalar("SELECT CAST(value AS TEXT) FROM ItemTable WHERE key = ?")
                .bind(STATSIG_BOOTSTRAP_KEY)
                .fetch_one(&mut connection)
                .await
                .unwrap();
        let statsig: Value = serde_json::from_str(&seeded).unwrap();
        assert_eq!(statsig["user"]["userID"], account::SUBJECT);
        assert_eq!(statsig["feature_gates"]["meta_mcp_tool"]["value"], true);
    }

    #[tokio::test]
    async fn malformed_statsig_cache_does_not_abort_model_sync() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");
        seed(
            &path,
            json!({
                "availableDefaultModels2": [],
                "aiSettings": {},
            }),
        )
        .await;
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
        sqlx::query("INSERT OR REPLACE INTO ItemTable(key, value) VALUES(?, ?)")
            .bind(ACCESS_TOKEN_KEY)
            .bind("not-a-jwt")
            .execute(&mut connection)
            .await
            .unwrap();
        sqlx::query("INSERT OR REPLACE INTO ItemTable(key, value) VALUES(?, ?)")
            .bind(STATSIG_BOOTSTRAP_KEY)
            .bind("{broken-json")
            .execute(&mut connection)
            .await
            .unwrap();
        drop(connection);

        let result = sync_at(&path, &[model()]).await.unwrap();
        assert_eq!(result.model_ids, vec!["local-model"]);
    }

    #[tokio::test]
    async fn pins_backend_url_without_touching_official_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");
        seed(
            &path,
            json!({
                "availableDefaultModels2": [],
                "aiSettings": {},
                "cursorCreds": {
                    "backendUrl": "https://api2.cursor.sh",
                    "credentialsDisplayName": "Prod",
                    "agentBackendUrlNonPrivacy": { "default": "https://api2.cursor.sh" },
                    "agentBackendUrlPrivacy": { "default": "https://agent.api5.cursor.sh" },
                    "websiteUrl": "https://cursor.com"
                }
            }),
        )
        .await;

        sync_at_with_service(&path, &[model()], Some("http://127.0.0.1:59112"))
            .await
            .unwrap();

        let state = read(&path).await;
        assert_eq!(state["cursorCreds"]["backendUrl"], "http://127.0.0.1:59112");
        assert_eq!(state["cursorCreds"]["credentialsDisplayName"], "Prod");
        assert_eq!(
            state["cursorCreds"]["agentBackendUrlNonPrivacy"]["default"],
            "https://api2.cursor.sh"
        );
        assert_eq!(
            state["cursorCreds"]["agentBackendUrlPrivacy"]["default"],
            "https://agent.api5.cursor.sh"
        );
        assert_eq!(state["cursorCreds"]["websiteUrl"], "https://cursor.com");
    }

    #[tokio::test]
    async fn pins_agent_urls_in_the_persisted_server_config() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");
        seed(
            &path,
            json!({ "availableDefaultModels2": [], "aiSettings": {} }),
        )
        .await;
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
        sqlx::query("INSERT OR REPLACE INTO ItemTable(key, value) VALUES(?, ?)")
            .bind(CURSOR_SERVER_CONFIG_KEY)
            .bind(
                json!({
                    "agentUrlConfig": {
                        "agentUrl": "https://agentn.global.api5.cursor.sh",
                        "agentnUrl": "https://agentn.global.api5.cursor.sh",
                        "localLoopInferenceUrl": "https://agentn.global.api5.cursor.sh"
                    }
                })
                .to_string(),
            )
            .execute(&mut connection)
            .await
            .unwrap();
        drop(connection);

        sync_at_with_service(&path, &[model()], Some("http://127.0.0.1:59112"))
            .await
            .unwrap();

        let mut connection = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
            .await
            .unwrap();
        let value: String =
            sqlx::query_scalar("SELECT CAST(value AS TEXT) FROM ItemTable WHERE key = ?")
                .bind(CURSOR_SERVER_CONFIG_KEY)
                .fetch_one(&mut connection)
                .await
                .unwrap();
        let config: Value = serde_json::from_str(&value).unwrap();
        assert_eq!(
            config["agentUrlConfig"]["agentnUrl"],
            "http://127.0.0.1:59112"
        );
        assert_eq!(
            config["agentUrlConfig"]["agentUrl"],
            "http://127.0.0.1:59112"
        );
        assert_eq!(config["agentUrlConfig"]["localLoopInferenceUrl"], "");
    }

    #[tokio::test]
    async fn malformed_state_is_left_untouched() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)",
        )
        .execute(&mut connection)
        .await
        .unwrap();
        sqlx::query("INSERT OR REPLACE INTO ItemTable(key, value) VALUES(?, ?)")
            .bind(APP_USER_KEY)
            .bind("{not-json")
            .execute(&mut connection)
            .await
            .unwrap();
        drop(connection);

        assert!(sync_at(&path, &[model()]).await.is_err());
        assert!(!path.with_extension(BACKUP_EXTENSION).exists());
    }

    #[tokio::test]
    async fn missing_state_key_is_a_noop() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        SqliteConnection::connect_with(&options).await.unwrap();

        sync_at(&path, &[model()]).await.unwrap();
        assert!(!path.with_extension(BACKUP_EXTENSION).exists());
    }
}
