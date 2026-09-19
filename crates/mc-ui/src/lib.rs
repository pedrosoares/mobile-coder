//! Freya components, shared by the desktop dev loop and the Android app.
//!
//! Phone-first: a single pane with a switcher, not a desktop IDE's split panes
//! shrunk down. The UI never talks to Claude itself; it drives an
//! [`mc_core::AgentHandle`] handed in by the app shell.
//!
//! # One rule worth knowing before editing anything here
//!
//! Writing to a `State` while a read guard on it is alive panics, and on
//! Android that panic takes the window down. The trap is that a temporary lives
//! to the end of its statement, and the scrutinee of an `if let` or `match`
//! lives for the whole construct:
//!
//! ```ignore
//! if let Some(x) = *thing.peek() { thing.set(None); }   // panics
//! let current = *thing.peek();                          // guard dropped here
//! if let Some(x) = current { thing.set(None); }         // fine
//! ```
//!
//! The condition of a plain `if` is its own temporary scope, so `if
//! *a.peek() != b { a.set(b) }` is safe - which is why this is easy to get
//! wrong. Read into a local first and the question does not arise. Three
//! separate crashes in this crate have been this, each found by running the app
//! rather than by the compiler.

pub mod chat;
pub mod clipboard;
pub mod files;
pub mod git;
pub mod markdown;
pub mod prompt;
pub mod safe_area;
pub mod terminal;
pub mod theme;

use std::{sync::Arc, time::Duration};

use freya::prelude::*;
use mc_core::{AgentHandle, ChatLog, FileBrowser, ShellLauncher};

/// Which pane the single-pane layout is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Chat,
    Terminal,
    Files,
    Git,
}

impl Pane {
    pub const ALL: [Pane; 4] = [Pane::Chat, Pane::Terminal, Pane::Files, Pane::Git];

    pub fn label(self) -> &'static str {
        match self {
            Pane::Chat => "Chat",
            // Short, because four tabs plus a title have to fit across a folded
            // phone: 968 logical pixels, and "Terminal" is the long one.
            Pane::Terminal => "Term",
            Pane::Files => "Files",
            Pane::Git => "Git",
        }
    }

    /// Whether this pane can be rendered on the current target. All can, since
    /// `freya-terminal` builds for Android with the patches in `patches/`.
    pub fn available(self) -> bool {
        true
    }
}

/// Standard Material app bar height. Taller than a bare row of buttons, which is
/// the point: it gives the top of the screen room instead of crowding the chat
/// against the status bar.
const APP_BAR_HEIGHT: f32 = 56.0;

/// Title on the left, pane switcher on the right, a hairline below.
fn app_bar(current: Pane, mut pane: State<Pane>) -> impl IntoElement {
    let mut tabs = rect().horizontal().spacing(4.).cross_align(Alignment::Center);

    // Settings are a native dialog on Android (see `safe_area`); on desktop the
    // key and endpoint come from the environment, so the button would do
    // nothing and is left out.
    if cfg!(target_os = "android") {
        tabs = tabs.child(
            Button::new()
                .compact()
                .flat()
                .on_press(|_| safe_area::request_settings())
                .child("⚙"),
        );
    }
    for option in Pane::ALL.into_iter().filter(|p| p.available()) {
        let button = Button::new()
            .compact()
            .on_press(move |_| pane.set(option))
            .child(option.label());
        // The active pane is filled; the rest stay quiet.
        tabs = tabs.child(if option == current { button.filled() } else { button.flat() });
    }

    rect()
        .width(Size::fill())
        .background(theme::SURFACE)
        .child(
            rect()
                .content(Content::Flex)
                .horizontal()
                .width(Size::fill())
                .height(Size::px(APP_BAR_HEIGHT))
                .padding((0., 16.))
                .cross_align(Alignment::Center)
                .child(
                    rect()
                        .width(Size::flex(1.))
                        .child(
                            label()
                                .text("mobile-coder")
                                .font_size(18.)
                                .font_weight(FontWeight::SEMI_BOLD)
                                .color(theme::INK),
                        ),
                )
                .child(tabs),
        )
        .child(rect().width(Size::fill()).height(Size::px(1.)).background(theme::DIVIDER))
}

