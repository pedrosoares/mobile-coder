//! Many conversations, one directory.
//!
//! A coding session is not one long thread: a bug fix, a dependency upgrade and
//! a question about a file have nothing to do with each other, and putting them
//! in one transcript costs money on every later turn (it is all re-sent) and
//! makes the model worse at each of them. So chats are separate, and this is
//! where they live - one file per chat, named by its id.
//!
//! The directory is the index. There is no separate catalogue to fall out of
//! step with it: listing reads the files. That costs a parse per chat when the
//! list is opened, which on a phone with a few dozen chats is nothing, and it
//! means a chat that exists is always listed and one that was deleted never is.

use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use crate::{Session, SessionId, SessionStore};

/// One chat, as the picker shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    pub id: SessionId,
    pub title: String,
    /// Messages in the transcript, which is a fair measure of "how much is in
    /// here" without opening it.
    pub turns: usize,
    pub updated: SystemTime,
}

#[derive(Debug, Clone)]
pub struct SessionLibrary {
    dir: PathBuf,
}

impl SessionLibrary {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path_of(&self, id: SessionId) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// The store for one chat, so saving goes through the same atomic write as
    /// before.
    pub fn store_for(&self, id: SessionId) -> SessionStore {
        SessionStore::new(self.path_of(id))
    }

    /// Take over a single-session file written before chats were plural.
    ///
    /// Runs once: after the move there is no legacy file left. Losing that
    /// conversation would be a data loss the user never agreed to, so it is
    /// adopted rather than ignored.
    pub fn adopt(&self, legacy: &Path) {
        if !legacy.exists() {
            return;
        }
        let Some(session) = SessionStore::new(legacy).load() else { return };
        if self.save(&session).is_ok() {
            let _ = std::fs::remove_file(legacy);
            tracing::info!(id = %session.id, "adopted the previous single session");
        }
    }

    /// Every chat, most recently changed first.
    pub fn list(&self) -> Vec<SessionEntry> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return Vec::new() };
        let mut chats: Vec<SessionEntry> = entries
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|e| e == "json"))
            .filter_map(|entry| {
                let session = SessionStore::new(entry.path()).load()?;
                let updated = entry
                    .metadata()
                    .and_then(|meta| meta.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                Some(SessionEntry {
                    id: session.id,
                    title: session.display_title(),
                    turns: session.transcript.len(),
                    updated,
                })
            })
            .collect();
        chats.sort_by_key(|chat| std::cmp::Reverse(chat.updated));
        chats
    }

    pub fn load(&self, id: SessionId) -> Option<Session> {
        self.store_for(id).load()
    }

    /// The chat to open at startup: whichever was last written.
    pub fn most_recent(&self) -> Option<Session> {
        self.list().first().and_then(|entry| self.load(entry.id))
    }

    pub fn save(&self, session: &Session) -> std::io::Result<()> {
        self.store_for(session.id).save(session)
    }

    /// Forget a chat. Gone means gone: the transcript is the only copy.
    pub fn delete(&self, id: SessionId) -> std::io::Result<()> {
        match std::fs::remove_file(self.path_of(id)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Project, Turn};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mc-library-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn session_with(prompt: &str) -> Session {
        let mut session = Session::new(Project { name: "demo".into(), path: "/root".into() });
        session.push(Turn::user_text(prompt));
        session
    }

    #[test]
    fn chats_are_listed_newest_first_and_titled_by_what_was_asked() {
        let dir = scratch("listing");
        let library = SessionLibrary::new(&dir);

        let first = session_with("fix the parser crash");
        library.save(&first).unwrap();
        // Filesystem timestamps are coarse; make the order unambiguous.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let second = session_with("upgrade tokio");
        library.save(&second).unwrap();

        let chats = library.list();
        assert_eq!(chats.len(), 2);
        assert_eq!(chats[0].title, "upgrade tokio");
        assert_eq!(chats[1].title, "fix the parser crash");
        assert_eq!(chats[0].turns, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_chat_can_be_opened_by_id_and_deleted() {
        let dir = scratch("roundtrip");
        let library = SessionLibrary::new(&dir);
        let session = session_with("hello");
        library.save(&session).unwrap();

        assert_eq!(library.load(session.id).map(|s| s.id), Some(session.id));
        library.delete(session.id).unwrap();
        assert!(library.load(session.id).is_none());
        assert!(library.list().is_empty());
        // Deleting what is not there is not a failure - two taps on Delete
        // should not produce an error the second time.
        library.delete(session.id).unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_previous_single_session_file_is_adopted_not_lost() {
        let dir = scratch("adopt");
        let legacy = dir.join("session.json");
        let session = session_with("from before");
        SessionStore::new(&legacy).save(&session).unwrap();

        let library = SessionLibrary::new(dir.join("sessions"));
        library.adopt(&legacy);

        assert!(!legacy.exists(), "the old file is moved, not copied");
        assert_eq!(library.list().len(), 1);
        assert_eq!(library.load(session.id).map(|s| s.id), Some(session.id));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_newest_chat_is_the_one_reopened() {
        let dir = scratch("recent");
        let library = SessionLibrary::new(&dir);
        library.save(&session_with("older")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let newer = session_with("newer");
        library.save(&newer).unwrap();

        assert_eq!(library.most_recent().map(|s| s.id), Some(newer.id));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
