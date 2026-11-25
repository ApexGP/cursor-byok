//! Implements Cursor analytics endpoints and event handling.
use axum::{
    body::{to_bytes, Body, Bytes},
    extract::Extension,
    http::{header, HeaderValue, Request, Response, StatusCode},
};
use base64::{engine::general_purpose::STANDARD, Engine};
use bytes::{BufMut, BytesMut};
use prost::Message;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::{api::cursor::proxy, local_app, Error, Result};

pub const BOOTSTRAP_STATSIG_PATH: &str = "/aiserver.v1.AnalyticsService/BootstrapStatsig";
const AGENT_RETRIES_GATE: &str = "nal_agent_retries";
/// Cursor's installed agent-host module fetches the model catalog directly from
/// the official backend, bypassing the local proxy, then overwrites the
/// workbench's BYOK catalog. Disabling the gate makes Cursor use the static
/// workbench path, which honors the local catalog.
const AGENT_HOST_GATE: &str = "cursor_agent_host";
/// The agent WebSocket transport dials the official backend directly and
/// bypasses the local proxy. Disable it so agent runs use the HTTP transport
/// (BidiAppend/RunSSE) that the local server implements.
const AGENT_WEBSOCKET_GATE: &str = "nal_websocket_client";
/// Inject MCP tools as a compact namespace catalog instead of sending every
/// tool schema in the model request.
const META_MCP_TOOL_GATE: &str = "meta_mcp_tool";
/// Keep only tool names (and server instructions) in the meta descriptors.
const MCP_META_SLIM_GATE: &str = "mcp_meta_tool_slim_descriptors";
/// Statsig returns false for gates missing from the bootstrap payload instead
/// of falling back to Cursor's compiled default; keep JSON schema transport on.
const MCP_INPUT_SCHEMA_JSON_GATE: &str = "mcp_input_schema_json";
const LOCAL_RULE: &str = "local_enabled";
const LEGACY_LOCAL_USER_ID: &str = "local_ultra";

#[derive(Clone, PartialEq, Message)]
struct BootstrapStatsigResponse {
    #[prost(string, tag = "1")]
    config: String,
    #[prost(uint64, tag = "2")]
    generated_at_ms: u64,
}

pub async fn bootstrap_statsig(
    Extension(upstream): Extension<proxy::CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    tracing::info!("Cursor Statsig bootstrap requested");
    let user_id = local_app::request_cursor_subject(request.headers());
    if local_app::request_uses_local_cursor_token(request.headers()) {
        consume_body(request).await?;
        return local_response(user_id.as_deref());
    }
    match proxy::forward_buffered(&upstream, request).await {
        Ok(response) if response.status.is_success() => match patch_upstream(response) {
            Ok(response) => Ok(response),
            Err(error) => {
                tracing::warn!(%error, "Cursor Statsig bootstrap was invalid; using local bootstrap");
                local_response(user_id.as_deref())
            }
        },
        Ok(response) => {
            tracing::warn!(status = %response.status, "Cursor Statsig bootstrap was rejected; using local bootstrap");
            local_response(user_id.as_deref())
        }
        Err(error) => {
            tracing::warn!(%error, "Cursor Statsig bootstrap was unavailable; using local bootstrap");
            local_response(user_id.as_deref())
        }
    }
}

async fn consume_body(request: Request<Body>) -> Result<()> {
    to_bytes(request.into_body(), usize::MAX)
        .await
        .map_err(|error| Error::Protocol(format!("cannot read Statsig request body: {error}")))?;
    Ok(())
}

fn patch_upstream(response: proxy::BufferedResponse) -> Result<Response<Body>> {
    let (framed, payload) = unary_payload(&response.body)?;
    let mut message = BootstrapStatsigResponse::decode(payload)?;
    let mut config = serde_json::from_str::<Value>(&message.config)?;
    patch_statsig_gates(&mut config)?;
    let now = chrono::Utc::now().timestamp_millis() as u64;
    message.generated_at_ms = now;
    message.config = serde_json::to_string(&config)?;
    Ok(response.with_body(encode_unary(&message, framed)))
}

