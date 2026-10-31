//! Implements Cursor analytics endpoints and event handling.
use axum::{
    body::{Body, Bytes},
    extract::Extension,
    http::{header, HeaderValue, Request, Response, StatusCode},
};
use base64::{engine::general_purpose::STANDARD, Engine};
use bytes::{BufMut, BytesMut};
use prost::Message;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::{api::cursor::proxy, Error, Result};

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
const LOCAL_RULE: &str = "local_enabled";

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
    match proxy::forward_buffered(&upstream, request).await {
        Ok(response) if response.status.is_success() => match patch_upstream(response) {
            Ok(response) => Ok(response),
            Err(error) => {
                tracing::warn!(%error, "Cursor Statsig bootstrap was invalid; using local bootstrap");
                local_response()
            }
        },
        Ok(response) => {
            tracing::warn!(status = %response.status, "Cursor Statsig bootstrap was rejected; using local bootstrap");
            local_response()
        }
        Err(error) => {
            tracing::warn!(%error, "Cursor Statsig bootstrap was unavailable; using local bootstrap");
            local_response()
        }
    }
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

fn local_response() -> Result<Response<Body>> {
    let generated_at_ms = chrono::Utc::now().timestamp_millis() as u64;
    let mut config = json!({
        "feature_gates": {},
        "dynamic_configs": {},
        "layer_configs": {},
        "user": {
            "userID": "local_ultra",
            "customIDs": { "localUserID": "local_ultra" }
        },
        "has_updates": true,
        "hash_used": "none",
        "sdkParams": {
            "stableID": "local_ultra",
            "disableDiagnosticsLogging": true
        },
        "time": generated_at_ms
    });
    patch_statsig_gates(&mut config)?;
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

fn patch_statsig_gates(config: &mut Value) -> Result<()> {
    let hash_used = config
        .get("hash_used")
        .and_then(Value::as_str)
        .unwrap_or("none")
        .to_owned();
    let retries_key = statsig_key(config, AGENT_RETRIES_GATE);
    let agent_host_key = statsig_key(config, AGENT_HOST_GATE);
    let agent_websocket_key = statsig_key(config, AGENT_WEBSOCKET_GATE);
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
    tracing::info!(
        hash_used,
        retries_key = %retries_key,
        agent_host_key = %agent_host_key,
        agent_websocket_key = %agent_websocket_key,
        "patched Cursor Statsig gates (agent host + websocket disabled)"
    );
    Ok(())
}

fn statsig_key(config: &Value, name: &str) -> String {
    match config.get("hash_used").and_then(Value::as_str) {
        Some("djb2") => djb2(name),
        Some("sha256") => STANDARD.encode(Sha256::digest(name.as_bytes())),
        _ => name.to_owned(),
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
}
