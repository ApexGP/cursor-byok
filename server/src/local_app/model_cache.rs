//! Keeps Cursor's persisted model catalog in sync with configured BYOK models.
//!
//! Cursor's agent-host module refreshes the catalog directly from the official
//! backend, bypassing the local MITM proxy. That refresh overwrites
//! `availableDefaultModels2` with the official-only list. We merge local models
//! back into Cursor's persisted state and add their IDs to
//! `aiSettings.modelOverrideEnabled`; the model picker re-adds override IDs even
//! when the base catalog no longer contains them.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::{json, Map, Value};
use sqlx::{sqlite::SqliteConnectOptions, Connection, Row, SqliteConnection};

use crate::{
    cursor::services::model_catalog::cursor_catalog_entries,
    model::ModelConfig,
    plugin::{PluginModelDescriptor, PluginRegistry},
    store::Store,
    Error, Result,
};

use super::account;

const APP_USER_KEY: &str =
    "src.vs.platform.reactivestorage.browser.reactiveStorageServiceImpl.persistentStorage.applicationUser";
const CURSOR_SERVER_CONFIG_KEY: &str = "cursorai/serverConfig";
const STATSIG_BOOTSTRAP_KEY: &str = "workbench.experiments.statsigBootstrap";
const SYNCED_IDS_KEY: &str = "cursorByokModelIds";
const HIDDEN_IDS_KEY: &str = "cursorByokHiddenModelIds";
const BACKUP_EXTENSION: &str = "vscdb.cursor-byok.bak";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const AGENT_HOST_GATE: &str = "cursor_agent_host";
const AGENT_WEBSOCKET_GATE: &str = "nal_websocket_client";
const AGENT_RETRIES_GATE: &str = "nal_agent_retries";

/// Serializes the read-modify-write of Cursor's single applicationUser JSON.
/// Concurrent model CRUD would otherwise race on stale model snapshots and
/// drop entries.
static SYNC_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub async fn sync_with(
    store: &Store,
    plugins: Option<&PluginRegistry>,
    service_url: Option<&str>,
) -> Result<CursorCatalogSync> {
    let _guard = SYNC_LOCK.lock().await;
    let path = account::state_db_path()?;
    if !path.exists() {
        return Ok(CursorCatalogSync::default());
    }
    let models = store.models().await?;
    let plugin_models = match plugins {
        Some(plugins) => plugins.configured_models().await,
        None => Vec::new(),
    };
    sync_at_with_service(&path, &models, &plugin_models, service_url).await
}

#[cfg(test)]
pub(crate) async fn sync_at(
    path: &Path,
    models: &[ModelConfig],
    plugins: &[PluginModelDescriptor],
) -> Result<CursorCatalogSync> {
    sync_at_with_service(path, models, plugins, None).await
}

