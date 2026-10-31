//! Keeps Cursor Agent traffic on the endpoint selected with `agent -e`.
use std::net::SocketAddr;

use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderValue, Response, StatusCode},
};
use prost::Message;

use crate::{cursor::transport::TransportRegistry, Result};

const HTTP2_CONFIG_FORCE_ALL_DISABLED: i32 = 1;

#[derive(Clone, PartialEq, Message)]
struct AgentUrlConfig {
    #[prost(string, tag = "1")]
    agent_url: String,
    #[prost(string, tag = "2")]
    agentn_url: String,
    #[prost(string, tag = "3")]
    local_loop_inference_url: String,
}

#[derive(Clone, PartialEq, Message)]
struct ServerConfigResponse {
    #[prost(string, tag = "6")]
    config_version: String,
    #[prost(int32, tag = "7")]
    http2_config: i32,
    #[prost(message, optional, tag = "27")]
    agent_url_config: Option<AgentUrlConfig>,
    #[prost(bool, optional, tag = "28")]
    cli_sandbox_default_enabled: Option<bool>,
}

pub async fn get(State(registry): State<TransportRegistry>) -> Result<Response<Body>> {
    let payload = server_config(registry.web_cache().service_addr()).encode_to_vec();
    let mut response = Response::new(Body::from(payload));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/proto"),
    );
    Ok(response)
}

fn server_config(service_addr: Option<SocketAddr>) -> ServerConfigResponse {
    let agent_url_config = service_addr.map(|address| {
        let url = format!("http://{address}");
        AgentUrlConfig {
            agent_url: url.clone(),
            agentn_url: url,
            local_loop_inference_url: String::new(),
        }
    });
    ServerConfigResponse {
        config_version: "cursor_byok_local_agent_v1".into(),
        http2_config: HTTP2_CONFIG_FORCE_ALL_DISABLED,
        agent_url_config,
        cli_sandbox_default_enabled: Some(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forces_agent_cli_to_use_the_selected_legacy_endpoint() {
        let config = server_config(None);
        assert_eq!(config.http2_config, HTTP2_CONFIG_FORCE_ALL_DISABLED);
        assert_eq!(config.cli_sandbox_default_enabled, Some(true));
    }

    #[test]
    fn points_the_agent_backend_to_the_local_service() {
        let config = server_config(Some("127.0.0.1:59112".parse().unwrap()));
        let agent = config.agent_url_config.expect("agent url config");
        assert_eq!(agent.agent_url, "http://127.0.0.1:59112");
        assert_eq!(agent.agentn_url, "http://127.0.0.1:59112");
    }
}
