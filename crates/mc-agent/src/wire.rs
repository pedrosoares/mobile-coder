//! The Messages API wire format, and the stream accumulator that rebuilds a
//! message from its deltas.
//!
//! There is no official Anthropic SDK for Rust, so this is ours to own. The
//! accumulator is the part worth reading: assembling `content` correctly is what
//! keeps tool calls and compaction state intact across turns.

use serde_json::{Value, json};

/// A message reassembled from a stream.
#[derive(Debug, Clone, Default)]
pub struct StreamedMessage {
    /// The content array, in the API's own shape.
    ///
    /// Stored verbatim and echoed back verbatim. Compaction blocks and thinking
    /// blocks are load-bearing state: reducing this to a display string is the
    /// classic way to silently break a long session.
    pub content: Vec<Value>,
    pub stop_reason: String,
    pub stop_details: Option<Value>,
    pub usage: Value,
}

impl StreamedMessage {
    /// Tool calls the model is waiting on.
    pub fn tool_uses(&self) -> Vec<ToolUse> {
        self.content
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
            .filter_map(|b| {
                Some(ToolUse {
                    id: b.get("id")?.as_str()?.to_string(),
                    name: b.get("name")?.as_str()?.to_string(),
                    input: b.get("input").cloned().unwrap_or_else(|| json!({})),
                })
            })
            .collect()
    }

    pub fn content_value(&self) -> Value {
        Value::Array(self.content.clone())
    }
}

#[derive(Debug, Clone)]
pub struct ToolUse {
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// One content block being built up from deltas.
#[derive(Debug, Clone)]
enum Block {
    Text {
        text: String,
    },
    /// Thinking blocks carry a signature that must survive the round trip.
    Thinking {
        text: String,
        signature: String,
    },
    ToolUse {
        id: String,
        name: String,
        partial_json: String,
    },
    /// Anything we do not model - compaction blocks, server tool results, block
    /// types added after this was written. Kept exactly as received rather than
    /// dropped, because dropping one corrupts the conversation in ways that only
    /// show up several turns later.
    Passthrough(Value),
}

/// Rebuilds a message from `content_block_*` / `message_*` events.
#[derive(Debug, Default)]
pub struct StreamAccumulator {
    blocks: Vec<Option<Block>>,
    message: StreamedMessage,
}

/// What the caller should do with an event, beyond accumulating it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Emitted {
    Nothing,
    Text(String),
    Thinking(String),
    /// A tool_use block is complete enough to show as pending.
    ToolStarted { id: String, name: String },
    Done,
}

impl StreamAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one decoded SSE `data:` payload.
    pub fn push(&mut self, event: &Value) -> Emitted {
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                if let Some(usage) = event.pointer("/message/usage") {
                    self.message.usage = usage.clone();
                }
                Emitted::Nothing
            }

            Some("content_block_start") => {
                let index = index_of(event);
                let raw = event.get("content_block").cloned().unwrap_or(json!({}));
                let block = match raw.get("type").and_then(Value::as_str) {
                    Some("text") => Block::Text {
                        text: raw
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    },
                    Some("thinking") => Block::Thinking {
                        text: raw
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        signature: String::new(),
                    },
                    Some("tool_use") => Block::ToolUse {
                        id: raw
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        name: raw
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        partial_json: String::new(),
                    },
                    _ => Block::Passthrough(raw),
                };

                let started = match &block {
                    Block::ToolUse { id, name, .. } => Some(Emitted::ToolStarted {
                        id: id.clone(),
                        name: name.clone(),
                    }),
                    _ => None,
                };
                self.put(index, block);
                started.unwrap_or(Emitted::Nothing)
            }

            Some("content_block_delta") => {
                let index = index_of(event);
                let delta = event.get("delta").cloned().unwrap_or(json!({}));
                let kind = delta.get("type").and_then(Value::as_str).unwrap_or("");

                match (kind, self.blocks.get_mut(index).and_then(Option::as_mut)) {
                    ("text_delta", Some(Block::Text { text })) => {
                        let chunk = str_field(&delta, "text");
                        text.push_str(&chunk);
                        Emitted::Text(chunk)
                    }
                    ("thinking_delta", Some(Block::Thinking { text, .. })) => {
                        let chunk = str_field(&delta, "thinking");
                        text.push_str(&chunk);
                        Emitted::Thinking(chunk)
                    }
                    ("signature_delta", Some(Block::Thinking { signature, .. })) => {
                        signature.push_str(&str_field(&delta, "signature"));
                        Emitted::Nothing
                    }
                    ("input_json_delta", Some(Block::ToolUse { partial_json, .. })) => {
                        partial_json.push_str(&str_field(&delta, "partial_json"));
                        Emitted::Nothing
                    }
                    _ => Emitted::Nothing,
                }
            }

            Some("message_delta") => {
                if let Some(reason) = event.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.message.stop_reason = reason.to_string();
                }
                if let Some(details) = event.pointer("/delta/stop_details")
                    && !details.is_null()
                {
                    self.message.stop_details = Some(details.clone());
                }
                if let Some(usage) = event.get("usage") {
                    merge(&mut self.message.usage, usage);
                }
                Emitted::Nothing
            }

            Some("message_stop") => Emitted::Done,
            _ => Emitted::Nothing,
        }
    }

    fn put(&mut self, index: usize, block: Block) {
        if self.blocks.len() <= index {
            self.blocks.resize(index + 1, None);
        }
        self.blocks[index] = Some(block);
    }

    /// Finish, producing the message to append to the transcript.
    pub fn finish(mut self) -> StreamedMessage {
        self.message.content = self
            .blocks
            .into_iter()
            .flatten()
            .map(|block| match block {
                Block::Text { text } => json!({ "type": "text", "text": text }),
                Block::Thinking { text, signature } => {
                    json!({ "type": "thinking", "thinking": text, "signature": signature })
                }
                Block::ToolUse {
                    id,
                    name,
                    partial_json,
                } => {
                    // Always parse - never string-match the serialized input.
                    // Escaping in tool inputs is not guaranteed to be stable.
                    let input = serde_json::from_str::<Value>(&partial_json)
                        .unwrap_or_else(|_| json!({}));
                    json!({ "type": "tool_use", "id": id, "name": name, "input": input })
                }
                Block::Passthrough(v) => v,
            })
            .collect();
        self.message
    }
}

