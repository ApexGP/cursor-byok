#[path = "support/fake_provider.rs"]
mod fake_provider;
#[path = "support/fixtures.rs"]
mod fixtures;

use std::{sync::Arc, time::Duration};

use axum::{
    body::Body,
    http::{Request, StatusCode},
    routing::post,
    Json, Router,
};
use chrono::{Duration as ChronoDuration, Utc};
use cursor_server::{
    control::{self, ControlService},
    diagnostics::{DiagnosticCode, DiagnosticEvent, DiagnosticsConfig, DiagnosticsHub},
    network::NetworkClients,
};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tower::ServiceExt;

async fn setup(
    upload_url: Option<String>,
) -> (
    Router,
    cursor_server::store::Store,
    tempfile::TempDir,
    Arc<DiagnosticsHub>,
) {
    let (directory, store) = fixtures::temp_store().await;
    let config = DiagnosticsConfig {
        enabled: true,
        upload_url,
        github_repository: "ApexGP/cursor-byok".into(),
        retention_days: 7,
        max_events: 200,
        log_window: Duration::from_secs(120),
        data_dir: directory.path().join("diagnostics"),
        app_version: "1.0.6".into(),
    };
    let hub = DiagnosticsHub::new(config, store.clone()).await.unwrap();
    hub.record(
        DiagnosticEvent::new(DiagnosticCode::Sync, "database is locked")
            .with_error("sync", "database is locked"),
    );
    let provider = Arc::new(fake_provider::FakeProvider::default());
    let control =
        ControlService::new(store.clone(), provider, NetworkClients::new(store.clone())).unwrap();
    control.set_diagnostics(hub.clone());
    (control::api_router(control), store, directory, hub)
}

async fn mock_upload() -> String {
    async fn handler() -> (StatusCode, Json<Value>) {
        let expires_at = (Utc::now() + ChronoDuration::days(30)).to_rfc3339();
        (
            StatusCode::CREATED,
            Json(json!({
                "url": "https://diag.example/r/r-20261111-0123456789abcdef0123456789abcdef",
                "expiresAt": expires_at
            })),
        )
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, Router::new().route("/", post(handler)))
            .await
            .unwrap();
    });
    format!("http://{address}/")
}

async fn send(router: Router, method: &str, path: &str, body: Value) -> (StatusCode, String) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn reports_and_frontend_errors_are_exposed() {
    let (router, _store, _directory, _hub) = setup(None).await;

    let (status, body) = send(
        router.clone(),
        "GET",
        "/__byok-api__/api/diagnostics/reports",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let reports: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(reports[0]["code"], "CB-SYNC-001");

    let (status, body) = send(
        router.clone(),
        "POST",
        "/__byok-api__/api/diagnostics/frontend",
        json!({"code": "CB-UI-001", "message": "render failed", "stack": "at App"}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(body.is_empty());

    let (status, body) = send(
        router,
        "GET",
        "/__byok-api__/api/diagnostics/reports",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let reports: Value = serde_json::from_str(&body).unwrap();
    assert!(reports
        .as_array()
        .unwrap()
        .iter()
        .any(|report| report["code"] == "CB-UI-001"));
}

#[tokio::test]
async fn issue_endpoint_uploads_once_and_returns_prefilled_url() {
    let upload = mock_upload().await;
    let (router, _store, _directory, _hub) = setup(Some(upload)).await;
    let (_, list) = send(
        router.clone(),
        "GET",
        "/__byok-api__/api/diagnostics/reports",
        json!({}),
    )
    .await;
    let reports: Value = serde_json::from_str(&list).unwrap();
    let id = reports[0]["reportId"].as_str().unwrap();

    let (status, body) = send(
        router,
        "POST",
        &format!("/__byok-api__/api/diagnostics/reports/{id}/issue"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let preview: Value = serde_json::from_str(&body).unwrap();
    assert!(preview["issueUrl"]
        .as_str()
        .unwrap()
        .starts_with("https://github.com/ApexGP/cursor-byok/issues/new?"));
    assert!(preview["bundleUrl"]
        .as_str()
        .unwrap()
        .contains("/r/r-20261111-"));
    assert!(preview["body"]
        .as_str()
        .unwrap()
        .contains("CURSOR_BYOK_DIAGNOSTIC_START"));
}

#[tokio::test]
async fn issue_endpoint_requires_upload_configuration() {
    let (router, _store, _directory, _hub) = setup(None).await;
    let (_, list) = send(
        router.clone(),
        "GET",
        "/__byok-api__/api/diagnostics/reports",
        json!({}),
    )
    .await;
    let reports: Value = serde_json::from_str(&list).unwrap();
    let id = reports[0]["reportId"].as_str().unwrap();

    let (status, _) = send(
        router,
        "POST",
        &format!("/__byok-api__/api/diagnostics/reports/{id}/issue"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn dismiss_hides_the_report() {
    let (router, _store, _directory, _hub) = setup(None).await;
    let (_, list) = send(
        router.clone(),
        "GET",
        "/__byok-api__/api/diagnostics/reports",
        json!({}),
    )
    .await;
    let reports: Value = serde_json::from_str(&list).unwrap();
    let id = reports[0]["reportId"].as_str().unwrap();

    let (status, _) = send(
        router.clone(),
        "POST",
        &format!("/__byok-api__/api/diagnostics/reports/{id}/dismiss"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, list) = send(
        router,
        "GET",
        "/__byok-api__/api/diagnostics/reports",
        json!({}),
    )
    .await;
    let reports: Value = serde_json::from_str(&list).unwrap();
    assert!(reports.as_array().unwrap().is_empty());
}
