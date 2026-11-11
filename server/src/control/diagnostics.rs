//! Exposes captured diagnostics and the prefilled issue flow.
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::{
    control::ControlService,
    diagnostics::{
        build_report, build_upload_and_link, ensure_upload_configured, issue_url, render_issue,
        DiagnosticCode, DiagnosticContext, DiagnosticEvent, ReportSummary,
    },
    Error, Result,
};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportDetail {
    pub summary: ReportSummary,
    pub issue_body: String,
    pub diagnostic_json: serde_json::Value,
    pub upload_available: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IssuePreview {
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub issue_url: String,
    pub bundle_url: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Deserialize)]
pub struct FrontendError {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub stack: Option<String>,
}

pub async fn list(State(service): State<ControlService>) -> Result<Json<Vec<ReportSummary>>> {
    let Some(hub) = service.diagnostics() else {
        return Ok(Json(Vec::new()));
    };
    Ok(Json(hub.reports().await))
}

pub async fn detail(
    State(service): State<ControlService>,
    Path(report_id): Path<String>,
) -> Result<Json<ReportDetail>> {
    let hub = hub(&service)?;
    let report = build_report(hub, &report_id).await?;
    let summary = hub
        .reports()
        .await
        .into_iter()
        .find(|summary| summary.report_id == report_id)
        .ok_or_else(|| not_found(&report_id))?;
    let diagnostic_json = serde_json::to_value(&report)?;
    let draft = render_issue(&report, None, &hub.config().github_repository)?;
    let upload_available = ensure_upload_configured(hub.config()).is_ok();
    Ok(Json(ReportDetail {
        summary,
        issue_body: draft.body,
        diagnostic_json,
        upload_available,
    }))
}

pub async fn create_issue(
    State(service): State<ControlService>,
    Path(report_id): Path<String>,
) -> Result<Json<IssuePreview>> {
    let hub = hub(&service)?;
    let config = hub.config();
    ensure_upload_configured(config)?;
    let report = build_report(hub, &report_id).await?;
    let link = build_upload_and_link(config, &report, &report.logs).await?;
    let draft = render_issue(&report, Some(&link), &config.github_repository)?;
    let url = issue_url(&config.github_repository, &draft);
    Ok(Json(IssuePreview {
        title: draft.title,
        body: draft.body,
        labels: draft.labels,
        issue_url: url,
        bundle_url: link.url,
        expires_at: link.expires_at,
    }))
}

pub async fn dismiss(
    State(service): State<ControlService>,
    Path(report_id): Path<String>,
) -> Result<StatusCode> {
    let hub = hub(&service)?;
    hub.dismiss(&report_id)
        .await
        .map_err(|_| not_found(&report_id))?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn frontend(
    State(service): State<ControlService>,
    Json(input): Json<FrontendError>,
) -> Result<StatusCode> {
    let hub = hub(&service)?;
    let code = match input.code.as_str() {
        "CB-UI-001" => DiagnosticCode::UiBoundary,
        _ => DiagnosticCode::UiUnhandled,
    };
    let mut context = DiagnosticContext::new("ui", "renderer");
    context.operation = "renderer".into();
    let mut event = DiagnosticEvent::new(code, input.message.chars().take(200).collect::<String>())
        .with_error(
            "frontend",
            input.message.chars().take(2000).collect::<String>(),
        )
        .with_context(context);
    if let Some(stack) = input.stack {
        event.error_chain = vec![stack.chars().take(4000).collect()];
    }
    hub.record(event);
    Ok(StatusCode::ACCEPTED)
}

fn hub(service: &ControlService) -> Result<&std::sync::Arc<crate::diagnostics::DiagnosticsHub>> {
    service
        .diagnostics()
        .ok_or_else(|| Error::Config("diagnostics are unavailable".into()))
}

fn not_found(report_id: &str) -> Error {
    Error::RunNotFound(format!("diagnostic report {report_id}"))
}