fn index_of(event: &Value) -> usize {
    event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize
}

fn str_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn merge(target: &mut Value, incoming: &Value) {
    match (target.as_object_mut(), incoming.as_object()) {
        (Some(t), Some(i)) => {
            for (k, v) in i {
                t.insert(k.clone(), v.clone());
            }
        }
        _ => *target = incoming.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(acc: &mut StreamAccumulator, events: &[Value]) {
        for e in events {
            acc.push(e);
        }
    }

    #[test]
    fn text_deltas_concatenate_in_order() {
        let mut acc = StreamAccumulator::new();
        feed(&mut acc, &[
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
        ]);
        let msg = acc.finish();
        assert_eq!(msg.content[0]["text"], "Hello");
        assert_eq!(msg.stop_reason, "end_turn");
    }

    #[test]
    fn tool_input_json_is_parsed_not_string_matched() {
        let mut acc = StreamAccumulator::new();
        feed(&mut acc, &[
            json!({"type":"content_block_start","index":0,
                   "content_block":{"type":"tool_use","id":"tu_1","name":"bash","input":{}}}),
            json!({"type":"content_block_delta","index":0,
                   "delta":{"type":"input_json_delta","partial_json":"{\"command\":"}}),
            json!({"type":"content_block_delta","index":0,
                   "delta":{"type":"input_json_delta","partial_json":"\"ls -la\"}"}}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}),
        ]);
        let msg = acc.finish();
        let uses = msg.tool_uses();
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].name, "bash");
        assert_eq!(uses[0].input["command"], "ls -la");
    }

    #[test]
    fn unknown_block_types_survive_the_round_trip() {
        // A block type we do not model must come back out unchanged - this is
        // what keeps compaction state intact.
        let mut acc = StreamAccumulator::new();
        let exotic = json!({"type":"compaction","id":"cmp_1","opaque":"xyz"});
        feed(&mut acc, &[
            json!({"type":"content_block_start","index":0,"content_block": exotic}),
            json!({"type":"message_stop"}),
        ]);
        assert_eq!(acc.finish().content[0], exotic);
    }

    #[test]
    fn thinking_signature_is_retained() {
        let mut acc = StreamAccumulator::new();
        feed(&mut acc, &[
            json!({"type":"content_block_start","index":0,
                   "content_block":{"type":"thinking","thinking":""}}),
            json!({"type":"content_block_delta","index":0,
                   "delta":{"type":"thinking_delta","thinking":"weighing"}}),
            json!({"type":"content_block_delta","index":0,
                   "delta":{"type":"signature_delta","signature":"sig123"}}),
        ]);
        let msg = acc.finish();
        assert_eq!(msg.content[0]["thinking"], "weighing");
        assert_eq!(msg.content[0]["signature"], "sig123");
    }

    #[test]
    fn blocks_keep_their_index_order_even_when_started_out_of_order() {
        let mut acc = StreamAccumulator::new();
        feed(&mut acc, &[
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":"second"}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"first"}}),
        ]);
        let msg = acc.finish();
        assert_eq!(msg.content[0]["text"], "first");
        assert_eq!(msg.content[1]["text"], "second");
    }
}
