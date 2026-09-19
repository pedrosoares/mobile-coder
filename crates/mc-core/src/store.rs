//! Keeping a session across restarts.
//!
//! Only the transcript is saved - the same array sent to the model. The chat
//! view is rebuilt from it by [`crate::ChatLog::from_session`], so there is one
//! source of truth rather than two files that can disagree.
//!
//! Writes are atomic (write beside, then rename): the app is a phone app, it can
//! be killed at any moment, and a half-written session would lose the whole
//! conversation rather than the last turn.

use std::path::{Path, PathBuf};

use crate::Session;

#[derive(Debug, Clone)]
pub struct SessionStore {
    path: PathBuf,
}

impl SessionStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load the saved session, or `None` when there is none.
    ///
    /// A corrupt file is reported and ignored rather than crashing the app on
    /// launch: losing the history beats being unable to start.
    pub fn load(&self) -> Option<Session> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        match serde_json::from_str(&text) {
            Ok(session) => Some(session),
            Err(e) => {
                tracing::warn!(path = %self.path.display(), %e, "ignoring an unreadable session");
                None
            }
        }
    }

    pub fn save(&self, session: &Session) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let staging = self.path.with_extension("tmp");
        std::fs::write(&staging, serde_json::to_vec(session)?)?;
        std::fs::rename(&staging, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Project, Turn};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mc-store-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("session.json")
    }

    fn session_with_history() -> Session {
        let mut session = Session::new(Project { name: "demo".into(), path: "/root".into() });
        session.push(Turn::user_text("add a test"));
        session.push(Turn::assistant(serde_json::json!([
            { "type": "text", "text": "Done." }
        ])));
        session
    }

    #[test]
    fn a_session_survives_a_round_trip() {
        let store = SessionStore::new(scratch("roundtrip"));
        let session = session_with_history();
        store.save(&session).unwrap();

        let loaded = store.load().expect("saved sessions load");
        assert_eq!(loaded.id, session.id, "the same conversation, not a new one");
        assert_eq!(loaded.messages(), session.messages(), "the model sees identical history");
        let _ = std::fs::remove_dir_all(store.path().parent().unwrap());
    }

    #[test]
    fn nothing_saved_yet_is_not_an_error() {
        assert!(SessionStore::new(scratch("missing")).load().is_none());
    }

    #[test]
    fn a_corrupt_file_is_ignored_rather_than_fatal() {
        let path = scratch("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ this is not json").unwrap();
        assert!(SessionStore::new(&path).load().is_none());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn saving_twice_leaves_no_temporary_file_behind() {
        let store = SessionStore::new(scratch("atomic"));
        store.save(&session_with_history()).unwrap();
        store.save(&session_with_history()).unwrap();
        let dir = store.path().parent().unwrap();
        let leftovers: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "left {leftovers:?}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
