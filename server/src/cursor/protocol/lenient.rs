//! Tolerant decoding for the Bidi channel.
//!
//! Cursor's agent harness ships its protobuf schema inside `agent-harness.cjs` and
//! upgrades it independently of the client version. Meanwhile `requiredRevision`
//! and `capabilities` in the harness manifest stayed identical across 3.31 -> 3.33,
//! so a client keeps accepting harness builds whose wire format already moved on.
//!
//! A single drifted field (a `map<string, bytes>` that becomes a plain blob, a
//! message that grows a field we read as bytes) makes `prost` desync and turns the
//! whole `AgentClientMessage` into an `INVALID_ARGUMENT`, which the client surfaces
//! as "The server failed 4 times" — for every request, in every conversation.
//!
//! This module removes that failure mode: the payload is walked with the protobuf
//! wire format itself, so each field's length is known exactly. Every field is then
//! probed on its own and only the ones that decode are kept. A field we no longer
//! understand is dropped and reported; its neighbours still work. Nothing here
//! invents data: the kept fields are the original byte ranges, concatenated.

use std::fmt::Debug;
use std::path::{Path, PathBuf};

use prost::Message;

use crate::cursor::protocol::proto::agent::v1 as agent;

/// One field of a serialized message, as located on the wire.
#[derive(Clone, Copy)]
struct RawField {
    number: u32,
    wire_type: u8,
    /// Byte range covering the tag and the payload, so keeping a field is a slice.
    start: usize,
    end: usize,
    /// Payload length for length-delimited fields, 0 otherwise.
    len: usize,
}

/// Where the walk stopped because the bytes no longer look like protobuf.
struct Walk {
    fields: Vec<RawField>,
    /// Offset of the first byte the walker could not interpret.
    failed_at: Option<usize>,
}

fn read_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *buf.get(*pos)?;
        *pos += 1;
        if shift >= 64 {
            return None;
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
    }
}

fn walk(buf: &[u8]) -> Walk {
    let mut fields = Vec::new();
    let mut pos = 0usize;
    while pos < buf.len() {
        let start = pos;
        let Some(tag) = read_varint(buf, &mut pos) else {
            return Walk {
                fields,
                failed_at: Some(start),
            };
        };
        let number = (tag >> 3) as u32;
        let wire_type = (tag & 7) as u8;
        if number == 0 {
            return Walk {
                fields,
                failed_at: Some(start),
            };
        }
        let len = match wire_type {
            0 => {
                if read_varint(buf, &mut pos).is_none() {
                    return Walk {
                        fields,
                        failed_at: Some(start),
                    };
                }
                0
            }
            1 => {
                if pos + 8 > buf.len() {
                    return Walk {
                        fields,
                        failed_at: Some(start),
                    };
                }
                pos += 8;
                0
            }
            5 => {
                if pos + 4 > buf.len() {
                    return Walk {
                        fields,
                        failed_at: Some(start),
                    };
                }
                pos += 4;
                0
            }
            2 => {
                let Some(len) = read_varint(buf, &mut pos) else {
                    return Walk {
                        fields,
                        failed_at: Some(start),
                    };
                };
                let len = len as usize;
                if pos + len > buf.len() {
                    return Walk {
                        fields,
                        failed_at: Some(start),
                    };
                }
                pos += len;
                len
            }
            // Groups (3/4) and the two invalid wire types (6/7) mean the bytes are
            // not a protobuf message here. Stop rather than guess.
            _ => {
                return Walk {
                    fields,
                    failed_at: Some(start),
                }
            }
        };
        fields.push(RawField {
            number,
            wire_type,
            start,
            end: pos,
            len,
        });
    }
    Walk {
        fields,
        failed_at: None,
    }
}

impl RawField {
    fn payload<'a>(&self, buf: &'a [u8]) -> &'a [u8] {
        // The tag is at most ten bytes and the length prefix six, so re-walking the
        // field is simpler than remembering a third offset.
        let mut pos = self.start;
        let _ = read_varint(buf, &mut pos);
        if self.wire_type == 2 {
            if read_varint(buf, &mut pos).is_none() {
                return &[];
            }
        }
        &buf[pos..self.end]
    }
}

