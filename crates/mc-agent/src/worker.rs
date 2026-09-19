//! A long-lived agent that serves a chat.
//!
//! One conversation, many turns: each prompt continues the same [`Session`], so
//! "now make it print twice" means something. Prompts are handled strictly one at
//! a time on a dedicated thread with its own tokio runtime, which keeps the agent
//! off the UI thread and independent of whichever async runtime the UI uses.

use std::path::PathBuf;

use mc_core::{AgentCommand, AgentHandle, Event, EventBus, Project, Session, SessionLibrary};
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
///
/// `library` keeps the conversations across restarts: the most recent one is
/// opened at startup and whichever is current is written after every turn, so a
/// phone app being killed costs at most the turn in flight.
pub fn spawn_with_library(
    bus: EventBus,
    project: Project,
    setup: Setup,
    library: Option<PathBuf>,
) -> AgentHandle {
    spawn_inner(bus, project, setup, library.map(SessionLibrary::new))
}

/// Start the worker without persistence.
pub fn spawn(bus: EventBus, project: Project, setup: Setup) -> AgentHandle {
    spawn_inner(bus, project, setup, None)
}

fn spawn_inner(
    bus: EventBus,
    project: Project,
    setup: Setup,
    library: Option<SessionLibrary>,
) -> AgentHandle {
    let (handle, mut prompts) = AgentHandle::new(bus.clone());
    let handle_cancel = handle.cancel_token();

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

            // Continue yesterday's conversation when there is one, so the
            // agent keeps the context it built up about the project.
            let mut session = match library.as_ref().and_then(SessionLibrary::most_recent) {
                Some(restored) => {
                    tracing::info!(turns = restored.transcript.len(), "resumed session");
                    restored
                }
                None => Session::new(project.clone()),
            };
            let cancel = handle_cancel;
            runtime.block_on(async move {
                while let Some(command) = prompts.recv().await {
                    let prompt = match command {
                        AgentCommand::Prompt(prompt) => prompt,

                        // Switching chats saves the one being left, because the
                        // in-memory transcript is ahead of the file whenever a
                        // turn ended in something unsaved - a cancellation, say.
                        AgentCommand::Open(id) => {
                            save(&library, &session);
                            match library.as_ref().and_then(|library| library.load(id)) {
                                Some(opened) => session = opened,
                                None => {
                                    tracing::warn!(%id, "no such chat; starting an empty one");
                                    session = Session::new(project.clone());
                                }
                            }
                            // Written again on the way in, so that "most
                            // recent" means the chat last *opened* rather than
                            // the one last left - otherwise switching away from
                            // a chat makes the one you left the one that
                            // reopens at launch.
                            save(&library, &session);
                            bus.emit(Event::SessionOpened { session: session.id });
                            continue;
                        }

                        AgentCommand::NewChat => {
                            save(&library, &session);
                            session = Session::new(project.clone());
                            // Saved immediately, so an empty chat the user made
                            // on purpose survives the app being killed before
                            // they type anything into it.
                            save(&library, &session);
                            bus.emit(Event::SessionOpened { session: session.id });
                            continue;
                        }
                    };

                    // A stop pressed while idle must not kill the next turn.
                    cancel.reset();
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
                    match agent.run_turn_cancellable(&mut session, &prompt, &cancel).await {
                        Ok(()) => {}
                        // run_turn already reported a refusal.
                        Err(AgentError::Refused(_)) => {}
                        Err(e) => bus.emit(Event::Failed {
                            session: session.id,
                            message: describe(&e),
                        }),
                    }

                    // Save whatever the turn produced, successful or not: a
                    // failed turn still moved the conversation along.
                    save(&library, &session);
                }
            });
        })
        .expect("failed to spawn the agent worker thread");

    handle
}

/// Write the session, if there is anywhere to write it.
///
/// A failure is logged and swallowed: the conversation is still in memory and
/// still usable, and stopping a turn because a write failed would turn a full
/// disk into a broken app.
fn save(library: &Option<SessionLibrary>, session: &Session) {
    if let Some(library) = library
        && let Err(e) = library.save(session)
    {
        tracing::error!(%e, "could not save the session");
    }
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
