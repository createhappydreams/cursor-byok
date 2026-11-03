//! Implements conversation inventory and deletion endpoints.
use axum::{
    extract::{Path, State},
    Json,
};

use crate::{
    store::{ConversationSummary, PurgedConversation},
    Error, Result,
};

use super::ControlService;

pub async fn list(State(service): State<ControlService>) -> Result<Json<Vec<ConversationSummary>>> {
    Ok(Json(service.conversations().await?))
}

pub async fn remove(
    State(service): State<ControlService>,
    Path(conversation_id): Path<String>,
) -> Result<Json<PurgedConversation>> {
    match service.purge_conversation(&conversation_id).await? {
        Some(report) => Ok(Json(report)),
        None => Err(Error::Store(format!(
            "conversation {conversation_id} is not stored"
        ))),
    }
}
