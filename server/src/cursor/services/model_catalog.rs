//! Publishes the configured model catalog to Cursor.
use axum::{
    body::{Body, Bytes},
    extract::{Extension, State},
    http::{header, HeaderValue, Request, Response, StatusCode},
};
use bytes::{BufMut, BytesMut};
use prost::Message;
use serde_json::{Map, Value};

use crate::{
    api::cursor::proxy::{self, CursorProxy},
    cursor::{protocol::proto::agent::v1 as agent, transport::TransportRegistry},
    model::{format_token_count, parse_token_count, ModelConfig},
    plugin::PluginModelDescriptor,
    Error, Result,
};

#[derive(Clone, PartialEq, Message)]
struct AvailableModelsAddition {
    #[prost(string, repeated, tag = "1")]
    model_names: Vec<String>,
    #[prost(message, repeated, tag = "2")]
    models: Vec<AvailableModel>,
}

#[derive(Clone, PartialEq, Message)]
struct AvailableModel {
    #[prost(string, tag = "1")]
    name: String,
    #[prost(bool, tag = "2")]
    default_on: bool,
    #[prost(bool, optional, tag = "4")]
    is_chat_only: Option<bool>,
    #[prost(bool, optional, tag = "5")]
    supports_agent: Option<bool>,
    #[prost(int32, optional, tag = "6")]
    degradation_status: Option<i32>,
    #[prost(message, optional, tag = "8")]
    tooltip_data: Option<TooltipData>,
    #[prost(bool, optional, tag = "9")]
    supports_thinking: Option<bool>,
    #[prost(bool, optional, tag = "10")]
    supports_images: Option<bool>,
    #[prost(bool, optional, tag = "11")]
    supports_auto_context: Option<bool>,
    #[prost(bool, optional, tag = "14")]
    supports_max_mode: Option<bool>,
    #[prost(int32, optional, tag = "15")]
    context_token_limit: Option<i32>,
    #[prost(int32, optional, tag = "16")]
    context_token_limit_for_max_mode: Option<i32>,
    #[prost(string, optional, tag = "17")]
    client_display_name: Option<String>,
    #[prost(string, optional, tag = "18")]
    server_model_name: Option<String>,
    #[prost(bool, optional, tag = "19")]
    supports_non_max_mode: Option<bool>,
    #[prost(message, optional, tag = "20")]
    tooltip_data_for_max_mode: Option<TooltipData>,
    #[prost(bool, optional, tag = "21")]
    is_recommended_for_background_composer: Option<bool>,
    #[prost(bool, optional, tag = "22")]
    supports_plan_mode: Option<bool>,
    #[prost(bool, optional, tag = "23")]
    is_user_added: Option<bool>,
    #[prost(string, optional, tag = "24")]
    inputbox_short_model_name: Option<String>,
    #[prost(bool, optional, tag = "25")]
    supports_sandboxing: Option<bool>,
    #[prost(bool, optional, tag = "26")]
    supports_cmd_k: Option<bool>,
    #[prost(bool, optional, tag = "27")]
    only_supports_cmd_k: Option<bool>,
    #[prost(message, repeated, tag = "29")]
    parameter_definitions: Vec<ModelParameterDefinition>,
    #[prost(message, repeated, tag = "30")]
    variants: Vec<ModelVariant>,
    #[prost(bool, optional, tag = "35")]
    is_hidden: Option<bool>,
    #[prost(string, repeated, tag = "36")]
    legacy_slugs: Vec<String>,
    #[prost(string, repeated, tag = "37")]
    id_aliases: Vec<String>,
    #[prost(int32, optional, tag = "38")]
    named_model_section_index: Option<i32>,
    #[prost(bool, optional, tag = "40")]
    visible_in_routed_model_view: Option<bool>,
    #[prost(string, optional, tag = "41")]
    vendor_name: Option<String>,
    #[prost(message, optional, tag = "42")]
    vendor: Option<AvailableModelVendor>,
    #[prost(bool, optional, tag = "43")]
    default_disabled_in_admin_allowlist: Option<bool>,
    #[prost(message, repeated, tag = "48")]
    model_picker_badges: Vec<ModelPickerBadge>,
}

