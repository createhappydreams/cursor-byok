//! Retention for conversation state.
//!
//! The server stores every message, checkpoint, tool round and content-addressed
//! blob it is handed, and `purge_conversation` deliberately leaves blobs to an
//! orphan sweep that does not exist. A long-lived install therefore grows
//! without bound: blob edges alone can outweigh the blobs they describe.
//!
//! Pruning runs in the background after the listener is bound. It used to run
//! before `bind`, which on a 1 GB database kept the port closed for minutes and
//! made every extra launch pile up on the write lock. Work is chunked now so a
//! batch holds the lock for milliseconds instead of the whole sweep.
//!
//! Dropping old blobs is safe by design: `run/engine.rs` keeps tool results
//! usable when a referenced blob is gone, and `context_sync` asks the client to
//! re-send what it no longer has.

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

/// Conversations deleted per transaction. Two keeps a batch well under a second
/// even when each one owns thousands of messages and checkpoints.
const CONVERSATION_BATCH: usize = 2;

/// Blobs deleted per transaction, with the edges that reference them.
const BLOB_BATCH: usize = 3_000;

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

        let mut report = PruneReport::default();

        let conversations: Vec<String> = sqlx::query_scalar(
            "SELECT conversation_id FROM conversations
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
        .fetch_all(&self.pool)
        .await?;

        for batch in conversations.chunks(CONVERSATION_BATCH) {
            let pruned = self.prune_conversations(batch).await?;
            report.conversations += pruned.conversations;
            report.messages += pruned.messages;
            report.checkpoints += pruned.checkpoints;
            tokio::task::yield_now().await;
        }

        let blobs: Vec<Vec<u8>> =
            sqlx::query_scalar("SELECT blob_id FROM blobs WHERE created_at_ms < ?")
                .bind(cutoff)
                .fetch_all(&self.pool)
                .await?;

        for batch in blobs.chunks(BLOB_BATCH) {
            let (edges, removed) = self.prune_blobs(batch).await?;
            report.blob_edges += edges;
            report.blobs += removed;
            tokio::task::yield_now().await;
        }

        let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
            .fetch_one(&self.pool)
            .await?;
        let freelist: i64 = sqlx::query_scalar("PRAGMA freelist_count")
            .fetch_one(&self.pool)
            .await?;
        report.reclaimed_bytes = (page_size.max(0) * freelist.max(0)) as u64;

        // Never VACUUM here. It rewrites the whole file while holding the write
        // lock, which on a gigabyte-sized database stalls every write for
        // minutes right after startup — adding a model in the console simply
        // hangs with no error. Freed pages are left for a manual maintenance
        // window; the sweep only trims the WAL, which is cheap.
        let _write = self.writes.lock().await;
        if let Err(error) = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&self.pool)
            .await
        {
            tracing::debug!(%error, "retention: wal checkpoint skipped");
        }

        Ok(report)
    }

    /// Deletes one chunk of conversations and every child row that hangs off
    /// them. Order mirrors `purge_conversation` so no child outlives its parent;
    /// checkpoint rows reference each other, so the final state is only
    /// consistent at commit time.
    async fn prune_conversations(&self, batch: &[String]) -> Result<PruneReport> {
        let mut report = PruneReport::default();
        let _write = self.writes.lock().await;
        let mut transaction = self.pool.begin().await?;

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
        for id in batch {
            sqlx::query("INSERT OR IGNORE INTO prune_conversations(conversation_id) VALUES (?)")
                .bind(id)
                .execute(&mut *transaction)
                .await?;
        }

        const STATEMENTS: [(&str, &str); 13] = [
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

        for (kind, statement) in STATEMENTS {
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

        sqlx::query("DROP TABLE prune_conversations")
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(report)
    }

    /// Deletes one chunk of blobs plus the edges that point at them.
    /// Returns `(edges_removed, blobs_removed)`.
    async fn prune_blobs(&self, batch: &[Vec<u8>]) -> Result<(u64, u64)> {
        let _write = self.writes.lock().await;
        let mut transaction = self.pool.begin().await?;

        sqlx::query("CREATE TEMP TABLE IF NOT EXISTS prune_blobs(blob_id BLOB PRIMARY KEY)")
            .execute(&mut *transaction)
            .await?;
        sqlx::query("DELETE FROM prune_blobs")
            .execute(&mut *transaction)
            .await?;
        for id in batch {
            sqlx::query("INSERT OR IGNORE INTO prune_blobs(blob_id) VALUES (?)")
                .bind(id)
                .execute(&mut *transaction)
                .await?;
        }

        let edges = sqlx::query(
            "DELETE FROM blob_edges
             WHERE parent_blob_id IN (SELECT blob_id FROM prune_blobs)
                OR child_blob_id IN (SELECT blob_id FROM prune_blobs)",
        )
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        let blobs = sqlx::query("DELETE FROM blobs WHERE blob_id IN (SELECT blob_id FROM prune_blobs)")
            .execute(&mut *transaction)
            .await?
            .rows_affected();

        sqlx::query("DROP TABLE prune_blobs")
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok((edges, blobs))
    }
}
