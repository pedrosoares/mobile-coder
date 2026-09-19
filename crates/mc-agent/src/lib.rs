//! The Claude agent loop.
//!
//! There is no first-party Anthropic SDK for Rust, so this crate owns the wire
//! format: `reqwest` for transport, `eventsource-stream` for SSE framing, and
//! [`wire::StreamAccumulator`] for rebuilding messages from deltas.

pub mod tools;
pub mod worker;
pub mod wire;

use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use mc_core::{Cancel, Event, EventBus, Session, SessionId, Turn};
use mc_sandbox::Sandbox;
use serde_json::{Value, json};

use crate::wire::{Emitted, StreamAccumulator, StreamedMessage};

pub const API_URL: &str = "https://api.anthropic.com/v1/messages";
pub const DEFAULT_MODEL: &str = "claude-opus-5";
pub const API_VERSION: &str = "2023-06-01";

/// Stops a misbehaving loop from billing indefinitely. Generous enough that real
/// work never reaches it.
const MAX_TOOL_ROUNDS: usize = 64;

/// Retries for a round that failed in transit: a stream that closed before
/// `message_stop`, or a request that could not be delivered at all.
///
/// Worth doing on a phone, where connectivity blips are routine - measured on a
/// Galaxy Z Fold6 with Samsung's "switch to mobile data" enabled, where a heavy
/// download inside the guest was followed by `error sending request` to a LAN
/// endpoint. Safe, because tools run only after a *complete* message arrives: a
/// failed round has executed nothing, so repeating it has no side effects.
///
/// Backoff totals ~15s, long enough to ride out a Wi-Fi/cellular handover.
const MAX_STREAM_RETRIES: u32 = 4;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("http transport failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("api returned {status}: {body}")]
    Api { status: u16, body: String },
    #[error("malformed event from the api: {0}")]
    Decode(#[from] serde_json::Error),
    /// The stream stopped before `message_stop`. Carries why, because "it ended"
    /// alone is undiagnosable - a dropped connection, a decode failure and a
    /// server that simply closed early all need different fixes.
    #[error("stream ended before the message finished: {0}")]
    Truncated(String),
    /// The server sent an `error` event mid-stream (e.g. `overloaded_error`).
    #[error("server error mid-stream: {0}")]
    StreamError(String),
    #[error("the model declined this request{}", .0.as_ref().map(|c| format!(" ({c})")).unwrap_or_default())]
    Refused(Option<String>),
    #[error("tool loop exceeded {MAX_TOOL_ROUNDS} rounds without settling")]
    Runaway,
    /// The user pressed stop. Not a failure: the turn ends where it stood.
    #[error("stopped")]
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub api_key: String,
    /// Full URL of the Messages endpoint, including `/v1/messages`.
    pub base_url: String,
    pub model: String,
    pub max_tokens: u32,
    /// `low` | `medium` | `high` | `xhigh` | `max`.
    ///
    /// The main cost/quality dial, and on a phone - cellular data, a battery, a
    /// thermal budget - the user has a real interest in turning it down. It
    /// belongs in settings, not hardcoded.
    pub effort: String,
    /// Show summarized reasoning. With the default (`omitted`) a long turn reads
    /// as a dead pause, which on a handset feels like a hang.
    pub show_thinking: bool,
    pub system: String,
    /// Summarize and drop older turns once a request's input passes this.
    ///
    /// Well under any current model's window, because the point is to compact
    /// *before* a request is refused, not after. A refusal is recoverable (see
    /// [`is_too_long`]) but costs a round trip and leaves the user waiting.
    pub compact_at_tokens: u32,
    /// How many turns to keep verbatim when compacting. Enough that the model
    /// still has the work in progress in full, not just a description of it.
    pub keep_recent_turns: usize,
}

impl AgentConfig {
    /// Build a config from the conventional Anthropic environment variables.
    ///
    /// | Variable             | Default                     |
    /// |----------------------|-----------------------------|
    /// | `ANTHROPIC_API_KEY`  | required for the real API   |
    /// | `ANTHROPIC_BASE_URL` | `https://api.anthropic.com` |
    /// | `ANTHROPIC_MODEL`    | `claude-opus-5`             |
    ///
    /// `ANTHROPIC_BASE_URL` is a server *root* (`http://localhost:1234`), matching
    /// what the official SDKs expect - `/v1/messages` is appended here. Pointing
    /// it at any server that speaks the Anthropic Messages API works, e.g. LM
    /// Studio. A key is not required when a custom base URL is set, since local
    /// servers usually do not check one.
    pub fn from_env() -> Result<Self, String> {
        let base = std::env::var("ANTHROPIC_BASE_URL").ok().filter(|v| !v.trim().is_empty());
        let key = std::env::var("ANTHROPIC_API_KEY").ok().filter(|v| !v.trim().is_empty());

        let api_key = match (&key, &base) {
            (Some(key), _) => key.clone(),
            // Local servers ignore the header, but it still has to be present.
            (None, Some(_)) => "local".to_string(),
            (None, None) => {
                return Err(
                    "ANTHROPIC_API_KEY is not set (or set ANTHROPIC_BASE_URL for a local server)"
                        .into(),
                );
            }
        };

        let mut config = Self::new(api_key);
        if let Some(base) = base {
            config.base_url = messages_url(&base);
        }
        if let Some(model) = std::env::var("ANTHROPIC_MODEL").ok().filter(|v| !v.trim().is_empty()) {
            config.model = model;
        }
        // The default suits a 200k window. A local model with 32k needs a much
        // lower one, and there is no way to ask a server what it will accept.
        if let Some(at) = std::env::var("MC_COMPACT_AT_TOKENS").ok().and_then(|v| v.trim().parse().ok())
        {
            config.compact_at_tokens = at;
        }
        Ok(config)
    }

    /// Whether this points at Anthropic's API rather than a compatible server.
    pub fn is_anthropic(&self) -> bool {
        self.base_url.starts_with("https://api.anthropic.com")
    }

    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: API_URL.to_string(),
            model: DEFAULT_MODEL.to_string(),
            max_tokens: 64_000,
            effort: "high".to_string(),
            show_thinking: true,
            system: default_system_prompt(),
            compact_at_tokens: 100_000,
            keep_recent_turns: 8,
        }
    }
}