#[derive(Clone, PartialEq, Message)]
struct TooltipData {
    #[prost(string, optional, tag = "7")]
    markdown_content: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct ModelParameterDefinition {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(string, tag = "2")]
    name: String,
    #[prost(string, optional, tag = "3")]
    markdown_tooltip: Option<String>,
    #[prost(message, optional, tag = "4")]
    parameter_type: Option<ModelParameterType>,
    #[prost(bool, optional, tag = "5")]
    is_cycleable_by_hotkey: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
struct ModelParameterType {
    #[prost(message, optional, tag = "1")]
    boolean_parameter: Option<BooleanParameter>,
    #[prost(message, optional, tag = "2")]
    enum_parameter: Option<EnumParameter>,
}

#[derive(Clone, PartialEq, Message)]
struct BooleanParameter {
    #[prost(message, repeated, tag = "1")]
    values: Vec<BooleanParameterValue>,
}

#[derive(Clone, PartialEq, Message)]
struct BooleanParameterValue {
    #[prost(string, tag = "1")]
    value: String,
    #[prost(string, optional, tag = "2")]
    display_name: Option<String>,
    #[prost(bool, optional, tag = "3")]
    increases_model_cost: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
struct EnumParameter {
    #[prost(message, repeated, tag = "1")]
    values: Vec<EnumParameterValue>,
}

#[derive(Clone, PartialEq, Message)]
struct EnumParameterValue {
    #[prost(string, tag = "1")]
    value: String,
    #[prost(string, optional, tag = "2")]
    display_name: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct ModelVariant {
    #[prost(message, repeated, tag = "1")]
    parameter_values: Vec<ModelParameterValue>,
    #[prost(string, tag = "2")]
    display_name: String,
    #[prost(bool, tag = "3")]
    is_max_mode: bool,
    #[prost(bool, optional, tag = "4")]
    is_default_max_config: Option<bool>,
    #[prost(bool, optional, tag = "5")]
    is_default_non_max_config: Option<bool>,
    #[prost(message, optional, tag = "6")]
    tooltip_data: Option<TooltipData>,
    #[prost(string, optional, tag = "8")]
    display_name_outside_picker: Option<String>,
    #[prost(string, optional, tag = "9")]
    variant_string_representation: Option<String>,
    #[prost(string, optional, tag = "11")]
    legacy_slug: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct ModelParameterValue {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(string, tag = "2")]
    value: String,
}

#[derive(Clone, PartialEq, Message)]
struct ModelPickerBadge {
    #[prost(string, tag = "1")]
    label: String,
    #[prost(int32, tag = "2")]
    variant: i32,
    #[prost(bool, tag = "3")]
    dismiss_on_selection: bool,
}

#[derive(Clone, PartialEq, Message)]
struct AvailableModelVendor {
    #[prost(int32, tag = "1")]
    id: i32,
    #[prost(string, tag = "2")]
    display_name: String,
}

#[derive(Clone, PartialEq, Message)]
struct UsableModelsAddition {
    #[prost(message, repeated, tag = "1")]
    models: Vec<agent::ModelDetails>,
}

#[derive(Clone, PartialEq, Message)]
struct DefaultModelResponse {
    #[prost(string, tag = "1")]
    model: String,
    #[prost(string, tag = "2")]
    thinking_model: String,
    #[prost(bool, tag = "3")]
    max_mode: bool,
    #[prost(string, tag = "4")]
    next_default_set_date: String,
}

#[derive(Clone, PartialEq, Message)]
struct DefaultModelNudgeDataResponse {
    #[prost(string, tag = "1")]
    nudge_date: String,
    #[prost(bool, tag = "2")]
    should_default_switch_on_new_chat: bool,
    #[prost(string, repeated, tag = "3")]
    models_with_no_default_switch: Vec<String>,
    #[prost(string, tag = "4")]
    conversion_model_override: String,
}

#[derive(Clone, PartialEq, Message)]
struct AvailableCppModelsResponse {
    #[prost(string, repeated, tag = "1")]
    models: Vec<String>,
    #[prost(string, optional, tag = "2")]
    default_model: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct ModelLabel {
    #[prost(string, tag = "1")]
    name: String,
    #[prost(string, tag = "2")]
    label: String,
    #[prost(string, optional, tag = "3")]
    short_label: Option<String>,
    #[prost(bool, optional, tag = "4")]
    supports_agent: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
struct ModelLabelsResponse {
    #[prost(message, repeated, tag = "1")]
    model_labels: Vec<ModelLabel>,
}

#[derive(Clone, PartialEq, Message)]
struct LastDefaultModelNudgeResponse {
    #[prost(string, tag = "1")]
    nudge_date: String,
}

const CLI_LOCAL_MODEL_API_KEY: &str = "cursor-byok-local";

const CONTEXTS: [(&str, &str); 5] = [
    ("200k", "200K"),
    ("356k", "356K"),
    ("500k", "500K"),
    ("800k", "800K"),
    ("1m", "1M"),
];
const EFFORTS: [(&str, &str); 5] = [
    ("low", "Low"),
    ("medium", "Medium"),
    ("high", "High"),
    ("xhigh", "Extra High"),
    ("max", "Max"),
];
const DEFAULT_CONTEXT: &str = "200k";

fn context_limit(context_window_tokens: Option<u64>) -> Option<i32> {
    context_window_tokens.and_then(|tokens| i32::try_from(tokens).ok())
}

fn context_options(context_window_tokens: Option<u64>) -> Vec<(String, String)> {
    let mut contexts = CONTEXTS
        .into_iter()
        .map(|(value, display_name)| (value.to_owned(), display_name.to_owned()))
        .collect::<Vec<_>>();
    if let Some(tokens) = context_window_tokens {
        let value = tokens.to_string();
        let duplicate = contexts
            .iter()
            .any(|(existing, _)| parse_token_count(existing) == Some(tokens));
        if !duplicate {
            contexts.push((value, format!("{} (Custom)", format_token_count(tokens))));
        }
    }
    contexts
}

pub async fn available_models(
    State(registry): State<TransportRegistry>,
    Extension(proxy): Extension<CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let models = registry.store().models().await?;
    let plugin_models = match registry.plugins() {
        Some(plugins) => plugins.configured_models().await,
        None => Vec::new(),
    };
    tracing::info!(
        model_count = models.len(),
        plugin_model_count = plugin_models.len(),
        "appending BYOK models to Cursor AvailableModels"
    );
    let mut available_models = models.iter().map(available_model).collect::<Vec<_>>();
    available_models.extend(plugin_models.iter().map(available_plugin_model));
    let local_count = models.len() + plugin_models.len();
    let local_names = models
        .iter()
        .map(|model| model.model_hash.clone())
        .chain(plugin_models.iter().map(|model| model.id.clone()))
        .collect::<Vec<_>>();
    let local = AvailableModelsAddition {
        model_names: local_names.clone(),
        models: available_models,
    }
    .encode_to_vec();
    match proxy::forward_buffered(&proxy, request).await {
        Ok(upstream) => merge_response(upstream, local, local_count > 0, &local_names),
        Err(error) => {
            tracing::warn!(%error, "Cursor AvailableModels upstream unavailable; using local catalog");
            Ok(local_response(local))
        }
    }
}

pub async fn usable_models(
    State(registry): State<TransportRegistry>,
    Extension(proxy): Extension<CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let models = registry.store().models().await?;
    let plugin_models = match registry.plugins() {
        Some(plugins) => plugins.configured_models().await,
        None => Vec::new(),
    };
    tracing::info!(
        model_count = models.len(),
        plugin_model_count = plugin_models.len(),
        "appending BYOK models to Cursor GetUsableModels"
    );
    let local = UsableModelsAddition {
        models: models
            .iter()
            .map(usable_model)
            .chain(plugin_models.iter().map(usable_plugin_model))
            .collect(),
    }
    .encode_to_vec();
    match proxy::forward_buffered(&proxy, request).await {
        Ok(upstream) => merge_response(upstream, local, false, &[]),
        Err(error) => {
            tracing::warn!(%error, "Cursor GetUsableModels upstream unavailable; using local catalog");
            Ok(local_response(local))
        }
    }
}

pub async fn cpp_available_models(
    State(registry): State<TransportRegistry>,
    Extension(proxy): Extension<CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let models = registry.store().models().await?;
    let plugin_models = configured_plugin_models(&registry).await;
    let names = models
        .iter()
        .map(|model| model.model_hash.clone())
        .chain(plugin_models.iter().map(|model| model.id.clone()))
        .collect::<Vec<_>>();
    tracing::info!(
        model_count = models.len(),
        plugin_model_count = plugin_models.len(),
        "appending BYOK models to Cursor CppService AvailableModels"
    );
    let local = AvailableCppModelsResponse {
        default_model: names.first().cloned(),
        models: names,
    };
    match proxy::forward_buffered(&proxy, request).await {
        Ok(upstream) if upstream.status.is_success() => {
            merge_unary(upstream, |merged: &mut AvailableCppModelsResponse| {
                merged.models.extend(local.models);
                if merged.default_model.as_deref().is_none_or(str::is_empty) {
                    merged.default_model = local.default_model;
                }
            })
        }
        Ok(upstream) => {
            tracing::warn!(
                status = %upstream.status,
                "Cursor CppService AvailableModels upstream rejected request; using local catalog"
            );
            Ok(local_response(local.encode_to_vec()))
        }
        Err(error) => {
            tracing::warn!(%error, "Cursor CppService AvailableModels upstream unavailable; using local catalog");
            Ok(local_response(local.encode_to_vec()))
        }
    }
}

pub async fn model_labels(
    State(registry): State<TransportRegistry>,
    Extension(proxy): Extension<CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let models = registry.store().models().await?;
    let plugin_models = configured_plugin_models(&registry).await;
    let local = ModelLabelsResponse {
        model_labels: models
            .iter()
            .map(|model| ModelLabel {
                name: model.model_hash.clone(),
                label: model.display_name.clone(),
                short_label: Some(model.display_name.clone()),
                supports_agent: Some(true),
            })
            .chain(plugin_models.iter().map(|model| ModelLabel {
                name: model.id.clone(),
                label: model.display_name.clone(),
                short_label: Some(model.display_name.clone()),
                supports_agent: Some(true),
            }))
            .collect(),
    };
    match proxy::forward_buffered(&proxy, request).await {
        Ok(upstream) if upstream.status.is_success() => {
            merge_unary(upstream, |merged: &mut ModelLabelsResponse| {
                merged.model_labels.extend(local.model_labels);
            })
        }
        Ok(upstream) => {
            tracing::warn!(
                status = %upstream.status,
                "Cursor GetModelLabels upstream rejected request; using local labels"
            );
            Ok(local_response(local.encode_to_vec()))
        }
        Err(error) => {
            tracing::warn!(%error, "Cursor GetModelLabels upstream unavailable; using local labels");
            Ok(local_response(local.encode_to_vec()))
        }
    }
}

pub async fn last_default_model_nudge() -> Result<Response<Body>> {
    Ok(local_response(
        LastDefaultModelNudgeResponse {
            nudge_date: "0".into(),
        }
        .encode_to_vec(),
    ))
}

pub async fn default_model_for_cli(
    State(registry): State<TransportRegistry>,
) -> Result<Response<Body>> {
    let models = registry.store().models().await?;
    let plugin_models = configured_plugin_models(&registry).await;
    Ok(local_response(
        agent::GetDefaultModelForCliResponse {
            model: default_model_details(&models, &plugin_models),
        }
        .encode_to_vec(),
    ))
}

pub async fn default_model(State(registry): State<TransportRegistry>) -> Result<Response<Body>> {
    let models = registry.store().models().await?;
    let plugin_models = configured_plugin_models(&registry).await;
    Ok(local_response(
        default_model_response(&models, &plugin_models).encode_to_vec(),
    ))
}

pub async fn default_model_nudge(
    State(registry): State<TransportRegistry>,
) -> Result<Response<Body>> {
    let models = registry.store().models().await?;
    let plugin_models = configured_plugin_models(&registry).await;
    Ok(local_response(
        default_model_nudge_response(&models, &plugin_models).encode_to_vec(),
    ))
}

async fn configured_plugin_models(registry: &TransportRegistry) -> Vec<PluginModelDescriptor> {
    match registry.plugins() {
        Some(plugins) => plugins.configured_models().await,
        None => Vec::new(),
    }
}

fn default_model_details(
    models: &[ModelConfig],
    plugin_models: &[PluginModelDescriptor],
) -> Option<agent::ModelDetails> {
    models
        .first()
        .map(usable_model)
        .or_else(|| plugin_models.first().map(usable_plugin_model))
}

fn default_model_id<'a>(
    models: &'a [ModelConfig],
    plugin_models: &'a [PluginModelDescriptor],
) -> &'a str {
    models
        .first()
        .map(|model| model.model_hash.as_str())
        .or_else(|| plugin_models.first().map(|model| model.id.as_str()))
        .unwrap_or_default()
}

fn default_model_response(
    models: &[ModelConfig],
    plugin_models: &[PluginModelDescriptor],
) -> DefaultModelResponse {
    let model = default_model_id(models, plugin_models).to_owned();
    DefaultModelResponse {
        thinking_model: model.clone(),
        model,
        max_mode: false,
        next_default_set_date: String::new(),
    }
}

fn default_model_nudge_response(
    models: &[ModelConfig],
    plugin_models: &[PluginModelDescriptor],
) -> DefaultModelNudgeDataResponse {
    DefaultModelNudgeDataResponse {
        nudge_date: "0".into(),
        should_default_switch_on_new_chat: false,
        models_with_no_default_switch: models
            .iter()
            .map(|model| model.model_hash.clone())
            .chain(plugin_models.iter().map(|model| model.id.clone()))
            .collect(),
        conversion_model_override: String::new(),
    }
}

/// Decodes a unary catalog response, applies `merge`, and re-encodes it while
/// preserving the original Connect framing. Only use this for small catalog
/// messages whose fields are fully modeled; `AvailableModels` keeps the raw
/// byte-append path so unknown upstream fields survive.
fn merge_unary<M: Message + Default>(
    upstream: proxy::BufferedResponse,
    merge: impl FnOnce(&mut M),
) -> Result<Response<Body>> {
    let (framed, payload) = unary_payload(&upstream.body)?;
    let mut message = M::decode(payload).unwrap_or_default();
    merge(&mut message);
    let encoded = message.encode_to_vec();
    let body = if framed {
        let mut merged = BytesMut::with_capacity(5 + encoded.len());
        merged.put_u8(0);
        merged.put_u32(encoded.len() as u32);
        merged.extend_from_slice(&encoded);
        merged.freeze()
    } else {
        Bytes::from(encoded)
    };
    Ok(upstream.with_body(body))
}

fn merge_response(
    upstream: proxy::BufferedResponse,
    extra: Vec<u8>,
    strip_policy: bool,
    local_names: &[String],
) -> Result<Response<Body>> {
    if !upstream.status.is_success() {
        tracing::warn!(status = %upstream.status, "Cursor model catalog upstream rejected request; using local catalog");
        return Ok(local_response(extra));
    }
    let (framed, payload) = unary_payload(&upstream.body)?;
    // Cursor's official picker filters models through `display_configuration`
    // and may auto-disable unused models. Both policies are only meaningful for
    // official IDs; leaving them in place hides appended BYOK models once the
    // Settings > Models page reloads the catalog.
    let payload = if strip_policy {
        strip_top_level_fields(payload, &CATALOG_POLICY_FIELDS)?
    } else {
        payload.to_vec()
    };
    let mut merged_payload = Vec::with_capacity(payload.len() + extra.len());
    merged_payload.extend_from_slice(&payload);
    merged_payload.extend_from_slice(&extra);
    log_merged_catalog(&merged_payload, local_names);
    let body = if framed {
        let mut merged = BytesMut::with_capacity(5 + merged_payload.len());
        merged.put_u8(0);
        merged.put_u32(merged_payload.len() as u32);
        merged.extend_from_slice(&merged_payload);
        merged.freeze()
    } else {
        Bytes::from(merged_payload)
    };
    Ok(upstream.with_body(body))
}

/// Official catalog fields that must not apply to appended BYOK models:
/// `disable_unused_models_after_n_hours`, `upgrade_unchanged_models_after_n_hours`,
/// `display_configuration`.
const CATALOG_POLICY_FIELDS: [u64; 3] = [12, 13, 15];

struct TopLevelField<'a> {
    number: u64,
    raw: &'a [u8],
}

fn parse_top_level(payload: &[u8]) -> Result<Vec<TopLevelField<'_>>> {
    let mut fields = Vec::new();
    let mut pos = 0;
    while pos < payload.len() {
        let start = pos;
        let (tag, read) = read_varint(&payload[pos..])?;
        pos += read;
        let number = tag >> 3;
        let wire = tag & 7;
        if number == 0 {
            return Err(Error::Protocol("invalid protobuf field number 0".into()));
        }
        pos = match wire {
            0 => pos + read_varint(&payload[pos..])?.1,
            1 => advance_field(pos, 8, payload.len())?,
            2 => {
                let (length, read) = read_varint(&payload[pos..])?;
                advance_field(
                    pos + read,
                    usize::try_from(length).unwrap_or(usize::MAX),
                    payload.len(),
                )?
            }
            5 => advance_field(pos, 4, payload.len())?,
            _ => {
                return Err(Error::Protocol(format!(
                    "unsupported protobuf wire type {wire}"
                )))
            }
        };
        fields.push(TopLevelField {
            number,
            raw: &payload[start..pos],
        });
    }
    Ok(fields)
}

fn read_varint(bytes: &[u8]) -> Result<(u64, usize)> {
    let mut value = 0_u64;
    for (index, byte) in bytes.iter().take(10).enumerate() {
        value |= u64::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            return Ok((value, index + 1));
        }
    }
    Err(Error::Protocol("truncated protobuf varint".into()))
}

