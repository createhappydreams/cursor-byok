//! Reads the conversation titles the Cursor client shows in its sidebar.
//!
//! Cursor keeps them in its own `state.vscdb`, table `composerHeaders`: the
//! `value` column is JSON whose `name` is the sidebar label and whose
//! `workspaceIdentifier.uri.fsPath` is the folder the chat belongs to. The
//! server only reads this database, except when the user deletes a
//! conversation from the BYOK console and asks for the Cursor entry to go too.
//!
//! The database is large (tens of GB is common) but `composerHeaders` is small,
//! so a read-only connection opened per request stays cheap.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::Row;

use crate::Result;

/// One row of the Cursor sidebar, keyed by composer id.
#[derive(Clone, Debug, Default)]
pub struct CursorComposer {
    /// Sidebar label. Empty when Cursor never generated one.
    pub name: String,
    /// Workspace folder the chat was written in.
    pub workspace: String,
    pub subagent: bool,
}

#[derive(Deserialize)]
struct HeaderValue {
    #[serde(default)]
    name: String,
    #[serde(default, rename = "workspaceIdentifier")]
    workspace: Option<WorkspaceIdentifier>,
}

#[derive(Deserialize)]
struct WorkspaceIdentifier {
    #[serde(default)]
    uri: Option<WorkspaceUri>,
}

#[derive(Deserialize)]
struct WorkspaceUri {
    #[serde(default, rename = "fsPath")]
    fs_path: String,
}

/// `state.vscdb` of the Cursor install that shares this machine.
pub fn state_db_path() -> Option<PathBuf> {
    let relative = ["Cursor", "User", "globalStorage", "state.vscdb"];
    let base = if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join("Library").join("Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
    }?;
    let mut path = base;
    for part in relative {
        path.push(part);
    }
    path.exists().then_some(path)
}

fn read_only_options(path: &std::path::Path) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .busy_timeout(Duration::from_secs(3))
}

/// Loads every sidebar header. Returns an empty map when Cursor is not installed
/// or its database cannot be opened; titles are decoration, never required.
pub async fn load() -> HashMap<String, CursorComposer> {
    let Some(path) = state_db_path() else {
        return HashMap::new();
    };
    let pool = match SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(read_only_options(&path))
        .await
    {
        Ok(pool) => pool,
        Err(error) => {
            tracing::debug!(%error, "cursor composer headers unavailable");
            return HashMap::new();
        }
    };
    let rows = sqlx::query("SELECT composerId, isSubagent, value FROM composerHeaders")
        .fetch_all(&pool)
        .await;
    pool.close().await;
    let Ok(rows) = rows else {
        return HashMap::new();
    };
    let mut out = HashMap::with_capacity(rows.len());
    for row in rows {
        let id: String = row.get(0);
        let subagent: String = row.try_get(1).unwrap_or_default();
        let value: String = row.try_get(2).unwrap_or_default();
        let parsed: HeaderValue = serde_json::from_str(&value).unwrap_or(HeaderValue {
            name: String::new(),
            workspace: None,
        });
        out.insert(
            id,
            CursorComposer {
                name: parsed.name,
                workspace: parsed
                    .workspace
                    .and_then(|workspace| workspace.uri)
                    .map(|uri| uri.fs_path)
                    .unwrap_or_default(),
                subagent: subagent != "0" && !subagent.is_empty(),
            },
        );
    }
    out
}

/// Drops the Cursor-side rows for the given conversations so they disappear
/// from the sidebar instead of lingering as unopenable entries.
///
/// Cursor must be closed: it holds the database open and will overwrite the
/// change from memory otherwise. Returns the number of rows removed.
pub async fn forget(ids: &[String]) -> Result<u64> {
    let Some(path) = state_db_path() else {
        return Ok(0);
    };
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;

    let mut removed = 0u64;
    for id in ids {
        let result = sqlx::query("DELETE FROM composerHeaders WHERE composerId = ?")
            .bind(id)
            .execute(&pool)
            .await?;
        removed += result.rows_affected();
        for pattern in [
            "composerData:{id}",
            "bubbleId:{id}:%",
            "checkpointId:{id}:%",
            "messageRequestContext:{id}:%",
        ] {
            let key = pattern.replace("{id}", id);
            let result = sqlx::query("DELETE FROM cursorDiskKV WHERE key LIKE ?")
                .bind(key)
                .execute(&pool)
                .await?;
            removed += result.rows_affected();
        }
    }
    pool.close().await;
    Ok(removed)
}
