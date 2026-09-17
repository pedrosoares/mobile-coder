//! A long-lived agent that serves a chat.
//!
//! One conversation, many turns: each prompt continues the same [`Session`], so
//! "now make it print twice" means something. Prompts are handled strictly one at
//! a time on a dedicated thread with its own tokio runtime, which keeps the agent
//! off the UI thread and independent of whichever async runtime the UI uses.

use mc_core::{AgentHandle, Event, EventBus, Project, Session};
use mc_sandbox::Sandbox;

use crate::{Agent, AgentConfig, AgentError};

/// Produces what the agent needs, at the moment a prompt arrives.
///
/// Called per prompt rather than once at startup, because on a phone neither
/// input is fixed: the rootfs may still be installing when the app opens, and
/// the key or endpoint can be changed while it runs. An `Err` is shown to the
/// user as the reason the prompt could not run.
pub type Setup = Box<dyn Fn() -> Result<(AgentConfig, Sandbox), String> + Send>;

/// Start the worker, returning the handle the UI drives it with.
pub fn spawn(bus: EventBus, project: Project, setup: Setup) -> AgentHandle {
    let (handle, mut prompts) = AgentHandle::new(bus.clone());

    std::thread::Builder::new()
        .name("mc-agent-worker".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(e) => {
                    tracing::error!(%e, "agent worker has no runtime; prompts will be ignored");
                    return;
                }
            };

            let mut session = Session::new(project);
            runtime.block_on(async move {
                while let Some(prompt) = prompts.recv().await {
                    let (config, sandbox) = match setup() {
                        Ok(parts) => parts,
                        Err(reason) => {
                            // Show the prompt, then why it could not run - the
                            // chat should never silently swallow a message.
                            bus.emit(Event::TurnStarted { session: session.id, prompt });
                            bus.emit(Event::Failed { session: session.id, message: reason });
                            continue;
                        }
                    };

                    let agent = Agent::new(config, sandbox, bus.clone());
                    match agent.run_turn(&mut session, &prompt).await {
                        Ok(()) => {}
                        // run_turn already reported a refusal.
                        Err(AgentError::Refused(_)) => {}
                        Err(e) => bus.emit(Event::Failed {
                            session: session.id,
                            message: describe(&e),
                        }),
                    }
                }
            });
        })
        .expect("failed to spawn the agent worker thread");

    handle
}

/// Turn an agent error into something a person can act on.
fn describe(error: &AgentError) -> String {
    match error {
        AgentError::Api { status: 401, .. } => {
            "The API key was rejected. Check it, or switch to a local endpoint.".into()
        }
        AgentError::Api { status: 429, .. } => {
            "Rate limited by the API. Wait a moment and try again.".into()
        }
        AgentError::Api { status, body } => {
            let detail: String = body.chars().take(300).collect();
            format!("The API returned {status}: {detail}")
        }
        AgentError::Http(e) if e.is_connect() => format!(
            "Could not reach the model server. Check the network and the endpoint. ({e})"
        ),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_setup_failure_is_shown_in_the_chat_rather_than_dropped() {
        let bus = EventBus::default();
        let mut events = bus.subscribe();
        let handle = spawn(
            bus,
            Project { name: "t".into(), path: ".".into() },
            Box::new(|| Err("The Linux environment is still installing.".into())),
        );
        assert!(handle.submit("hello"));

        let mut log = mc_core::ChatLog::default();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match events.try_recv() {
                Ok(event) => {
                    let failed = matches!(event, Event::Failed { .. });
                    log.apply(event);
                    if failed {
                        break;
                    }
                }
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }

        assert_eq!(log.items[0], mc_core::ChatItem::User("hello".into()));
        assert!(matches!(
            &log.items[1],
            mc_core::ChatItem::Error(m) if m.contains("still installing")
        ));
        assert!(!log.busy);
    }
}
