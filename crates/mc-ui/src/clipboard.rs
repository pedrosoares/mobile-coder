//! Copying text out of the app.
//!
//! The clipboard is one per process and reached from deep inside the render
//! tree, so it is installed once by the app shell rather than threaded through
//! every component as a prop. Desktop installs a writer that talks to the
//! window system; Android installs none and instead polls [`take_pending`] from
//! the UI thread, because `ClipboardManager` may only be touched there.

use std::{
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

/// How long "Copied" stays on the status line.
const TOAST_LIFETIME: Duration = Duration::from_secs(2);

type Writer = Box<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

static WRITER: OnceLock<Writer> = OnceLock::new();
/// Text waiting for a platform that can only copy from its own thread.
static PENDING: Mutex<Option<String>> = Mutex::new(None);
static TOAST: Mutex<Option<(String, Instant)>> = Mutex::new(None);

/// Install the platform's clipboard. Later calls are ignored.
pub fn install(writer: Writer) {
    let _ = WRITER.set(writer);
}

/// Copy `text`, and leave a short message for the status line.
///
/// Never fails loudly: a copy that did not happen is worth a line of text, not
/// an error in the transcript.
pub fn copy(text: &str) {
    if text.is_empty() {
        return;
    }
    let outcome = match WRITER.get() {
        Some(writer) => writer(text),
        None => {
            // No writer: hand it to whoever is polling (Android's UI thread).
            // Only the newest copy matters, so this replaces rather than queues.
            match PENDING.lock() {
                Ok(mut slot) => {
                    *slot = Some(text.to_string());
                    Ok(())
                }
                Err(_) => Err("clipboard is unavailable".into()),
            }
        }
    };
    let message = match outcome {
        Ok(()) => "Copied".to_string(),
        Err(e) => format!("Could not copy: {e}"),
    };
    if let Ok(mut slot) = TOAST.lock() {
        *slot = Some((message, Instant::now()));
    }
}

/// Text to put on the clipboard, if any. Clears it.
pub fn take_pending() -> Option<String> {
    PENDING.lock().ok().and_then(|mut slot| slot.take())
}

/// The message to show, until it expires.
pub fn toast() -> Option<String> {
    let mut slot = TOAST.lock().ok()?;
    let (message, at) = slot.as_ref()?;
    if at.elapsed() > TOAST_LIFETIME {
        *slot = None;
        return None;
    }
    Some(message.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One process, one clipboard: these share the statics, so they run as one
    /// test rather than racing each other.
    #[test]
    fn pending_text_is_queued_and_announced() {
        assert!(take_pending().is_none());

        copy("hello");
        assert_eq!(take_pending().as_deref(), Some("hello"));
        // Taken once only - the UI thread must not paste it twice.
        assert!(take_pending().is_none());
        assert_eq!(toast().as_deref(), Some("Copied"));

        // An empty copy is not a copy.
        copy("");
        assert!(take_pending().is_none());

        // Only the newest survives; an unread copy is stale by definition.
        copy("first");
        copy("second");
        assert_eq!(take_pending().as_deref(), Some("second"));
    }
}
