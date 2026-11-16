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

use serde::{Deserialize, Serialize};
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

/// Per-family row counts for one conversation removed from Cursor's database.
///
/// `remaining` is re-counted after the deletes: when Cursor was closed, it
/// reads 0; a non-zero value means Cursor rewrote the rows from memory (it
/// was still running) and the chat is not really gone yet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CursorSideRemoval {
    /// Rows removed from the `composerHeaders` table (the sidebar list).
    pub composer_headers: u64,
    /// Chat bubbles, including their stream/tool-call shards.
    pub bubbles: u64,
    /// Conversation checkpoints (Cursor's own restore points).
    pub checkpoints: u64,
    /// `composerConvHeaders:*` rows, the history-list entries of newer builds.
    pub conversation_headers: u64,
    /// `composerData`, transcript tail and writer diagnostics.
    pub conversation_data: u64,
    /// `ofsContent:*` cached file contents the conversation referenced.
    pub file_caches: u64,
    /// `messageRequestContext:*` rows, when present.
    pub request_contexts: u64,
    /// ItemTable editor-pane state kept for this conversation.
    pub ui_state: u64,
    /// Total rows removed across every family.
    pub removed: u64,
    /// Whether any row matched at all.
    pub found: bool,
    /// Rows still matching after the deletes; 0 means really gone.
    pub remaining: u64,
}

impl CursorSideRemoval {
    fn is_found(&self) -> bool {
        self.removed > 0
    }
}

/// Every `cursorDiskKV` key family a conversation owns.
///
/// Surveyed against Cursor 3.30: `bubbleId` carries the chat bubbles and
/// their `/s:` stream shards plus `/t:` tool-call shards, `checkpointId` the
/// restore points, `composerConvHeaders` the sidebar history entries,
/// `composerData` the conversation document itself, `composerRootWriterDiagnostic`
/// and `composerTranscriptHeightTail` writer state, `ofsContent` cached copies
/// of files the agent read, and `messageRequestContext` request metadata.
fn cursor_disk_patterns(id: &str) -> Vec<String> {
    vec![
        format!("bubbleId:{id}:%"),
        format!("checkpointId:{id}:%"),
        format!("composerConvHeaders:{id}:%"),
        format!("composerData:{id}"),
        format!("composerRootWriterDiagnostic:{id}"),
        format!("composerTranscriptHeightTail:{id}"),
        format!("ofsContent:{id}:%"),
        format!("messageRequestContext:{id}:%"),
    ]
}

/// `ItemTable` keys newer builds keep per conversation (editor pane state).
fn item_table_keys(id: &str) -> [String; 3] {
    [
        format!("cursor/glass.editorPanelFullscreen/{id}"),
        format!("glass.agentConversationPanes.{id}"),
        format!("glass/cursor.editorPanelVisibility.agent/{id}"),
    ]
}

async fn count_rows_like(pool: &sqlx::SqlitePool, patterns: &[String]) -> Result<u64> {
    let mut total = 0u64;
    for pattern in patterns {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cursorDiskKV WHERE key LIKE ?")
            .bind(pattern)
            .fetch_one(pool)
            .await?;
        total += count.max(0) as u64;
    }
    Ok(total)
}

async fn count_item_table_keys(pool: &sqlx::SqlitePool, keys: &[String]) -> Result<u64> {
    let mut total = 0u64;
    for key in keys {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ItemTable WHERE [key] = ?")
            .bind(key)
            .fetch_one(pool)
            .await?;
        total += count.max(0) as u64;
    }
    Ok(total)
}

/// Drops every Cursor-side row for the given conversations so they vanish
/// from the sidebar, the history list and the on-disk caches.
///
/// Cursor must be closed: it holds the database open and rewrites deleted
/// rows from memory otherwise, which the caller can detect through a
/// non-zero `remaining` count.
pub async fn forget_detailed(ids: &[String]) -> Result<CursorSideRemoval> {
    let mut report = CursorSideRemoval::default();
    let Some(path) = state_db_path() else {
        return Ok(report);
    };
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;

    for id in ids {
        let disk = cursor_disk_patterns(id);
        let items = item_table_keys(id);

        let composer_headers = sqlx::query("DELETE FROM composerHeaders WHERE composerId = ?")
            .bind(id)
            .execute(&pool)
            .await?
            .rows_affected();

        let mut conversation_headers = 0u64;
        let mut bubbles = 0u64;
        let mut checkpoints = 0u64;
        let mut conversation_data = 0u64;
        let mut file_caches = 0u64;
        let mut request_contexts = 0u64;
        for pattern in &disk {
            let result = sqlx::query("DELETE FROM cursorDiskKV WHERE key LIKE ?")
                .bind(pattern)
                .execute(&pool)
                .await?;
            let affected = result.rows_affected();
            match pattern.as_str() {
                p if p.starts_with("composerConvHeaders:") => conversation_headers += affected,
                p if p.starts_with("bubbleId:") => bubbles += affected,
                p if p.starts_with("checkpointId:") => checkpoints += affected,
                p if p.starts_with("messageRequestContext:") => request_contexts += affected,
                p if p.starts_with("ofsContent:") => file_caches += affected,
                _ => conversation_data += affected,
            }
        }

        let mut ui_state = 0u64;
        for key in &items {
            let result = sqlx::query("DELETE FROM ItemTable WHERE [key] = ?")
                .bind(key)
                .execute(&pool)
                .await?;
            ui_state += result.rows_affected();
        }

        report.composer_headers += composer_headers;
        report.conversation_headers += conversation_headers;
        report.bubbles += bubbles;
        report.checkpoints += checkpoints;
        report.conversation_data += conversation_data;
        report.file_caches += file_caches;
        report.request_contexts += request_contexts;
        report.ui_state += ui_state;
        report.removed += composer_headers
            + conversation_headers
            + bubbles
            + checkpoints
            + conversation_data
            + file_caches
            + request_contexts
            + ui_state;

        let header_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM composerHeaders WHERE composerId = ?")
            .bind(id)
            .fetch_one(&pool)
            .await?;
        report.remaining += count_rows_like(&pool, &disk).await?;
        report.remaining += header_rows.max(0) as u64;
        report.remaining += count_item_table_keys(&pool, &items).await?;
    }
    pool.close().await;
    report.found = report.is_found();
    Ok(report)
}