fn local_response(user_id: Option<&str>) -> Result<Response<Body>> {
    let config = local_statsig_bootstrap(user_id)?;
    let generated_at_ms = config
        .get("time")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis() as u64);
    let message = BootstrapStatsigResponse {
        config: serde_json::to_string(&config)?,
        generated_at_ms,
    };
    let body = message.encode_to_vec();
    let mut response = Response::new(Body::from(body.clone()));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/proto"),
    );
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        body.len()
            .to_string()
            .parse()
            .expect("body length is a valid header value"),
    );
    Ok(response)
}

/// Builds a self-contained local Statsig bootstrap whose `user.userID` matches
/// Cursor's JWT subject so the client's `StatsigDataAdapter.setData` and
/// `updateUserSync` hash to the same cache key.
pub(crate) fn local_statsig_bootstrap(user_id: Option<&str>) -> Result<Value> {
    let generated_at_ms = chrono::Utc::now().timestamp_millis() as u64;
    let user_id = effective_user_id(user_id);
    let mut config = json!({
        "feature_gates": {},
        "dynamic_configs": {},
        "layer_configs": {},
        "user": {
            "userID": user_id,
            "customIDs": { "localUserID": user_id }
        },
        "has_updates": true,
        "hash_used": "none",
        "sdkParams": {
            "stableID": user_id,
            "disableDiagnosticsLogging": true
        },
        "time": generated_at_ms
    });
    patch_statsig_gates(&mut config)?;
    Ok(config)
}

/// Keeps a locally generated (or legacy `local_ultra`) Statsig bootstrap cache
/// aligned with the active Cursor JWT subject without mutating official
/// server-issued user metadata.
pub(crate) fn reconcile_local_statsig_user(config: &mut Value, user_id: Option<&str>) {
    let Some(root) = config.as_object_mut() else {
        return;
    };
    let is_local = root.get("hash_used").and_then(Value::as_str) == Some("none");
    let current_id = root
        .get("user")
        .and_then(|user| user.get("userID"))
        .and_then(Value::as_str);
    if !is_local && current_id != Some(LEGACY_LOCAL_USER_ID) {
        return;
    }
    let target_id = effective_user_id(user_id);
    if !root.get("user").is_some_and(Value::is_object) {
        root.insert("user".into(), Value::Object(Map::new()));
    }
    if let Some(user) = root.get_mut("user").and_then(Value::as_object_mut) {
        user.insert("userID".into(), Value::String(target_id.to_owned()));
        if let Some(custom_ids) = user.get_mut("customIDs").and_then(Value::as_object_mut) {
            if custom_ids.contains_key("localUserID") {
                custom_ids.insert("localUserID".into(), Value::String(target_id.to_owned()));
            }
        }
    }
    if let Some(sdk_params) = root.get_mut("sdkParams").and_then(Value::as_object_mut) {
        if sdk_params.get("stableID").and_then(Value::as_str) == Some(LEGACY_LOCAL_USER_ID) {
            sdk_params.insert("stableID".into(), Value::String(target_id.to_owned()));
        }
    }
}

fn effective_user_id(user_id: Option<&str>) -> &str {
    user_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(local_app::LOCAL_CURSOR_SUBJECT)
}

pub(crate) fn patch_statsig_gates(config: &mut Value) -> Result<()> {
    let hash_used = config
        .get("hash_used")
        .and_then(Value::as_str)
        .unwrap_or("none")
        .to_owned();
    let retries_key = statsig_key(config, AGENT_RETRIES_GATE);
    let agent_host_key = statsig_key(config, AGENT_HOST_GATE);
    let agent_websocket_key = statsig_key(config, AGENT_WEBSOCKET_GATE);
    let meta_mcp_key = statsig_key(config, META_MCP_TOOL_GATE);
    let meta_slim_key = statsig_key(config, MCP_META_SLIM_GATE);
    let input_schema_json_key = statsig_key(config, MCP_INPUT_SCHEMA_JSON_GATE);
    let now = chrono::Utc::now().timestamp_millis() as u64;
    let root = config
        .as_object_mut()
        .ok_or_else(|| Error::Protocol("Statsig bootstrap config must be an object".into()))?;
    // A cached Statsig bootstrap with has_updates=false makes Cursor keep the
    // server-driven gate values, so force a refresh whenever we patch.
    root.insert("has_updates".into(), Value::Bool(true));
    root.insert("time".into(), now.into());
    let gates = root
        .entry("feature_gates")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| Error::Protocol("Statsig feature_gates must be an object".into()))?;
    gates.insert(retries_key.clone(), gate(&retries_key, true));
    gates.insert(agent_host_key.clone(), gate(&agent_host_key, false));
    gates.insert(
        agent_websocket_key.clone(),
        gate(&agent_websocket_key, false),
    );
    gates.insert(meta_mcp_key.clone(), gate(&meta_mcp_key, true));
    gates.insert(meta_slim_key.clone(), gate(&meta_slim_key, true));
    gates.insert(
        input_schema_json_key.clone(),
        gate(&input_schema_json_key, true),
    );
    tracing::info!(
        hash_used,
        retries_key = %retries_key,
        agent_host_key = %agent_host_key,
        agent_websocket_key = %agent_websocket_key,
        meta_mcp_key = %meta_mcp_key,
        meta_slim_key = %meta_slim_key,
        input_schema_json_key = %input_schema_json_key,
        "patched Cursor Statsig gates (agent host + websocket disabled, MCP meta enabled)"
    );
    Ok(())
}

