//! The desktop dev loop.
//!
//! Two modes:
//!
//!   cargo run -p desktop                      launch the UI
//!   cargo run -p desktop -- --prompt "..."    run one agent turn, print it
//!
//! The second mode is the fast way to exercise `mc-agent` against the real API:
//! no device, no APK, no Gradle. It runs against a host-shell sandbox rather than
//! proot, which is the point - the agent cannot tell the difference, so streaming
//! and tool-loop bugs surface here in seconds instead of on a phone.

use std::path::PathBuf;

use freya::prelude::{LaunchConfig, WindowConfig, launch};
use mc_agent::worker;
use mc_agent::{Agent, AgentConfig};
use mc_core::{Event, EventBus, Project, Session};
use mc_sandbox::Sandbox;

/// Where the conversation is kept, inside the workspace.
/// Where the chats are kept inside the workspace, one file per chat.
const SESSIONS_DIR: &str = ".mobile-coder-chats";
/// The single file used before chats were plural.
const LEGACY_SESSION_FILE: &str = ".mobile-coder-session.json";

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,desktop=debug,mc_agent=debug".into()),
        )
        .init();

    let mut args = std::env::args().skip(1);
    if let Some(flag) = args.next() {
        if flag == "--prompt" {
            let prompt = args.collect::<Vec<_>>().join(" ");
            if prompt.trim().is_empty() {
                eprintln!("usage: cargo run -p desktop -- --prompt \"your prompt\"");
                std::process::exit(2);
            }
            return run_one_turn(&prompt);
        }
        eprintln!("unknown argument: {flag}");
        std::process::exit(2);
    }

    let (agent, workspace) = start_chat_agent();
    // Show yesterday's conversation, rebuilt from the transcript the worker
    // resumed from.
    let sessions = workspace
        .as_ref()
        .map(|dir| {
            let library = mc_core::SessionLibrary::new(dir.join(SESSIONS_DIR));
            library.adopt(&dir.join(LEGACY_SESSION_FILE));
            std::sync::Arc::new(library)
        });
    let restored = sessions
        .as_ref()
        .and_then(|library| library.most_recent())
        .map(|session| mc_core::ChatLog::from_session(&session));
    // Browse the same directory the agent works in. "/" in the Files pane is
    // the workspace root, matching how the agent's host sandbox sees it.
    let files: Option<std::sync::Arc<dyn mc_core::FileBrowser>> = workspace
        .clone()
        .map(|dir| std::sync::Arc::new(mc_sandbox::fs::RootedFs::new(dir, "/")) as _);
    // Your own shell, in the same workspace the agent uses.
    let cwd = workspace.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let shell: Option<std::sync::Arc<dyn mc_core::ShellLauncher>> =
        Some(std::sync::Arc::new(mc_sandbox::HostShell { cwd: cwd.clone() }));
    // The Git pane runs git where the agent runs it: on this machine, in the
    // workspace. On a phone that is the sandbox; here it is the host shell.
    let sandbox: Option<std::sync::Arc<dyn mc_sandbox::SandboxFactory>> =
        Some(std::sync::Arc::new(HostSandbox(cwd.clone())));

    // Desktop has no Keystore; the token comes from the environment, the same
    // way the API key does.
    if let Some(token) = mc_github::token::from_env() {
        mc_github::token::set(token);
    }

    // Copy buttons reach the window system through Freya's clipboard, which
    // exists only once a window does - so this runs on the render thread, from
    // a button handler, and never from the agent's threads.
    mc_ui::clipboard::install(Box::new(|text| {
        freya::clipboard::Clipboard::set(text.to_string()).map_err(|e| format!("{e:?}"))
    }));

    // A phone-shaped window, so layout problems show up here rather than on
    // device where the iteration loop is far slower.
    launch(
        LaunchConfig::new().with_window(
            WindowConfig::new_app(mc_ui::MobileCoder {
                agent,
                native_composer: false,
                files,
                shell,
                sandbox,
                sessions,
                restored,
            })
                .with_size(420., 860.)
                .with_title("mobile-coder (desktop)"),
        ),
    )
}

