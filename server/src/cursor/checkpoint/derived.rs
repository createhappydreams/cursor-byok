//! Derives Todo, Plan, and related checkpoint state from Messages.
use std::collections::HashMap;

use prost::Message;
use serde_json::Value;

use crate::{
    cursor::{
        prompting::{fold_derived_state, fold_derived_state_from, DerivedState},
        protocol::proto::agent::v1 as pb,
    },
    model::{CanonicalMessage, MessageContent},
    store::BlobId,
    Error, Result,
};

use super::CheckpointBuilder;

impl CheckpointBuilder {
    pub(super) async fn build_derived_state(
        &self,
        messages: &[CanonicalMessage],
    ) -> Result<(Vec<BlobId>, Option<BlobId>)> {
        let changes = fold_derived_state(messages);
        let state = if changes.todos.is_some() && !self.base.todos.is_empty() {
            fold_derived_state_from(
                messages,
                DerivedState {
                    todos: Some(self.base_todo_state().await?),
                    plan: None,
                },
            )
        } else {
            changes
        };
        // Tolerate shapes that models actually emit: `{"todos": [...]}`, a bare array,
        // or a wrapper without a todo list at all (then the previous list is kept).
        let todo_values = state.todos.as_ref().and_then(todo_items);
        let mut todo_ids = if todo_values.is_none() {
            self.base
                .todos
                .iter()
                .map(|raw| BlobId::from_bytes(raw))
                .collect::<Result<Vec<_>>>()?
        } else {
            Vec::new()
        };
        for (index, todo) in todo_values.into_iter().flatten().enumerate() {
            let id = todo_text(todo, &["id", "key", "slug"])
                .unwrap_or_else(|| format!("todo-{}", index + 1));
            let content = todo_text(
                todo,
                &[
                    "content",
                    "title",
                    "text",
                    "description",
                    "task",
                    "label",
                    "summary",
                ],
            )
            .unwrap_or_else(|| {
                tracing::warn!(
                    index,
                    todo_id = %id,
                    "TodoWrite item has no textual content; using its id as the label"
                );
                id.clone()
            });
            let status = todo_status(todo);
            let message = pb::TodoItem {
                id: id.into(),
                content: content.into(),
                status: status as i32,
                created_at: 0,
                updated_at: 0,
                dependencies: todo
                    .get("dependencies")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect(),
            };
            let mut encoded = Vec::new();
            message.encode(&mut encoded)?;
            let id = BlobId::digest(&encoded);
            if self.base.todos.get(index).map(|raw| raw.as_slice()) == Some(id.as_bytes()) {
                todo_ids.push(id);
            } else {
                todo_ids.push(self.sync.persist(&encoded, &[]).await?);
            }
        }
        let plan_id = match state.plan.as_ref().and_then(plan_text) {
            Some(text) => {
                let mut encoded = Vec::new();
                pb::ConversationPlan { plan: text.into() }.encode(&mut encoded)?;
                let id = BlobId::digest(&encoded);
                if self.base.plan.as_deref() == Some(id.as_bytes()) {
                    Some(id)
                } else {
                    Some(self.sync.persist(&encoded, &[]).await?)
                }
            }
            None => None,
        };
        Ok((todo_ids, plan_id))
    }

    async fn base_todo_state(&self) -> Result<serde_json::Value> {
        let mut todos = Vec::with_capacity(self.base.todos.len());
        for raw_id in &self.base.todos {
            let id = BlobId::from_bytes(raw_id)?;
            let data = self.sync.get(&id).await?.ok_or_else(|| {
                Error::Protocol(format!("Cursor Todo Blob is missing: {}", id.to_base64()))
            })?;
            let todo = pb::TodoItem::decode(data.as_slice())?;
            let status = match pb::TodoStatus::try_from(todo.status) {
                Ok(pb::TodoStatus::InProgress) => "in_progress",
                Ok(pb::TodoStatus::Completed) => "completed",
                Ok(pb::TodoStatus::Cancelled) => "cancelled",
                Ok(pb::TodoStatus::Pending) => "pending",
                Ok(pb::TodoStatus::Unspecified) | Err(_) => {
                    return Err(Error::Protocol(format!(
                        "unknown Cursor Todo status: {}",
                        todo.status
                    )))
                }
            };
            todos.push(serde_json::json!({
                "id": todo.id,
                "content": todo.content,
                "status": status,
                "dependencies": todo.dependencies,
            }));
        }
        Ok(serde_json::json!({"merge": false, "todos": todos}))
    }
}