fn advance_field(pos: usize, length: usize, total: usize) -> Result<usize> {
    pos.checked_add(length)
        .filter(|next| *next <= total)
        .ok_or_else(|| Error::Protocol("truncated protobuf field".into()))
}

fn strip_top_level_fields(payload: &[u8], doomed: &[u64]) -> Result<Vec<u8>> {
    let fields = parse_top_level(payload)?;
    let removed = fields
        .iter()
        .filter(|field| doomed.contains(&field.number))
        .map(|field| field.number)
        .collect::<Vec<_>>();
    if !removed.is_empty() {
        tracing::info!(
            fields = ?removed,
            "stripping Cursor model catalog policies that would hide BYOK models"
        );
    }
    let mut out = Vec::with_capacity(payload.len());
    for field in fields {
        if !doomed.contains(&field.number) {
            out.extend_from_slice(field.raw);
        }
    }
    Ok(out)
}

fn read_length_delimited(raw: &[u8]) -> Option<&[u8]> {
    let (tag, tag_len) = read_varint(raw).ok()?;
    if tag & 7 != 2 {
        return None;
    }
    let (length, length_len) = read_varint(raw.get(tag_len..)?).ok()?;
    let start = tag_len + length_len;
    let end = start.checked_add(usize::try_from(length).ok()?)?;
    raw.get(start..end)
}

