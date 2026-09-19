//! The chat as the user sees it, derived from agent events.
//!
//! [`ChatLog`] is a reducer: feed it [`Event`]s in order and it maintains the list
//! of things a chat screen draws. It lives here rather than in `mc-ui` so the
//! rules - where a streamed delta goes, what a retry throws away, when the agent
//! counts as busy - are plain functions with tests, not logic buried in rendering.
//!
//! It is deliberately *not* the conversation sent to the API. That is
//! [`crate::Session::transcript`], kept verbatim for the model; this is a view of
//! it for a human, free to summarise and reshape.

use serde_json::Value;
use tokio::sync::mpsc;

use crate::{
    cancel::Cancel,
    event::{Event, EventBus, EventRx},
    session::SessionId,
};

/// How a tool call is going.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolState {
    Running,
    Succeeded(String),
    Failed(String),
}

/// One thing on the chat screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatItem {
    User(String),
    /// Streamed text from the model; grows as deltas arrive.
    Assistant(String),
    /// Summarised reasoning; grows as deltas arrive.
    Thinking(String),
    Tool {
        id: String,
        name: String,
        /// The most informative single line of the input - for `bash`, the command.
        summary: String,
        state: ToolState,
    },
    /// A problem the user should see, already worded for a person.
    Error(String),
    /// Informational line from the app itself, not the model.
    Notice(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChatLog {
    pub items: Vec<ChatItem>,
    /// A turn is in progress. The input is disabled while this holds, so prompts
    /// cannot pile up behind a long build the user has forgotten about.
    pub busy: bool,
    /// Tokens the last request sent, as the API counted them. `None` until the
    /// first response - an estimate here would be worse than nothing, because
    /// the number's whole value is that it is the real one.
    pub context_tokens: Option<u32>,
}

impl ChatLog {
    /// Rebuild the visible conversation from a restored transcript.
    ///
    /// The transcript is what the model sees, so this is a translation back into
    /// what a person saw: user messages, assistant text, and tool calls with
    /// their results. Thinking is dropped - it is context for the model, and
    /// stale reasoning is noise when reopening yesterday's session.
    pub fn from_session(session: &crate::Session) -> Self {
        let mut log = Self::default();
        // Say up front that the transcript is not the whole conversation -
        // otherwise reopening a compacted session looks like messages went
        // missing.
        if session.summary.is_some() {
            log.notice(
                "Earlier messages in this conversation were summarized to fit the model's context.",
            );
        }
        for turn in &session.transcript {
            let blocks = turn.content.as_array().cloned().unwrap_or_default();
            for block in blocks {
                let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
                let text = |key: &str| {
                    block.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
                };
                match (turn.role.as_str(), kind) {
                    ("user", "text") => log.items.push(ChatItem::User(text("text"))),
                    ("assistant", "text") => log.items.push(ChatItem::Assistant(text("text"))),
                    ("assistant", "tool_use") => {
                        let name = text("name");
                        let input = block.get("input").cloned().unwrap_or_default();
                        log.items.push(ChatItem::Tool {
                            summary: summarise_input(&name, &input),
                            id: text("id"),
                            name,
                            state: ToolState::Running,
                        });
                    }
                    ("user", "tool_result") => {
                        let id = block
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let output = match block.get("content") {
                            Some(Value::String(s)) => s.clone(),
                            other => other.map(ToString::to_string).unwrap_or_default(),
                        };
                        let failed = block
                            .get("is_error")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        log.apply(Event::ToolCompleted {
                            session: session.id,
                            id,
                            is_error: failed,
                            output,
                        });
                    }
                    _ => {}
                }
            }
        }
        // A turn interrupted by the app closing leaves a tool still "running".
        for item in &mut log.items {
            if let ChatItem::Tool { state, .. } = item
                && *state == ToolState::Running
            {
                *state = ToolState::Failed("interrupted".into());
            }
        }
        log
    }

    pub fn notice(&mut self, text: impl Into<String>) {
        self.items.push(ChatItem::Notice(text.into()));
    }

    pub fn apply(&mut self, event: Event) {
        match event {
            Event::TurnStarted { prompt, .. } => {
                self.busy = true;
                self.items.push(ChatItem::User(prompt));
            }

            Event::TextDelta { text, .. } => match self.items.last_mut() {
                Some(ChatItem::Assistant(buffer)) => buffer.push_str(&text),
                _ => self.items.push(ChatItem::Assistant(text)),
            },

            Event::ThinkingDelta { text, .. } => match self.items.last_mut() {
                Some(ChatItem::Thinking(buffer)) => buffer.push_str(&text),
                _ => self.items.push(ChatItem::Thinking(text)),
            },

            Event::ToolRequested { id, name, input, .. } => {
                self.items.push(ChatItem::Tool {
                    summary: summarise_input(&name, &input),
                    id,
                    name,
                    state: ToolState::Running,
                });
            }

            Event::ToolCompleted {
                id,
                is_error,
                output,
                ..
            } => {
                // Search from the end: ids are unique, and the match is almost
                // always the most recent item.
                if let Some(ChatItem::Tool { state, .. }) = self
                    .items
                    .iter_mut()
                    .rev()
                    .find(|item| matches!(item, ChatItem::Tool { id: t, .. } if *t == id))
                {
                    *state = if is_error {
                        ToolState::Failed(output)
                    } else {
                        ToolState::Succeeded(output)
                    };
                }
            }

            Event::StreamRetrying { attempt, reason, .. } => {
                // Everything streamed since the last user message or tool call
                // belongs to the attempt that broke. Leaving it would show the
                // retried answer after a half-copy of itself.
                while matches!(
                    self.items.last(),
                    Some(ChatItem::Assistant(_) | ChatItem::Thinking(_))
                ) {
                    self.items.pop();
                }
                self.items.push(ChatItem::Notice(format!(
                    "Connection interrupted ({reason}). Retrying, attempt {attempt}."
                )));
            }

            Event::TurnEnded { stop_reason, .. } => {
                self.busy = false;
                // A clean end is not worth a line. Anything else is: `max_tokens`
                // in particular means the answer was cut off mid-thought.
                match stop_reason.as_str() {
                    "end_turn" | "tool_use" | "stop_sequence" => {}
                    "cancelled" => self.notice("Stopped."),
                    "max_tokens" => self.notice("The response hit the length limit and was cut off."),
                    other => self.notice(format!("Turn ended: {other}")),
                }
            }

            Event::Failed { message, .. } => {
                self.busy = false;
                // A tool that never completed is not "running" any more.
                for item in &mut self.items {
                    if let ChatItem::Tool { state, .. } = item
                        && *state == ToolState::Running
                    {
                        *state = ToolState::Failed("interrupted".into());
                    }
                }
                self.items.push(ChatItem::Error(message));
            }

            Event::SessionOpened { .. } => {
                // The transcript for the new chat is loaded by whoever owns the
                // library; all this log knows is that the old one is not it.
                *self = ChatLog::default();
            }

            Event::ContextUsage { input_tokens, .. } => {
                self.context_tokens = Some(input_tokens);
            }

            Event::Compacted { dropped, .. } => {
                // Say it plainly. The model is about to answer from a summary
                // rather than from what was actually said, and a person who
                // does not know that will read the next answer differently.
                self.notice(format!(
                    "Summarized {dropped} earlier message{} to stay within the model's context.                      The details are in the summary now, not the transcript.",
                    if dropped == 1 { "" } else { "s" }
                ));
            }

            Event::PtyOutput { .. } => {}
        }
    }
}

/// One line that tells the user what a tool call is doing.
fn summarise_input(name: &str, input: &serde_json::Value) -> String {
    let field = |key: &str| input.get(key).and_then(|v| v.as_str()).map(str::to_string);
    let summary = match name {
        "bash" => field("command"),
        "read_file" | "write_file" | "edit_file" => field("path"),
        "search" => field("pattern"),
        _ => None,
    }
    .unwrap_or_else(|| input.to_string());
    summary.lines().next().unwrap_or_default().to_string()
}

/// What the UI asks the worker to do.
///
/// One channel rather than three, so the order is the order the user pressed
/// things in: a prompt sent just before switching chats must not arrive after
/// the switch and land in the wrong transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentCommand {
    Prompt(String),
    /// Save the current chat and open this one.
    Open(SessionId),
    /// Save the current chat and start an empty one.
    NewChat,
}

