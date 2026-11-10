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

/// Removes only the data this server stored. Cursor's own copy is left alone on
/// purpose: touching its database is risky, so it is a separate action the user
/// triggers from the sidebar button.
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

#[derive(Serialize)]
pub struct SidebarRemoval {
    /// Rows deleted from Cursor's `state.vscdb`.
    pub removed: u64,
    /// Whether any row matched, i.e. the chat existed in the sidebar.
    pub found: bool,
}

/// Drops the Cursor-side rows so the chat disappears from its sidebar.
///
/// Deliberately separate from [`remove`]: it writes to another application's
/// database, so the user asks for it explicitly and is told to close Cursor
/// first — otherwise Cursor rewrites the rows from memory.
pub async fn remove_sidebar_entry(
    Path(conversation_id): Path<String>,
) -> Result<Json<SidebarRemoval>> {
    let removed = composer_headers::forget(&[conversation_id]).await?;
    Ok(Json(SidebarRemoval {
        removed,
        found: removed > 0,
    }))
}