/// A sandbox that is just this machine. See [`mc_sandbox::Sandbox::host`].
#[derive(Debug)]
struct HostSandbox(PathBuf);

impl mc_sandbox::SandboxFactory for HostSandbox {
    fn create(&self) -> Result<mc_sandbox::Sandbox, String> {
        Ok(mc_sandbox::Sandbox::host())
    }

    /// Beside the agent's workspace, not at `/root/projects`: this sandbox is
    /// the machine itself.
    fn projects_dir(&self) -> String {
        self.0.join("projects").display().to_string()
    }
}

/// Start the agent behind the chat, if a model is configured.
///
/// On desktop the sandbox is the host shell, so the model's commands run on this
/// machine. They run in a dedicated workspace rather than wherever the app was
/// launched from - otherwise starting it from the repo would let a model edit
/// the repo itself. `MC_WORKSPACE` overrides the location.
fn start_chat_agent() -> (Option<mc_core::AgentHandle>, Option<PathBuf>) {
    let config = match AgentConfig::from_env() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("chat disabled: {e}");
            return (None, None);
        }
    };

    let workspace = std::env::var_os("MC_WORKSPACE")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("mobile-coder-workspace"));
    if let Err(e) = std::fs::create_dir_all(&workspace).and_then(|()| std::env::set_current_dir(&workspace)) {
        eprintln!("chat disabled: cannot use workspace {}: {e}", workspace.display());
        return (None, None);
    }
    eprintln!(
        "[chat] {} model={} - commands run on THIS machine in {}",
        config.base_url,
        config.model,
        workspace.display()
    );

    let handle = worker::spawn_with_library(
        EventBus::default(),
        Project { name: "workspace".into(), path: workspace.clone() },
        Box::new(move || Ok((config.clone(), Sandbox::host()))),
        Some(workspace.join(SESSIONS_DIR)),
    );
    (Some(handle), Some(workspace))
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn run_one_turn(prompt: &str) {
    let config = match AgentConfig::from_env() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("{e}");
            eprintln!("  export ANTHROPIC_API_KEY=sk-ant-...");
            eprintln!("  or, for LM Studio:");
            eprintln!("  export ANTHROPIC_BASE_URL=http://localhost:1234 ANTHROPIC_MODEL=<model>");
            std::process::exit(1);
        }
    };
    eprintln!("\x1b[2m[endpoint] {} model={}\x1b[0m", config.base_url, config.model);

    let bus = EventBus::default();
    let agent = Agent::new(config, Sandbox::host(), bus.clone());
    let mut session = Session::new(Project {
        name: "scratch".into(),
        path: PathBuf::from("."),
    });

    // Print deltas as they arrive, so a long turn shows progress rather than
    // looking hung - the same reason the phone UI streams.
    let mut events = bus.subscribe();
    let printer = tokio::spawn(async move {
        use std::io::Write;
        while let Ok(event) = events.recv().await {
            match event {
                Event::TextDelta { text, .. } => {
                    print!("{text}");
                    let _ = std::io::stdout().flush();
                }
                Event::ToolRequested { name, input, .. } => {
                    println!("\n\x1b[2m[tool] {name} {input}\x1b[0m");
                }
                Event::ToolCompleted { is_error, output, .. } => {
                    let head: String = output.chars().take(400).collect();
                    let tag = if is_error { "tool failed" } else { "tool ok" };
                    println!("\x1b[2m[{tag}] {}\x1b[0m", head.trim());
                }
                Event::StreamRetrying { attempt, reason, .. } => {
                    // A terminal cannot un-print the partial output, so mark
                    // where the retried response starts instead.
                    println!("\n\x1b[33m[stream broke: {reason} - retry {attempt}]\x1b[0m");
                }
                Event::TurnEnded { stop_reason, .. } => {
                    println!("\n\x1b[2m[turn ended: {stop_reason}]\x1b[0m");
                    break;
                }
                Event::Failed { message, .. } => {
                    eprintln!("\n[failed] {message}");
                    break;
                }
                _ => {}
            }
        }
    });

    if let Err(e) = agent.run_turn(&mut session, prompt).await {
        eprintln!("\nerror: {e}");
        std::process::exit(1);
    }
    let _ = printer.await;
}