/// What the UI holds to talk to the agent: send commands in, watch events out.
///
/// Cloneable and `Send`. Deliberately free of any `mc-agent` type, so `mc-ui`
/// can drive an agent without depending on the crate that talks to Claude.
#[derive(Debug, Clone)]
pub struct AgentHandle {
    commands: mpsc::UnboundedSender<AgentCommand>,
    bus: EventBus,
    cancel: Cancel,
}

impl AgentHandle {
    /// Create a handle and the receiving end the worker consumes.
    pub fn new(bus: EventBus) -> (Self, mpsc::UnboundedReceiver<AgentCommand>) {
        let (commands, rx) = mpsc::unbounded_channel();
        (Self { commands, bus, cancel: Cancel::new() }, rx)
    }

    /// The token the worker resets before each turn and honours during it.
    pub fn cancel_token(&self) -> Cancel {
        self.cancel.clone()
    }

    /// Stop the turn that is running, if any.
    pub fn stop(&self) {
        self.cancel.cancel();
    }

    /// Queue a prompt. Returns false if the worker is gone.
    pub fn submit(&self, prompt: impl Into<String>) -> bool {
        self.send(AgentCommand::Prompt(prompt.into()))
    }

    /// Open another chat. The current one is saved first.
    pub fn open_chat(&self, id: SessionId) -> bool {
        self.send(AgentCommand::Open(id))
    }