/// Turn a server root into the Messages endpoint URL.
///
/// Tolerates the mistakes people actually make with this variable: a trailing
/// slash, and a URL that already includes `/v1` or `/v1/messages`.
pub fn messages_url(base: &str) -> String {
    let base = base.trim().trim_end_matches('/');
    let base = base.strip_suffix("/v1/messages").unwrap_or(base);
    let base = base.strip_suffix("/v1").unwrap_or(base);
    format!("{base}/v1/messages")
}

pub fn default_system_prompt() -> String {
    // Kept byte-stable: this sits inside the cached prefix, so interpolating
    // anything per-request (a timestamp, a session id) would invalidate the cache
    // on every single turn.
    "You are the coding agent inside mobile-coder, a development environment that runs \
     entirely on an Android phone. Your tools execute in a Linux sandbox on the device.\n\n\
     The device is battery-powered and thermally limited. Prefer incremental builds over \
     clean rebuilds, and avoid gratuitously long-running commands.\n\n\
     Use `edit_file` rather than rewriting whole files, so changes stay reviewable on a \
     small screen."
        .to_string()
}

pub struct Agent {
    http: reqwest::Client,
    config: AgentConfig,
    sandbox: Sandbox,
    bus: EventBus,
}

impl Agent {
    pub fn new(config: AgentConfig, sandbox: Sandbox, bus: EventBus) -> Self {
        Self {
            http: mc_core::http::client(),
            config,
            sandbox,
            bus,
        }
    }

    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    /// Run one user turn to completion, including any tool rounds it triggers.
    pub async fn run_turn(
        &self,
        session: &mut Session,
        user_text: &str,
    ) -> Result<(), AgentError> {
        self.run_turn_cancellable(session, user_text, &Cancel::new()).await
    }

