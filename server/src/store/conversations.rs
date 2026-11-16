//! Creates, loads, and updates Conversations.
use serde::Serialize;
use sqlx::{Row, Sqlite, Transaction};

use crate::{
    model::{CheckpointId, Conversation, ConversationId, RunId},
    Error, Result,
};

use super::{now_ms, Store};

/// A run whose row has not moved for this long is considered dead.
const STALE_RUN_MS: i64 = 10 * 60 * 1000;

/// One row of the conversation inventory shown by the control console.
#[derive(Clone, Debug, Serialize)]
pub struct ConversationSummary {
    pub conversation_id: String,
    /// First user prompt, trimmed for display. Empty when the conversation has none.
    pub preview: String,
    /// Sidebar label Cursor shows for this chat, when its database is readable.
    pub title: String,
    /// Workspace folder the chat belongs to, from the same source.
    pub workspace: String,
    pub subagent: bool,
    pub message_count: i64,
    pub checkpoint_count: i64,
    pub last_used_ms: i64,
    /// Whether Cursor's own database still knows this chat (sidebar entry).
    pub in_cursor: bool,
    /// Whether this server still stores data for the chat. Rows that only
    /// exist in Cursor's sidebar set this to false.
    pub local: bool,
}

/// Row counts removed by [`Store::purge_conversation`].
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct PurgedConversation {
    pub messages: i64,
    pub checkpoints: i64,
    pub runs: i64,
    pub llm_calls: i64,
    /// Content-addressed blobs reclaimed: the raw request/response bytes the
    /// run traces pointed at. Counted only when no other row still needs them.
    pub blobs: i64,
}

