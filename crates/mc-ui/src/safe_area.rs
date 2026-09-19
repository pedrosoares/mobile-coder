//! Space the UI must leave for the system bars and the soft keyboard.
//!
//! Since `targetSdk 35` Android draws apps edge to edge and no longer resizes the
//! window for the keyboard (`adjustResize` is ignored). Freya has no inset API,
//! so the Android shell measures `WindowInsets` in Kotlin and pushes them here in
//! logical pixels. Without this the header sits under the status bar and the
//! message box under the keyboard - the one place a chat cannot afford it.
//!
//! Stored in atomics because the values arrive on the Android UI thread while
//! Freya renders on its own; desktop never sets them and gets zeros.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

static TOP: AtomicU32 = AtomicU32::new(0);
static BOTTOM: AtomicU32 = AtomicU32::new(0);

/// What the platform's native message box should be doing.
///
/// On Android the box is a separate window the Freya UI cannot control directly,
/// so the UI publishes a mode and the shell follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ComposerMode {
    /// Not shown.
    Hidden = 0,
    /// Messages to the agent.
    Chat = 1,
    /// Lines and control keys for the shell.
    Terminal = 2,
}

/// The app opens on the chat.
static COMPOSER_MODE: AtomicU8 = AtomicU8::new(ComposerMode::Chat as u8);

pub fn set_composer_mode(mode: ComposerMode) {
    COMPOSER_MODE.store(mode as u8, Ordering::Relaxed);
}

/// Set when the user asks for settings, cleared once the shell has opened them.
///
/// The settings form has to be native for the same reason the message box is:
/// Freya receives no on-screen keyboard text on Android. The UI cannot open an
/// Android dialog itself, so it raises a flag the shell polls.
static SETTINGS_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn request_settings() {
    SETTINGS_REQUESTED.store(true, Ordering::Relaxed);
}

/// Take the pending request, if any.
pub fn take_settings_request() -> bool {
    SETTINGS_REQUESTED.swap(false, Ordering::Relaxed)
}

pub fn composer_mode() -> ComposerMode {
    match COMPOSER_MODE.load(Ordering::Relaxed) {
        1 => ComposerMode::Chat,
        2 => ComposerMode::Terminal,
        _ => ComposerMode::Hidden,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SafeArea {
    pub top: f32,
    pub bottom: f32,
}

pub fn set(top: f32, bottom: f32) {
    TOP.store(top.max(0.0).to_bits(), Ordering::Relaxed);
    BOTTOM.store(bottom.max(0.0).to_bits(), Ordering::Relaxed);
}

pub fn get() -> SafeArea {
    SafeArea {
        top: f32::from_bits(TOP.load(Ordering::Relaxed)),
        bottom: f32::from_bits(BOTTOM.load(Ordering::Relaxed)),
    }
}