    /// As [`Self::run_turn`], stoppable while it runs.
    ///
    /// Stopping is checked in all three places a turn can be waiting: streaming
    /// the response, running a tool, and between rounds. The transcript keeps
    /// whatever completed, so the conversation stays valid and can continue.
    pub async fn run_turn_cancellable(
        &self,
        session: &mut Session,
        user_text: &str,
        cancel: &Cancel,
    ) -> Result<(), AgentError> {
        let id = session.id;
        session.push(Turn::user_text(user_text));
        self.bus.emit(Event::TurnStarted {
            session: id,
            prompt: user_text.to_string(),
        });

        let mut compacted_here = false;
        for _ in 0..MAX_TOOL_ROUNDS {
            if cancel.is_cancelled() {
                return self.finish_cancelled(session);
            }

            let message = match self.stream_with_retry(session, cancel).await {
                Ok(message) => message,
                Err(AgentError::Cancelled) => return self.finish_cancelled(session),
                // The request was refused for length. Without this the session
                // is finished: every later turn sends the same oversized
                // transcript and fails identically, and on a phone there is no
                // way to edit it. Compact and try the round again - once, so a
                // transcript that is too long even after compacting fails
                // honestly instead of looping.
                Err(e) if is_too_long(&e) && !compacted_here => {
                    compacted_here = true;
                    tracing::warn!("the request was refused for length; compacting");
                    if self.compact_now(session).await == 0 {
                        return Err(e);
                    }
                    continue;
                }
                Err(e) => return Err(e),
            };

            // Tell the UI how full the context is before anything else in this
            // round can fail: the number is useful even when the turn is not.
            self.emit_usage(session.id, &message.usage);

            // Append the content verbatim, before inspecting it. Thinking blocks
            // and compaction state have to survive the round trip intact.
            session.push(Turn::assistant(message.content_value()));

            // A refusal arrives as HTTP 200, so it is only visible here.
            if message.stop_reason == "refusal" {
                let category = message
                    .stop_details
                    .as_ref()
                    .and_then(|d| d.get("category"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                self.bus.emit(Event::Failed {
                    session: id,
                    message: "the model declined this request".into(),
                });
                return Err(AgentError::Refused(category));
            }

            let tool_uses = message.tool_uses();
            if tool_uses.is_empty() {
                self.bus.emit(Event::TurnEnded {
                    session: id,
                    stop_reason: message.stop_reason,
                });
                // Between turns, never inside one: compaction is another request
                // to the model, and the user is waiting on this answer.
                self.compact_if_full(session, input_tokens(&message.usage)).await;
                return Ok(());
            }

            // Every result for this assistant turn goes back in ONE user
            // message. Splitting them across messages trains the model out of
            // making parallel tool calls.
            //
            // These run sequentially for now; running independent calls
            // concurrently is a straightforward later win, and the single-message
            // reply shape below is what makes it safe to do.
            let mut results = Vec::with_capacity(tool_uses.len());
            let mut stopped = false;
            for call in tool_uses {
                self.bus.emit(Event::ToolRequested {
                    session: id,
                    id: call.id.clone(),
                    name: call.name.clone(),
                    input: call.input.clone(),
                });

                // Every pending call still needs a tool_result, or the
                // conversation is structurally invalid - so after a stop the
                // rest are answered rather than skipped.
                let outcome = if stopped || cancel.is_cancelled() {
                    stopped = true;
                    crate::tools::ToolOutcome::stopped()
                } else {
                    tools::execute_cancellable(&self.sandbox, &call.name, &call.input, cancel).await
                };

                self.bus.emit(Event::ToolCompleted {
                    session: id,
                    id: call.id.clone(),
                    is_error: outcome.is_error,
                    output: outcome.text.clone(),
                });

                // Even a failure returns a tool_result - a dropped block leaves
                // the conversation structurally invalid.
                results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": call.id,
                    "content": outcome.text,
                    "is_error": outcome.is_error,
                }));
            }

            session.push(Turn::tool_results(results));

            if stopped {
                return self.finish_cancelled(session);
            }
        }