/// What one salvage run had to throw away, for the operator log.
#[derive(Debug, Default)]
pub struct SalvageReport {
    /// `level:field` entries that could not be decoded.
    pub dropped: Vec<String>,
    /// Fields the walker kept, by level.
    pub kept: usize,
    pub failed_at: Option<usize>,
    pub strict_error: Option<String>,
    /// Which `AgentClientMessage` member the payload carried.
    pub kind: Option<String>,
}

impl SalvageReport {
    fn merge(&mut self, other: SalvageReport) {
        self.dropped.extend(other.dropped);
        self.kept += other.kept;
        if self.failed_at.is_none() {
            self.failed_at = other.failed_at;
        }
    }
}

/// Keeps every field of `raw` that decodes on its own into `M`.
///
/// The returned message is built from the surviving byte ranges, so it is a strict
/// subset of the input: no field is reinterpreted, and a field that fails its probe
/// cannot shift the ones after it (their lengths came from the walk, not the probe).
fn salvage<M>(raw: &[u8], level: &str) -> (Option<M>, SalvageReport)
where
    M: Message + Default,
{
    let mut report = SalvageReport::default();
    let walked = walk(raw);
    report.failed_at = walked.failed_at;
    if walked.fields.is_empty() {
        return (None, report);
    }
    let mut kept_ranges: Vec<(usize, usize)> = Vec::new();
    for field in &walked.fields {
        let slice = &raw[field.start..field.end];
        match M::decode(slice) {
            Ok(_) => {
                kept_ranges.push((field.start, field.end));
                report.kept += 1;
            }
            Err(error) => {
                report
                    .dropped
                    .push(format!("{level}.#{}: {}", field.number, short(&error)));
            }
        }
    }
    if kept_ranges.is_empty() {
        return (None, report);
    }
    let mut joined = Vec::with_capacity(raw.len());
    for (start, end) in kept_ranges {
        joined.extend_from_slice(&raw[start..end]);
    }
    (M::decode(joined.as_slice()).ok(), report)
}

fn short(error: &prost::DecodeError) -> String {
    let text = error.to_string();
    // prost nests the whole field path; the leaf is what identifies the problem.
    text.chars().take(160).collect()
}

/// Salvages `ConversationStateStructure` field by field.
///
/// This is the level that actually drifted: `file_states` (a `map<string, bytes>`)
/// and `summary` are reported by the harness as the fields where the client's bytes
/// stop matching the vendored schema. Each field is probed alone, so a map entry that
/// no longer parses only costs us that one map.
fn salvage_state(raw: &[u8]) -> (Option<agent::ConversationStateStructure>, SalvageReport) {
    salvage::<agent::ConversationStateStructure>(raw, "state")
}

