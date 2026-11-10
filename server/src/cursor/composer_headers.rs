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
    /// Multi-root windows carry the folder list elsewhere: a `.code-workspace`
    /// file, or a generated `workspace.json`. Only `uri` is populated for the
    /// single-folder case, which is why both shapes must be handled.
    #[serde(default, rename = "configPath")]
    config_path: Option<WorkspaceUri>,
}

#[derive(Deserialize)]
struct WorkspaceUri {
    #[serde(default, rename = "fsPath")]
    fs_path: String,
}

impl WorkspaceIdentifier {
    /// Folder the chat belongs to, or empty when Cursor recorded no workspace.
    fn folder(self) -> String {
        if let Some(uri) = self.uri {
            if !uri.fs_path.is_empty() {
                return uri.fs_path;
            }
        }
        let Some(config) = self.config_path else {
            return String::new();
        };
        if config.fs_path.is_empty() {
            return String::new();
        }
        workspace_from_config(&config.fs_path)
    }
}

/// Turns a `configPath` into something the console can group by.
///
/// A `.code-workspace` file already names the project, so its own path is the
/// label Cursor shows. A generated `workspace.json` is only an opaque hash
/// directory, so its first folder stands in for the project instead.
fn workspace_from_config(fs_path: &str) -> String {
    let path = PathBuf::from(fs_path);
    let is_code_workspace = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("code-workspace"));
    if is_code_workspace {
        return fs_path.to_string();
    }
    let Ok(text) = std::fs::read_to_string(&path) else {
        return String::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return String::new();
    };
    let Some(first) = value
        .get("folders")
        .and_then(|folders| folders.as_array())
        .and_then(|folders| folders.first())
    else {
        return String::new();
    };
    if let Some(uri) = first.get("uri").and_then(|uri| uri.as_str()) {
        return file_uri_to_path(uri);
    }
    first
        .get("path")
        .and_then(|path| path.as_str())
        .unwrap_or_default()
        .to_string()
}

/// `file:///c%3A/Users/me/project` -> `c:/Users/me/project`.
fn file_uri_to_path(uri: &str) -> String {
    let rest = uri.strip_prefix("file://").unwrap_or(uri);
    let decoded = percent_decode(rest);
    let trimmed = decoded.strip_prefix('/').unwrap_or(&decoded);
    // Keep POSIX paths intact: only Windows drive letters lose the leading slash.
    if trimmed.as_bytes().get(1) == Some(&b':') {
        trimmed.to_string()
    } else {
        decoded
    }
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex_value(bytes[index + 1]), hex_value(bytes[index + 2])) {
                out.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
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
                workspace: parsed.workspace.map(WorkspaceIdentifier::folder).unwrap_or_default(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_windows_file_uri() {
        assert_eq!(
            file_uri_to_path("file:///c%3A/Users/me/project"),
            "c:/Users/me/project"
        );
    }

    #[test]
    fn keeps_posix_file_uri() {
        assert_eq!(file_uri_to_path("file:///home/me/project"), "/home/me/project");
    }

    #[test]
    fn code_workspace_file_is_its_own_label() {
        let path =
            r"c:/Users/me/AppData/Roaming/Cursor/glassMultiRootWorkspaces/shankblock-6.code-workspace";
        assert_eq!(workspace_from_config(path), path);
    }

    #[test]
    fn missing_config_file_degrades_to_empty() {
        assert_eq!(workspace_from_config(r"c:/nope/workspace.json"), "");
    }

    #[test]
    fn single_folder_uri_wins() {
        let parsed: WorkspaceIdentifier =
            serde_json::from_str(r#"{"id":"x","uri":{"fsPath":"h:\\openblock"}}"#).unwrap();
        assert_eq!(parsed.folder(), r"h:\openblock");
    }

    #[test]
    fn empty_window_has_no_folder() {
        let parsed: WorkspaceIdentifier = serde_json::from_str(r#"{"id":"empty-window"}"#).unwrap();
        assert_eq!(parsed.folder(), "");
    }
}