impl Store {
    pub async fn conversation(
        &self,
        conversation_id: &ConversationId,
    ) -> Result<Option<Conversation>> {
        let row = sqlx::query(
            "SELECT current_checkpoint_id, active_run_id
             FROM conversations WHERE conversation_id = ?",
        )
        .bind(conversation_id.as_str())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| Conversation {
            conversation_id: conversation_id.clone(),
            current_checkpoint_id: CheckpointId(row.get(0)),
            active_run_id: row.get::<Option<String>, _>(1).map(RunId),
        }))
    }

    pub(crate) async fn ensure_conversation_tx(
        tx: &mut Transaction<'_, Sqlite>,
        conversation_id: &ConversationId,
    ) -> Result<CheckpointId> {
        sqlx::query(
            "INSERT OR IGNORE INTO conversations(conversation_id, updated_at_ms) VALUES (?, ?)",
        )
        .bind(conversation_id.as_str())
        .bind(now_ms())
        .execute(&mut **tx)
        .await?;

        let current: Option<i64> = sqlx::query_scalar(
            "SELECT current_checkpoint_id FROM conversations WHERE conversation_id = ?",
        )
        .bind(conversation_id.as_str())
        .fetch_one(&mut **tx)
        .await?;
        if let Some(current) = current {
            return Ok(CheckpointId(current));
        }

        let digest = super::checkpoints::message_digest(&[])?;
        let root = sqlx::query(
            "INSERT INTO conversation_checkpoints
             (conversation_id, parent_checkpoint_id, state_digest, created_at_ms)
             VALUES (?, NULL, ?, ?)",
        )
        .bind(conversation_id.as_str())
        .bind(digest.as_slice())
        .bind(now_ms())
        .execute(&mut **tx)
        .await?
        .last_insert_rowid();
        sqlx::query(
            "UPDATE conversations SET current_checkpoint_id = ?, updated_at_ms = ?
             WHERE conversation_id = ? AND current_checkpoint_id IS NULL",
        )
        .bind(root)
        .bind(now_ms())
        .bind(conversation_id.as_str())
        .execute(&mut **tx)
        .await?;
        Ok(CheckpointId(root))
    }

    pub(crate) async fn require_active_head_tx(
        tx: &mut Transaction<'_, Sqlite>,
        conversation_id: &ConversationId,
        run_id: &RunId,
        expected: CheckpointId,
    ) -> Result<()> {
        let row = sqlx::query(
            "SELECT current_checkpoint_id, active_run_id FROM conversations WHERE conversation_id = ?",
        )
        .bind(conversation_id.as_str())
        .fetch_optional(&mut **tx)
        .await?;
        match row {
            Some(row)
                if row.get::<Option<i64>, _>(0) == Some(expected.0)
                    && row.get::<Option<&str>, _>(1) == Some(run_id.as_str()) =>
            {
                Ok(())
            }
            _ => Err(Error::Store(format!(
                "run {run_id} no longer owns conversation {conversation_id} at checkpoint {expected}"
            ))),
        }
    }

    /// Inventory of every stored conversation, newest first.
    pub async fn conversations_overview(&self) -> Result<Vec<ConversationSummary>> {
        let rows = sqlx::query(
            "SELECT c.conversation_id,
                    c.updated_at_ms,
                    (SELECT COUNT(*) FROM messages m
                      WHERE m.conversation_id = c.conversation_id),
                    (SELECT COUNT(*) FROM conversation_checkpoints k
                      WHERE k.conversation_id = c.conversation_id),
                    COALESCE((SELECT COALESCE(
                                 json_extract(m.payload_json, '$.content.parts[0].text'),
                                 json_extract(m.payload_json, '$.content.text'))
                              FROM messages m
                              WHERE m.conversation_id = c.conversation_id
                                AND m.role = 'user'
                                AND m.origin IN ('user', 'prompt')
                                AND COALESCE(
                                      json_extract(m.payload_json, '$.content.parts[0].text'),
                                      json_extract(m.payload_json, '$.content.text'), '')
                                    LIKE '%<user_query>%'
                              ORDER BY m.created_at_ms
                              LIMIT 1), '')
             FROM conversations c
             ORDER BY c.updated_at_ms DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| ConversationSummary {
                conversation_id: row.get(0),
                last_used_ms: row.get(1),
                message_count: row.get(2),
                checkpoint_count: row.get(3),
                preview: short_preview(row.get(4)),
                title: String::new(),
                workspace: String::new(),
                subagent: false,
                in_cursor: false,
                local: true,
            })
            .collect())
    }

    /// Deletes one conversation and everything derived from it.
    ///
    /// Blobs are content addressed and shared between conversations, so they are
    /// left to the orphan sweep instead of being removed here.
    pub async fn purge_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Option<PurgedConversation>> {
        let _write = self.writes.lock().await;
        let mut transaction = self.pool.begin().await?;

        let exists: Option<Option<String>> =
            sqlx::query_scalar("SELECT active_run_id FROM conversations WHERE conversation_id = ?")
                .bind(conversation_id)
                .fetch_optional(&mut *transaction)
                .await?;
        let Some(active_run) = exists else {
            return Ok(None);
        };
        if let Some(run_id) = active_run {
            // A run that stopped without committing (killed process, crash) keeps
            // its status at `running` forever. Only a recently updated run is
            // treated as live, otherwise the conversation could never be removed.
            let live: Option<i64> = sqlx::query_scalar(
                "SELECT 1 FROM runs
                 WHERE run_id = ? AND status = 'running' AND updated_at_ms > ?",
            )
            .bind(&run_id)
            .bind(now_ms() - STALE_RUN_MS)
            .fetch_optional(&mut *transaction)
            .await?;
            if live.is_some() {
                return Err(Error::Store(format!(
                    "conversation {conversation_id} is still running ({run_id})"
                )));
            }
        }

        let run_ids = "SELECT run_id FROM runs WHERE conversation_id = ?";
        let call_ids = "SELECT call_id FROM llm_calls WHERE conversation_id = ?";
        let trace_ids = "SELECT request_id FROM cursor_run_traces WHERE conversation_id = ?";

        let mut report = PurgedConversation::default();

        // The trace artifacts point at content-addressed blobs that hold the raw
        // request and response bytes of this conversation. They are not owned by
        // a single row, so deleting the artifact rows alone would leave the chat
        // content on disk until the age-based retention sweep, which can be days
        // away. Collect what this conversation reaches first; the rows are gone
        // afterwards, so the walk has to happen now.
        let reachable: Vec<Vec<u8>> = sqlx::query_scalar(
            "WITH RECURSIVE roots(blob_id) AS (
                 SELECT a.blob_id FROM cursor_run_trace_artifacts a
                 JOIN cursor_run_traces t ON t.request_id = a.request_id
                 WHERE t.conversation_id = ?
             ), reach(blob_id) AS (
                 SELECT blob_id FROM roots
                 UNION
                 SELECT e.child_blob_id FROM blob_edges e
                 JOIN reach r ON e.parent_blob_id = r.blob_id
             )
             SELECT blob_id FROM reach",
        )
        .bind(conversation_id)
        .fetch_all(&mut *transaction)
        .await?;

        for statement in [
            format!("DELETE FROM tool_round_calls WHERE round_id IN (SELECT round_id FROM tool_rounds WHERE run_id IN ({run_ids}))"),
            format!("DELETE FROM tool_rounds WHERE run_id IN ({run_ids})"),
            format!("DELETE FROM llm_call_response_chunks WHERE call_id IN ({call_ids})"),
            format!("DELETE FROM llm_call_requests WHERE call_id IN ({call_ids})"),
            format!("DELETE FROM cursor_run_trace_artifacts WHERE request_id IN ({trace_ids})"),
        ] {
            sqlx::query(&statement)
                .bind(conversation_id)
                .execute(&mut *transaction)
                .await?;
        }

        sqlx::query("DELETE FROM cursor_run_traces WHERE conversation_id = ?")
            .bind(conversation_id)
            .execute(&mut *transaction)
            .await?;

        let runs = sqlx::query("DELETE FROM runs WHERE conversation_id = ?")
            .bind(conversation_id)
            .execute(&mut *transaction)
            .await?
            .rows_affected();

        sqlx::query("DELETE FROM checkpoint_messages WHERE conversation_id = ?")
            .bind(conversation_id)
            .execute(&mut *transaction)
            .await?;

        let checkpoints =
            sqlx::query("DELETE FROM conversation_checkpoints WHERE conversation_id = ?")
                .bind(conversation_id)
                .execute(&mut *transaction)
                .await?
                .rows_affected();

        let messages = sqlx::query("DELETE FROM messages WHERE conversation_id = ?")
            .bind(conversation_id)
            .execute(&mut *transaction)
            .await?
            .rows_affected();

        sqlx::query("DELETE FROM input_anchors WHERE conversation_id = ?")
            .bind(conversation_id)
            .execute(&mut *transaction)
            .await?;

        let llm_calls = sqlx::query("DELETE FROM llm_calls WHERE conversation_id = ?")
            .bind(conversation_id)
            .execute(&mut *transaction)
            .await?
            .rows_affected();

        sqlx::query("DELETE FROM conversations WHERE conversation_id = ?")
            .bind(conversation_id)
            .execute(&mut *transaction)
            .await?;

        // Reclaim the blobs this conversation held. They are content-addressed
        // and therefore shared: an identical byte range can back other chats,
        // checkpoints or traces. Only blobs that nothing references any more
        // may go, so the set is computed by reachability instead of being
        // deleted wholesale.
        let blobs = reclaim_blobs(&mut transaction, &reachable).await?;

        transaction.commit().await?;

        report.messages = messages as i64;
        report.checkpoints = checkpoints as i64;
        report.runs = runs as i64;
        report.llm_calls = llm_calls as i64;
        report.blobs = blobs as i64;
        Ok(Some(report))
    }
}

