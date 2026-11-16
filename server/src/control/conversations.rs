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

/// Batch removal request: the console sends every ticked conversation at once
/// instead of one request per row, which matters when a project holds hundreds.
#[derive(serde::Deserialize)]
pub struct SelectionPurge {
    pub conversation_ids: Vec<String>,
    /// Also drop the chats from Cursor's own database.
    pub cursor: bool,
}

/// Aggregated outcome of a batch removal.
#[derive(Serialize, Default)]
pub struct SelectionRemoval {
    /// Summed local row counts across every conversation in the batch.
    pub local: PurgedConversation,
    /// How many of the selected conversations actually had local rows.
    pub local_conversations: usize,
    /// Cursor-side report for the whole batch, one database pass.
    pub cursor: Option<CursorSideRemoval>,
    pub cursor_error: Option<String>,
    /// Conversations that could not be removed locally, with the reason.
    pub failed: Vec<SelectionFailure>,
}

#[derive(Serialize)]
pub struct SelectionFailure {
    pub conversation_id: String,
    pub error: String,
}

/// Removes a whole selection in one request.
///
/// Local rows go first and are transactional per conversation, so one failure
/// (a chat that is still running) does not abort the rest of the batch. The
/// Cursor side is a single pass over every id: `forget_detailed` opens the
/// database once and deletes each conversation's rows inside it.
pub async fn remove_selection(
    State(service): State<ControlService>,
    Json(body): Json<SelectionPurge>,
) -> Result<Json<SelectionRemoval>> {
    let mut report = SelectionRemoval::default();
    for conversation_id in &body.conversation_ids {
        match service.purge_conversation(conversation_id).await {
            Ok(Some(purged)) => {
                report.local_conversations += 1;
                report.local.messages += purged.messages;
                report.local.checkpoints += purged.checkpoints;
                report.local.runs += purged.runs;
                report.local.llm_calls += purged.llm_calls;
                report.local.blobs += purged.blobs;
            }
            Ok(None) => {}
            Err(error) => report.failed.push(SelectionFailure {
                conversation_id: conversation_id.clone(),
                error: error.to_string(),
            }),
        }
    }

    if body.cursor && !body.conversation_ids.is_empty() {
        match composer_headers::forget_detailed(&body.conversation_ids).await {
            Ok(cursor) => report.cursor = Some(cursor),
            Err(error) => {
                tracing::warn!(%error, "cursor-side batch removal failed; local rows were purged");
                report.cursor_error = Some(
                    "Cursor 数据库无法写入；本地数据已删除。请完全退出 Cursor 后重试「清侧边栏」。"
                        .to_string(),
                );
            }
        }
    }

    Ok(Json(report))
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
