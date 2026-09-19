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

/// What an unused chat is called until something is said in it.
pub const DEFAULT_TITLE: &str = "New chat";

/// Placeholder titles, which are not titles: a chat saved with one of these is
/// named after its first message instead. The second is what sessions written
/// before chats were plural carry, and they are still on devices.
const PLACEHOLDER_TITLES: [&str; 2] = [DEFAULT_TITLE, "New session"];

/// A title has to fit a phone's list row.
const TITLE_CHARS: usize = 48;

/// The first line of `text`, short enough to read at a glance.
fn clip_title(text: &str) -> String {
    let line = text.lines().find(|line| !line.trim().is_empty()).unwrap_or("").trim();
    if line.chars().count() <= TITLE_CHARS {
        return line.to_string();
    }
    format!("{}…", line.chars().take(TITLE_CHARS - 1).collect::<String>().trim_end())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub title: String,
    pub project: Project,
    pub transcript: Vec<Turn>,
    /// What the turns that were dropped to stay inside the model's context
    /// said, in prose.
    ///
    /// It lives outside the transcript, and goes to the model as a second
    /// system block rather than as a message, because a transcript has a shape
    /// the API enforces - a `tool_result` must follow the `tool_use` it answers,
    /// and roles alternate. Splicing a summary in as a message breaks both;
    /// dropping a *prefix* of whole exchanges breaks neither.
    ///
    /// `default` so sessions written before compaction existed still load.
    #[serde(default)]
    pub summary: Option<String>,
}

impl Session {
    pub fn new(project: Project) -> Self {
        Self {
            id: SessionId::new(),
            title: DEFAULT_TITLE.into(),
            project,
            transcript: Vec::new(),
            summary: None,
        }
    }

    pub fn push(&mut self, turn: Turn) {
        self.transcript.push(turn);
    }

    /// What to call this chat in a list.
    ///
    /// Derived rather than stored: the first thing the user asked is what they
    /// will recognise it by, and asking them to name a conversation before
    /// having it is a chore nobody does.
    pub fn display_title(&self) -> String {
        if !self.title.is_empty() && !PLACEHOLDER_TITLES.contains(&self.title.as_str()) {
            return self.title.clone();
        }
        self.transcript
            .iter()
            .find(|turn| turn.role == "user")
            .and_then(|turn| turn.content.as_array()?.first()?.get("text")?.as_str())
            .map(clip_title)
            .unwrap_or_else(|| DEFAULT_TITLE.to_string())
    }

    /// The `messages` array for the next request.
    pub fn messages(&self) -> Vec<serde_json::Value> {
        self.transcript
            .iter()
            .map(|t| serde_json::json!({ "role": t.role, "content": t.content }))
            .collect()
    }

    /// Where the transcript can be cut so that everything before it is dropped,
    /// keeping at least `keep_recent` turns.
    ///
    /// Not just any index. A `tool_result` is only valid directly after the
    /// `tool_use` it answers, so a cut in the middle of an exchange produces a
    /// request the API rejects outright - and a conversation that can never be
    /// sent again is worse than one that is too long. The only safe place to cut
    /// is where a user *typed* something: everything before it is complete.
    ///
    /// `None` when there is nothing worth dropping.
    pub fn compaction_cut(&self, keep_recent: usize) -> Option<usize> {
        let latest = self.transcript.len().checked_sub(keep_recent)?;
        (1..=latest).rev().find(|&i| self.is_prompt(i))
    }

    /// Whether turn `i` is a message the user typed, rather than tool results
    /// sent back under the user role.
    fn is_prompt(&self, i: usize) -> bool {
        let Some(turn) = self.transcript.get(i) else { return false };
        turn.role == "user"
            && turn
                .content
                .as_array()
                .and_then(|blocks| blocks.first())
                .and_then(|block| block.get("type"))
                .and_then(serde_json::Value::as_str)
                == Some("text")
    }

