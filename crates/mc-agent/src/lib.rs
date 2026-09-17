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
use mc_core::{Event, EventBus, Session, Turn};
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
        let id = session.id;
        session.push(Turn::user_text(user_text));
        self.bus.emit(Event::TurnStarted {
            session: id,
            prompt: user_text.to_string(),
        });

        for _ in 0..MAX_TOOL_ROUNDS {
            let message = self.stream_with_retry(session).await?;

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
            for call in tool_uses {
                self.bus.emit(Event::ToolRequested {
                    session: id,
                    id: call.id.clone(),
                    name: call.name.clone(),
                    input: call.input.clone(),
                });

                let outcome = tools::execute(&self.sandbox, &call.name, &call.input).await;

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
        }

        Err(AgentError::Runaway)
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
    async fn stream_with_retry(&self, session: &Session) -> Result<StreamedMessage, AgentError> {
        let mut attempt = 0;
        loop {
            let result = self.stream_once(session).await;
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
            "system": [{
                "type": "text",
                "text": self.config.system,
                "cache_control": { "type": "ephemeral" }
            }],
            "thinking": {
                "type": "adaptive",
                "display": if self.config.show_thinking { "summarized" } else { "omitted" }
            },
            "output_config": { "effort": self.config.effort },
            "messages": session.messages(),
        })
    }
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