fn top_level_strings(payload: &[u8], number: u64) -> Vec<String> {
    parse_top_level(payload)
        .unwrap_or_default()
        .into_iter()
        .filter(|field| field.number == number)
        .filter_map(|field| read_length_delimited(field.raw))
        .filter_map(|bytes| std::str::from_utf8(bytes).ok().map(str::to_owned))
        .collect()
}

fn log_merged_catalog(payload: &[u8], local_names: &[String]) {
    let model_names = top_level_strings(payload, 1);
    let models = parse_top_level(payload)
        .unwrap_or_default()
        .into_iter()
        .filter(|field| field.number == 2)
        .filter_map(|field| read_length_delimited(field.raw))
        .filter_map(|inner| top_level_strings(inner, 1).into_iter().next())
        .collect::<Vec<_>>();
    let missing_local = local_names
        .iter()
        .filter(|name| !models.contains(name))
        .cloned()
        .collect::<Vec<_>>();
    tracing::info!(
        model_count = models.len(),
        model_name_count = model_names.len(),
        missing_local = ?missing_local,
        "Cursor model catalog merged response"
    );
}

fn local_response(body: Vec<u8>) -> Response<Body> {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/proto"),
    );
    response
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
            "cannot merge compressed or terminal model catalog frame: flags={flags}"
        )));
    }
    Ok((true, &body[5..]))
}

