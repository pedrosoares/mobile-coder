//! Projects and sessions.
//!
//! A *project* is a directory inside the sandbox rootfs. A *session* is one
//! conversation rooted at a project, plus the transcript that conversation has
//! accumulated.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A directory inside the rootfs that the agent works in.
///
/// `path` is a *guest* path - what it is called inside proot - not a host path.
/// Translating the two is `mc-sandbox`'s job, and keeping that boundary sharp is
/// what stops guest paths leaking into the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    pub path: PathBuf,
}

/// One entry in the conversation, stored in the API's own shape.
///
/// `content` is the raw content array, kept verbatim. This is deliberate: server
/// -side compaction returns blocks that must be echoed back unchanged on the next
/// request, and flattening them to a display string silently destroys that state.
/// The UI renders *from* this; it is not a rendering of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub role: String,
    pub content: serde_json::Value,
}

impl Turn {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: serde_json::json!([{ "type": "text", "text": text.into() }]),
        }
    }

    /// Store an assistant turn exactly as the API returned it.
    pub fn assistant(content: serde_json::Value) -> Self {
        Self {
            role: "assistant".into(),
            content,
        }
    }

    /// All `tool_result` blocks for one assistant turn go back as a *single*
    /// user message - splitting them across messages teaches the model to stop
    /// making parallel tool calls.
    pub fn tool_results(blocks: Vec<serde_json::Value>) -> Self {
        Self {
            role: "user".into(),
            content: serde_json::Value::Array(blocks),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub title: String,
    pub project: Project,
    pub transcript: Vec<Turn>,
}

impl Session {
    pub fn new(project: Project) -> Self {
        Self {
            id: SessionId::new(),
            title: "New session".into(),
            project,
            transcript: Vec::new(),
        }
    }

    pub fn push(&mut self, turn: Turn) {
        self.transcript.push(turn);
    }

    /// The `messages` array for the next request.
    pub fn messages(&self) -> Vec<serde_json::Value> {
        self.transcript
            .iter()
            .map(|t| serde_json::json!({ "role": t.role, "content": t.content }))
            .collect()
    }
}
