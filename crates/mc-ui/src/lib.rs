//! Freya components, shared by the desktop dev loop and the Android app.
//!
//! Phone-first: a single pane with a switcher, not a desktop IDE's split panes
//! shrunk down. The UI never talks to Claude itself; it drives an
//! [`mc_core::AgentHandle`] handed in by the app shell.

pub mod chat;
pub mod files;
pub mod markdown;
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
}

impl Pane {
    pub const ALL: [Pane; 3] = [Pane::Chat, Pane::Terminal, Pane::Files];

    pub fn label(self) -> &'static str {
        match self {
            Pane::Chat => "Chat",
            Pane::Terminal => "Terminal",
            Pane::Files => "Files",
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
        let log = use_state(move || {
            let mut log = ChatLog::default();
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

        // Long-lived tasks, for the same reason: an agent reply that arrives
        // while the Terminal is on screen still lands in the chat.
        let agent = self.agent.clone();
        use_hook(move || {
            chat::collect_events(agent, log);
            terminal::deliver_input(shell);
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
            Pane::Files => safe_area::ComposerMode::Hidden,
        });

        let body: Element = match *pane.read() {
            Pane::Chat => chat::ChatView {
                agent: self.agent.clone(),
                native_composer: self.native_composer,
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
