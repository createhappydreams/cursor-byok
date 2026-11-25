//! Restores Conversation Messages and pending Tool state from a checkpoint.
use crate::{
    cursor::{checkpoint::messages, protocol::proto::agent::v1 as pb},
    model::CanonicalMessage,
    store::BlobId,
    Error, Result,
};

use super::CheckpointBuilder;

impl CheckpointBuilder {
    pub async fn import_prefetched(&self, blobs: &[pb::PreFetchedBlob]) -> Result<()> {
        for blob in blobs {
            let expected = BlobId::from_bytes(&blob.id)?;
            let actual = self.store.put_blob(&blob.value, &[]).await?;
            if expected != actual {
                return Err(Error::Protocol(format!(
                    "prefetched Blob hash mismatch: {}",
                    expected.to_base64()
                )));
            }
        }
        Ok(())
    }

    pub async fn hydrate_messages(
        &self,
        state: Option<&pb::ConversationStateStructure>,
    ) -> Result<Vec<CanonicalMessage>> {
        let mut messages = Vec::new();
        let Some(state) = state else {
            return Ok(messages);
        };
        for (ordinal, raw_id) in state.root_prompt_messages_json.iter().enumerate() {
            // A harness whose schema moved on can hand us blob ids we no longer have,
            // or ids that are not ids at all. Skipping one message keeps the rest of
            // the history; failing here would turn a partial state into a 400 and
            // take the whole run down, which is exactly what the tolerant decode path
            // is meant to prevent.
            let id = match BlobId::from_bytes(raw_id) {
                Ok(id) => id,
                Err(error) => {
                    tracing::warn!(ordinal, %error, "hydrate: dropping unreadable root message id");
                    continue;
                }
            };
            let Some(data) = self.sync.get(&id).await? else {
                tracing::warn!(
                    ordinal,
                    blob = %id.to_base64(),
                    "hydrate: dropping a root message whose blob this server does not have"
                );
                continue;
            };
            match messages::decode(
                &data,
                format!("cursor-root:{}:{ordinal}", id.to_base64()),
            ) {
                Ok(message) => messages.push(message),
                Err(error) => tracing::warn!(
                    ordinal,
                    blob = %id.to_base64(),
                    %error,
                    "hydrate: dropping a root message that no longer decodes"
                ),
            }
        }
        Ok(messages)
    }
}