/// Deletes the given blobs that no surviving row references, together with the
/// edges that pointed at them. Returns the number of blobs removed.
///
/// A blob survives when a trace artifact still cites it, when it is reachable
/// from such an artifact, or when a blob outside the candidate set still has an
/// edge to it: the last case covers content shared with a chat that is not being
/// purged, which must keep working after this transaction.
async fn reclaim_blobs(
    transaction: &mut Transaction<'_, Sqlite>,
    candidates: &[Vec<u8>],
) -> Result<u64> {
    if candidates.is_empty() {
        return Ok(0);
    }

    sqlx::query("CREATE TEMP TABLE IF NOT EXISTS purge_blobs(blob_id BLOB PRIMARY KEY)")
        .execute(&mut **transaction)
        .await?;
    sqlx::query("DELETE FROM purge_blobs")
        .execute(&mut **transaction)
        .await?;
    for blob in candidates {
        sqlx::query("INSERT OR IGNORE INTO purge_blobs(blob_id) VALUES (?)")
            .bind(blob)
            .execute(&mut **transaction)
            .await?;
    }

    // Live set: every candidate a surviving artifact cites, plus everything
    // reachable downwards from those, plus any candidate a surviving outside
    // blob still has an edge to.
    sqlx::query("CREATE TEMP TABLE IF NOT EXISTS purge_live(blob_id BLOB PRIMARY KEY)")
        .execute(&mut **transaction)
        .await?;
    sqlx::query("DELETE FROM purge_live")
        .execute(&mut **transaction)
        .await?;
    sqlx::query(
        "INSERT OR IGNORE INTO purge_live
         SELECT blob_id FROM purge_blobs
         WHERE blob_id IN (SELECT blob_id FROM cursor_run_trace_artifacts)",
    )
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT OR IGNORE INTO purge_live
         SELECT e.child_blob_id FROM blob_edges e
         WHERE e.child_blob_id IN (SELECT blob_id FROM purge_blobs)
           AND e.parent_blob_id NOT IN (SELECT blob_id FROM purge_blobs)",
    )
    .execute(&mut **transaction)
    .await?;
    // Descendants of a live blob must live too; iterate until it settles.
    loop {
        let added = sqlx::query(
            "INSERT OR IGNORE INTO purge_live
             SELECT e.child_blob_id FROM blob_edges e
             JOIN purge_live l ON e.parent_blob_id = l.blob_id
             WHERE e.child_blob_id IN (SELECT blob_id FROM purge_blobs)",
        )
        .execute(&mut **transaction)
        .await?
        .rows_affected();
        if added == 0 {
            break;
        }
    }

    sqlx::query(
        "DELETE FROM blob_edges
         WHERE (parent_blob_id IN (SELECT blob_id FROM purge_blobs WHERE blob_id NOT IN (SELECT blob_id FROM purge_live))
             OR child_blob_id IN (SELECT blob_id FROM purge_blobs WHERE blob_id NOT IN (SELECT blob_id FROM purge_live)))",
    )
    .execute(&mut **transaction)
    .await?;
    let blobs = sqlx::query(
        "DELETE FROM blobs
         WHERE blob_id IN (SELECT blob_id FROM purge_blobs WHERE blob_id NOT IN (SELECT blob_id FROM purge_live))",
    )
    .execute(&mut **transaction)
    .await?
    .rows_affected();

    sqlx::query("DROP TABLE purge_blobs")
        .execute(&mut **transaction)
        .await?;
    sqlx::query("DROP TABLE purge_live")
        .execute(&mut **transaction)
        .await?;
    Ok(blobs)
}