fn available_model(model: &ModelConfig) -> AvailableModel {
    let contexts = context_options(model.context_window_tokens);
    let tooltip = model_tooltip(model);
    let variants = model_variants(
        &model.model_hash,
        &model.display_name,
        &tooltip,
        &contexts,
        true,
    );
    let legacy_slugs: Vec<String> = variants
        .iter()
        .filter_map(|variant| variant.legacy_slug.clone())
        .collect();
    AvailableModel {
        name: model.model_hash.clone(),
        default_on: true,
        is_chat_only: None,
        supports_agent: Some(true),
        degradation_status: Some(0),
        tooltip_data: Some(tooltip.clone()),
        supports_thinking: Some(true),
        supports_images: Some(true),
        supports_auto_context: None,
        supports_max_mode: Some(true),
        context_token_limit: context_limit(model.context_window_tokens),
        context_token_limit_for_max_mode: context_limit(model.context_window_tokens),
        client_display_name: Some(model.display_name.clone()),
        server_model_name: Some(model.model_hash.clone()),
        supports_non_max_mode: Some(true),
        tooltip_data_for_max_mode: Some(tooltip),
        is_recommended_for_background_composer: Some(false),
        supports_plan_mode: Some(true),
        // Cursor treats `is_user_added=true` as a BYOK custom model and filters
        // it against the account's BYOK entitlement once plugins/account state
        // load. BYOK models injected by this server look like normal server
        // models instead so the picker keeps them visible.
        is_user_added: None,
        inputbox_short_model_name: Some(model.display_name.clone()),
        supports_sandboxing: Some(true),
        supports_cmd_k: Some(false),
        only_supports_cmd_k: None,
        parameter_definitions: model_parameters(&contexts, true),
        variants,
        is_hidden: None,
        id_aliases: Vec::new(),
        legacy_slugs,
        named_model_section_index: Some(1),
        // Cursor's Settings → Models list filters on
        // `visibleInRoutedModelView !== true`; official named models omit the
        // field. Keep our models in the named-model list so they appear in
        // both Settings and the picker.
        visible_in_routed_model_view: None,
        vendor_name: Some("cursor".into()),
        vendor: Some(AvailableModelVendor {
            id: 6,
            display_name: "Cursor".into(),
        }),
        default_disabled_in_admin_allowlist: None,
        model_picker_badges: vec![ModelPickerBadge {
            label: model
                .group_name
                .clone()
                .unwrap_or_else(|| provider_host(&model.base_url)),
            variant: 1,
            dismiss_on_selection: false,
        }],
    }
}

/// 徽章回退标签:base_url 的主机名。入库时已校验为带主机的 HTTP(S) URL,
/// 解析失败仅是理论分支,此时原样返回 base_url。
fn provider_host(base_url: &str) -> String {
    reqwest::Url::parse(base_url.trim())
        .ok()
        .and_then(|url| url.host_str().map(str::to_lowercase))
        .unwrap_or_else(|| base_url.trim().into())
}

fn model_parameters(
    contexts: &[(String, String)],
    thinking: bool,
) -> Vec<ModelParameterDefinition> {
    let mut parameters = vec![ModelParameterDefinition {
        id: "context".into(),
        name: "Context".into(),
        markdown_tooltip: Some("Context size used to trigger conversation compaction.".into()),
        parameter_type: Some(ModelParameterType {
            boolean_parameter: None,
            enum_parameter: Some(EnumParameter {
                values: contexts
                    .iter()
                    .map(|(value, display_name)| EnumParameterValue {
                        value: value.clone(),
                        display_name: Some(display_name.clone()),
                    })
                    .collect(),
            }),
        }),
        is_cycleable_by_hotkey: Some(false),
    }];
    if thinking {
        parameters.push(ModelParameterDefinition {
            id: "reasoning".into(),
            name: "Effort".into(),
            markdown_tooltip: Some("Effort the model uses to generate its response.".into()),
            parameter_type: Some(ModelParameterType {
                boolean_parameter: None,
                enum_parameter: Some(EnumParameter {
                    values: EFFORTS
                        .into_iter()
                        .map(|(value, display_name)| EnumParameterValue {
                            value: value.into(),
                            display_name: Some(display_name.into()),
                        })
                        .collect(),
                }),
            }),
            is_cycleable_by_hotkey: Some(true),
        });
    }
    parameters.push(ModelParameterDefinition {
        id: "fast".into(),
        name: "Fast".into(),
        markdown_tooltip: Some("Significantly faster but consumes more usage".into()),
        parameter_type: Some(ModelParameterType {
            boolean_parameter: Some(BooleanParameter {
                values: vec![
                    BooleanParameterValue {
                        value: "false".into(),
                        display_name: None,
                        increases_model_cost: None,
                    },
                    BooleanParameterValue {
                        value: "true".into(),
                        display_name: Some("Fast".into()),
                        increases_model_cost: Some(true),
                    },
                ],
            }),
            enum_parameter: None,
        }),
        is_cycleable_by_hotkey: Some(false),
    });
    parameters
}

fn model_variants(
    name: &str,
    display_name: &str,
    tooltip: &TooltipData,
    contexts: &[(String, String)],
    thinking: bool,
) -> Vec<ModelVariant> {
    // 非思考模型没有 Effort 轴,变体网格只剩 Context × Fast。
    let efforts: &[Option<(&str, &str)>] = if thinking {
        &[
            Some(EFFORTS[0]),
            Some(EFFORTS[1]),
            Some(EFFORTS[2]),
            Some(EFFORTS[3]),
            Some(EFFORTS[4]),
        ]
    } else {
        &[None]
    };
    let mut variants = Vec::with_capacity(contexts.len() * efforts.len() * 2);
    for (context, context_name) in contexts {
        for effort in efforts {
            for fast in [false, true] {
                variants.push(model_variant(
                    name,
                    display_name,
                    tooltip,
                    context,
                    context_name,
                    *effort,
                    fast,
                ));
            }
        }
    }
    variants
}

