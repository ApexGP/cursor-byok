#[path = "support/fake_provider.rs"]
mod fake_provider;
#[path = "support/fixtures.rs"]
mod fixtures;

use std::sync::Arc;

use axum::{
    body::Body,
    http::{header, Request, StatusCode},
};
use cursor_server::{
    control,
    model::{ModelConfigInput, ModelType, OPENAI_CHAT_ENDPOINT},
    network::NetworkClients,
};
use serde_json::{json, Value};
use tower::ServiceExt;

fn model_input() -> ModelConfigInput {
    ModelConfigInput {
        sort_order: 0,
        display_name: "Example".into(),
        group_name: Some("work".into()),
        model_type: ModelType::OpenAi,
        base_url: "https://example.com/v1".into(),
        use_full_url: false,
        api_key: "upstream".into(),
        tooltip_data: "test".into(),
        model_id: "qwen/model".into(),
        reasoning_effort: None,
        openai_endpoint: OPENAI_CHAT_ENDPOINT.into(),
        openai_extra_params_enabled: false,
        openai_extra_params: json!({}),
        custom_headers_enabled: false,
        custom_headers: json!({}),
        anthropic_extra_params_enabled: false,
        anthropic_extra_params: json!({}),
        context_window_tokens: None,
        max_completion_tokens: None,
        anthropic_max_tokens: None,
        anthropic_thinking_effort: None,
        thinking_budget_tokens: None,
    }
}

async fn setup() -> (axum::Router, cursor_server::store::Store, tempfile::TempDir) {
    let (directory, store) = fixtures::temp_store().await;
    store.create_model(&model_input()).await.unwrap();
    let provider = Arc::new(fake_provider::FakeProvider::default());
    let control =
        control::ControlService::new(store.clone(), provider, NetworkClients::new(store.clone()))
            .unwrap();
    (control::api_router(control), store, directory)
}

#[tokio::test]
async fn default_model_endpoint_persists_and_rejects_unknown_ids() {
    let (router, store, _directory) = setup().await;
    let model = store.models().await.unwrap().remove(0);

    let (status, body) = send(
        router.clone(),
        "GET",
        "/__byok-api__/api/models/default",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["model_id"], model.model_hash);

    let (status, _) = send(
        router.clone(),
        "PUT",
        "/__byok-api__/api/models/default",
        json!({"model_id": model.model_hash}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        store.default_model_id().await.unwrap().as_deref(),
        Some(model.model_hash.as_str())
    );

    let (status, _) = send(
        router,
        "PUT",
        "/__byok-api__/api/models/default",
        json!({"model_id": "missing-model"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        store.default_model_id().await.unwrap().as_deref(),
        Some(model.model_hash.as_str())
    );
}

async fn send(router: axum::Router, method: &str, path: &str, body: Value) -> (StatusCode, String) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    let response = router
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}