pub(crate) async fn sync_at_with_service(
    path: &Path,
    models: &[ModelConfig],
    plugins: &[PluginModelDescriptor],
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
    let root_object = root
        .as_object_mut()
        .ok_or_else(|| Error::Config("Cursor applicationUser state is not an object".into()))?;

    let entries = cursor_catalog_entries(models, plugins);
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
    let mut hidden_ids = root_object
        .get(HIDDEN_IDS_KEY)
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
    // A local model the user explicitly turned off must stay in
    // `modelOverrideDisabled`; otherwise every sync (startup, model edit, plugin
    // change) silently re-enables it in Cursor's picker.
    let stale_ids = previous_ids
        .iter()
        .filter(|id| !new_ids.iter().any(|new_id| new_id == *id))
        .cloned()
        .collect::<Vec<_>>();
    hidden_ids.retain(|id| !stale_ids.contains(id));

    merge_catalog(
        root_object
            .entry("availableDefaultModels2")
            .or_insert_with(|| Value::Array(Vec::new())),
        &previous_ids,
        &new_ids,
        entries,
    )?;
    let catalog_ids = root_object
        .get("availableDefaultModels2")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.get("name").and_then(Value::as_str))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let settings = root_object.entry("aiSettings").or_insert_with(|| json!({}));
    let settings = settings
        .as_object_mut()
        .ok_or_else(|| Error::Config("Cursor aiSettings is not an object".into()))?;
    remove_ids(settings, "modelOverrideEnabled", &stale_ids);
    // `userAddedModels` makes Cursor synthesize a second, minimal
    // `isUserAdded=true` catalog row. Keep only our full `availableDefaultModels2`
    // entry and the visibility override, and clean up IDs written by older builds.
    remove_ids(settings, "userAddedModels", &stale_ids);
    remove_ids(settings, "userAddedModels", &new_ids);
    remove_ids(settings, "modelOverrideDisabled", &stale_ids);

    // Cursor prunes `modelOverrideDisabled` entries missing from a catalog
    // refresh, which silently turns them back on. Keep our own mirror of
    // explicitly hidden IDs and restore those that disappeared. An ID that
    // Cursor explicitly enabled (`modelOverrideEnabled` without a matching
    // disabled entry) clears the mirror, so re-enabling still works.
    let current_enabled = ids(settings, "modelOverrideEnabled");
    let mut current_disabled = ids(settings, "modelOverrideDisabled");
    hidden_ids.retain(|id| current_disabled.contains(id) || !current_enabled.contains(id));
    for id in &current_disabled {
        if !hidden_ids.contains(id) {
            hidden_ids.push(id.clone());
        }
    }
    for id in &hidden_ids {
        if catalog_ids.iter().any(|catalog| catalog == id) && !current_disabled.contains(id) {
            current_disabled.push(id.clone());
        }
    }
    settings.insert(
        "modelOverrideDisabled".into(),
        string_array(&current_disabled),
    );

    let mut enabled_ids = Vec::new();
    for id in &new_ids {
        if current_disabled.iter().any(|disabled| disabled == id) {
            // Cursor checks `modelOverrideEnabled` before
            // `modelOverrideDisabled`, so a stale enable entry would override
            // the user's explicit choice.
            remove_ids(settings, "modelOverrideEnabled", std::slice::from_ref(id));
        } else {
            enabled_ids.push(id.clone());
        }
    }
    add_ids(settings, "modelOverrideEnabled", &enabled_ids);
    if let Some(service_url) = service_url {
        patch_cursor_creds(root_object, service_url);
    }
    root_object.insert(SYNCED_IDS_KEY.into(), string_array(&new_ids));
    root_object.insert(HIDDEN_IDS_KEY.into(), string_array(&hidden_ids));

    let updated = serde_json::to_string(&root)?;
    backup(path)?;
    let mut transaction = connection.begin().await?;
    sqlx::query("INSERT OR REPLACE INTO ItemTable(key, value) VALUES(?, ?)")
        .bind(APP_USER_KEY)
        .bind(updated)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    if let Err(error) = patch_statsig_bootstrap(path).await {
        tracing::warn!(%error, "failed to patch Cursor Statsig cache");
    }
    if let Some(service_url) = service_url {
        if let Err(error) = patch_server_config_cache(path, service_url).await {
            tracing::warn!(%error, "failed to patch Cursor server-config cache");
        }
    }
    tracing::info!(
        model_count = new_ids.len(),
        previous_count = previous_ids.len(),
        "synced local models into Cursor state cache"
    );
    Ok(CursorCatalogSync {
        model_ids: new_ids,
        previous_ids,
    })
}