    /// Drop everything before `cut`, remembering it as `summary`.
    ///
    /// Returns how many turns were dropped. Summaries chain: the new one is
    /// expected to cover the old one, which the caller passes in when asking for
    /// it, so what is kept here replaces rather than appends.
    pub fn compact(&mut self, cut: usize, summary: String) -> usize {
        let dropped = cut.min(self.transcript.len());
        self.transcript.drain(..dropped);
        self.summary = Some(summary);
        dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> Project {
        Project { name: "demo".into(), path: PathBuf::from("/root") }
    }

    /// A conversation with tool use in it: prompt, tool call, results, answer.
    fn exchange(prompt: &str) -> Vec<Turn> {
        vec![
            Turn::user_text(prompt),
            Turn::assistant(serde_json::json!([
                { "type": "tool_use", "id": "t1", "name": "bash", "input": {} }
            ])),
            Turn::tool_results(vec![serde_json::json!({
                "type": "tool_result", "tool_use_id": "t1", "content": "ok"
            })]),
            Turn::assistant(serde_json::json!([{ "type": "text", "text": "done" }])),
        ]
    }

    fn session_of(exchanges: usize) -> Session {
        let mut session = Session::new(project());
        for i in 0..exchanges {
            session.transcript.extend(exchange(&format!("prompt {i}")));
        }
        session
    }

    #[test]
    fn a_chat_is_named_after_the_first_thing_asked_in_it() {
        let mut session = Session::new(project());
        assert_eq!(session.display_title(), DEFAULT_TITLE, "nothing said yet");

        session.push(Turn::user_text("fix the parser crash on empty input"));
        assert_eq!(session.display_title(), "fix the parser crash on empty input");

        // A session written before chats were plural carries the old default,
        // which is a placeholder and not a name.
        session.title = "New session".into();
        assert_eq!(session.display_title(), "fix the parser crash on empty input");

        // A title someone chose is kept.
        session.title = "parser work".into();
        assert_eq!(session.display_title(), "parser work");
    }

    #[test]
    fn a_long_first_message_is_cut_to_fit_a_list_row() {
        let mut session = Session::new(project());
        session.push(Turn::user_text(format!("{}\nsecond line", "x".repeat(100))));
        let title = session.display_title();
        assert_eq!(title.chars().count(), TITLE_CHARS);
        assert!(title.ends_with('…'));
        assert!(!title.contains("second line"), "one line, not the whole message");
    }

    #[test]
    fn a_cut_lands_on_a_typed_message_never_inside_an_exchange() {
        let session = session_of(3);
        // Four turns per exchange, so prompts are at 0, 4, 8.
        assert_eq!(session.compaction_cut(4), Some(8));
        assert_eq!(session.compaction_cut(6), Some(4));
    }

    #[test]
    fn a_cut_never_orphans_a_tool_result() {
        let session = session_of(4);
        for keep in 1..session.transcript.len() {
            let Some(cut) = session.compaction_cut(keep) else { continue };
            let first = &session.transcript[cut];
            assert_eq!(first.role, "user", "keeping from {cut} with keep={keep}");
            let kind = first.content[0]["type"].as_str();
            assert_eq!(kind, Some("text"), "a tool_result would have no tool_use");
        }
    }

    #[test]
    fn nothing_to_drop_is_not_an_error() {
        // The first turn is never dropped on its own: cutting at 0 drops
        // nothing, and there is no earlier exchange to summarize.
        assert_eq!(session_of(1).compaction_cut(4), None);
        assert_eq!(Session::new(project()).compaction_cut(4), None);
        // Asking to keep more than exists.
        assert_eq!(session_of(2).compaction_cut(100), None);
    }

    #[test]
    fn compacting_keeps_the_tail_and_remembers_the_rest() {
        let mut session = session_of(3);
        let cut = session.compaction_cut(4).expect("something to drop");
        let dropped = session.compact(cut, "they set up a C project".into());

        assert_eq!(dropped, 8);
        assert_eq!(session.transcript.len(), 4);
        assert_eq!(session.transcript[0].content[0]["text"], "prompt 2");
        assert_eq!(session.summary.as_deref(), Some("they set up a C project"));
        // And what is left is still a valid request: it opens with a prompt.
        assert_eq!(session.messages()[0]["role"], "user");
    }

    #[test]
    fn a_session_written_before_compaction_still_loads() {
        let old = serde_json::json!({
            "id": SessionId::new(),
            "title": "old",
            "project": { "name": "demo", "path": "/root" },
            "transcript": [],
        });
        let session: Session = serde_json::from_value(old).expect("loads without a summary");
        assert!(session.summary.is_none());
    }
}