/// Backwards-compatible wrapper: only the total removed count is returned.
pub async fn forget(ids: &[String]) -> Result<u64> {
    Ok(forget_detailed(ids).await?.removed)
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

    const SAMPLE_ID: &str = "216c2f3b-3098-4179-a48f-4cb624c7de52";

    #[test]
    fn disk_patterns_cover_every_key_family() {
        let patterns = cursor_disk_patterns(SAMPLE_ID);
        // Every observed key family must be matched by exactly one pattern.
        for key in [
            format!("bubbleId:{SAMPLE_ID}:0d40b5f6-7e32-4e7f-b964-0208df56838b"),
            format!("bubbleId:{SAMPLE_ID}:0d40b5f6-7e32-4e7f-b964-0208df56838b/s:0"),
            format!("bubbleId:{SAMPLE_ID}:0d40b5f6-7e32-4e7f-b964-0208df56838b/t:call_8cbf538a4a13491a9f1260f98e29118d"),
            format!("bubbleId:{SAMPLE_ID}:0d40b5f6-7e32-4e7f-b964-0208df56838b/t1:call_20d9386495c846469508bee98939"),
            format!("checkpointId:{SAMPLE_ID}:18771c23-ce32-4205-a14f-0b8958920c42"),
            format!("composerConvHeaders:{SAMPLE_ID}:000000"),
            format!("composerData:{SAMPLE_ID}"),
            format!("composerRootWriterDiagnostic:{SAMPLE_ID}"),
            format!("composerTranscriptHeightTail:{SAMPLE_ID}"),
            format!("ofsContent:{SAMPLE_ID}:file:///d%3A/Users/CHD/Documents/work_code/shankblock/AGENTS.md"),
            format!("messageRequestContext:{SAMPLE_ID}:abc"),
        ] {
            assert!(
                patterns.iter().any(|pattern| sql_like_match(pattern, &key)),
                "no pattern matches {key}"
            );
        }
    }

    #[test]
    fn disk_patterns_do_not_leak_across_conversations() {
        let patterns = cursor_disk_patterns(SAMPLE_ID);
        // A different conversation id must not match: the prefix is fixed
        // per family, so a sibling id sharing a prefix cannot sneak through.
        for key in [
            format!("bubbleId:216c2f3b-3098-4179-a48f-4cb624c7de529:%"),
            format!("composerData:{SAMPLE_ID}-extra"),
        ] {
            for pattern in &patterns {
                assert!(
                    !sql_like_match(pattern, &key),
                    "pattern {pattern} should not match {key}"
                );
            }
        }
    }

    #[test]
    fn item_table_keys_are_exact() {
        let keys = item_table_keys(SAMPLE_ID);
        assert_eq!(
            keys,
            [
                format!("cursor/glass.editorPanelFullscreen/{SAMPLE_ID}"),
                format!("glass.agentConversationPanes.{SAMPLE_ID}"),
                format!("glass/cursor.editorPanelVisibility.agent/{SAMPLE_ID}"),
            ]
        );
    }

    /// SQLite `LIKE` semantics, with `%` and `_` as wildcards, so the tests
    /// can assert which keys each deletion pattern actually matches.
    fn sql_like_match(pattern: &str, key: &str) -> bool {
        fn match_from(pattern: &[char], key: &[char]) -> bool {
            match pattern.first() {
                None => key.is_empty(),
                Some('%') => {
                    let rest = &pattern[1..];
                    (0..=key.len()).any(|skip| match_from(rest, &key[skip..]))
                }
                Some('_') => !key.is_empty() && match_from(&pattern[1..], &key[1..]),
                Some(&expected) => {
                    key.first() == Some(&expected) && match_from(&pattern[1..], &key[1..])
                }
            }
        }
        match_from(
            &pattern.chars().collect::<Vec<_>>(),
            &key.chars().collect::<Vec<_>>(),
        )
    }
}