/// Cursor loads `workbench.experiments.statsigBootstrap` before the local
/// `BootstrapStatsig` response is applied, so the agent-host gate must also be
/// disabled in this persisted cache. The cache is otherwise server-driven.
async fn patch_statsig_bootstrap(path: &Path) -> Result<()> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .busy_timeout(BUSY_TIMEOUT);
    let mut connection = SqliteConnection::connect_with(&options).await?;
    let row = sqlx::query("SELECT CAST(value AS TEXT) AS value FROM ItemTable WHERE key = ?")
        .bind(STATSIG_BOOTSTRAP_KEY)
        .fetch_optional(&mut connection)
        .await?;
    let Some(row) = row else {
        return Ok(());
    };
    let text: String = row.try_get("value")?;
    let mut root: Value = serde_json::from_str(&text)
        .map_err(|error| Error::Config(format!("parse Cursor Statsig cache: {error}")))?;
    let host_key = djb2(AGENT_HOST_GATE);
    let websocket_key = djb2(AGENT_WEBSOCKET_GATE);
    let retries_key = djb2(AGENT_RETRIES_GATE);
    let gates = root
        .get_mut("feature_gates")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| Error::Config("Cursor Statsig cache has no feature_gates".into()))?;
    set_gate(gates, &host_key, false);
    set_gate(gates, &websocket_key, false);
    set_gate(gates, &retries_key, true);
    if let Some(object) = root.as_object_mut() {
        object.insert("has_updates".into(), Value::Bool(true));
        object.insert(
            "time".into(),
            (chrono::Utc::now().timestamp_millis() as u64).into(),
        );
    }
    let updated = serde_json::to_string(&root)?;
    sqlx::query("INSERT OR REPLACE INTO ItemTable(key, value) VALUES(?, ?)")
        .bind(STATSIG_BOOTSTRAP_KEY)
        .bind(updated)
        .execute(&mut connection)
        .await?;
    tracing::info!(
        agent_host_key = %host_key,
        agent_websocket_key = %websocket_key,
        retries_key = %retries_key,
        "disabled cursor_agent_host and nal_websocket_client in Cursor Statsig cache"
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

fn set_gate(gates: &mut Map<String, Value>, key: &str, value: bool) {
    let entry = gates
        .entry(key.to_owned())
        .or_insert_with(|| serde_json::json!({ "name": key }));
    if let Some(object) = entry.as_object_mut() {
        object.insert("name".into(), key.into());
        object.insert("value".into(), value.into());
        object
            .entry("rule_id")
            .or_insert_with(|| "local_enabled".into());
        object
            .entry("ruleID")
            .or_insert_with(|| "local_enabled".into());
        object.entry("id_type").or_insert_with(|| "userID".into());
        object.entry("idType").or_insert_with(|| "userID".into());
        object
            .entry("secondary_exposures")
            .or_insert_with(|| Value::Array(Vec::new()));
        object
            .entry("secondaryExposures")
            .or_insert_with(|| Value::Array(Vec::new()));
    }
}

fn djb2(value: &str) -> String {
    value
        .encode_utf16()
        .fold(0_u32, |hash, character| {
            hash.wrapping_mul(31).wrapping_add(u32::from(character))
        })
        .to_string()
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
    creds.insert("backendUrl".into(), service_url.into());
    tracing::info!(service_url, "pinned Cursor backendUrl to the local service");
}

fn ids(settings: &Map<String, Value>, key: &str) -> Vec<String> {
    settings
        .get(key)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn add_ids(settings: &mut Map<String, Value>, key: &str, ids: &[String]) {
    let values = settings
        .entry(key)
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(values) = values.as_array_mut() else {
        return;
    };
    for id in ids {
        if !values.iter().any(|value| value.as_str() == Some(id)) {
            values.push(Value::String(id.clone()));
        }
    }
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

fn backup(path: &Path) -> Result<PathBuf> {
    let backup = path.with_extension(BACKUP_EXTENSION);
    std::fs::copy(path, &backup)?;
    for suffix in ["-wal", "-shm"] {
        let source = sidecar_path(path, suffix);
        if source.exists() {
            std::fs::copy(&source, sidecar_path(&backup, suffix))?;
        }
    }
    Ok(backup)
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
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
        let mut connection = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
            .await
            .unwrap();
        let value: String =
            sqlx::query_scalar("SELECT CAST(value AS TEXT) FROM ItemTable WHERE key = ?")
                .bind(APP_USER_KEY)
                .fetch_one(&mut connection)
                .await
                .unwrap();
        serde_json::from_str(&value).unwrap()
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

        sync_at(&path, &[model()], &[]).await.unwrap();

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
        assert!(settings["modelOverrideEnabled"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "local-model"));
        assert!(!settings["modelOverrideDisabled"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "local-model" || value == "stale"));
        assert!(!settings["userAddedModels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "local-model"));
        assert_eq!(state[SYNCED_IDS_KEY], json!(["local-model"]));
        assert_eq!(state[HIDDEN_IDS_KEY], json!([]));
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

        sync_at(&path, &[model()], &[]).await.unwrap();

        let state = read(&path).await;
        let settings = &state["aiSettings"];
        assert!(settings["modelOverrideDisabled"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "local-model"));
        assert!(!settings["modelOverrideEnabled"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "local-model"));
        assert!(state["availableDefaultModels2"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == "local-model"));
        assert_eq!(state[HIDDEN_IDS_KEY], json!(["local-model"]));
    }

    #[tokio::test]
    async fn restores_hidden_models_after_a_catalog_refresh_prunes_them() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");
        seed(
            &path,
            json!({
                "availableDefaultModels2": [
                    {"name": "official-hidden", "clientDisplayName": "Official"},
                ],
                "aiSettings": {
                    "modelOverrideEnabled": [],
                    "modelOverrideDisabled": [],
                },
                "cursorByokHiddenModelIds": ["official-hidden"],
            }),
        )
        .await;

        sync_at(&path, &[], &[]).await.unwrap();

        let state = read(&path).await;
        assert_eq!(
            state["aiSettings"]["modelOverrideDisabled"],
            json!(["official-hidden"])
        );
        assert_eq!(state[HIDDEN_IDS_KEY], json!(["official-hidden"]));
    }

    #[tokio::test]
    async fn keeps_hidden_entries_for_models_that_left_the_catalog() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");
        seed(
            &path,
            json!({
                "availableDefaultModels2": [],
                "aiSettings": {
                    "modelOverrideEnabled": [],
                    "modelOverrideDisabled": [],
                },
                "cursorByokHiddenModelIds": ["retired-model"],
            }),
        )
        .await;

        sync_at(&path, &[], &[]).await.unwrap();

        let state = read(&path).await;
        assert!(state["aiSettings"]["modelOverrideDisabled"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(state[HIDDEN_IDS_KEY], json!(["retired-model"]));
    }

    #[tokio::test]
    async fn clears_the_hidden_mirror_when_the_user_re_enables_a_model() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");
        seed(
            &path,
            json!({
                "availableDefaultModels2": [
                    {"name": "official-hidden", "clientDisplayName": "Official"},
                ],
                "aiSettings": {
                    "modelOverrideEnabled": ["official-hidden"],
                    "modelOverrideDisabled": [],
                },
                "cursorByokHiddenModelIds": ["official-hidden"],
            }),
        )
        .await;

        sync_at(&path, &[], &[]).await.unwrap();

        let state = read(&path).await;
        assert!(state["aiSettings"]["modelOverrideDisabled"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(state[HIDDEN_IDS_KEY], json!([]));
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
        let host_key = djb2(AGENT_HOST_GATE);
        let websocket_key = djb2(AGENT_WEBSOCKET_GATE);
        let retries_key = djb2(AGENT_RETRIES_GATE);
        let statsig = json!({
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

        sync_at(&path, &[model()], &[]).await.unwrap();

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
        assert_eq!(statsig["feature_gates"][host_key]["value"], false);
        assert_eq!(statsig["feature_gates"][websocket_key]["value"], false);
        assert_eq!(statsig["feature_gates"][retries_key]["value"], true);
        assert_eq!(statsig["has_updates"], true);
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

        sync_at_with_service(&path, &[model()], &[], Some("http://127.0.0.1:59112"))
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

        sync_at_with_service(&path, &[model()], &[], Some("http://127.0.0.1:59112"))
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

        assert!(sync_at(&path, &[model()], &[]).await.is_err());
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

        sync_at(&path, &[model()], &[]).await.unwrap();
        assert!(!path.with_extension(BACKUP_EXTENSION).exists());
    }
}