        Err(AgentError::Runaway)
    }

    /// Publish how much of the context the last request used.
    fn emit_usage(&self, session: SessionId, usage: &Value) {
        let Some(input_tokens) = input_tokens(usage) else { return };
        self.bus.emit(Event::ContextUsage {
            session,
            input_tokens,
            output_tokens: usage.get("output_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
        });
    }

    /// Compact if the last request was large enough to be worth it.
    async fn compact_if_full(&self, session: &mut Session, used: Option<u32>) {
        if used.is_none_or(|used| used < self.config.compact_at_tokens) {
            return;
        }
        self.compact_now(session).await;
    }

    /// Summarize the older part of the transcript and drop it.
    ///
    /// Returns how many turns went. Zero means nothing could be dropped - a
    /// single enormous exchange, most likely - and the caller has to treat that
    /// as "this did not help" rather than as success.
    ///
    /// A failure to summarize is deliberately not fatal and not shown: the turn
    /// the user asked for already succeeded, and the next one will try again.
    async fn compact_now(&self, session: &mut Session) -> usize {
        let Some(cut) = session.compaction_cut(self.config.keep_recent_turns) else {
            return 0;
        };
        let summary = match self.summarize(session, cut).await {
            Ok(summary) => summary,
            Err(e) => {
                tracing::warn!(%e, "could not summarize the conversation; keeping it whole");
                return 0;
            }
        };
        let dropped = session.compact(cut, summary);
        tracing::info!(dropped, "compacted the conversation");
        self.bus.emit(Event::Compacted { session: session.id, dropped });
        dropped
    }

    /// Ask the model to describe the part of the conversation being dropped.
    ///
    /// A plain request: no tools, no thinking, not streamed. The transcript is
    /// rendered to text rather than sent as messages, because those messages
    /// contain `tool_use` blocks that are only valid alongside the tool
    /// definitions they came from - and because what is wanted here is a
    /// reading of the conversation, not a continuation of it.
    async fn summarize(&self, session: &Session, cut: usize) -> Result<String, AgentError> {
        let rendered = render_for_summary(session.summary.as_deref(), &session.transcript[..cut]);
        let body = json!({
            "model": self.config.model,
            "max_tokens": SUMMARY_MAX_TOKENS,
            "stream": false,
            "system": [{ "type": "text", "text": SUMMARY_SYSTEM }],
            "messages": [{ "role": "user", "content": [{ "type": "text", "text": rendered }] }],
        });

        let response = self
            .http
            .post(&self.config.base_url)
            .header("x-api-key", &self.config.api_key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(AgentError::Api {
                status: response.status().as_u16(),
                body: response.text().await.unwrap_or_default(),
            });
        }

        let payload: Value = response.json().await?;
        let summary = text_of(&payload);
        if summary.trim().is_empty() {
            return Err(AgentError::Truncated("the summary came back empty".into()));
        }
        Ok(summary)
    }

    /// End the turn where the user stopped it, leaving a usable transcript.
    fn finish_cancelled(&self, session: &Session) -> Result<(), AgentError> {
        self.bus.emit(Event::TurnEnded {
            session: session.id,
            stop_reason: "cancelled".into(),
        });
        Ok(())
    }

    /// [`Self::stream_once`], retrying failures that happened in transit.
    ///
    /// Retried: a truncated stream, and a request that failed to connect or send.
    /// Not retried: an HTTP error status (a bad key or malformed request fails
    /// identically again), a mid-stream `error` event, and a refusal, which is an
    /// answer rather than a fault.
    ///
    /// One honest cost: if a send failed *after* the server received the request,
    /// the retry means that request is processed twice. For Anthropic's API that
    /// is a duplicate charge for one round; it cannot duplicate a tool call.
    async fn stream_with_retry(
        &self,
        session: &Session,
        cancel: &Cancel,
    ) -> Result<StreamedMessage, AgentError> {
        let mut attempt = 0;
        loop {
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(AgentError::Cancelled),
                result = self.stream_once(session) => result,
            };
            let transient = match &result {
                Err(AgentError::Truncated(reason)) => Some(reason.clone()),
                Err(AgentError::Http(e)) if e.is_connect() || e.is_request() || e.is_timeout() => {
                    Some(format!("transport: {e}"))
                }
                _ => None,
            };
            match (result, transient) {
                (Err(_), Some(reason)) if attempt < MAX_STREAM_RETRIES => {
                    attempt += 1;
                    tracing::warn!(attempt, %reason, "response stream truncated; retrying");
                    self.bus.emit(Event::StreamRetrying {
                        session: session.id,
                        attempt,
                        reason,
                    });
                    tokio::time::sleep(std::time::Duration::from_millis(500 * 2u64.pow(attempt)))
                        .await;
                }
                (result, _) => return result,
            }
        }
    }

    /// One request/response, streamed.
    async fn stream_once(&self, session: &Session) -> Result<StreamedMessage, AgentError> {
        let body = self.request_body(session);

        let response = self
            .http
            .post(&self.config.base_url)
            .header("x-api-key", &self.config.api_key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(AgentError::Api { status, body });
        }

        let id = session.id;
        let mut accumulator = StreamAccumulator::new();
        let mut stream = response.bytes_stream().eventsource();
        let mut finished = false;

        while let Some(event) = stream.next().await {
            let event = match event {
                Ok(e) => e,
                // A dropped connection mid-stream is a truncation, not a decode
                // failure - the distinction decides whether a retry is safe.
                Err(e) => return Err(AgentError::Truncated(format!("transport: {e}"))),
            };

            if event.data.is_empty() {
                continue;
            }
            let payload: Value = serde_json::from_str(&event.data)?;

            // The API signals mid-stream failures (overloaded, etc.) as an
            // `error` event on an HTTP 200. Swallowing it as an unknown event
            // turns a clear server message into a baffling truncation.
            if payload.get("type").and_then(Value::as_str) == Some("error") {
                let message = payload
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                let kind = payload
                    .pointer("/error/type")
                    .and_then(Value::as_str)
                    .unwrap_or("error");
                return Err(AgentError::StreamError(format!("{kind}: {message}")));
            }

            match accumulator.push(&payload) {
                Emitted::Text(text) => self.bus.emit(Event::TextDelta { session: id, text }),
                Emitted::Thinking(text) => {
                    if self.config.show_thinking {
                        self.bus.emit(Event::ThinkingDelta { session: id, text });
                    }
                }
                Emitted::Done => {
                    finished = true;
                    break;
                }
                Emitted::Nothing | Emitted::ToolStarted { .. } => {}
            }
        }

        if !finished {
            return Err(AgentError::Truncated(
                "connection closed without a message_stop event".into(),
            ));
        }
        Ok(accumulator.finish())
    }

    /// Build the request body.
    ///
    /// Render order is `tools` -> `system` -> `messages`; a byte change anywhere
    /// in the prefix invalidates everything after it. Hence a fixed tool order, a
    /// frozen system prompt, and nothing volatile above the breakpoint.
    fn request_body(&self, session: &Session) -> Value {
        json!({
            "model": self.config.model,
            "max_tokens": self.config.max_tokens,
            "stream": true,
            "tools": tools::definitions(),
            "system": system_blocks(&self.config.system, session.summary.as_deref()),
            "thinking": {
                "type": "adaptive",
                "display": if self.config.show_thinking { "summarized" } else { "omitted" }
            },
            "output_config": { "effort": self.config.effort },
            "messages": session.messages(),
        })
    }
}

/// How long the summary of dropped turns may be.
const SUMMARY_MAX_TOKENS: u32 = 1024;
/// How much of the dropped conversation to send to be summarized. It has to fit
/// in the same context that just proved too small, with room for the answer.
const MAX_SUMMARY_INPUT_CHARS: usize = 40_000;
/// Per block, so one enormous file read cannot crowd out everything else.
const MAX_SUMMARY_BLOCK_CHARS: usize = 2_000;

