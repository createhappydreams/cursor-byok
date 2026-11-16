//! Implements conversation inventory and deletion endpoints.
use axum::{
    extract::{Path, State},
    Json,
};
use serde::Serialize;

use crate::{
    cursor::{composer_headers, composer_headers::CursorSideRemoval},
    store::{ConversationSummary, PurgedConversation},
    Error, Result,
};

use super::ControlService;

pub async fn list(State(service): State<ControlService>) -> Result<Json<Vec<ConversationSummary>>> {
    let mut items = service.conversations().await?;
    // Cursor keeps the sidebar label in its own database; borrow it so the
    // console lists conversations under the names the user actually sees.
    let headers = composer_headers::load().await;
    let mut known: std::collections::HashSet<String> = Default::default();
    if !headers.is_empty() {
        for item in &mut items {
            known.insert(item.conversation_id.clone());
            if let Some(header) = headers.get(&item.conversation_id) {
                item.in_cursor = true;
                item.title = header.name.clone();
                item.workspace = header.workspace.clone();
                item.subagent = header.subagent;
            }
        }
        // Chats that only exist in Cursor's sidebar (cloud chats, conversations
        // from before this server ran) are listed too, so they can be managed
        // and deleted from here instead of lingering unmanaged.
        for (id, header) in &headers {
            if known.contains(id) {
                continue;
            }
            items.push(ConversationSummary {
                conversation_id: id.clone(),
                preview: String::new(),
                title: header.name.clone(),
                workspace: header.workspace.clone(),
                subagent: header.subagent,
                message_count: 0,
                checkpoint_count: 0,
                last_used_ms: 0,
                in_cursor: true,
                local: false,
            });
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

/// One-shot removal: this server's rows plus everything Cursor keeps.
///
/// The local purge runs first because it is under our control; the Cursor side
/// is best-effort and its failure (database busy, Cursor running) is reported
/// instead of failing the request, so the caller still learns what happened.
#[derive(Serialize)]
pub struct EverywhereRemoval {
    /// Local rows removed; None when this server had no data for the chat.
    pub local: Option<PurgedConversation>,
    /// Per-family Cursor-side removal report; None when it could not be opened.
    pub cursor: Option<CursorSideRemoval>,
    /// Why the Cursor side was skipped, when it was.
    pub cursor_error: Option<String>,
}

pub async fn remove_everywhere(
    State(service): State<ControlService>,
    Path(conversation_id): Path<String>,
) -> Result<Json<EverywhereRemoval>> {
    let local = service.purge_conversation(&conversation_id).await?;
    let cursor = match composer_headers::forget_detailed(&[conversation_id.clone()]).await {
        Ok(report) => Some(report),
        Err(error) => {
            tracing::warn!(
                %error,
                conversation_id = %conversation_id,
                "cursor-side removal failed; local rows were purged"
            );
            None
        }
    };
    let cursor_error = if cursor.is_none() {
        Some(format!(
            "Cursor 数据库无法写入；本地数据已删除。请完全退出 Cursor 后重试「清侧边栏」。"
        ))
    } else {
        None
    };
    Ok(Json(EverywhereRemoval {
        local,
        cursor,
        cursor_error,
    }))
}

/// Drops the Cursor-side rows so the chat disappears from its sidebar.
///
/// Deliberately separate from [`remove`]: it writes to another application's
/// database, so the user asks for it explicitly and is told to close Cursor
/// first — otherwise Cursor rewrites the rows from memory. The report includes
/// a `remaining` count that must be 0 for the chat to be truly gone.
pub async fn remove_sidebar_entry(
    Path(conversation_id): Path<String>,
) -> Result<Json<CursorSideRemoval>> {
    let report = composer_headers::forget_detailed(&[conversation_id]).await?;
    Ok(Json(report))
}