pub(crate) fn statsig_key(config: &Value, name: &str) -> String {
    match config.get("hash_used").and_then(Value::as_str) {
        Some("djb2") => djb2(name),
        Some("sha256") => STANDARD.encode(Sha256::digest(name.as_bytes())),
        _ => name.to_owned(),
    }
}

pub(crate) fn djb2(value: &str) -> String {
    value
        .encode_utf16()
        .fold(0_u32, |hash, character| {
            hash.wrapping_mul(31).wrapping_add(u32::from(character))
        })
        .to_string()
}

fn gate(name: &str, value: bool) -> Value {
    json!({
        "name": name,
        "value": value,
        "rule_id": LOCAL_RULE,
        "ruleID": LOCAL_RULE,
        "group_name": LOCAL_RULE,
        "groupName": LOCAL_RULE,
        "secondary_exposures": [],
        "secondaryExposures": [],
        "undelegated_secondary_exposures": [],
        "undelegatedSecondaryExposures": [],
        "is_device_based": false,
        "isDeviceBased": false,
        "id_type": "userID",
        "idType": "userID"
    })
}

fn unary_payload(body: &Bytes) -> Result<(bool, &[u8])> {
    if body.len() < 5 {
        return Ok((false, body));
    }
    let flags = body[0];
    let length = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;
    if length != body.len() - 5 {
        return Ok((false, body));
    }
    if flags != 0 {
        return Err(Error::Protocol(format!(
            "cannot patch compressed or terminal Statsig frame: flags={flags}"
        )));
    }
    Ok((true, &body[5..]))
}