    /// Start an empty chat.
    pub fn new_chat(&self) -> bool {
        self.send(AgentCommand::NewChat)
    }

    pub fn send(&self, command: AgentCommand) -> bool {
        self.commands.send(command).is_ok()
    }

    pub fn subscribe(&self) -> EventRx {
        self.bus.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::session::SessionId;

    fn sid() -> SessionId {
        SessionId::new()
    }

    fn feed(log: &mut ChatLog, events: Vec<Event>) {
        for event in events {
            log.apply(event);
        }
    }

    #[test]
    fn a_turn_with_a_tool_call_reads_in_order() {
        let s = sid();
        let mut log = ChatLog::default();
        feed(&mut log, vec![
            Event::TurnStarted { session: s, prompt: "list files".into() },
            Event::ThinkingDelta { session: s, text: "I'll ".into() },
            Event::ThinkingDelta { session: s, text: "run ls".into() },
            Event::ToolRequested { session: s, id: "t1".into(), name: "bash".into(), input: json!({"command": "ls -la"}) },
            Event::ToolCompleted { session: s, id: "t1".into(), is_error: false, output: "a.txt".into() },
            Event::TextDelta { session: s, text: "One ".into() },
            Event::TextDelta { session: s, text: "file.".into() },
            Event::TurnEnded { session: s, stop_reason: "end_turn".into() },
        ]);

        assert_eq!(log.items, vec![
            ChatItem::User("list files".into()),
            ChatItem::Thinking("I'll run ls".into()),
            ChatItem::Tool {
                id: "t1".into(),
                name: "bash".into(),
                summary: "ls -la".into(),
                state: ToolState::Succeeded("a.txt".into()),
            },
            ChatItem::Assistant("One file.".into()),
        ]);
        assert!(!log.busy);
    }

    #[test]
    fn busy_holds_for_the_whole_turn() {
        let s = sid();
        let mut log = ChatLog::default();
        log.apply(Event::TurnStarted { session: s, prompt: "x".into() });
        assert!(log.busy);
        log.apply(Event::TextDelta { session: s, text: "y".into() });
        assert!(log.busy);
        log.apply(Event::TurnEnded { session: s, stop_reason: "end_turn".into() });
        assert!(!log.busy);
    }

    #[test]
    fn a_retry_discards_the_partial_answer_but_keeps_completed_tools() {
        let s = sid();
        let mut log = ChatLog::default();
        feed(&mut log, vec![
            Event::TurnStarted { session: s, prompt: "go".into() },
            Event::ToolRequested { session: s, id: "t1".into(), name: "bash".into(), input: json!({"command": "make"}) },
            Event::ToolCompleted { session: s, id: "t1".into(), is_error: false, output: "ok".into() },
            Event::ThinkingDelta { session: s, text: "half a thought".into() },
            Event::TextDelta { session: s, text: "half an ans".into() },
            Event::StreamRetrying { session: s, attempt: 1, reason: "reset".into() },
            Event::TextDelta { session: s, text: "The whole answer.".into() },
        ]);

        let texts: Vec<&ChatItem> = log.items.iter().collect();
        assert!(matches!(texts[1], ChatItem::Tool { .. }), "completed tool kept");
        assert!(matches!(texts[2], ChatItem::Notice(_)), "retry noted");
        assert_eq!(texts[3], &ChatItem::Assistant("The whole answer.".into()));
        assert!(
            !log.items.iter().any(|i| matches!(i, ChatItem::Assistant(t) if t.contains("half"))),
            "partial text must not survive a retry"
        );
    }

    #[test]
    fn a_failure_ends_the_turn_and_marks_unfinished_tools() {
        let s = sid();
        let mut log = ChatLog::default();
        feed(&mut log, vec![
            Event::TurnStarted { session: s, prompt: "build".into() },
            Event::ToolRequested { session: s, id: "t1".into(), name: "bash".into(), input: json!({"command": "cargo build"}) },
            Event::Failed { session: s, message: "connection lost".into() },
        ]);
        assert!(!log.busy);
        assert!(matches!(
            &log.items[1],
            ChatItem::Tool { state: ToolState::Failed(why), .. } if why == "interrupted"
        ));
        assert_eq!(log.items.last(), Some(&ChatItem::Error("connection lost".into())));
    }

    #[test]
    fn a_truncated_answer_is_called_out() {
        let s = sid();
        let mut log = ChatLog::default();
        log.apply(Event::TurnEnded { session: s, stop_reason: "max_tokens".into() });
        assert!(matches!(log.items.last(), Some(ChatItem::Notice(n)) if n.contains("cut off")));
    }

    #[test]
    fn tool_summaries_show_the_useful_field_on_one_line() {
        assert_eq!(summarise_input("bash", &json!({"command": "cd x &&\nmake"})), "cd x &&");
        assert_eq!(summarise_input("write_file", &json!({"path": "/root/a.c", "content": "..."})), "/root/a.c");
        assert_eq!(summarise_input("unknown", &json!({"a": 1})), r#"{"a":1}"#);
    }

    #[test]
    fn a_restored_transcript_reads_like_the_original_conversation() {
        use crate::{Project, Session, Turn};

        // Build the conversation the way a real turn does.
        let mut session = Session::new(Project { name: "p".into(), path: "/root".into() });
        session.push(Turn::user_text("list the files"));
        session.push(Turn::assistant(json!([
            { "type": "thinking", "thinking": "ls will do", "signature": "sig" },
            { "type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "ls -la"} }
        ])));
        session.push(Turn::tool_results(vec![json!({
            "type": "tool_result", "tool_use_id": "t1", "content": "a.txt\nb.txt", "is_error": false
        })]));
        session.push(Turn::assistant(json!([{ "type": "text", "text": "Two files." }])));

        let log = ChatLog::from_session(&session);
        assert_eq!(log.items, vec![
            ChatItem::User("list the files".into()),
            ChatItem::Tool {
                id: "t1".into(),
                name: "bash".into(),
                summary: "ls -la".into(),
                state: ToolState::Succeeded("a.txt\nb.txt".into()),
            },
            ChatItem::Assistant("Two files.".into()),
        ]);
        assert!(!log.busy, "a restored session is idle");
    }

    #[test]
    fn a_tool_left_running_by_a_crash_is_shown_as_interrupted() {
        use crate::{Project, Session, Turn};
        let mut session = Session::new(Project { name: "p".into(), path: "/root".into() });
        session.push(Turn::user_text("build it"));
        session.push(Turn::assistant(json!([
            { "type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "make"} }
        ])));

        let log = ChatLog::from_session(&session);
        assert!(matches!(
            &log.items[1],
            ChatItem::Tool { state: ToolState::Failed(why), .. } if why == "interrupted"
        ));
    }

    #[test]
    fn what_the_user_does_reaches_the_worker_in_the_order_they_did_it() {
        let (handle, mut rx) = AgentHandle::new(EventBus::default());
        let other = SessionId::new();
        assert!(handle.submit("hello"));
        assert!(handle.open_chat(other));
        assert!(handle.new_chat());

        // One channel, so a prompt sent just before a switch cannot arrive
        // after it and land in the wrong transcript.
        assert_eq!(rx.try_recv().unwrap(), AgentCommand::Prompt("hello".into()));
        assert_eq!(rx.try_recv().unwrap(), AgentCommand::Open(other));
        assert_eq!(rx.try_recv().unwrap(), AgentCommand::NewChat);
    }

    #[test]
    fn opening_a_chat_clears_what_was_on_screen() {
        let mut log = ChatLog::default();
        feed(&mut log, vec![
            Event::TurnStarted { session: sid(), prompt: "old chat".into() },
            Event::TextDelta { session: sid(), text: "old reply".into() },
        ]);
        assert!(!log.items.is_empty());

        log.apply(Event::SessionOpened { session: sid() });
        // Emptied, not merged: the transcript for the new chat is loaded from
        // the library by whoever owns it.
        assert_eq!(log, ChatLog::default());
    }
}
