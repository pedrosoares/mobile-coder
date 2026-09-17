//! The terminal pane: an interactive shell in the sandbox.
//!
//! On desktop, keys go straight from Freya to the PTY. On Android they cannot -
//! Freya's NativeActivity receives no on-screen keyboard text, the same limit the
//! chat works around - so the native message box switches into a terminal mode
//! and its input arrives through [`send_input`]. That call comes from the Android
//! UI thread while a [`TerminalHandle`] lives on Freya's (it is `Rc`-based), so
//! input is queued here and drained on Freya's side.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use freya::{prelude::*, terminal::*};
use mc_core::{ShellCommand, ShellLauncher};

use crate::theme;

static INPUT: Mutex<VecDeque<Vec<u8>>> = Mutex::new(VecDeque::new());

/// Queue bytes for the terminal - a typed line with its `\r`, or a control
/// sequence from an extra-keys button. Callable from any thread.
pub fn send_input(bytes: Vec<u8>) {
    if let Ok(mut queue) = INPUT.lock() {
        // Bounded, so input sent while no shell is running cannot pile up and
        // replay into the next one as a burst of stale commands.
        if queue.len() < 256 {
            queue.push_back(bytes);
        }
    }
}

fn drain_input() -> Vec<Vec<u8>> {
    INPUT.lock().map(|mut q| q.drain(..).collect()).unwrap_or_default()
}

fn start(launcher: &Arc<dyn ShellLauncher>) -> Result<TerminalHandle, String> {
    let ShellCommand { program, args, env, cwd } = launcher.command()?;
    let mut command = CommandBuilder::new(program);
    command.args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    if let Some(cwd) = cwd {
        command.cwd(cwd);
    }
    TerminalHandle::new(TerminalId::new(), PtyBackend::new(command), None)
        .map_err(|e| format!("Could not start the shell: {e}"))
}

/// The Terminal pane. A component, so its hooks and its shell live in their own
/// scope and the shell ends when the pane is left.
/// A shell, or the reason there isn't one. `None` means "not started yet".
pub type ShellSession = Option<Result<TerminalHandle, String>>;

pub struct TerminalView {
    pub launcher: Option<Arc<dyn ShellLauncher>>,
    /// Owned by the app root, so leaving the pane does not kill a running
    /// command and lose its output.
    pub session: State<ShellSession>,
}

impl PartialEq for TerminalView {
    fn eq(&self, other: &Self) -> bool {
        self.session == other.session
            && match (&self.launcher, &other.launcher) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
    }
}

impl Component for TerminalView {
    fn render(&self) -> impl IntoElement {
        match &self.launcher {
            Some(launcher) => Session {
                launcher: launcher.clone(),
                session: self.session,
            }
            .into_element(),
            None => rect()
                .width(Size::fill())
                .height(Size::fill())
                .center()
                .color(theme::MUTED)
                .child("A shell is not available here.")
                .into_element(),
        }
    }
}

struct Session {
    launcher: Arc<dyn ShellLauncher>,
    session: State<ShellSession>,
}

impl PartialEq for Session {
    fn eq(&self, other: &Self) -> bool {
        self.session == other.session && Arc::ptr_eq(&self.launcher, &other.launcher)
    }
}

/// Mark the session ended when the shell exits, so the pane can offer a restart.
///
/// `spawn_forever`, not `spawn`: the watcher has to outlive the pane, or a shell
/// that exits while the user is on another tab would still look alive.
fn watch_exit(handle: TerminalHandle, mut state: State<ShellSession>) {
    spawn_forever(async move {
        let id = handle.id();
        handle.closed().await;
        // Only if this is still the current shell - a restart may have
        // replaced it in the meantime.
        if matches!(&*state.peek(), Some(Ok(current)) if current.id() == id) {
            state.set(Some(Err("The shell exited.".into())));
        }
    });
}

/// Deliver input queued by the Android message box, for as long as the app runs.
pub fn deliver_input(session: State<ShellSession>) {
    spawn_forever(async move {
        loop {
            async_io::Timer::after(Duration::from_millis(40)).await;
            let chunks = drain_input();
            if chunks.is_empty() {
                continue;
            }
            if let Some(Ok(handle)) = &*session.peek() {
                for chunk in chunks {
                    let _ = handle.write(&chunk);
                }
                handle.scroll_to_bottom();
            }
        }
    });
}

impl Component for Session {
    fn render(&self) -> impl IntoElement {
        let launcher = self.launcher.clone();
        let mut session = self.session;

        // Start on first visit only; later visits find the shell still running,
        // with whatever it printed while the user was away.
        use_hook(move || {
            if session.peek().is_none() {
                let started = start(&launcher);
                if let Ok(handle) = &started {
                    watch_exit(handle.clone(), session);
                }
                session.set(Some(started));
            }
        });

        let launcher = self.launcher.clone();
        let a11y_id = use_a11y();
        let current = session.read().clone();

        let body: Element = match current {
            None => rect().width(Size::fill()).height(Size::fill()).into_element(),
            Some(Ok(handle)) => {
                let keys = handle.clone();
                let wheel = handle.clone();
                Terminal::new(handle)
                    .font_family(crate::markdown::MONO)
                    .font_size(13.)
                    .background(theme::GROUND)
                    .foreground(theme::INK)
                    .a11y_id(a11y_id)
                    .a11y_role(AccessibilityRole::Terminal)
                    .a11y_auto_focus(true)
                    .on_mouse_down(move |_| a11y_id.request_focus())
                    .on_wheel(move |e: Event<WheelEventData>| wheel.scroll(-(e.delta_y / 20.) as i32))
                    // Desktop keyboard. On Android no key events arrive here;
                    // input comes through `send_input` instead.
                    .on_key_down(move |e: Event<KeyboardEventData>| {
                        let _ = keys.write_key(&e.key, e.modifiers);
                    })
                    .into_element()
            }
            Some(Err(message)) => rect()
                .width(Size::fill())
                .padding(16.)
                .spacing(12.)
                .child(label().text(message).font_size(14.).color(theme::MUTED))
                .child(
                    Button::new()
                        .compact()
                        .filled()
                        .on_press(move |_| {
                            let started = start(&launcher);
                            if let Ok(handle) = &started {
                                watch_exit(handle.clone(), session);
                            }
                            session.set(Some(started));
                        })
                        .child("Start a new shell"),
                )
                .into_element(),
        };

        rect()
            .width(Size::fill())
            .height(Size::fill())
            .background(theme::GROUND)
            .padding(6.)
            .child(body)
    }
}
