//! Implements conversation inventory and deletion endpoints.
use axum::{
    extract::{Path, State},
    Json,
};
use serde::Serialize;

use crate::{
    cursor::composer_headers,
    store::{ConversationSummary, PurgedConversation},
    Error, Result,
};

use super::ControlService;

pub async fn list(State(service): State<ControlService>) -> Result<Json<Vec<ConversationSummary>>> {
    let mut items = service.conversations().await?;
    // Cursor keeps the sidebar label in its own database; borrow it so the
    // console lists conversations under the names the user actually sees.
    let headers = composer_headers::load().await;
    if !headers.is_empty() {
        for item in &mut items {
            if let Some(header) = headers.get(&item.conversation_id) {
                item.title = header.name.clone();
                item.workspace = header.workspace.clone();
                item.subagent = header.subagent;
            }
        }
    }
    Ok(Json(items))
}

#[derive(Serialize)]
pub struct PurgeResponse {
    #[serde(flatten)]
    purged: PurgedConversation,
    /// Rows removed from Cursor's own sidebar database. `None` when it could not
    /// be reached — usually because Cursor is still running and holds it open.
    cursor_rows: Option<u64>,
}

pub async fn remove(
    State(service): State<ControlService>,
    Path(conversation_id): Path<String>,
) -> Result<Json<PurgeResponse>> {
    let purged = service.purge_conversation(&conversation_id).await?;
    // The sidebar entry is deleted even when the local data is already gone:
    // the first attempt can fail because Cursor was running, and the user needs
    // a way to retry after closing it.
    let cursor_rows = match composer_headers::forget(&[conversation_id.clone()]).await {
        Ok(rows) => Some(rows),
        Err(error) => {
            tracing::warn!(%error, "cursor sidebar entry was not removed");
            None
        }
    };
    if purged.is_none() && !matches!(cursor_rows, Some(rows) if rows > 0) {
        return Err(Error::Store(format!(
            "conversation {conversation_id} is not stored"
        )));
    }
    Ok(Json(PurgeResponse {
        purged: purged.unwrap_or_default(),
        cursor_rows,
    }))
}