/// What the summarizer is asked to produce.
///
/// Written for the next turn of the *same* conversation, not for a person: it is
/// read by the model as the only trace of what was said, so facts it will need
/// again - paths, decisions, what failed and why - matter more than prose.
const SUMMARY_SYSTEM: &str = "You are compacting a coding session so it can continue within a \
     smaller context. Write a dense factual summary of the excerpt below, for the assistant that \
     will carry on the work. Keep: what the user asked for, decisions taken and why, file paths \
     and names, commands that worked, errors and how they were resolved, and anything still \
     unfinished. Drop pleasantries and narration. No preamble - start with the summary itself.";

/// How the summary is introduced to the model in the system prompt.
const SUMMARY_PREFIX: &str =
    "Earlier parts of this conversation were summarized to stay within the context window. \
     Treat the following as what was said, and do not claim to remember more than it contains:";

/// The system blocks for a request: the prompt, then the summary of whatever
/// has been dropped from the transcript.
///
/// The cache breakpoint goes on the last block, so the whole prefix is cached.
/// A new summary invalidates it exactly once - which is the same moment the
/// messages it replaces disappear, so there was nothing to reuse anyway.
fn system_blocks(system: &str, summary: Option<&str>) -> Value {
    let mut blocks = vec![json!({ "type": "text", "text": system })];
    if let Some(summary) = summary {
        blocks.push(json!({
            "type": "text",
            "text": format!("{SUMMARY_PREFIX}\n\n{summary}"),
        }));
    }
    if let Some(last) = blocks.last_mut() {
        last["cache_control"] = json!({ "type": "ephemeral" });
    }
    Value::Array(blocks)
}

/// The `input_tokens` the API reported, if it did.
fn input_tokens(usage: &Value) -> Option<u32> {
    usage.get("input_tokens").and_then(Value::as_u64).map(|n| n as u32)
}