/// Salvages `AgentRunRequest`, decoding `conversation_state` through [`salvage_state`].
///
/// The state is rebuilt from what survived and re-encoded, so the parent message stays
/// valid even when parts of the state were dropped.
fn salvage_run_request(raw: &[u8]) -> (Option<agent::AgentRunRequest>, SalvageReport) {
    let walked = walk(raw);
    let mut report = SalvageReport {
        failed_at: walked.failed_at,
        ..Default::default()
    };
    if walked.fields.is_empty() {
        return (None, report);
    }

    let mut rebuilt: Vec<u8> = Vec::with_capacity(raw.len());
    for field in &walked.fields {
        let payload = field.payload(raw);
        // Messages that carry what the run needs get their own salvage pass, so one
        // drifted field inside them does not cost the whole object: losing
        // `mcp_tools` means the model is offered no MCP tools at all, and losing
        // `model_details` breaks routing.
        if field.number == 1 && field.wire_type == 2 {
            // The state is the big one: keep its own dropped-field ledger.
            let (state, state_report) = salvage_state(payload);
            report.merge(state_report);
            match state {
                Some(state) => {
                    let encoded = encode_message(&state);
                    push_varint(&mut rebuilt, u64::from((1u32 << 3) | 2));
                    push_varint(&mut rebuilt, encoded.len() as u64);
                    rebuilt.extend_from_slice(&encoded);
                    report.kept += 1;
                }
                None => report
                    .dropped
                    .push("run_request.#1: conversation_state".into()),
            }
            continue;
        }
        let nested: Option<(u32, Vec<u8>)> = match field.number {
            2 => salvage_member::<agent::ConversationAction>(payload, "action")
                .map(|value| (2, encode_message(&value))),
            3 => salvage_member::<agent::ModelDetails>(payload, "model_details")
                .map(|value| (3, encode_message(&value))),
            4 => salvage_member::<agent::McpTools>(payload, "mcp_tools")
                .map(|value| (4, encode_message(&value))),
            7 => salvage_member::<agent::SkillOptions>(payload, "skill_options")
                .map(|value| (7, encode_message(&value))),
            9 => salvage_member::<agent::RequestedModel>(payload, "requested_model")
                .map(|value| (9, encode_message(&value))),
            17 => salvage_member::<agent::PreFetchedBlob>(payload, "prefetched_blob")
                .map(|value| (17, encode_message(&value))),
            _ => None,
        };
        if let Some((number, encoded)) = nested {
            push_varint(&mut rebuilt, u64::from((number << 3) | 2));
            push_varint(&mut rebuilt, encoded.len() as u64);
            rebuilt.extend_from_slice(&encoded);
            report.kept += 1;
            continue;
        }
        if matches!(field.number, 2 | 3 | 4 | 7 | 9 | 17) && field.wire_type == 2 {
            report
                .dropped
                .push(format!("run_request.#{}: unreadable message", field.number));
            continue;
        }
        let slice = &raw[field.start..field.end];
        match agent::AgentRunRequest::decode(slice) {
            Ok(_) => {
                rebuilt.extend_from_slice(slice);
                report.kept += 1;
            }
            Err(error) => report
                .dropped
                .push(format!("run_request.#{}: {}", field.number, short(&error))),
        }
    }
    if rebuilt.is_empty() {
        return (None, report);
    }
    (agent::AgentRunRequest::decode(rebuilt.as_slice()).ok(), report)
}

/// Serializes a salvaged message back into wire bytes for its parent.
fn encode_message<M: Message>(message: &M) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.encoded_len());
    message.encode(&mut out).ok();
    out
}

fn push_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Decodes an `AgentClientMessage` that the strict path rejected.
///
/// Every `oneof` member gets the same field-by-field treatment, not just the run
/// request: a harness that changes `uint32 id` into a UUID string breaks the exec
/// result, the stream close and the interaction response too, and each of those is
/// sent after the run has already started. Salvaging one member but not the others
/// only moves the 400 a few hundred milliseconds later.
///
/// Returns `None` when nothing usable survived, in which case the caller keeps its
/// original error: a payload that is not a client message at all should still fail.
pub fn decode_client_message(raw: &[u8], strict_error: &str) -> Option<agent::AgentClientMessage> {
    let walked = walk(raw);
    if walked.fields.is_empty() {
        return None;
    }
    let mut report = SalvageReport {
        strict_error: Some(strict_error.to_string()),
        failed_at: walked.failed_at,
        ..Default::default()
    };
    let mut message: Option<agent::AgentClientMessage> = None;
    let mut kind = "unknown";

    for field in &walked.fields {
        let payload = field.payload(raw);
        let member = match (field.number, field.wire_type) {
            (1, 2) => {
                kind = "run_request";
                salvage_run_request(payload)
                    .0
                    .map(agent::agent_client_message::Message::RunRequest)
            }
            // The result-carrying members share the same shape of drift: `id` is a
            // varint in the vendored schema and a UUID string in the harness, and one
            // bad field used to take the whole member with it.
            (2, 2) => {
                kind = "exec_client_message";
                salvage_member::<agent::ExecClientMessage>(payload, "exec")
                    .map(agent::agent_client_message::Message::ExecClientMessage)
            }
            (5, 2) => {
                kind = "exec_client_control_message";
                salvage_member::<agent::ExecClientControlMessage>(payload, "control")
                    .map(agent::agent_client_message::Message::ExecClientControlMessage)
            }
            (6, 2) => {
                kind = "interaction_response";
                salvage_member::<agent::InteractionResponse>(payload, "interaction")
                    .map(agent::agent_client_message::Message::InteractionResponse)
            }
            (3, 2) => {
                kind = "kv_client_message";
                salvage_member::<agent::KvClientMessage>(payload, "kv")
                    .map(agent::agent_client_message::Message::KvClientMessage)
            }
            (4, 2) => {
                kind = "conversation_action";
                salvage_member::<agent::ConversationAction>(payload, "action")
                    .map(agent::agent_client_message::Message::ConversationAction)
            }
            (7, 2) => {
                kind = "client_heartbeat";
                salvage_member::<agent::ClientHeartbeat>(payload, "heartbeat")
                    .map(agent::agent_client_message::Message::ClientHeartbeat)
            }
            (8, 2) => {
                kind = "prewarm_request";
                salvage_member::<agent::PrewarmRequest>(payload, "prewarm")
                    .map(agent::agent_client_message::Message::PrewarmRequest)
            }
            _ => {
                report
                    .dropped
                    .push(format!("client.#{}: unknown member", field.number));
                None
            }
        };
        if let Some(member) = member {
            // One message per append; the last decodable member wins, which matches
            // the oneof semantics of the wire format.
            message = Some(agent::AgentClientMessage {
                message: Some(member),
            });
        }
    }

    if let Some(message) = &message {
        report.kind = Some(kind.to_string());
        write_dump(raw, &report, message);
    }
    message
}