fn model_variant(
    name: &str,
    display_name: &str,
    tooltip: &TooltipData,
    context: &str,
    context_name: &str,
    effort: Option<(&str, &str)>,
    fast: bool,
) -> ModelVariant {
    let mut suffix = Vec::with_capacity(3);
    if context != DEFAULT_CONTEXT {
        suffix.push(context_name);
    }
    if let Some((_, effort_name)) = effort {
        suffix.push(effort_name);
    }
    if fast {
        suffix.push("Fast");
    }
    let suffix = suffix.join(" ");
    let display_name = if suffix.is_empty() {
        display_name.to_owned()
    } else {
        format!(
            "{display_name} <span style=\"color: var(--cursor-text-tertiary);\">{suffix}</span>"
        )
    };
    let is_default =
        context == DEFAULT_CONTEXT && !fast && effort.is_none_or(|(effort, _)| effort == "high");
    let mut parameter_values = vec![ModelParameterValue {
        id: "context".into(),
        value: context.into(),
    }];
    if let Some((effort, _)) = effort {
        parameter_values.push(ModelParameterValue {
            id: "reasoning".into(),
            value: effort.into(),
        });
    }
    parameter_values.push(ModelParameterValue {
        id: "fast".into(),
        value: fast.to_string(),
    });
    ModelVariant {
        parameter_values,
        display_name: display_name.clone(),
        is_max_mode: false,
        is_default_max_config: is_default.then_some(true),
        is_default_non_max_config: is_default.then_some(true),
        tooltip_data: Some(tooltip.clone()),
        display_name_outside_picker: Some(display_name),
        variant_string_representation: Some(match effort {
            Some((effort, _)) => {
                format!("{name}[context={context},reasoning={effort},fast={fast}]")
            }
            None => format!("{name}[context={context},fast={fast}]"),
        }),
        legacy_slug: Some(format!(
            "{name}-{context}{}{}",
            effort
                .map(|(effort, _)| format!("-{effort}"))
                .unwrap_or_default(),
            if fast { "-fast" } else { "" }
        )),
    }
}

fn model_tooltip(model: &ModelConfig) -> TooltipData {
    TooltipData {
        markdown_content: Some(model.tooltip_data.clone()),
    }
}

fn available_plugin_model(model: &PluginModelDescriptor) -> AvailableModel {
    let tooltip = TooltipData {
        markdown_content: model.description.clone(),
    };
    // Effort 与上下文档位由宿主统一提供,与内置模型一致;插件不再声明这两项。
    let contexts = context_options(None);
    let variants = model_variants(&model.id, &model.display_name, &tooltip, &contexts, true);
    let legacy_slugs: Vec<String> = variants
        .iter()
        .filter_map(|variant| variant.legacy_slug.clone())
        .collect();
    AvailableModel {
        name: model.id.clone(),
        default_on: true,
        is_chat_only: None,
        supports_agent: Some(true),
        degradation_status: Some(0),
        tooltip_data: Some(tooltip.clone()),
        supports_thinking: Some(true),
        supports_images: Some(model.images),
        supports_auto_context: None,
        supports_max_mode: Some(false),
        context_token_limit: None,
        context_token_limit_for_max_mode: None,
        client_display_name: Some(model.display_name.clone()),
        server_model_name: Some(model.id.clone()),
        supports_non_max_mode: Some(true),
        tooltip_data_for_max_mode: Some(tooltip.clone()),
        is_recommended_for_background_composer: Some(false),
        supports_plan_mode: Some(true),
        is_user_added: None,
        inputbox_short_model_name: Some(model.display_name.clone()),
        supports_sandboxing: Some(true),
        supports_cmd_k: Some(false),
        only_supports_cmd_k: None,
        parameter_definitions: model_parameters(&contexts, true),
        variants,
        is_hidden: None,
        id_aliases: Vec::new(),
        legacy_slugs,
        named_model_section_index: Some(1),
        visible_in_routed_model_view: None,
        vendor_name: Some(model.provider_type.clone()),
        vendor: Some(AvailableModelVendor {
            id: 6,
            display_name: model.provider_type.clone(),
        }),
        default_disabled_in_admin_allowlist: None,
        model_picker_badges: vec![ModelPickerBadge {
            label: model.plugin_name.clone(),
            variant: 1,
            dismiss_on_selection: false,
        }],
    }
}

/// Serializes the same catalog entries Cursor receives over the wire into the
/// camelCase JSON shape it persists in `state.vscdb`. Used by the local model
/// cache synchronizer so the picker keeps BYOK models after the agent-host
/// module refreshes its own official-only catalog.
pub(crate) fn cursor_catalog_entries(
    models: &[ModelConfig],
    plugins: &[PluginModelDescriptor],
) -> Vec<Value> {
    models
        .iter()
        .map(|model| available_model_json(&available_model(model)))
        .chain(
            plugins
                .iter()
                .map(|model| available_model_json(&available_plugin_model(model))),
        )
        .collect()
}

