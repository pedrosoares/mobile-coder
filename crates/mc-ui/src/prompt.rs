//! Asking the user for a line of text, on a platform that cannot do it inline.
//!
//! Freya receives no on-screen keyboard text inside a `NativeActivity` (see
//! [`crate::safe_area`]), so on Android every field - a token, a repository
//! name, a commit message - has to be a native dialog. The UI cannot open one,
//! so it leaves a request here, the Android shell polls for it, shows the
//! dialog, and posts the answer back.
//!
//! On desktop there is no such problem and the pane renders its own input; the
//! same request/answer pair is used either way, so the pane has one code path
//! rather than two.

use std::sync::{
    Mutex,
    atomic::{AtomicU8, Ordering},
};

/// What is being asked for. The numbers cross the JNI boundary, so they are
/// part of the interface with `MainActivity` and must not be reordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prompt {
    GithubToken = 1,
    RepoName = 2,
    CommitMessage = 3,
}

impl Prompt {
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Prompt::GithubToken),
            2 => Some(Prompt::RepoName),
            3 => Some(Prompt::CommitMessage),
            _ => None,
        }
    }

    /// Title and hint for the dialog, so the wording lives with the UI rather
    /// than being duplicated in Kotlin.
    pub fn title(self) -> &'static str {
        match self {
            Prompt::GithubToken => "GitHub token",
            Prompt::RepoName => "New repository",
            Prompt::CommitMessage => "Commit message",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Prompt::GithubToken => "github_pat_…",
            Prompt::RepoName => "my-project",
            Prompt::CommitMessage => "What changed and why",
        }
    }

    /// Whether the text should be masked while typing.
    pub fn is_secret(self) -> bool {
        matches!(self, Prompt::GithubToken)
    }
}

static PENDING: AtomicU8 = AtomicU8::new(0);
static ANSWER: Mutex<Option<(Prompt, String)>> = Mutex::new(None);

/// Ask the shell to collect `prompt`.
pub fn request(prompt: Prompt) {
    PENDING.store(prompt as u8, Ordering::Relaxed);
}

/// Take the pending request, if any. Called by the Android shell.
pub fn take_request() -> Option<Prompt> {
    Prompt::from_code(PENDING.swap(0, Ordering::Relaxed))
}

/// Hand back what the user typed. An empty answer means they cancelled, and is
/// dropped here so no caller has to treat "" as a special case.
pub fn answer(prompt: Prompt, text: String) {
    if text.trim().is_empty() {
        return;
    }
    if let Ok(mut slot) = ANSWER.lock() {
        *slot = Some((prompt, text));
    }
}

/// Take the answer to `prompt`, if one has arrived.
///
/// Answers are taken by whoever asked: a commit message must not be collected
/// by the screen waiting for a repository name, which is what the kind is for.
pub fn take_answer(prompt: Prompt) -> Option<String> {
    let mut slot = ANSWER.lock().ok()?;
    match slot.as_ref() {
        Some((kind, _)) if *kind == prompt => slot.take().map(|(_, text)| text),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_is_taken_once_and_survives_the_trip_through_a_number() {
        request(Prompt::CommitMessage);
        assert_eq!(take_request(), Some(Prompt::CommitMessage));
        assert_eq!(take_request(), None);
    }

    #[test]
    fn an_answer_only_reaches_whoever_asked_for_it() {
        answer(Prompt::RepoName, "my-project".into());
        assert_eq!(take_answer(Prompt::CommitMessage), None, "not this screen's answer");
        assert_eq!(take_answer(Prompt::RepoName).as_deref(), Some("my-project"));
        assert_eq!(take_answer(Prompt::RepoName), None, "taken once");

        // Cancelling is an empty answer, and is not an answer at all.
        answer(Prompt::RepoName, "   ".into());
        assert_eq!(take_answer(Prompt::RepoName), None);
    }
}
