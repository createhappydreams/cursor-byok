//! Startup retention for conversation state.
//!
//! The server stores every message, checkpoint, tool round and content-addressed
//! blob it is handed, and `purge_conversation` deliberately leaves blobs to an
//! orphan sweep that does not exist. A long-lived install therefore grows
//! without bound: blob edges alone can outweigh the blobs they describe.
//!
//! This runs once at startup, before the listener is bound, so it never races
//! with a live request and never needs to interrupt one. Dropping old blobs is
//! safe by design: `run/engine.rs` already keeps tool results usable when a
//! referenced blob is gone, and `context_sync` asks the client to re-send what
//! it no longer has.

use serde::Serialize;

use crate::Result;

use super::{now_ms, Store};

/// Rows removed by [`Store::prune_old_data`].
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct PruneReport {
    pub conversations: u64,
    pub messages: u64,
    pub checkpoints: u64,
    pub blobs: u64,
    pub blob_edges: u64,
    /// Bytes sitting on the freelist after the deletes; the file only shrinks
    /// when [`Self::vacuumed`] is true.
    pub reclaimed_bytes: u64,
    pub vacuumed: bool,
}

impl PruneReport {
    pub fn rows_removed(&self) -> u64 {
        self.conversations
            + self.messages
            + self.checkpoints
            + self.blobs
            + self.blob_edges
    }
}

/// A vacuum rewrites the whole database, so only pay for it when the freed
/// pages are worth the I/O.
const VACUUM_THRESHOLD_BYTES: u64 = 128 * 1024 * 1024;

/// Runs retention once and logs the outcome. Failures never block startup.
pub async fn run_startup_retention(store: &Store, keep_days: i64) {
    match store.prune_old_data(keep_days).await {
        Ok(report) if report.rows_removed() > 0 => {
            tracing::info!(
                keep_days,
                conversations = report.conversations,
                messages = report.messages,
                checkpoints = report.checkpoints,
                blobs = report.blobs,
                blob_edges = report.blob_edges,
                reclaimed_mb = report.reclaimed_bytes / 1_048_576,
                vacuumed = report.vacuumed,
                "retention: pruned conversation data"
            );
        }
        Ok(_) => tracing::debug!(keep_days, "retention: nothing to prune"),
        Err(error) => tracing::warn!(%error, keep_days, "retention: prune failed"),
    }
}

impl Store {
    /// Drops conversations untouched for `keep_days`, plus blobs created before
    /// the same cutoff, and compacts the file when enough pages were freed.
    ///
    /// A conversation whose `active_run_id` still points at a live run is kept
    /// regardless of age, so a background subagent can always be resumed.
    pub async fn prune_old_data(&self, keep_days: i64) -> Result<PruneReport> {
        let keep_days = keep_days.max(1);
        let cutoff = now_ms() - keep_days * 86_400_000;
        // A run that was killed keeps `status = running` forever; only a recent
        // one is treated as live, otherwise nothing would ever be prunable.
        let stale_after = now_ms() - 3_600_000;

        let _write = self.writes.lock().await;
        let mut report = PruneReport::default();
        let mut transaction = self.pool.begin().await?;

        // Checkpoint rows reference each other, so the final state is only
        // consistent at commit time.
        sqlx::query("PRAGMA defer_foreign_keys = ON")
            .execute(&mut *transaction)
            .await?;

        sqlx::query(
            "CREATE TEMP TABLE IF NOT EXISTS prune_conversations(
                conversation_id TEXT PRIMARY KEY
             )",
        )
        .execute(&mut *transaction)
        .await?;
        sqlx::query("DELETE FROM prune_conversations")
            .execute(&mut *transaction)
            .await?;
        sqlx::query(
            "INSERT INTO prune_conversations(conversation_id)
             SELECT conversation_id FROM conversations
             WHERE updated_at_ms < ?
               AND (
                 active_run_id IS NULL
                 OR NOT EXISTS (
                   SELECT 1 FROM runs r
                   WHERE r.run_id = conversations.active_run_id
                     AND r.status = 'running'
                     AND r.updated_at_ms > ?
                 )
               )",
        )
        .bind(cutoff)
        .bind(stale_after)
        .execute(&mut *transaction)
        .await?;

