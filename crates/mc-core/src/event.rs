//! A broadcast bus carrying streaming deltas from the agent to the UI.
//!
//! Broadcast rather than mpsc so that several views (transcript, status line,
//! token meter) can watch the same turn without the agent knowing they exist.

use tokio::sync::broadcast;

use crate::session::SessionId;

/// Everything the UI can learn about work in progress.
#[derive(Debug, Clone)]
pub enum Event {
    /// A turn started. Carries the prompt, so every view shows the user's message
    /// no matter where it came from (the chat input, an adb intent, a script).
    TurnStarted { session: SessionId, prompt: String },
    /// A chunk of assistant text. These arrive at speed - the UI must not
    /// re-layout the whole transcript per delta.
    TextDelta { session: SessionId, text: String },
    /// A chunk of summarized reasoning, when `thinking.display` asks for it.
    ThinkingDelta { session: SessionId, text: String },
    /// The model asked for a tool. Emitted before execution so the UI can show
    /// it pending, and so an approval gate has something to attach to.
    ToolRequested {
        session: SessionId,
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// A tool finished. `is_error` mirrors the `is_error` we send back to the API.
    ToolCompleted {
        session: SessionId,
        id: String,
        is_error: bool,
        output: String,
    },
    /// Raw bytes from an interactive PTY (the user's terminal, not a tool call).
    PtyOutput { session: SessionId, bytes: Vec<u8> },
    /// A response stream broke before completing and is being retried.
    ///
    /// Any text or thinking deltas received since the last `TurnStarted` or
    /// `ToolCompleted` belong to the failed attempt: the UI must discard them,
    /// or the retried response is displayed after a half-copy of itself.
    StreamRetrying {
        session: SessionId,
        attempt: u32,
        reason: String,
    },
    /// How much of the model's context the last request used, as the API
    /// counted it. Emitted per response, so the meter moves as tool output
    /// piles up within a single turn rather than only between turns.
    ContextUsage {
        session: SessionId,
        input_tokens: u32,
        output_tokens: u32,
    },
    /// Older turns were summarized and dropped to stay inside the context.
    /// Worth telling the user: the model no longer has those messages verbatim.
    Compacted { session: SessionId, dropped: usize },
    /// A different chat is now the current one - opened, or newly made. The UI
    /// rebuilds its transcript from the library rather than being sent it:
    /// a whole conversation through a broadcast channel, to every subscriber,
    /// for something that happens on a tap, is a waste.
    SessionOpened { session: SessionId },
    /// The turn ended. `stop_reason` is the API's, verbatim.
    TurnEnded {
        session: SessionId,
        stop_reason: String,
    },
    /// Something failed. Carries a message already fit to show a human.
    Failed { session: SessionId, message: String },
}

pub type EventRx = broadcast::Receiver<Event>;

/// Cloneable handle to the bus. Dropping every sender closes the channel.
#[derive(Debug, Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    pub fn subscribe(&self) -> EventRx {
        self.tx.subscribe()
    }

    /// Publish an event. A send with no subscribers is not an error - during
    /// startup, and on Android after the activity is backgrounded, there may
    /// legitimately be nobody listening.
    pub fn emit(&self, event: Event) {
        let _ = self.tx.send(event);
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(1024)
    }
}
