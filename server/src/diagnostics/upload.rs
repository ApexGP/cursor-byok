//! Upload client for sealed diagnostic bundles.
//!
//! The upload URL comes exclusively from [`DiagnosticsConfig::upload_url`]. No
//! production domain is embedded and there is no fallback endpoint. The caller
//! decides when to upload; capture never calls into this module.

use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::{
    diagnostics::{
        bundle::{build_bundle, Bundle},
        logs::LogTail,
        report::{BundleLink, DiagnosticReport},
        store::DiagnosticsConfig,
    },
    Error, Result,
};

/// Total request timeout for one upload attempt.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(30);
/// Relay responses are tiny; refuse to buffer more than this from a peer.
const MAX_UPLOAD_RESPONSE_BYTES: usize = 64 * 1024;
const SCHEMA_VERSION: &str = "1";

/// A bundle accepted by the diagnostic service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadedBundle {
    pub url: String,
    pub expires_at: DateTime<Utc>,
}

/// Response body returned by the diagnostic service.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadResponse {
    url: String,
    expires_at: String,
}

/// Returns the configured upload URL or a clear configuration error.
///
/// There is deliberately no default endpoint: an unset or blank URL must fail
/// fast instead of sending a bundle to an unknown service.
pub fn ensure_upload_configured(config: &DiagnosticsConfig) -> Result<&str> {
    match config.upload_url.as_deref().map(str::trim) {
        Some(url) if !url.is_empty() => Ok(url),
        _ => Err(Error::Config(
            "diagnostics upload URL is not configured".to_string(),
        )),
    }
}

pub async fn upload_bundle(
    config: &DiagnosticsConfig,
    report_id: &str,
    bundle: &Bundle,
) -> Result<UploadedBundle> {
    upload_bundle_with_timeout(config, report_id, bundle, UPLOAD_TIMEOUT).await
}

async fn upload_bundle_with_timeout(
    config: &DiagnosticsConfig,
    report_id: &str,
    bundle: &Bundle,
    timeout: Duration,
) -> Result<UploadedBundle> {
    let url = ensure_upload_configured(config)?;
    let client = reqwest::Client::builder().timeout(timeout).build()?;
    let response = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/zip")
        .header("X-Report-Id", report_id)
        .header("X-Report-SHA256", &bundle.sha256)
        .header("X-Schema-Version", SCHEMA_VERSION)
        .header("X-App-Version", &config.app_version)
        .header("X-Platform", std::env::consts::OS)
        .body(bundle.bytes.clone())
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        return Err(Error::Protocol(format!(
            "diagnostic upload rejected with HTTP {status}"
        )));
    }

    let body = read_response_body(response).await?;
    let parsed: UploadResponse = serde_json::from_slice(&body)
        .map_err(|error| Error::Protocol(format!("invalid diagnostic upload response: {error}")))?;
    let expires_at = DateTime::parse_from_rfc3339(parsed.expires_at.trim())
        .map_err(|error| Error::Protocol(format!("invalid diagnostic upload expiry: {error}")))?
        .with_timezone(&Utc);
    let link_url = validate_link_url(&parsed.url)?;

    Ok(UploadedBundle {
        url: link_url,
        expires_at,
    })
}