        // Order mirrors Store::purge_conversation so no child outlives its parent.
        // The label decides which counter the affected rows land in.
        const CONVERSATION_STATEMENTS: [(&str, &str); 13] = [
            ("other", "DELETE FROM tool_round_calls WHERE round_id IN (
                 SELECT round_id FROM tool_rounds
                 WHERE run_id IN (SELECT run_id FROM runs
                                  WHERE conversation_id IN (SELECT conversation_id FROM prune_conversations)))"),
            ("other", "DELETE FROM tool_rounds WHERE run_id IN (
                 SELECT run_id FROM runs
                 WHERE conversation_id IN (SELECT conversation_id FROM prune_conversations))"),
            ("other", "DELETE FROM llm_call_response_chunks WHERE call_id IN (
                 SELECT call_id FROM llm_calls
                 WHERE conversation_id IN (SELECT conversation_id FROM prune_conversations))"),
            ("other", "DELETE FROM llm_call_requests WHERE call_id IN (
                 SELECT call_id FROM llm_calls
                 WHERE conversation_id IN (SELECT conversation_id FROM prune_conversations))"),
            ("other", "DELETE FROM cursor_run_trace_artifacts WHERE request_id IN (
                 SELECT request_id FROM cursor_run_traces
                 WHERE conversation_id IN (SELECT conversation_id FROM prune_conversations))"),
            ("other", "DELETE FROM cursor_run_traces
             WHERE conversation_id IN (SELECT conversation_id FROM prune_conversations)"),
            ("other", "DELETE FROM runs
             WHERE conversation_id IN (SELECT conversation_id FROM prune_conversations)"),
            ("checkpoint", "DELETE FROM checkpoint_messages
             WHERE conversation_id IN (SELECT conversation_id FROM prune_conversations)"),
            ("checkpoint", "DELETE FROM conversation_checkpoints
             WHERE conversation_id IN (SELECT conversation_id FROM prune_conversations)"),
            ("message", "DELETE FROM messages
             WHERE conversation_id IN (SELECT conversation_id FROM prune_conversations)"),
            ("other", "DELETE FROM input_anchors
             WHERE conversation_id IN (SELECT conversation_id FROM prune_conversations)"),
            ("other", "DELETE FROM llm_calls
             WHERE conversation_id IN (SELECT conversation_id FROM prune_conversations)"),
            ("conversation", "DELETE FROM conversations
             WHERE conversation_id IN (SELECT conversation_id FROM prune_conversations)"),
        ];

        for (kind, statement) in CONVERSATION_STATEMENTS {
            let affected = sqlx::query(statement)
                .execute(&mut *transaction)
                .await?
                .rows_affected();
            match kind {
                "conversation" => report.conversations += affected,
                "checkpoint" => report.checkpoints += affected,
                "message" => report.messages += affected,
                _ => {}
            }
        }

        // Blobs are a cache: drop the ones that predate the cutoff, then the
        // edges that now point at nothing.
        sqlx::query(
            "CREATE TEMP TABLE IF NOT EXISTS prune_blobs(blob_id BLOB PRIMARY KEY)",
        )
        .execute(&mut *transaction)
        .await?;
        sqlx::query("DELETE FROM prune_blobs")
            .execute(&mut *transaction)
            .await?;
        sqlx::query("INSERT INTO prune_blobs(blob_id) SELECT blob_id FROM blobs WHERE created_at_ms < ?")
            .bind(cutoff)
            .execute(&mut *transaction)
            .await?;

        report.blob_edges = sqlx::query(
            "DELETE FROM blob_edges
             WHERE parent_blob_id IN (SELECT blob_id FROM prune_blobs)
                OR child_blob_id IN (SELECT blob_id FROM prune_blobs)",
        )
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        report.blobs = sqlx::query("DELETE FROM blobs WHERE blob_id IN (SELECT blob_id FROM prune_blobs)")
            .execute(&mut *transaction)
            .await?
            .rows_affected();

        sqlx::query("DROP TABLE prune_blobs")
            .execute(&mut *transaction)
            .await?;
        sqlx::query("DROP TABLE prune_conversations")
            .execute(&mut *transaction)
            .await?;

        transaction.commit().await?;

        let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
            .fetch_one(&self.pool)
            .await?;
        let freelist: i64 = sqlx::query_scalar("PRAGMA freelist_count")
            .fetch_one(&self.pool)
            .await?;
        report.reclaimed_bytes = (page_size.max(0) * freelist.max(0)) as u64;

        if report.reclaimed_bytes >= VACUUM_THRESHOLD_BYTES {
            sqlx::query("VACUUM").execute(&self.pool).await?;
            report.vacuumed = true;
        }

        Ok(report)
    }
}
