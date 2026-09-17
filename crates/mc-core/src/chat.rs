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

use tokio::sync::mpsc;

use crate::event::{Event, EventBus, EventRx};

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
}

impl ChatLog {
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

/// What the UI holds to talk to the agent: send prompts in, watch events out.
///
/// Cloneable and `Send`. Deliberately free of any `mc-agent` type, so `mc-ui`
/// can drive an agent without depending on the crate that talks to Claude.
#[derive(Debug, Clone)]
pub struct AgentHandle {
    prompts: mpsc::UnboundedSender<String>,
    bus: EventBus,
}

impl AgentHandle {
    /// Create a handle and the receiving end the worker consumes.
    pub fn new(bus: EventBus) -> (Self, mpsc::UnboundedReceiver<String>) {
        let (prompts, rx) = mpsc::unbounded_channel();
        (Self { prompts, bus }, rx)
    }

    /// Queue a prompt. Returns false if the worker is gone.
    pub fn submit(&self, prompt: impl Into<String>) -> bool {
        self.prompts.send(prompt.into()).is_ok()
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
    fn submitting_reaches_the_worker() {
        let (handle, mut rx) = AgentHandle::new(EventBus::default());
        assert!(handle.submit("hello"));
        assert_eq!(rx.try_recv().unwrap(), "hello");
    }
}
