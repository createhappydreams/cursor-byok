//! Implements model configuration endpoints.
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    model::{ModelConfig, ModelConfigInput},
    Result,
};

use super::{
    ControlService, DiscoveredModels, LegacyModelImportPreview, LegacyModelImportResult,
    ModelConnectivityResult, ModelDiscoveryInput,
};

#[derive(Deserialize)]
pub struct SaveModels {
    pub models: Vec<ModelConfigInput>,
}

#[derive(Deserialize, Default)]
pub struct CreateModelsOptions {
    /// Model whose stored credential a new (empty-key) model should inherit.
    #[serde(default)]
    pub copy_api_key_from: Option<String>,
}

#[derive(Deserialize)]
pub struct ModelOrder {
    pub model_hashes: Vec<String>,
}

pub async fn list(State(service): State<ControlService>) -> Result<Json<Vec<Value>>> {
    let models = service.models().await?;
    Ok(Json(models.iter().map(model_summary).collect()))
}

pub async fn create(
    State(service): State<ControlService>,
    Query(options): Query<CreateModelsOptions>,
    Json(mut input): Json<SaveModels>,
) -> Result<(StatusCode, Json<Vec<Value>>)> {
    // Duplicating a model in the console starts from a masked key, so the stored
    // credential is inherited here instead of travelling through the browser.
    if let Some(source) = options.copy_api_key_from.as_deref() {
        if input
            .models
            .iter()
            .any(|model| model.api_key.trim().is_empty())
        {
            if let Some(stored) = service
                .models()
                .await?
                .into_iter()
                .find(|model| model.model_hash == source)
            {
                for model in &mut input.models {
                    if model.api_key.trim().is_empty() {
                        model.api_key = stored.api_key.clone();
                    }
                }
            }
        }
    }
    let models = service.create_models(&input.models).await?;
    Ok((
        StatusCode::CREATED,
        Json(models.iter().map(model_summary).collect()),
    ))
}

pub async fn reorder(
    State(service): State<ControlService>,
    Json(input): Json<ModelOrder>,
) -> Result<Json<Vec<Value>>> {
    let models = service.reorder_models(&input.model_hashes).await?;
    Ok(Json(models.iter().map(model_summary).collect()))
}

pub async fn remove(
    State(service): State<ControlService>,
    Path(model_hash): Path<String>,
) -> Result<StatusCode> {
    service.delete_model(&model_hash).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn update(
    State(service): State<ControlService>,
    Path(model_hash): Path<String>,
    Json(input): Json<ModelConfigInput>,
) -> Result<Json<Value>> {
    let model = service.update_model(&model_hash, &input).await?;
    Ok(Json(model_summary(&model)))
}

pub async fn test(
    State(service): State<ControlService>,
    Path((model_hash, test_id)): Path<(String, String)>,
) -> Result<Json<ModelConnectivityResult>> {
    Ok(Json(service.test_model(&model_hash, &test_id).await?))
}

pub async fn cancel(
    State(service): State<ControlService>,
    Path((_model_hash, test_id)): Path<(String, String)>,
) -> Result<StatusCode> {
    service.cancel_model_test(&test_id);
    Ok(StatusCode::NO_CONTENT)
}

pub async fn discover(
    State(service): State<ControlService>,
    Json(input): Json<ModelDiscoveryInput>,
) -> Result<Json<DiscoveredModels>> {
    Ok(Json(service.discover_models(&input).await?))
}

pub async fn import_v0049(
    State(service): State<ControlService>,
) -> Result<Json<LegacyModelImportResult>> {
    Ok(Json(service.import_v0049_models().await?))
}

pub async fn preview_v0049(
    State(service): State<ControlService>,
) -> Result<Json<LegacyModelImportPreview>> {
    Ok(Json(service.preview_v0049_models().await?))
}

/// Console-safe view of a model: the provider credential is replaced by a hint.
///
/// The console never needs the plaintext key. It is prefilled only when the user
/// types a new one, and an empty key on update means "keep the stored value".
fn model_summary(model: &ModelConfig) -> Value {
    let mut value = serde_json::to_value(model).unwrap_or(Value::Null);
    if let Some(object) = value.as_object_mut() {
        object.insert("api_key".into(), Value::String(String::new()));
        object.insert(
            "api_key_masked".into(),
            Value::String(mask_api_key(&model.api_key)),
        );
        object.insert(
            "has_api_key".into(),
            Value::Bool(!model.api_key.trim().is_empty()),
        );
    }
    value
}

fn mask_api_key(key: &str) -> String {
    let key = key.trim();
    let chars: Vec<char> = key.chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    if chars.len() <= 12 {
        return "••••".into();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

#[cfg(test)]
mod tests {
    use super::mask_api_key;

    #[test]
    fn keeps_head_and_tail_only() {
        let masked = mask_api_key("sk-68f7e8eb8dca4a4e841f69f22734d6a3");
        assert_eq!(masked, "sk-6…d6a3");
        assert!(!masked.contains("e8eb8dca"));
    }

    #[test]
    fn short_keys_degrade_to_placeholder() {
        assert_eq!(mask_api_key("sk-1234"), "••••");
        assert_eq!(mask_api_key(""), "");
        assert_eq!(mask_api_key("   "), "");
    }
}