fn available_model_json(model: &AvailableModel) -> Value {
    let mut object = Map::new();
    object.insert("name".into(), model.name.clone().into());
    object.insert("defaultOn".into(), model.default_on.into());
    insert_optional(&mut object, "isChatOnly", model.is_chat_only);
    insert_optional(&mut object, "supportsAgent", model.supports_agent);
    insert_optional(&mut object, "degradationStatus", model.degradation_status);
    if let Some(tooltip) = &model.tooltip_data {
        object.insert("tooltipData".into(), tooltip_json(tooltip));
    }
    insert_optional(&mut object, "supportsThinking", model.supports_thinking);
    insert_optional(&mut object, "supportsImages", model.supports_images);
    insert_optional(
        &mut object,
        "supportsAutoContext",
        model.supports_auto_context,
    );
    insert_optional(&mut object, "supportsMaxMode", model.supports_max_mode);
    insert_optional(&mut object, "contextTokenLimit", model.context_token_limit);
    insert_optional(
        &mut object,
        "contextTokenLimitForMaxMode",
        model.context_token_limit_for_max_mode,
    );
    if let Some(value) = &model.client_display_name {
        object.insert("clientDisplayName".into(), value.clone().into());
    }
    if let Some(value) = &model.server_model_name {
        object.insert("serverModelName".into(), value.clone().into());
    }
    insert_optional(
        &mut object,
        "supportsNonMaxMode",
        model.supports_non_max_mode,
    );
    if let Some(tooltip) = &model.tooltip_data_for_max_mode {
        object.insert("tooltipDataForMaxMode".into(), tooltip_json(tooltip));
    }
    insert_optional(
        &mut object,
        "isRecommendedForBackgroundComposer",
        model.is_recommended_for_background_composer,
    );
    insert_optional(&mut object, "supportsPlanMode", model.supports_plan_mode);
    insert_optional(&mut object, "isUserAdded", model.is_user_added);
    if let Some(value) = &model.inputbox_short_model_name {
        object.insert("inputboxShortModelName".into(), value.clone().into());
    }
    insert_optional(&mut object, "supportsSandboxing", model.supports_sandboxing);
    insert_optional(&mut object, "supportsCmdK", model.supports_cmd_k);
    insert_optional(&mut object, "onlySupportsCmdK", model.only_supports_cmd_k);
    object.insert(
        "parameterDefinitions".into(),
        parameter_definitions_json(&model.parameter_definitions),
    );
    object.insert("variants".into(), variants_json(&model.variants));
    insert_optional(&mut object, "isHidden", model.is_hidden);
    object.insert("legacySlugs".into(), string_array_json(&model.legacy_slugs));
    object.insert("idAliases".into(), string_array_json(&model.id_aliases));
    insert_optional(
        &mut object,
        "namedModelSectionIndex",
        model.named_model_section_index,
    );
    insert_optional(
        &mut object,
        "visibleInRoutedModelView",
        model.visible_in_routed_model_view,
    );
    if let Some(value) = &model.vendor_name {
        object.insert("vendorName".into(), value.clone().into());
    }
    if let Some(vendor) = &model.vendor {
        object.insert(
            "vendor".into(),
            serde_json::json!({
                "id": vendor.id,
                "displayName": vendor.display_name,
            }),
        );
    }
    insert_optional(
        &mut object,
        "defaultDisabledInAdminAllowlist",
        model.default_disabled_in_admin_allowlist,
    );
    object.insert(
        "modelPickerBadges".into(),
        Value::Array(
            model
                .model_picker_badges
                .iter()
                .map(|badge| {
                    serde_json::json!({
                        "label": badge.label,
                        "variant": badge.variant,
                        "dismissOnSelection": badge.dismiss_on_selection,
                    })
                })
                .collect(),
        ),
    );
    Value::Object(object)
}

fn insert_optional<T: Into<Value>>(object: &mut Map<String, Value>, key: &str, value: Option<T>) {
    if let Some(value) = value {
        object.insert(key.into(), value.into());
    }
}

fn string_array_json(values: &[String]) -> Value {
    Value::Array(values.iter().cloned().map(Value::String).collect())
}

fn tooltip_json(tooltip: &TooltipData) -> Value {
    let mut object = Map::new();
    if let Some(value) = &tooltip.markdown_content {
        object.insert("markdownContent".into(), value.clone().into());
    }
    Value::Object(object)
}

fn parameter_definitions_json(definitions: &[ModelParameterDefinition]) -> Value {
    Value::Array(
        definitions
            .iter()
            .map(|definition| {
                let mut object = Map::new();
                object.insert("id".into(), definition.id.clone().into());
                object.insert("name".into(), definition.name.clone().into());
                if let Some(value) = &definition.markdown_tooltip {
                    object.insert("markdownTooltip".into(), value.clone().into());
                }
                if let Some(parameter_type) = &definition.parameter_type {
                    object.insert("parameterType".into(), parameter_type_json(parameter_type));
                }
                insert_optional(
                    &mut object,
                    "isCycleableByHotkey",
                    definition.is_cycleable_by_hotkey,
                );
                Value::Object(object)
            })
            .collect(),
    )
}

fn parameter_type_json(parameter_type: &ModelParameterType) -> Value {
    let mut object = Map::new();
    if let Some(enum_parameter) = &parameter_type.enum_parameter {
        object.insert(
            "enumParameter".into(),
            serde_json::json!({
                "values": enum_parameter
                    .values
                    .iter()
                    .map(|value| {
                        let mut item = Map::new();
                        item.insert("value".into(), value.value.clone().into());
                        if let Some(display_name) = &value.display_name {
                            item.insert("displayName".into(), display_name.clone().into());
                        }
                        Value::Object(item)
                    })
                    .collect::<Vec<_>>(),
            }),
        );
    }
    if let Some(boolean_parameter) = &parameter_type.boolean_parameter {
        object.insert(
            "booleanParameter".into(),
            serde_json::json!({
                "values": boolean_parameter
                    .values
                    .iter()
                    .map(|value| {
                        let mut item = Map::new();
                        item.insert("value".into(), value.value.clone().into());
                        if let Some(display_name) = &value.display_name {
                            item.insert("displayName".into(), display_name.clone().into());
                        }
                        if let Some(increases) = value.increases_model_cost {
                            item.insert("increasesModelCost".into(), increases.into());
                        }
                        Value::Object(item)
                    })
                    .collect::<Vec<_>>(),
            }),
        );
    }
    Value::Object(object)
}

fn variants_json(variants: &[ModelVariant]) -> Value {
    Value::Array(
        variants
            .iter()
            .map(|variant| {
                let mut object = Map::new();
                object.insert(
                    "parameterValues".into(),
                    Value::Array(
                        variant
                            .parameter_values
                            .iter()
                            .map(|value| {
                                serde_json::json!({
                                    "id": value.id,
                                    "value": value.value,
                                })
                            })
                            .collect(),
                    ),
                );
                object.insert("displayName".into(), variant.display_name.clone().into());
                object.insert("isMaxMode".into(), variant.is_max_mode.into());
                insert_optional(
                    &mut object,
                    "isDefaultMaxConfig",
                    variant.is_default_max_config,
                );
                insert_optional(
                    &mut object,
                    "isDefaultNonMaxConfig",
                    variant.is_default_non_max_config,
                );
                if let Some(tooltip) = &variant.tooltip_data {
                    object.insert("tooltipData".into(), tooltip_json(tooltip));
                }
                if let Some(value) = &variant.display_name_outside_picker {
                    object.insert("displayNameOutsidePicker".into(), value.clone().into());
                }
                if let Some(value) = &variant.variant_string_representation {
                    object.insert("variantStringRepresentation".into(), value.clone().into());
                }
                if let Some(value) = &variant.legacy_slug {
                    object.insert("legacySlug".into(), value.clone().into());
                }
                Value::Object(object)
            })
            .collect(),
    )
}