/// Field-by-field recovery for a plain message: keeps what decodes, drops the rest.
fn salvage_member<M>(raw: &[u8], level: &str) -> Option<M>
where
    M: Message + Default,
{
    salvage::<M>(raw, level).0
}

/// Writes one text file per salvaged payload so the next drift is diagnosable
/// without a special build: field numbers, lengths, and what was dropped.
fn write_dump(raw: &[u8], report: &SalvageReport, message: &agent::AgentClientMessage) {
    use sha2::{Digest, Sha256};

    let dir = match crate::config::managed_data_dir() {
        Ok(dir) => dir.join("decode-failures"),
        Err(_) => return,
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let digest = Sha256::digest(raw);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or_default();
    let path = dir.join(format!("{stamp}-{}.txt", &hex::encode(digest)[..12]));

    let mut out = String::new();
    out.push_str(&format!(
        "harness={}\nkind={}\nlen={}\nsha256={}\n",
        harness_version().unwrap_or_else(|| "unknown".into()),
        report.kind.as_deref().unwrap_or("unknown"),
        raw.len(),
        hex::encode(digest)
    ));
    if let Some(error) = &report.strict_error {
        out.push_str(&format!("strict_error={error}\n"));
    }
    if let Some(offset) = report.failed_at {
        out.push_str(&format!("wire_failed_at={offset}\n"));
        let from = offset.saturating_sub(32);
        let to = (offset + 32).min(raw.len());
        out.push_str(&format!("window={}\n", hex::encode(&raw[from..to])));
    }
    // Field outline: enough to see which field changed shape without dumping payloads.
    let outline: Vec<String> = walk(raw)
        .fields
        .iter()
        .map(|field| format!("#{}/wt{}/len{}", field.number, field.wire_type, field.len))
        .collect();
    out.push_str(&format!("wire={}\n", outline.join(" ")));
    out.push_str(&format!("dropped={}\n", report.dropped.join(" | ")));
    out.push_str(&format!("kept_fields={}\n", report.kept));
    if let Some(agent::agent_client_message::Message::RunRequest(request)) = &message.message {
        out.push_str(&format!(
            "conversation_id={}\nstate_turns={}\nstate_root_messages={}\n",
            request.conversation_id.clone().unwrap_or_default(),
            request
                .conversation_state
                .as_ref()
                .map_or(0, |state| state.turns.len()),
            request
                .conversation_state
                .as_ref()
                .map_or(0, |state| state.root_prompt_messages_json.len()),
        ));
    }
    // For results the interesting question is whether a correlation key survived.
    if let Some(agent::agent_client_message::Message::ExecClientMessage(exec)) = &message.message {
        out.push_str(&format!(
            "exec_numeric_id={}\nexec_id={}\n",
            exec.id, exec.exec_id
        ));
    }
    if let Some(agent::agent_client_message::Message::InteractionResponse(response)) =
        &message.message
    {
        out.push_str(&format!("interaction_numeric_id={}\n", response.id));
    }
    let _ = std::fs::write(&path, out);
    tracing::warn!(
        path = %path.display(),
        dropped = report.dropped.len(),
        kept = report.kept,
        "bidi: decoded AgentClientMessage with the tolerant path"
    );
}

/// The harness build the client is running, when its manifest is readable.
fn harness_version() -> Option<String> {
    let root: PathBuf = dirs::home_dir()?.join("AppData").join("Roaming").join("Cursor");
    harness_version_from(&root.join("agent-harness"))
}

fn harness_version_from(root: &Path) -> Option<String> {
    let current = std::fs::read_to_string(root.join("current")).ok()?;
    let generation = current.trim();
    let manifest = std::fs::read_to_string(root.join("versions").join(generation).join("manifest.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&manifest).ok()?;
    let version = value.get("mainVersion").and_then(|item| item.as_str())?;
    Some(format!("{version}/{generation}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode<M: Message>(message: &M) -> Vec<u8> {
        let mut buffer = Vec::new();
        message.encode(&mut buffer).unwrap();
        buffer
    }

    /// Emits one length-delimited field exactly as it would appear on the wire.
    fn wrap(number: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        push_varint(&mut out, u64::from((number << 3) | 2));
        push_varint(&mut out, payload.len() as u64);
        out.extend_from_slice(payload);
        out
    }

    fn state_with_turns(turns: Vec<Vec<u8>>) -> agent::ConversationStateStructure {
        agent::ConversationStateStructure {
            turns,
            summary: Some(b"{}".to_vec()),
            ..Default::default()
        }
    }

    fn run_request(state: agent::ConversationStateStructure) -> agent::AgentRunRequest {
        agent::AgentRunRequest {
            conversation_state: Some(state),
            conversation_id: Some("conv-1".into()),
            ..Default::default()
        }
    }

    fn client(request: agent::AgentRunRequest) -> agent::AgentClientMessage {
        agent::AgentClientMessage {
            message: Some(agent::agent_client_message::Message::RunRequest(request)),
        }
    }

    /// Builds a client message whose `conversation_state` carries one extra field the
    /// vendored schema does not understand — the shape of a harness schema drift.
    fn payload_with_drifted_state_field() -> Vec<u8> {
        let mut state = encode(&state_with_turns(vec![b"a".to_vec()]));
        // tag 12 (file_states, map<string, bytes>) with a payload that is not a map
        // entry: the strict decoder desyncs right here, as reported by the client.
        state.extend_from_slice(&[0x62, 0x03, 0xff, 0xff, 0x7f]);

        let mut request = wrap(1, &state);
        request.extend_from_slice(&wrap(5, b"conv-1"));
        wrap(1, &request)
    }

    #[test]
    fn recovers_an_exec_result_whose_id_became_a_uuid() {
        // Cursor 3.33 sends a UUID string where the vendored schema declares
        // `uint32 id`, so the strict decoder rejects the whole tool result.
        let uuid = b"865744c7-09f4-4482-a4bf-319f356d4f4a";
        assert_eq!(uuid.len(), 36);
        let mut exec = Vec::new();
        exec.extend_from_slice(&wrap(1, uuid));
        exec.extend_from_slice(&wrap(15, b"call_abc123"));
        let mut result = Vec::new();
        agent::ReadResult::default().encode(&mut result).unwrap();
        exec.extend_from_slice(&wrap(7, &result));
        let payload = wrap(2, &exec);

        assert!(
            agent::AgentClientMessage::decode(payload.as_slice()).is_err(),
            "the fixture must defeat the strict decoder"
        );

        let recovered = decode_client_message(&payload, "strict failed")
            .expect("the tool result must survive an unreadable id");
        let Some(agent::agent_client_message::Message::ExecClientMessage(exec)) = recovered.message
        else {
            panic!("expected an exec client message");
        };
        assert_eq!(exec.id, 0, "the numeric id cannot be recovered");
        assert_eq!(
            exec.exec_id, "call_abc123",
            "exec_id is the correlation key that survives the drift"
        );
        assert!(
            matches!(
                exec.message,
                Some(agent::exec_client_message::Message::ReadResult(_))
            ),
            "the tool result itself must be kept"
        );
    }

    #[test]
    fn keeps_everything_when_the_payload_is_intact() {
        let payload = encode(&client(run_request(state_with_turns(vec![b"a".to_vec()]))));
        let (message, report) = salvage::<agent::AgentClientMessage>(&payload, "client");
        let message = message.expect("intact payload must decode");
        assert!(report.dropped.is_empty(), "unexpected drops: {:?}", report.dropped);
        let Some(agent::agent_client_message::Message::RunRequest(request)) = message.message else {
            panic!("expected a run request");
        };
        assert_eq!(request.conversation_state.unwrap().turns.len(), 1);
    }

    #[test]
    fn drops_a_drifted_field_and_keeps_the_rest() {
        let payload = payload_with_drifted_state_field();

        let strict = agent::AgentClientMessage::decode(payload.as_slice());
        assert!(strict.is_err(), "the fixture must defeat the strict decoder");

        let recovered = decode_client_message(&payload, "strict failed")
            .expect("the tolerant path must recover the request");
        let Some(agent::agent_client_message::Message::RunRequest(request)) = recovered.message else {
            panic!("expected a run request");
        };
        assert_eq!(request.conversation_id.as_deref(), Some("conv-1"));
        let state = request.conversation_state.expect("state survives");
        assert_eq!(
            state.turns.len(),
            1,
            "the undecodable field must not take the rest of the state with it"
        );
        assert_eq!(state.summary.as_deref(), Some(b"{}".as_slice()));
    }

    #[test]
    fn keeps_history_when_one_turn_is_corrupt() {
        // A corrupt turn is still valid bytes for `repeated bytes`, so the state must
        // keep it; what matters is that neighbours are never lost.
        let state = state_with_turns(vec![b"first".to_vec(), vec![0xff, 0x07], b"third".to_vec()]);
        let payload = encode(&client(run_request(state)));
        let recovered = decode_client_message(&payload, "strict failed")
            .expect("intact payload still goes through the tolerant path");
        let Some(agent::agent_client_message::Message::RunRequest(request)) = recovered.message else {
            panic!("expected a run request");
        };
        assert_eq!(request.conversation_state.unwrap().turns.len(), 3);
    }

    #[test]
    fn keeps_a_request_whose_neighbour_field_drifted() {
        // Tag 12 inside AgentRunRequest is a bool we do not read; send it as bytes so
        // its own probe fails while the state field stays intact.
        let mut request = wrap(1, &encode(&state_with_turns(vec![b"a".to_vec()])));
        request.extend_from_slice(&wrap(12, b"drift"));
        let payload = wrap(1, &request);

        let recovered = decode_client_message(&payload, "strict failed")
            .expect("the state must survive a drifted neighbour");
        let Some(agent::agent_client_message::Message::RunRequest(request)) = recovered.message else {
            panic!("expected a run request");
        };
        assert_eq!(request.conversation_state.unwrap().turns.len(), 1);
    }

    #[test]
    fn returns_none_for_payloads_that_are_not_client_messages() {
        assert!(decode_client_message(b"not a protobuf at all", "strict failed").is_none());
        assert!(decode_client_message(&[0x07], "strict failed").is_none());
    }

    #[test]
    fn walk_reports_group_wire_types_instead_of_guessing() {
        let walked = walk(&[0x0b, 0x00]);
        assert!(walked.fields.is_empty());
        assert_eq!(walked.failed_at, Some(0));
    }

    #[test]
    fn varint_round_trips() {
        for value in [0u64, 1, 127, 128, 300, 16_383, u32::MAX as u64] {
            let mut buffer = Vec::new();
            push_varint(&mut buffer, value);
            let mut pos = 0;
            assert_eq!(read_varint(&buffer, &mut pos), Some(value));
            assert_eq!(pos, buffer.len());
        }
    }
}