pub(super) fn update_current_step_state(
    messages: &[CanonicalMessage],
) -> Option<pb::CommunicateUpdateTurnState> {
    let result_indices = messages
        .iter()
        .filter_map(|message| match &message.content {
            MessageContent::ToolResult(result) => {
                update_message_index(&result.content).map(|index| (result.call_id.as_str(), index))
            }
            _ => None,
        })
        .collect::<HashMap<_, _>>();
    let mut state = pb::CommunicateUpdateTurnState::default();
    for message in messages {
        let MessageContent::Assistant { tool_calls, .. } = &message.content else {
            continue;
        };
        for call in tool_calls {
            if normalize(&call.name) != "updatecurrentstep" {
                continue;
            }
            if let (Some(step), Some(message_index)) = (
                call.arguments
                    .get("current_step")
                    .and_then(serde_json::Value::as_str),
                result_indices.get(call.call_id.as_str()),
            ) {
                state.history.push(pb::CommunicateUpdateHistoryEntry {
                    step: step.into(),
                    message_index: *message_index,
                });
            }
            if let Some(summary) = call
                .arguments
                .get("final_summary")
                .and_then(serde_json::Value::as_str)
            {
                state.final_summary = Some(summary.into());
            }
            if let Some(subtitle) = call
                .arguments
                .get("completed_subtitle")
                .and_then(serde_json::Value::as_str)
            {
                state.completed_subtitle = Some(subtitle.into());
            }
        }
    }
    (!state.history.is_empty()
        || state.final_summary.is_some()
        || state.completed_subtitle.is_some())
    .then_some(state)
}

fn update_message_index(output: &str) -> Option<u32> {
    let value: serde_json::Value = serde_json::from_str(output).ok()?;
    value
        .get("success")
        .and_then(|success| success.get("message_index"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|index| u32::try_from(index).ok())
}

fn normalize(name: &str) -> String {
    name.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn todo_items(value: &Value) -> Option<&Vec<Value>> {
    value
        .get("todos")
        .and_then(Value::as_array)
        .or_else(|| value.as_array())
}

fn todo_status(todo: &Value) -> pb::TodoStatus {
    let Some(status) = todo.get("status").and_then(Value::as_str) else {
        return pb::TodoStatus::Pending;
    };
    match normalize(status).as_str() {
        "inprogress" | "doing" | "active" | "started" => pb::TodoStatus::InProgress,
        "completed" | "complete" | "done" => pb::TodoStatus::Completed,
        "cancelled" | "canceled" | "skipped" => pb::TodoStatus::Cancelled,
        "" | "pending" | "todo" | "notstarted" | "open" => pb::TodoStatus::Pending,
        normalized => {
            tracing::warn!(
                status = %status,
                normalized,
                "unknown TodoWrite status; treating it as pending"
            );
            pb::TodoStatus::Pending
        }
    }
}

/// Models disagree on todo item labels (`content` / `title` / `text` / ...), and some
/// emit rich-text arrays instead of a plain string, so accept every shape we have seen.
fn todo_text(todo: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| value_text(todo.get(*key)?))
}

fn value_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => {
            let trimmed = text.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        Value::Array(parts) => {
            let joined = parts
                .iter()
                .filter_map(value_text)
                .collect::<Vec<_>>()
                .join(" ");
            (!joined.is_empty()).then_some(joined)
        }
        Value::Object(object) => ["text", "content", "value"]
            .iter()
            .find_map(|key| value_text(object.get(*key)?)),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(_) | Value::Null => None,
    }
}

fn plan_text(value: &Value) -> Option<String> {
    if let Some(text) = value_text(value) {
        return Some(text);
    }
    ["plan", "overview", "title", "summary"]
        .iter()
        .find_map(|key| value.get(*key).and_then(value_text))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{todo_status, todo_text};
    use crate::cursor::protocol::proto::agent::v1 as pb;

    #[test]
    fn todo_text_accepts_alternate_keys_and_rich_text() {
        assert_eq!(
            todo_text(&json!({"title": "Run tests"}), &["content", "title"]),
            Some("Run tests".into())
        );
        assert_eq!(
            todo_text(
                &json!({"content": [{"type": "text", "text": "Hello"}, {"text": "world"}]}),
                &["content"]
            ),
            Some("Hello world".into())
        );
        assert_eq!(
            todo_text(&json!({"id": "a"}), &["content", "title"]),
            None,
            "missing label falls back to the caller, not a protocol error"
        );
    }

    #[test]
    fn todo_status_defaults_to_pending() {
        assert_eq!(todo_status(&json!({})), pb::TodoStatus::Pending);
        assert_eq!(
            todo_status(&json!({"status": "Completed"})),
            pb::TodoStatus::Completed
        );
        assert_eq!(
            todo_status(&json!({"status": "in-progress"})),
            pb::TodoStatus::InProgress
        );
        assert_eq!(
            todo_status(&json!({"status": "something-new"})),
            pb::TodoStatus::Pending
        );
    }
}
