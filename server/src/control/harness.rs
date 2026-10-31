//! Implements local application control endpoints.
use axum::{extract::State, Json};

use crate::{
    local_app::{CursorHarnessStatus, SetEnabled},
    Result,
};

use super::ControlService;

pub async fn status(State(service): State<ControlService>) -> Result<Json<CursorHarnessStatus>> {
    Ok(Json(service.cursor_harness().status().await?))
}

pub async fn initialize_ca(
    State(service): State<ControlService>,
) -> Result<Json<CursorHarnessStatus>> {
    Ok(Json(service.cursor_harness().initialize_ca().await?))
}

pub async fn set_enabled(
    State(service): State<ControlService>,
    Json(input): Json<SetEnabled>,
) -> Result<Json<CursorHarnessStatus>> {
    Ok(Json(
        service.cursor_harness().set_enabled(input.enabled).await?,
    ))
}

pub async fn sync_model_cache(
    State(service): State<ControlService>,
) -> Result<Json<crate::local_app::model_cache::CursorCatalogSync>> {
    Ok(Json(service.sync_cursor_model_cache().await?))
}

pub async fn restart_cursor(
    State(service): State<ControlService>,
) -> Result<Json<CursorHarnessStatus>> {
    service.cursor_harness().restart_cursor().await?;
    Ok(Json(service.cursor_harness().status().await?))
}
