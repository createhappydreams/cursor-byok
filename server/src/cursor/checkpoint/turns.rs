//! Projects buffered steps into Cursor Conversation turns.
use prost::Message;

use crate::{
    cursor::{checkpoint::PendingSteps, protocol::proto::agent::v1 as pb},
    store::{BlobEdge, BlobId},
    Error, Result,
};

use super::CheckpointBuilder;

#[derive(Clone)]
pub(super) struct TurnFrontier {
    pub(super) preceding: Vec<BlobId>,
    pub(super) current_id: Option<BlobId>,
    pub(super) current: pb::AgentConversationTurnStructure,
}

impl CheckpointBuilder {
    pub(super) async fn project_turns(
        &mut self,
        mode: i32,
        presentation: &PendingSteps,
    ) -> Result<Vec<BlobId>> {
        self.ensure_turn(mode).await?;
        let Some(turn) = self.turn.as_mut() else {
            return Ok(blob_ids(&self.base.turns, "base turn"));
        };
        let changed = !presentation.steps.is_empty();
        for step in &presentation.steps {
            let mut encoded = Vec::new();
            step.encode(&mut encoded)?;
            let id = self.sync.persist(&encoded, &[]).await?;
            turn.current.steps.push(id.as_bytes().to_vec());
        }
        if changed || turn.current_id.is_none() {
            let wrapper = pb::ConversationTurnStructure {
                turn: Some(
                    pb::conversation_turn_structure::Turn::AgentConversationTurn(
                        turn.current.clone(),
                    ),
                ),
            };
            let mut encoded = Vec::new();
            wrapper.encode(&mut encoded)?;
            let mut edges = Vec::with_capacity(turn.current.steps.len() + 1);
            edges.push(BlobEdge {
                child: BlobId::from_bytes(&turn.current.user_message)?,
                field_name: "agent_conversation_turn.user_message".into(),
            });
            for (index, raw_id) in turn.current.steps.iter().enumerate() {
                edges.push(BlobEdge {
                    child: BlobId::from_bytes(raw_id)?,
                    field_name: format!("agent_conversation_turn.steps[{index}]"),
                });
            }
            turn.current_id = Some(self.sync.persist(&encoded, &edges).await?);
        }
        let mut ids = turn.preceding.clone();
        ids.push(
            turn.current_id
                .clone()
                .ok_or_else(|| Error::Protocol("Cursor current Turn has no BlobID".into()))?,
        );
        Ok(ids)
    }

    async fn ensure_turn(&mut self, mode: i32) -> Result<()> {
        if self.turns_initialized {
            return Ok(());
        }
        self.turns_initialized = true;
        let base_ids = blob_ids(&self.base.turns, "base turn");
        if let Some(mut user) = self.turn_user.clone() {
            user.mode = mode;
            let mut encoded = Vec::new();
            user.encode(&mut encoded)?;
            let user_id = self.sync.persist(&encoded, &[]).await?;
            self.turn = Some(TurnFrontier {
                preceding: base_ids,
                current_id: None,
                current: pb::AgentConversationTurnStructure {
                    user_message: user_id.as_bytes().to_vec(),
                    steps: Vec::new(),
                    request_id: Some(self.sync.request_id().into()),
                    encrypted_model: None,
                    dynamic_tool_count: None,
                    send_message_step_indices: Vec::new(),
                },
            });
            return Ok(());
        }
        let Some((current_id, preceding)) = base_ids.split_last() else {
            return Ok(());
        };
        let data = self.sync.get(current_id).await?.ok_or_else(|| {
            Error::Protocol(format!(
                "missing current Turn Blob {}",
                current_id.to_base64()
            ))
        })?;
        let wrapper = pb::ConversationTurnStructure::decode(data.as_slice())?;
        let Some(pb::conversation_turn_structure::Turn::AgentConversationTurn(current)) =
            wrapper.turn
        else {
            return Err(Error::Protocol(
                "current Cursor Turn is not an agent conversation turn".into(),
            ));
        };
        self.turn = Some(TurnFrontier {
            preceding: preceding.to_vec(),
            current_id: Some(current_id.clone()),
            current,
        });
        Ok(())
    }
}

/// Turns a list of raw ids from `conversation_state` into blob ids, dropping the ones
/// that are not ids at all.
///
/// A newer harness can change what it stores in `turns`; the ids it sends are then no
/// longer 32 bytes. Failing the whole run over one unreadable entry would defeat the
/// tolerant decode path, which is why the rest of the list is kept.
fn blob_ids(raw: &[Vec<u8>], what: &str) -> Vec<BlobId> {
    raw.iter()
        .filter_map(|bytes| match BlobId::from_bytes(bytes) {
            Ok(id) => Some(id),
            Err(error) => {
                tracing::warn!(%error, what, "checkpoint: dropping unreadable blob id");
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_valid_ids_and_drops_the_rest() {
        let valid = vec![7u8; 32];
        let ids = blob_ids(&[valid.clone(), vec![1, 2, 3], vec![]], "base turn");
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0].as_bytes(), valid.as_slice());
    }

    #[test]
    fn an_entirely_unreadable_list_is_empty_not_fatal() {
        assert!(blob_ids(&[b"json".to_vec(), vec![0xff; 40]], "base turn").is_empty());
    }
}