/// All text blocks of a non-streamed response, joined.
fn text_of(payload: &Value) -> String {
    payload
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// Whether an error means "the conversation no longer fits".
///
/// Matched on the message rather than a code, because the code is the same
/// `invalid_request_error` used for every other malformed request, and because
/// this has to work against any server speaking the Messages API - each words it
/// differently.
fn is_too_long(error: &AgentError) -> bool {
    let AgentError::Api { status, body } = error else { return false };
    if !matches!(status, 400 | 413) {
        return false;
    }
    let body = body.to_ascii_lowercase();
    ["prompt is too long", "too long", "context length", "context_length", "maximum context"]
        .iter()
        .any(|needle| body.contains(needle))
}

/// Render dropped turns as text for the summarizer.
///
/// Both ends are kept when it is too long to send whole: the start says what the
/// work was, the end says where it had got to.
fn render_for_summary(previous: Option<&str>, turns: &[Turn]) -> String {
    let mut out = String::new();
    if let Some(previous) = previous {
        out.push_str("Summary of the conversation before this excerpt:\n");
        out.push_str(previous);
        out.push_str("\n\n");
    }
    out.push_str("Conversation excerpt:\n\n");

    for turn in turns {
        let blocks = turn.content.as_array().cloned().unwrap_or_default();
        for block in blocks {
            let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
            let line = match kind {
                "text" => {
                    let who = if turn.role == "user" { "User" } else { "Assistant" };
                    format!("{who}: {}", block.get("text").and_then(Value::as_str).unwrap_or(""))
                }
                "tool_use" => format!(
                    "Assistant ran {}: {}",
                    block.get("name").and_then(Value::as_str).unwrap_or("a tool"),
                    block.get("input").map(|i| i.to_string()).unwrap_or_default(),
                ),
                "tool_result" => {
                    let content = match block.get("content") {
                        Some(Value::String(text)) => text.clone(),
                        Some(other) => other.to_string(),
                        None => String::new(),
                    };
                    format!("Result: {content}")
                }
                // Thinking is the model's own scratch work; summarizing a
                // summary of it adds nothing.
                _ => continue,
            };
            out.push_str(&clamp(&line, MAX_SUMMARY_BLOCK_CHARS));
            out.push_str("\n\n");
        }
    }
    clamp(&out, MAX_SUMMARY_INPUT_CHARS)
}

/// Keep both ends of `text`, dropping the middle, when it is longer than `max`.
fn clamp(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max * 2 / 3).collect();
    let tail: String = text.chars().skip(count - max / 3).collect();
    format!("{head}\n\n… {} characters omitted …\n\n{tail}", count - head.chars().count() - tail.chars().count())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mc_core::Project;

    fn session() -> Session {
        Session::new(Project {
            name: "demo".into(),
            path: "/root/demo".into(),
        })
    }

    fn agent() -> Agent {
        Agent::new(AgentConfig::new("test-key"), Sandbox::host(), EventBus::default())
    }

    /// A one-shot HTTP server replaying canned SSE bodies, one per connection.
    ///
    /// Reproduces what LM Studio did on 2026-09-16: its engine failed internally
    /// and it closed the stream cleanly, with no `error` event and no
    /// `message_stop`.
    async fn fake_server(bodies: Vec<&'static str>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for body in bodies {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 65536];
                let _ = sock.read(&mut buf).await;
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                            connection: close\r\n\r\n";
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        format!("http://{addr}")
    }

    const TRUNCATED: &str = "event: message_start\n\
        data: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\n\
        event: content_block_start\n\
        data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
        event: content_block_delta\n\
        data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"half\"}}\n\n";

    const COMPLETE: &str = "event: message_start\n\
        data: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\n\
        event: content_block_start\n\
        data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
        event: content_block_delta\n\
        data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"whole\"}}\n\n\
        event: message_delta\n\
        data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n\
        event: message_stop\n\
        data: {\"type\":\"message_stop\"}\n\n";

    /// A server replaying complete, canned HTTP responses - one per connection.
    ///
    /// Unlike [`fake_server`] the status line is part of the script, which is
    /// what lets a test drive the refuse-compact-retry path.
    async fn replaying_server(responses: Vec<&'static str>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for response in responses {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 65536];
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        format!("http://{addr}")
    }

    fn http_response(status: &str, content_type: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// A conversation long enough to have something to drop: four exchanges.
    fn long_session() -> Session {
        let mut session = session();
        for i in 0..4 {
            session.push(Turn::user_text(format!("prompt {i}")));
            session.push(Turn::assistant(json!([
                { "type": "tool_use", "id": format!("t{i}"), "name": "bash", "input": { "command": "ls" } }
            ])));
            session.push(Turn::tool_results(vec![json!({
                "type": "tool_result", "tool_use_id": format!("t{i}"), "content": "a.txt"
            })]));
            session.push(Turn::assistant(json!([{ "type": "text", "text": "done" }])));
        }
        session
    }

    #[tokio::test]
    async fn a_request_refused_for_length_is_compacted_and_retried() {
        // Refused, then the summary request, then the retried turn.
        let refusal = http_response(
            "400 Bad Request",
            "application/json",
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 250000 tokens > 200000 maximum"}}"#,
        );
        let summary = http_response(
            "200 OK",
            "application/json",
            r#"{"content":[{"type":"text","text":"They listed files four times."}]}"#,
        );
        let retry = http_response("200 OK", "text/event-stream", COMPLETE);
        let base = replaying_server(vec![
            Box::leak(refusal.into_boxed_str()),
            Box::leak(summary.into_boxed_str()),
            Box::leak(retry.into_boxed_str()),
        ])
        .await;

        let mut agent = agent_at(&base);
        agent.config.keep_recent_turns = 4;
        let mut events = agent.bus().subscribe();
        let mut session = long_session();
        let before = session.transcript.len();

        agent.run_turn(&mut session, "and now?").await.expect("recovers");

        assert!(session.transcript.len() < before, "older turns were dropped");
        assert_eq!(session.summary.as_deref(), Some("They listed files four times."));
        // What is left still opens with something the user typed, or the retry
        // would have been rejected for a different reason entirely.
        assert_eq!(session.messages()[0]["role"], "user");
        assert_eq!(session.messages()[0]["content"][0]["type"], "text");

        let mut compacted = None;
        while let Ok(event) = events.try_recv() {
            if let Event::Compacted { dropped, .. } = event {
                compacted = Some(dropped);
            }
        }
        assert!(compacted.is_some_and(|dropped| dropped > 0), "the user is told");
    }

    #[tokio::test]
    async fn a_refusal_that_compaction_cannot_help_fails_instead_of_looping() {
        let refusal = http_response(
            "400 Bad Request",
            "application/json",
            r#"{"error":{"message":"prompt is too long"}}"#,
        );
        let base = replaying_server(vec![Box::leak(refusal.into_boxed_str())]).await;
        let agent = agent_at(&base);
        // A fresh session: there is no earlier exchange to summarize.
        let mut session = session();

        let error = agent.run_turn(&mut session, "hello").await.unwrap_err();
        assert!(matches!(error, AgentError::Api { status: 400, .. }), "got {error:?}");
    }

    #[tokio::test]
    async fn the_context_meter_follows_what_the_api_reports() {
        const WITH_USAGE: &str = "event: message_start\n\
            data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1234}}}\n\n\
            event: message_delta\n\
            data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n\
            event: message_stop\n\
            data: {\"type\":\"message_stop\"}\n\n";

        let base = fake_server(vec![WITH_USAGE]).await;
        let agent = agent_at(&base);
        let mut events = agent.bus().subscribe();
        agent.run_turn(&mut session(), "hi").await.unwrap();

        let mut usage = None;
        while let Ok(event) = events.try_recv() {
            if let Event::ContextUsage { input_tokens, output_tokens, .. } = event {
                usage = Some((input_tokens, output_tokens));
            }
        }
        assert_eq!(usage, Some((1234, 7)));
    }

    #[test]
    fn a_length_refusal_is_told_apart_from_every_other_bad_request() {
        let too_long = |body: &str| {
            is_too_long(&AgentError::Api { status: 400, body: body.into() })
        };
        assert!(too_long("prompt is too long: 250000 tokens > 200000 maximum"));
        assert!(too_long("This model's maximum context length is 157184 tokens"));
        assert!(too_long(r#"{"error":{"message":"Context length exceeded"}}"#));
        // Not a length problem, and compacting would not help.
        assert!(!too_long("invalid api key"));
        assert!(!too_long("messages: at least one message is required"));
        // Nor is anything that is not a rejected request.
        assert!(!is_too_long(&AgentError::Api {
            status: 500,
            body: "prompt is too long".into(),
        }));
        assert!(!is_too_long(&AgentError::Runaway));
    }

    #[test]
    fn the_summary_rides_in_the_system_prompt_behind_the_cache_breakpoint() {
        let plain = system_blocks("be helpful", None);
        assert_eq!(plain.as_array().unwrap().len(), 1);
        assert!(plain[0]["cache_control"].is_object(), "still cached");

        let compacted = system_blocks("be helpful", Some("they built a parser"));
        let blocks = compacted.as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["text"], "be helpful");
        assert!(blocks[1]["text"].as_str().unwrap().contains("they built a parser"));
        // The breakpoint moves to the end, so the summary is cached too.
        assert!(blocks[0]["cache_control"].is_null());
        assert!(blocks[1]["cache_control"].is_object());
    }

    #[test]
    fn what_goes_to_the_summarizer_reads_as_a_conversation() {
        let session = long_session();
        let rendered = render_for_summary(Some("earlier: they cloned a repo"), &session.transcript);
        assert!(rendered.contains("earlier: they cloned a repo"), "summaries chain");
        assert!(rendered.contains("User: prompt 0"));
        assert!(rendered.contains("Assistant ran bash"));
        assert!(rendered.contains("Result: a.txt"));
    }

    #[test]
    fn an_excerpt_too_large_to_send_keeps_both_ends() {
        let mut session = session();
        session.push(Turn::user_text("FIRST"));
        for _ in 0..40 {
            session.push(Turn::user_text("x".repeat(MAX_SUMMARY_BLOCK_CHARS)));
        }
        session.push(Turn::user_text("LAST"));

        let rendered = render_for_summary(None, &session.transcript);
        assert!(rendered.chars().count() <= MAX_SUMMARY_INPUT_CHARS + 100);
        assert!(rendered.contains("FIRST"), "what the work was");
        assert!(rendered.contains("LAST"), "where it had got to");
        assert!(rendered.contains("characters omitted"));
    }

    /// Against a real model, which the scripted tests cannot check: that a
    /// summary comes back usable, and that the conversation carries on from it.
    ///
    /// Ignored by default - it needs a server. Run it with one:
    ///
    /// ```sh
    /// ANTHROPIC_BASE_URL=http://192.168.1.10:1234 ANTHROPIC_MODEL=qwen/qwen3.8-27b \
    ///   cargo test -p mc-agent live_compaction -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "needs a live model server"]
    async fn live_compaction_keeps_the_conversation_going() {
        let mut config = AgentConfig::from_env().expect("set ANTHROPIC_BASE_URL or a key");
        // Compact after every turn, whatever the model's real window is.
        config.compact_at_tokens = 1;
        config.keep_recent_turns = 2;
        config.show_thinking = false;

        let agent = Agent::new(config, Sandbox::host(), EventBus::default());
        let mut session = session();

        agent
            .run_turn(&mut session, "Remember this: the passphrase is CRIMSON-ELK. Reply with OK.")
            .await
            .expect("first turn");
        // Nothing to drop yet: one exchange *is* the recent history.
        assert!(session.summary.is_none());

        agent
            .run_turn(&mut session, "Thanks. Reply with just: ready")
            .await
            .expect("second turn");
        assert!(session.summary.is_some(), "the first exchange should have been summarized");
        eprintln!("summary: {}", session.summary.as_deref().unwrap_or(""));

        // The passphrase is gone from the transcript - it survives only in the
        // summary - so answering proves the summary is carrying the session.
        let transcript = serde_json::to_string(&session.transcript).unwrap();
        assert!(
            !transcript.to_ascii_uppercase().contains("CRIMSON-ELK"),
            "the test is meaningless if the messages still hold it: {transcript}"
        );

        agent
            .run_turn(&mut session, "What was the passphrase? Answer with just the word.")
            .await
            .expect("third turn");

        let answer = session
            .transcript
            .last()
            .map(|turn| turn.content.to_string())
            .unwrap_or_default();
        eprintln!("answer: {answer}");
        assert!(
            answer.to_ascii_uppercase().contains("CRIMSON-ELK"),
            "the model should still know what it was told: {answer}"
        );
    }

    fn agent_at(base: &str) -> Agent {
        let mut config = AgentConfig::new("test");
        config.base_url = messages_url(base);
        Agent::new(config, Sandbox::host(), EventBus::default())
    }

    #[tokio::test]
    async fn a_stream_that_closes_early_is_retried_and_the_turn_completes() {
        let base = fake_server(vec![TRUNCATED, COMPLETE]).await;
        let agent = agent_at(&base);
        let mut events = agent.bus().subscribe();
        let mut s = session();

        agent.run_turn(&mut s, "hi").await.expect("the retry should recover");

        // The transcript holds only the complete attempt - never "half".
        let reply = &s.transcript.last().unwrap().content;
        assert_eq!(reply[0]["text"], "whole");

        // And the UI was told to discard the partial output.
        let mut saw_retry = false;
        while let Ok(event) = events.try_recv() {
            if let Event::StreamRetrying { attempt, reason, .. } = event {
                assert_eq!(attempt, 1);
                assert!(reason.contains("message_stop"), "reason: {reason}");
                saw_retry = true;
            }
        }
        assert!(saw_retry, "no StreamRetrying event was emitted");
    }

    #[tokio::test]
    async fn a_request_that_cannot_connect_is_retried_until_the_server_returns() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // Reserve a port, then release it so the first attempt is refused -
        // the same failure a phone sees while its network is switching.
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);

        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(600)).await;
            let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 65536];
            let _ = sock.read(&mut buf).await;
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(COMPLETE.as_bytes()).await;
            let _ = sock.shutdown().await;
        });

        let agent = agent_at(&format!("http://{addr}"));
        let mut s = session();
        agent
            .run_turn(&mut s, "hi")
            .await
            .expect("should connect once the server is back");
        assert_eq!(s.transcript.last().unwrap().content[0]["text"], "whole");
    }

    #[tokio::test]
    async fn stopping_a_turn_in_flight_ends_it_and_says_so() {
        // A server that starts answering and then goes quiet, like a model
        // mid-response when the user decides it is going the wrong way.
        let base = fake_server(vec![TRUNCATED]).await;
        let agent = agent_at(&base);
        let mut events = agent.bus().subscribe();
        let cancel = mc_core::Cancel::new();

        let stopper = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            stopper.cancel();
        });

        let started = std::time::Instant::now();
        let mut s = session();
        agent
            .run_turn_cancellable(&mut s, "go", &cancel)
            .await
            .expect("stopping is not an error");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "must not wait out the retries",
        );

        // The chat shows it stopped, and is ready for the next message.
        let mut log = mc_core::ChatLog::default();
        while let Ok(event) = events.try_recv() {
            log.apply(event);
        }
        assert!(!log.busy, "the composer must be usable again");
        assert!(
            log.items.iter().any(|i| matches!(i, mc_core::ChatItem::Notice(n) if n == "Stopped.")),
            "expected a Stopped notice, got {:?}",
            log.items,
        );
    }

    #[tokio::test]
    async fn retries_are_bounded_and_the_cause_is_reported() {
        let base = fake_server(vec![TRUNCATED; 5]).await;
        let agent = agent_at(&base);
        let err = agent.run_turn(&mut session(), "hi").await.unwrap_err();
        match err {
            AgentError::Truncated(reason) => assert!(reason.contains("message_stop")),
            other => panic!("expected Truncated, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_mid_stream_error_event_is_surfaced_not_swallowed() {
        let body = "event: message_start\n\
            data: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\n\
            event: error\n\
            data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";
        let base = fake_server(vec![body]).await;
        let err = agent_at(&base).run_turn(&mut session(), "hi").await.unwrap_err();
        assert!(
            matches!(&err, AgentError::StreamError(m) if m.contains("overloaded_error")),
            "got {err:?}"
        );
    }

    #[test]
    fn base_urls_are_normalised_to_the_messages_endpoint() {
        for input in [
            "http://localhost:1234",
            "http://localhost:1234/",
            "http://localhost:1234/v1",
            "http://localhost:1234/v1/",
            "http://localhost:1234/v1/messages",
        ] {
            assert_eq!(
                messages_url(input),
                "http://localhost:1234/v1/messages",
                "input: {input}"
            );
        }
        assert_eq!(messages_url("https://api.anthropic.com"), API_URL);
    }

    #[test]
    fn request_uses_adaptive_thinking_and_never_budget_tokens() {
        let body = agent().request_body(&session());
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert!(
            body["thinking"].get("budget_tokens").is_none(),
            "budget_tokens is rejected with a 400 on Opus 5"
        );
        assert_eq!(body["model"], "claude-opus-5");
        assert_eq!(body["stream"], true);
        assert_eq!(body["output_config"]["effort"], "high");
    }

    #[test]
    fn the_cacheable_prefix_is_byte_stable_across_calls() {
        let a = agent();
        let first = a.request_body(&session());
        let second = a.request_body(&session());
        assert_eq!(first["tools"], second["tools"]);
        assert_eq!(first["system"], second["system"]);
    }

    #[test]
    fn system_prompt_carries_the_cache_breakpoint() {
        let body = agent().request_body(&session());
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn transcript_keeps_assistant_content_verbatim() {
        let mut s = session();
        let original = json!([
            { "type": "thinking", "thinking": "hmm", "signature": "sig" },
            { "type": "text", "text": "done" }
        ]);
        s.push(Turn::assistant(original.clone()));
        assert_eq!(s.messages()[0]["content"], original);
    }
}