/// Reads at most [`MAX_UPLOAD_RESPONSE_BYTES`] from the relay response.
async fn read_response_body(mut response: reqwest::Response) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_UPLOAD_RESPONSE_BYTES as u64)
    {
        return Err(response_too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > MAX_UPLOAD_RESPONSE_BYTES {
            return Err(response_too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn response_too_large() -> Error {
    Error::Protocol(format!(
        "diagnostic upload response exceeds {MAX_UPLOAD_RESPONSE_BYTES} bytes"
    ))
}

/// Accepts only absolute `http(s)` links with a host.
///
/// The relay is a network peer; never hand schemes such as `javascript:` or
/// `file:` to the UI or an OS open handler.
fn validate_link_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    let parsed = url::Url::parse(trimmed)
        .map_err(|error| Error::Protocol(format!("invalid diagnostic bundle url: {error}")))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(Error::Protocol(format!(
            "invalid diagnostic bundle url scheme: {trimmed}"
        )));
    }
    Ok(trimmed.to_string())
}

/// Builds the bundle and uploads it, returning the link to embed in an issue.
///
/// The sealed zip lives only in the [`Bundle`] local variable and is dropped
/// when this function returns, whether the upload succeeded or failed.
pub async fn build_upload_and_link(
    config: &DiagnosticsConfig,
    report: &DiagnosticReport,
    log_tail: &LogTail,
) -> Result<BundleLink> {
    ensure_upload_configured(config)?;
    let bundle = build_bundle(report, log_tail).await?;
    let uploaded = upload_bundle(config, &report.report_id, &bundle).await?;
    Ok(BundleLink {
        url: uploaded.url,
        sha256: bundle.sha256,
        expires_at: uploaded.expires_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::code::DiagnosticCode;
    use crate::diagnostics::report::test_support::test_report;
    use crate::Error;
    use axum::{
        body::Bytes,
        extract::{DefaultBodyLimit, State},
        http::{header::CONTENT_TYPE, HeaderMap, StatusCode},
        response::IntoResponse,
        routing::post,
        Json, Router,
    };
    use serde_json::{json, Value};
    use sha2::Digest;
    use std::{path::PathBuf, sync::Arc, time::Duration};
    use tokio::sync::Mutex;

    #[derive(Clone)]
    struct MockResponse {
        status: StatusCode,
        body: Value,
    }

    #[derive(Default)]
    struct Captured {
        headers: Option<HeaderMap>,
        body: Option<Vec<u8>>,
    }

    type MockState = Arc<(Arc<Mutex<Captured>>, MockResponse)>;

    async fn mock_handler(
        State(state): State<MockState>,
        headers: HeaderMap,
        body: Bytes,
    ) -> impl IntoResponse {
        let mut captured = state.0.lock().await;
        captured.headers = Some(headers);
        captured.body = Some(body.to_vec());
        (state.1.status, Json(state.1.body.clone()))
    }

    async fn spawn_mock(status: StatusCode, body: Value) -> (String, Arc<Mutex<Captured>>) {
        let captured = Arc::new(Mutex::new(Captured::default()));
        let state: MockState = Arc::new((captured.clone(), MockResponse { status, body }));
        let router = Router::new()
            .route("/upload", post(mock_handler))
            .layer(DefaultBodyLimit::max(
                crate::diagnostics::bundle::MAX_BUNDLE_BYTES,
            ))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        (format!("http://{addr}/upload"), captured)
    }

    fn config(upload_url: Option<String>) -> DiagnosticsConfig {
        DiagnosticsConfig {
            enabled: true,
            upload_url,
            github_repository: "ApexGP/cursor-byok".to_string(),
            retention_days: 7,
            max_events: 200,
            log_window: Duration::from_secs(60),
            data_dir: PathBuf::from("/nonexistent/diagnostics"),
            app_version: "1.0.6".to_string(),
        }
    }

    fn empty_log_tail() -> LogTail {
        LogTail::default()
    }

    fn config_with_data_dir(upload_url: Option<String>, data_dir: PathBuf) -> DiagnosticsConfig {
        let mut configured = config(upload_url);
        configured.data_dir = data_dir;
        configured
    }

    /// Fails if bundle production code created its configured data directory or
    /// left any file under the observable temp root.
    fn assert_no_local_bundle(config: &DiagnosticsConfig, temp: &tempfile::TempDir) {
        assert!(
            !config.data_dir.exists(),
            "diagnostics data dir was created: {}",
            config.data_dir.display()
        );
        assert!(
            std::fs::read_dir(temp.path()).unwrap().next().is_none(),
            "bundle leaked into {}",
            temp.path().display()
        );
    }

    fn pseudo_random_lines(count: usize, line_bytes: usize) -> LogTail {
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut lines = Vec::with_capacity(count);
        for _ in 0..count {
            let mut line = String::with_capacity(line_bytes);
            for _ in 0..line_bytes {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                line.push((b'!' + (state % 94) as u8) as char);
            }
            lines.push(line);
        }
        LogTail {
            lines,
            truncated: false,
            error: None,
        }
    }

    #[test]
    fn ensure_upload_configured_requires_an_explicit_url() {
        assert!(matches!(
            ensure_upload_configured(&config(None)),
            Err(Error::Config(_))
        ));
        assert!(matches!(
            ensure_upload_configured(&config(Some("   ".to_string()))),
            Err(Error::Config(_))
        ));
        assert_eq!(
            ensure_upload_configured(&config(Some(" https://diag.example/upload ".to_string())))
                .unwrap(),
            "https://diag.example/upload"
        );
    }

    #[tokio::test]
    async fn upload_posts_the_zip_with_the_contract_headers() {
        let report = test_report(DiagnosticCode::Sync);
        let bundle = build_bundle(&report, &empty_log_tail()).await.unwrap();
        let report_id = report.report_id.clone();
        let (url, captured) = spawn_mock(
            StatusCode::CREATED,
            json!({
                "id": report_id,
                "url": "https://diag.example/r/r-20261111-0123456789abcdef0123456789abcdef",
                "expiresAt": "2026-12-11T12:34:56Z",
            }),
        )
        .await;

        let uploaded = upload_bundle(&config(Some(url)), &report_id, &bundle)
            .await
            .unwrap();

        assert_eq!(
            uploaded.url,
            "https://diag.example/r/r-20261111-0123456789abcdef0123456789abcdef"
        );
        assert_eq!(
            uploaded.expires_at,
            "2026-12-11T12:34:56Z".parse::<DateTime<Utc>>().unwrap()
        );

        let captured = captured.lock().await;
        let headers = captured.headers.as_ref().unwrap();
        assert_eq!(headers.get(CONTENT_TYPE).unwrap(), "application/zip");
        assert_eq!(headers.get("x-report-id").unwrap(), report_id.as_str());
        assert_eq!(
            headers.get("x-report-sha256").unwrap(),
            bundle.sha256.as_str()
        );
        assert_eq!(headers.get("x-schema-version").unwrap(), "1");
        assert_eq!(headers.get("x-app-version").unwrap(), "1.0.6");
        assert_eq!(headers.get("x-platform").unwrap(), std::env::consts::OS);
        assert_eq!(captured.body.as_deref(), Some(bundle.bytes.as_slice()));
    }

    #[tokio::test]
    async fn upload_rejects_server_errors() {
        let report = test_report(DiagnosticCode::Sync);
        let bundle = build_bundle(&report, &empty_log_tail()).await.unwrap();
        let (url, _) = spawn_mock(StatusCode::INTERNAL_SERVER_ERROR, json!({})).await;

        let error = upload_bundle(&config(Some(url)), &report.report_id, &bundle)
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Protocol(_)), "{error}");
    }

    #[tokio::test]
    async fn upload_rejects_success_bodies_without_a_url() {
        let report = test_report(DiagnosticCode::Sync);
        let bundle = build_bundle(&report, &empty_log_tail()).await.unwrap();
        let (url, _) = spawn_mock(StatusCode::CREATED, json!({ "id": "r-1" })).await;

        let error = upload_bundle(&config(Some(url)), &report.report_id, &bundle)
            .await
            .unwrap_err();
        assert!(
            matches!(error, Error::Protocol(_) | Error::Json(_)),
            "{error}"
        );
    }

    #[tokio::test]
    async fn upload_rejects_invalid_expiry_values() {
        let report = test_report(DiagnosticCode::Sync);
        let bundle = build_bundle(&report, &empty_log_tail()).await.unwrap();
        let (url, _) = spawn_mock(
            StatusCode::CREATED,
            json!({ "url": "https://diag.example/r/x", "expiresAt": "soon" }),
        )
        .await;

        let error = upload_bundle(&config(Some(url)), &report.report_id, &bundle)
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Protocol(_)), "{error}");
    }

    #[tokio::test]
    async fn upload_surfaces_connection_failures() {
        let report = test_report(DiagnosticCode::Sync);
        let bundle = build_bundle(&report, &empty_log_tail()).await.unwrap();
        let error = upload_bundle(
            &config(Some("http://127.0.0.1:1/upload".to_string())),
            &report.report_id,
            &bundle,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, Error::Http(_)), "{error}");
    }

    #[tokio::test]
    async fn upload_times_out_without_panicking() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((_socket, _)) = listener.accept().await {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });

        let report = test_report(DiagnosticCode::Sync);
        let bundle = build_bundle(&report, &empty_log_tail()).await.unwrap();
        let error = upload_bundle_with_timeout(
            &config(Some(format!("http://{addr}/upload"))),
            &report.report_id,
            &bundle,
            Duration::from_millis(200),
        )
        .await
        .unwrap_err();
        match error {
            Error::Http(error) => assert!(error.is_timeout(), "{error}"),
            other => panic!("expected timeout, got {other}"),
        }
    }

    #[tokio::test]
    async fn build_upload_and_link_failure_leaves_no_local_bundle() {
        let temp = tempfile::tempdir().unwrap();
        let report = test_report(DiagnosticCode::Sync);
        let configured = config_with_data_dir(
            Some("http://127.0.0.1:1/upload".to_string()),
            temp.path().join("diagnostics"),
        );
        let result = build_upload_and_link(&configured, &report, &empty_log_tail()).await;
        assert!(result.is_err());
        assert_no_local_bundle(&configured, &temp);
    }

    #[tokio::test]
    async fn build_upload_and_link_success_leaves_no_local_bundle() {
        let temp = tempfile::tempdir().unwrap();
        let report = test_report(DiagnosticCode::Sync);
        let (url, captured) = spawn_mock(
            StatusCode::CREATED,
            json!({
                "id": report.report_id,
                "url": "https://diag.example/r/r-20261111-0123456789abcdef0123456789abcdef",
                "expiresAt": "2026-12-11T12:34:56Z",
            }),
        )
        .await;
        let configured = config_with_data_dir(Some(url), temp.path().join("diagnostics"));

        let link = build_upload_and_link(&configured, &report, &empty_log_tail())
            .await
            .unwrap();

        assert_eq!(
            link.url,
            "https://diag.example/r/r-20261111-0123456789abcdef0123456789abcdef"
        );
        assert_eq!(
            link.expires_at,
            "2026-12-11T12:34:56Z".parse::<DateTime<Utc>>().unwrap()
        );
        let body = captured.lock().await.body.clone().unwrap();
        let expected = hex::encode(sha2::Sha256::digest(&body));
        assert_eq!(link.sha256, expected);
        assert_no_local_bundle(&configured, &temp);
    }

    #[tokio::test]
    async fn build_upload_and_link_trims_oversized_logs_before_upload() {
        let report = test_report(DiagnosticCode::Sync);
        // Oversized tail forces build_bundle's trim loop, after which the
        // trimmed bundle must still upload successfully.
        let log_tail = pseudo_random_lines(8, 768 * 1024);
        let (url, captured) = spawn_mock(
            StatusCode::CREATED,
            json!({
                "id": report.report_id,
                "url": "https://diag.example/r/r-20261111-0123456789abcdef0123456789abcdef",
                "expiresAt": "2026-12-11T12:34:56Z",
            }),
        )
        .await;

        let link = build_upload_and_link(&config(Some(url)), &report, &log_tail)
            .await
            .unwrap();
        assert_eq!(
            link.url,
            "https://diag.example/r/r-20261111-0123456789abcdef0123456789abcdef"
        );

        let body = captured.lock().await.body.clone().unwrap();
        assert!(body.len() <= crate::diagnostics::bundle::MAX_BUNDLE_BYTES);
        assert_eq!(link.sha256, hex::encode(sha2::Sha256::digest(&body)));
    }

    #[tokio::test]
    async fn upload_rejects_non_http_bundle_urls() {
        let report = test_report(DiagnosticCode::Sync);
        let bundle = build_bundle(&report, &empty_log_tail()).await.unwrap();
        for bad_url in [
            "javascript:alert(1)",
            "file:///tmp/bundle.zip",
            "data:text/html,<script>alert(1)</script>",
        ] {
            let (url, _) = spawn_mock(
                StatusCode::CREATED,
                json!({ "url": bad_url, "expiresAt": "2026-12-11T12:34:56Z" }),
            )
            .await;
            let error = upload_bundle(&config(Some(url)), &report.report_id, &bundle)
                .await
                .unwrap_err();
            assert!(matches!(error, Error::Protocol(_)), "{bad_url}: {error}");
        }
    }

    #[tokio::test]
    async fn upload_rejects_oversized_response_bodies() {
        let report = test_report(DiagnosticCode::Sync);
        let bundle = build_bundle(&report, &empty_log_tail()).await.unwrap();
        let (url, _) = spawn_mock(
            StatusCode::CREATED,
            json!({
                "url": "https://diag.example/r/x",
                "expiresAt": "2026-12-11T12:34:56Z",
                "padding": "x".repeat(MAX_UPLOAD_RESPONSE_BYTES + 1),
            }),
        )
        .await;

        let error = upload_bundle(&config(Some(url)), &report.report_id, &bundle)
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Protocol(_)), "{error}");
        assert!(error.to_string().contains("exceeds"), "{error}");
    }

    #[tokio::test]
    async fn build_upload_and_link_requires_configuration_before_building() {
        let report = test_report(DiagnosticCode::Sync);
        let error = build_upload_and_link(&config(None), &report, &empty_log_tail())
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Config(_)), "{error}");
    }
}