/// The application root.
pub struct MobileCoder {
    /// `None` when no model is configured; the chat explains how to fix that.
    pub agent: Option<AgentHandle>,
    /// The platform draws the message box itself, so the chat must not.
    ///
    /// Set on Android. Freya's `Input` cannot receive text from an on-screen
    /// keyboard there (it runs in a `NativeActivity`, which has no
    /// `InputConnection`), so the Android shell overlays a native `EditText`
    /// instead, and reports its height as part of the bottom safe area.
    pub native_composer: bool,
    /// Read access to the sandbox's files for the Files pane.
    pub files: Option<Arc<dyn FileBrowser>>,
    /// Starts the shell for the Terminal pane.
    pub shell: Option<Arc<dyn ShellLauncher>>,
    /// Makes a sandbox for the Git pane to run git in.
    pub sandbox: Option<Arc<dyn mc_sandbox::SandboxFactory>>,
    /// Where the chats are kept. Without one the app still works - it just has
    /// a single unnamed conversation, which is what it had before.
    pub sessions: Option<Arc<mc_core::SessionLibrary>>,
    /// The conversation restored from disk, if there was one.
    pub restored: Option<ChatLog>,
}

impl App for MobileCoder {
    fn render(&self) -> impl IntoElement {
        // Replaces any theme already provided - on Android, the light theme
        // `freya-android` installs at the root - so this reaches every component
        // and the status bar.
        use_init_theme(theme::freya_theme);
        let pane = use_state(|| Pane::Chat);
        let mut insets = use_state(safe_area::get);

        // Everything a pane must not lose when the user looks at another tab
        // lives here, in the root's scope, which is never unmounted.
        let agent = self.agent.clone();
        let restored = self.restored.clone();
        let log = use_state(move || {
            let mut log = restored.unwrap_or_default();
            if agent.is_none() {
                log.notice(
                    "No model is configured. On desktop set ANTHROPIC_API_KEY, or \
                     ANTHROPIC_BASE_URL for a local server. On Android use \
                     apps/android/agent.sh --key or --endpoint.",
                );
            }
            log
        });
        let draft = use_state(String::new);
        let shell = use_state(terminal::ShellSession::default);
        let files_cwd = use_state(String::new);
        let git_draft = use_state(String::new);
        let git_asking = use_state(|| None);
        let picking = use_state(|| false);

        // Long-lived tasks, for the same reason: an agent reply that arrives
        // while the Terminal is on screen still lands in the chat.
        let agent = self.agent.clone();
        let sandbox = self.sandbox.clone();
        let sessions = self.sessions.clone();
        let following = self.agent.clone();
        use_hook(move || {
            chat::collect_events(agent, log);
            chat::follow_open_chats(following, sessions, log);
            terminal::deliver_input(shell);
            // The git worker owns a runtime of its own, so it starts once here
            // rather than per visit to the pane.
            if let Some(factory) = sandbox {
                git::start(factory);
            }
        });

        // Poll rather than subscribe: insets change on the Android UI thread,
        // outside Freya, and this is cheap. On desktop they never change.
        use_hook(move || {
            spawn(async move {
                loop {
                    async_io::Timer::after(Duration::from_millis(120)).await;
                    let now = safe_area::get();
                    if *insets.peek() != now {
                        insets.set(now);
                    }
                }
            });
        });
        let area = *insets.read();

        let current = *pane.read();
        // The native box follows the pane: messages on Chat, shell input on
        // Terminal, and nothing on Files, where it would float over the list.
        safe_area::set_composer_mode(match current {
            Pane::Chat => safe_area::ComposerMode::Chat,
            Pane::Terminal => safe_area::ComposerMode::Terminal,
            // Neither the Files list nor the Git pane has anywhere to type:
            // both collect text in a dialog when they need it.
            Pane::Files | Pane::Git => safe_area::ComposerMode::Hidden,
        });

        let body: Element = match *pane.read() {
            Pane::Chat => chat::ChatView {
                agent: self.agent.clone(),
                native_composer: self.native_composer,
                library: self.sessions.clone(),
                picking,
                log,
                draft,
            }
            .into_element(),
            Pane::Files => files::FilesView {
                browser: self.files.clone(),
                cwd: files_cwd,
            }
            .into_element(),
            Pane::Terminal => terminal::TerminalView {
                launcher: self.shell.clone(),
                session: shell,
            }
            .into_element(),
            Pane::Git => git::GitView {
                // The same platform limit that gives the chat a native message
                // box: no typed text reaches Freya on Android.
                native_prompts: self.native_composer,
                draft: git_draft,
                asking: git_asking,
            }
            .into_element(),
        };

        // `Size::flex` children only size themselves inside a `Content::Flex`
        // parent; without it they collapse to nothing.
        rect()
            .content(Content::Flex)
            .width(Size::fill())
            .height(Size::fill())
            .background(theme::GROUND)
            .color(theme::INK)
            // The app bar extends up behind the status bar in the same colour, so
            // the two read as one surface; the inset keeps its content clear.
            .child(rect().width(Size::fill()).height(Size::px(area.top)).background(theme::SURFACE))
            .child(app_bar(current, pane))
            .child(rect().width(Size::fill()).height(Size::flex(1.)).child(body))
            .child(rect().width(Size::fill()).height(Size::px(area.bottom)).background(theme::SURFACE))
    }
}