fn encode_unary(message: &impl Message, framed: bool) -> Bytes {
    let payload = message.encode_to_vec();
    if !framed {
        return Bytes::from(payload);
    }
    let mut output = BytesMut::with_capacity(5 + payload.len());
    output.put_u8(0);
    output.put_u32(payload.len() as u32);
    output.extend_from_slice(&payload);
    output.freeze()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disables_the_agent_host_gate_and_enables_retries() {
        let mut config = json!({ "feature_gates": {}, "hash_used": "none" });
        patch_statsig_gates(&mut config).unwrap();
        assert_eq!(config["feature_gates"][AGENT_RETRIES_GATE]["value"], true);
        assert_eq!(config["feature_gates"][AGENT_HOST_GATE]["value"], false);
        assert_eq!(
            config["feature_gates"][AGENT_WEBSOCKET_GATE]["value"],
            false
        );
        assert_eq!(config["feature_gates"][META_MCP_TOOL_GATE]["value"], true);
        assert_eq!(config["feature_gates"][MCP_META_SLIM_GATE]["value"], true);
        assert_eq!(
            config["feature_gates"][MCP_INPUT_SCHEMA_JSON_GATE]["value"],
            true
        );
    }

    #[test]
    fn hashed_gate_keys_follow_the_statsig_hash_mode() {
        let config = json!({ "hash_used": "djb2" });
        assert_eq!(statsig_key(&config, AGENT_HOST_GATE), djb2(AGENT_HOST_GATE));
        let config = json!({ "hash_used": "sha256" });
        assert_eq!(
            statsig_key(&config, AGENT_HOST_GATE),
            STANDARD.encode(Sha256::digest(AGENT_HOST_GATE.as_bytes()))
        );
    }

    #[test]
    fn local_bootstrap_uses_the_jwt_subject_as_statsig_user_id() {
        let default = local_statsig_bootstrap(None).unwrap();
        assert_eq!(default["user"]["userID"], local_app::LOCAL_CURSOR_SUBJECT);
        assert_eq!(
            default["user"]["customIDs"]["localUserID"],
            local_app::LOCAL_CURSOR_SUBJECT
        );
        assert_eq!(default["feature_gates"][META_MCP_TOOL_GATE]["value"], true);
        assert_eq!(default["feature_gates"][MCP_META_SLIM_GATE]["value"], true);

        let official = local_statsig_bootstrap(Some("auth0|user_123")).unwrap();
        assert_eq!(official["user"]["userID"], "auth0|user_123");
        assert_eq!(
            official["user"]["customIDs"]["localUserID"],
            "auth0|user_123"
        );

        let blank = local_statsig_bootstrap(Some("   ")).unwrap();
        assert_eq!(blank["user"]["userID"], local_app::LOCAL_CURSOR_SUBJECT);
    }

    #[test]
    fn reconcile_local_statsig_user_repairs_legacy_and_preserves_official_users() {
        let mut legacy = json!({
            "hash_used": "none",
            "user": {
                "userID": "local_ultra",
                "customIDs": { "localUserID": "local_ultra" }
            },
            "sdkParams": { "stableID": "local_ultra" }
        });
        reconcile_local_statsig_user(&mut legacy, Some("cursor-local-user"));
        assert_eq!(legacy["user"]["userID"], "cursor-local-user");
        assert_eq!(
            legacy["user"]["customIDs"]["localUserID"],
            "cursor-local-user"
        );
        assert_eq!(legacy["sdkParams"]["stableID"], "cursor-local-user");

        let mut malformed_user = json!({
            "hash_used": "none",
            "user": "corrupted"
        });
        reconcile_local_statsig_user(&mut malformed_user, Some("cursor-local-user"));
        assert_eq!(malformed_user["user"]["userID"], "cursor-local-user");

        let mut official = json!({
            "hash_used": "djb2",
            "user": {
                "userID": "auth0|user_real",
                "customIDs": { "stableID": "real-stable", "teamID": "123" }
            }
        });
        reconcile_local_statsig_user(&mut official, Some("cursor-local-user"));
        assert_eq!(official["user"]["userID"], "auth0|user_real");
        assert_eq!(official["user"]["customIDs"]["teamID"], "123");

        let mut non_object = json!(["not", "an", "object"]);
        reconcile_local_statsig_user(&mut non_object, Some("cursor-local-user"));
        assert!(non_object.is_array());
    }

    #[test]
    fn rejects_malformed_statsig_configs_and_compressed_frames() {
        assert!(patch_statsig_gates(&mut json!(null)).is_err());
        assert!(patch_statsig_gates(&mut json!({ "feature_gates": [] })).is_err());
        assert!(unary_payload(&Bytes::from_static(&[1, 0, 0, 0, 1, 0])).is_err());
    }

    #[tokio::test]
    async fn local_token_bootstrap_returns_matching_subject_without_upstream() {
        let store = crate::store::Store::connect("sqlite::memory:")
            .await
            .unwrap();
        let clients = crate::network::NetworkClients::new(store);
        let proxy = proxy::CursorProxy::cursor(clients);
        let request = Request::builder()
            .method("POST")
            .uri(BOOTSTRAP_STATSIG_PATH)
            .header(
                header::AUTHORIZATION,
                local_app::local_cursor_authorization(),
            )
            .body(Body::from(&b"ignored-request-body"[..]))
            .unwrap();

        let response = bootstrap_statsig(Extension(proxy), request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let message = BootstrapStatsigResponse::decode(body).unwrap();
        let config: Value = serde_json::from_str(&message.config).unwrap();
        assert_eq!(config["user"]["userID"], local_app::LOCAL_CURSOR_SUBJECT);
        assert_eq!(config["feature_gates"][META_MCP_TOOL_GATE]["value"], true);
        assert_eq!(config["feature_gates"][MCP_META_SLIM_GATE]["value"], true);
        assert_eq!(
            config["feature_gates"][MCP_INPUT_SCHEMA_JSON_GATE]["value"],
            true
        );
        assert_eq!(config["feature_gates"][AGENT_HOST_GATE]["value"], false);
        assert_eq!(
            config["feature_gates"][AGENT_WEBSOCKET_GATE]["value"],
            false
        );
    }
}
