//! Selects local handling or the configured official Cursor upstream.
use std::time::Instant;

use axum::{
    body::{to_bytes, Body, Bytes},
    extract::Extension,
    http::{header, Request, Response},
};

use crate::Result;

const CURSOR_UPSTREAM: &str = "https://api2.cursor.sh";
pub const UPSTREAM_URL_HEADER: &str = "x-server-upstream-url";

#[derive(Clone)]
pub struct CursorProxy {
    clients: crate::network::NetworkClients,
    upstream: String,
}

pub struct BufferedResponse {
    pub status: axum::http::StatusCode,
    pub headers: axum::http::HeaderMap,
    pub body: Bytes,
}

impl BufferedResponse {
    pub fn into_response(self) -> Response<Body> {
        let body = self.body.clone();
        self.with_body(body)
    }

    pub fn with_body(mut self, body: Bytes) -> Response<Body> {
        self.headers.insert(
            header::CONTENT_LENGTH,
            body.len()
                .to_string()
                .parse()
                .expect("body length is always a valid header value"),
        );
        let mut response = Response::new(Body::from(body));
        *response.status_mut() = self.status;
        *response.headers_mut() = self.headers;
        response
    }
}

impl CursorProxy {
    pub fn cursor(clients: crate::network::NetworkClients) -> Self {
        Self {
            clients,
            upstream: CURSOR_UPSTREAM.into(),
        }
    }

    async fn client(&self) -> Result<reqwest::Client> {
        self.clients.cursor_client().await
    }
}

pub async fn forward(
    Extension(proxy): Extension<CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    forward_request(&proxy, request, None).await
}

/// Upper bound for buffered control-plane calls. A stalled upstream must not
/// leave Cursor waiting indefinitely for model metadata or account state.
const BUFFERED_UPSTREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub(crate) async fn forward_to_service(
    proxy: &CursorProxy,
    request: Request<Body>,
    service_url: &str,
) -> Result<Response<Body>> {
    forward_request(proxy, request, Some(service_url)).await
}

async fn forward_request(
    proxy: &CursorProxy,
    request: Request<Body>,
    service_url: Option<&str>,
) -> Result<Response<Body>> {
    let started = Instant::now();
    let (parts, body) = request.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map_or("/", |value| value.as_str())
        .to_owned();
    let url = match service_url {
        Some(service_url) => format!("{}{}", service_url.trim_end_matches('/'), path),
        None => upstream_url(&parts.headers, &proxy.upstream, &path)?,
    };

    let mut headers = parts.headers;
    headers.remove(UPSTREAM_URL_HEADER);
    headers.remove(header::HOST);
    remove_hop_by_hop_headers(&mut headers);

    let client = proxy.client().await?;
    let upstream = client
        .request(parts.method.clone(), url)
        .headers(headers)
        .body(reqwest::Body::wrap_stream(body.into_data_stream()))
        .send()
        .await;

    let upstream = match upstream {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(
                method = %parts.method,
                path,
                elapsed_ms = started.elapsed().as_millis(),
                error = %error_chain(&error),
                "Cursor upstream request failed"
            );
            return Err(error.into());
        }
    };

    let status = upstream.status();
    let mut response_headers = upstream.headers().clone();
    remove_hop_by_hop_headers(&mut response_headers);
    let mut response = Response::new(Body::from_stream(upstream.bytes_stream()));
    *response.status_mut() = status;
    *response.headers_mut() = response_headers;

    tracing::info!(
        method = %parts.method,
        path,
        %status,
        elapsed_ms = started.elapsed().as_millis(),
        "forwarded Cursor backend request"
    );
    Ok(response)
}

pub async fn forward_buffered(
    proxy: &CursorProxy,
    request: Request<Body>,
) -> Result<BufferedResponse> {
    let (parts, body) = request.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map_or("/", |value| value.as_str());
    let url = upstream_url(&parts.headers, &proxy.upstream, path)?;
    let mut headers = parts.headers;
    headers.remove(UPSTREAM_URL_HEADER);
    headers.remove(header::HOST);
    remove_hop_by_hop_headers(&mut headers);
    headers.insert(
        "connect-accept-encoding",
        axum::http::HeaderValue::from_static("identity"),
    );
    headers.insert(
        header::ACCEPT_ENCODING,
        axum::http::HeaderValue::from_static("identity"),
    );
    let body = to_bytes(body, usize::MAX)
        .await
        .map_err(|error| crate::Error::Protocol(format!("cannot read request body: {error}")))?;
    let started = Instant::now();
    let client = proxy.client().await?;
    let method = parts.method;
    let mut attempt = 0;
    let upstream = loop {
        attempt += 1;
        match client
            .request(method.clone(), url.clone())
            .headers(headers.clone())
            .timeout(BUFFERED_UPSTREAM_TIMEOUT)
            .body(body.clone())
            .send()
            .await
        {
            Ok(response) => break response,
            // A connect failure means the request never reached upstream, so
            // retrying a buffered control call cannot duplicate side effects.
            Err(error) if attempt == 1 && error.is_connect() => {
                tracing::warn!(
                    path,
                    elapsed_ms = started.elapsed().as_millis(),
                    error = %error_chain(&error),
                    "retrying buffered Cursor upstream request after connect failure"
                );
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Err(error) => {
                tracing::error!(
                    path,
                    elapsed_ms = started.elapsed().as_millis(),
                    error = %error_chain(&error),
                    "buffered Cursor upstream request failed"
                );
                return Err(error.into());
            }
        }
    };
    let status = upstream.status();
    let mut headers = upstream.headers().clone();
    remove_hop_by_hop_headers(&mut headers);
    let body = upstream.bytes().await?;
    Ok(BufferedResponse {
        status,
        headers,
        body,
    })
}

/// reqwest's Display only says "error sending request"; include the source
/// chain so the log distinguishes a dead local proxy from an upstream reset.
fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

fn upstream_url(headers: &axum::http::HeaderMap, fallback: &str, path: &str) -> Result<String> {
    let Some(value) = headers.get(UPSTREAM_URL_HEADER) else {
        return Ok(format!("{fallback}{path}"));
    };
    let value = value
        .to_str()
        .map_err(|error| crate::Error::Protocol(format!("invalid upstream URL header: {error}")))?;
    let url = reqwest::Url::parse(value)
        .map_err(|error| crate::Error::Protocol(format!("invalid upstream URL: {error}")))?;
    let host = url.host_str().unwrap_or_default();
    if url.scheme() != "https" || !crate::local_app::proxy_host_allowed(host) {
        return Err(crate::Error::Protocol(
            "upstream URL must target a managed HTTPS host".into(),
        ));
    }
    Ok(url.into())
}

fn remove_hop_by_hop_headers(headers: &mut axum::http::HeaderMap) {
    let connection_headers = headers
        .get(header::CONNECTION)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for name in connection_headers {
        headers.remove(name);
    }
    for name in [
        header::CONNECTION,
        header::PROXY_AUTHENTICATE,
        header::PROXY_AUTHORIZATION,
        header::TE,
        header::TRAILER,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
    ] {
        headers.remove(name);
    }
    headers.remove("keep-alive");
}