/// Cursor wraps the typed prompt in `<user_query>` inside a larger context block,
/// so the tag body is the only part worth showing.
fn short_preview(text: String) -> String {
    let body = match (text.find("<user_query>"), text.find("</user_query>")) {
        (Some(start), Some(end)) if end > start + "<user_query>".len() => {
            &text[start + "<user_query>".len()..end]
        }
        _ => text.as_str(),
    };
    let single_line = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = single_line.trim();
    if trimmed.is_empty() || trimmed.starts_with('<') {
        return String::new();
    }
    if trimmed.chars().count() <= 120 {
        return trimmed.to_string();
    }
    trimmed.chars().take(117).collect::<String>() + "..."
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    /// Minimal schema for the blob graph: the real one comes from migrations,
    /// but reclaim_blobs only reads these three tables.
    async fn graph() -> sqlx::SqlitePool {
        let options = SqliteConnectOptions::new()
            .filename(":memory:")
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        for statement in [
            "CREATE TABLE blobs(blob_id BLOB PRIMARY KEY, data BLOB NOT NULL, created_at_ms INTEGER NOT NULL)",
            "CREATE TABLE blob_edges(parent_blob_id BLOB NOT NULL, child_blob_id BLOB NOT NULL, field_name TEXT NOT NULL)",
            "CREATE TABLE cursor_run_trace_artifacts(request_id TEXT NOT NULL, seq INTEGER NOT NULL, blob_id BLOB NOT NULL)",
        ] {
            sqlx::query(statement).execute(&pool).await.unwrap();
        }
        pool
    }

    fn id(byte: u8) -> Vec<u8> {
        vec![byte; 32]
    }

    async fn blob(pool: &sqlx::SqlitePool, value: u8) {
        sqlx::query("INSERT INTO blobs(blob_id, data, created_at_ms) VALUES (?, ?, 0)")
            .bind(id(value))
            .bind(vec![value])
            .execute(pool)
            .await
            .unwrap();
    }

    async fn edge(pool: &sqlx::SqlitePool, parent: u8, child: u8) {
        sqlx::query("INSERT INTO blob_edges(parent_blob_id, child_blob_id, field_name) VALUES (?, ?, 'x')")
            .bind(id(parent))
            .bind(id(child))
            .execute(pool)
            .await
            .unwrap();
    }

    async fn exists(pool: &sqlx::SqlitePool, value: u8) -> bool {
        let found: Option<i64> = sqlx::query_scalar("SELECT 1 FROM blobs WHERE blob_id = ?")
            .bind(id(value))
            .fetch_optional(pool)
            .await
            .unwrap();
        found.is_some()
    }

    #[tokio::test]
    async fn reclaims_unreferenced_blobs_only() {
        let pool = graph().await;
        for value in [1, 2, 3] {
            blob(&pool, value).await;
        }
        // The purged conversation cited 1; another chat's artifact still cites 2,
        // and 2 embeds 3.
        sqlx::query("INSERT INTO cursor_run_trace_artifacts(request_id, seq, blob_id) VALUES ('keep', 0, ?1)")
            .bind(id(2))
            .execute(&pool)
            .await
            .unwrap();
        edge(&pool, 2, 3).await;

        let mut transaction = pool.begin().await.unwrap();
        let removed = reclaim_blobs(&mut transaction, &[id(1), id(2), id(3)])
            .await
            .unwrap();
        transaction.commit().await.unwrap();

        assert_eq!(removed, 1, "only the unshared blob goes");
        assert!(!exists(&pool, 1).await);
        assert!(exists(&pool, 2).await, "still cited by a live artifact");
        assert!(exists(&pool, 3).await, "reachable from a live blob");
    }

    #[tokio::test]
    async fn keeps_blob_embedded_by_a_surviving_parent() {
        let pool = graph().await;
        blob(&pool, 1).await;
        blob(&pool, 9).await;
        // Blob 1 is shared content: a blob outside the candidate set embeds it.
        edge(&pool, 9, 1).await;

        let mut transaction = pool.begin().await.unwrap();
        let removed = reclaim_blobs(&mut transaction, &[id(1)]).await.unwrap();
        transaction.commit().await.unwrap();

        assert_eq!(removed, 0);
        assert!(exists(&pool, 1).await);
    }

    #[tokio::test]
    async fn empty_candidate_set_is_a_no_op() {
        let pool = graph().await;
        let mut transaction = pool.begin().await.unwrap();
        assert_eq!(reclaim_blobs(&mut transaction, &[]).await.unwrap(), 0);
        transaction.commit().await.unwrap();
    }
}