fn cli_local_model_credentials() -> agent::model_details::Credentials {
    agent::model_details::Credentials::ApiKeyCredentials(agent::ApiKeyCredentials {
        api_key: CLI_LOCAL_MODEL_API_KEY.into(),
        base_url: None,
    })
}

fn usable_plugin_model(model: &PluginModelDescriptor) -> agent::ModelDetails {
    agent::ModelDetails {
        model_id: model.id.clone(),
        display_model_id: model.id.clone(),
        display_name: model.display_name.clone(),
        display_name_short: model.display_name.clone(),
        thinking_details: Some(agent::ThinkingDetails::default()),
        credentials: Some(cli_local_model_credentials()),
        ..Default::default()
    }
}

fn usable_model(model: &ModelConfig) -> agent::ModelDetails {
    agent::ModelDetails {
        model_id: model.model_hash.clone(),
        display_model_id: model.model_hash.clone(),
        display_name: model.display_name.clone(),
        display_name_short: model.display_name.clone(),
        thinking_details: Some(agent::ThinkingDetails::default()),
        credentials: Some(cli_local_model_credentials()),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelType, OPENAI_CHAT_ENDPOINT};

    fn model() -> ModelConfig {
        ModelConfig {
            model_hash: "local-model-hash".into(),
            sort_order: 0,
            display_name: "Local Model".into(),
            group_name: None,
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
            context_window_tokens: None,
            max_completion_tokens: None,
            anthropic_max_tokens: None,
            anthropic_thinking_effort: None,
            thinking_budget_tokens: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn cli_model_details_use_local_routing_credentials() {
        let details = usable_model(&model());
        assert_eq!(details.model_id, "local-model-hash");
        assert_eq!(details.display_name, "Local Model");
        let agent::model_details::Credentials::ApiKeyCredentials(credentials) =
            details.credentials.expect("API credentials")
        else {
            panic!("expected API key credentials");
        };
        assert_eq!(credentials.api_key, CLI_LOCAL_MODEL_API_KEY);
        assert_eq!(credentials.base_url, None);
        assert_ne!(credentials.api_key, "provider-secret");
    }

    #[test]
    fn cli_plugin_model_details_use_local_routing_credentials() {
        let details = usable_plugin_model(&PluginModelDescriptor {
            id: "plugin:test/provider/model".into(),
            plugin_id: "plugin:test".into(),
            plugin_name: "Test Plugin".into(),
            provider_id: "provider".into(),
            model_id: "model".into(),
            display_name: "Plugin Model".into(),
            description: None,
            icon: String::new(),
            provider_type: "test".into(),
            max_output_tokens: None,
            images: false,
            enabled: true,
        });
        assert_eq!(details.model_id, "plugin:test/provider/model");
        let agent::model_details::Credentials::ApiKeyCredentials(credentials) =
            details.credentials.expect("API credentials")
        else {
            panic!("expected API key credentials");
        };
        assert_eq!(credentials.api_key, CLI_LOCAL_MODEL_API_KEY);
        assert_eq!(credentials.base_url, None);
    }

    #[test]
    fn local_models_look_like_normal_server_models() {
        let available = available_model(&model());
        assert_eq!(available.is_user_added, None);
        assert_eq!(available.is_hidden, None);
        assert_eq!(available.visible_in_routed_model_view, None);
        assert_eq!(available.supports_auto_context, None);
        assert!(available.id_aliases.is_empty());
        assert!(available
            .legacy_slugs
            .contains(&"local-model-hash-200k-high".to_string()));
    }

    #[test]
    fn configured_context_window_is_sent_to_cursor() {
        let mut model = model();
        model.context_window_tokens = Some(1_000_000);
        let available = available_model(&model);
        assert_eq!(available.context_token_limit, Some(1_000_000));
        assert_eq!(available.context_token_limit_for_max_mode, Some(1_000_000));
    }

    #[test]
    fn strips_only_catalog_policy_fields_from_the_upstream_wire() {
        let payload = [
            0x0A, 0x01, b'a', // field 1: model_names
            0x12, 0x02, 0x08, 0x01, // field 2: models
            0x60, 0x18, // field 12: disable_unused_models_after_n_hours
            0x68, 0x18, // field 13: upgrade_unchanged_models_after_n_hours
            0x7A, 0x02, 0x08, 0x01, // field 15: display_configuration
            0xA0, 0x01, 0x00, // field 20: unrelated scalar
        ];
        let stripped = strip_top_level_fields(&payload, &CATALOG_POLICY_FIELDS).unwrap();
        let numbers = parse_top_level(&stripped)
            .unwrap()
            .into_iter()
            .map(|field| field.number)
            .collect::<Vec<_>>();
        assert_eq!(numbers, vec![1, 2, 20]);
    }

    #[test]
    fn rejects_truncated_catalog_wire_data() {
        assert!(strip_top_level_fields(&[0x7A, 0x05, 0x00], &CATALOG_POLICY_FIELDS).is_err());
        assert!(strip_top_level_fields(&[0x08], &CATALOG_POLICY_FIELDS).is_err());
    }

    #[test]
    fn cli_default_responses_use_the_local_model_hash() {
        let models = vec![model()];
        let details = default_model_details(&models, &[]).expect("default model");
        assert_eq!(details.model_id, "local-model-hash");

        let response = default_model_response(&models, &[]);
        assert_eq!(response.model, "local-model-hash");
        assert_eq!(response.thinking_model, "local-model-hash");

        let nudge = default_model_nudge_response(&models, &[]);
        assert_eq!(
            nudge.models_with_no_default_switch,
            vec!["local-model-hash"]
        );
    }
}
