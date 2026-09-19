//! Session and project model, plus the event bus the UI subscribes to.
//!
//! `mc-core` deliberately depends on no other workspace crate. It knows nothing
//! about Claude and nothing about proot; both of those sit above it.

pub mod cancel;
pub mod chat;
pub mod event;
pub mod files;
pub mod http;
pub mod jobs;
pub mod library;
pub mod session;
pub mod store;
pub mod shell;

pub use cancel::Cancel;
pub use chat::{AgentCommand, AgentHandle, ChatItem, ChatLog, ToolState};
pub use event::{Event, EventBus, EventRx};
pub use files::{EntryKind, FileBrowser, FileEntry, FilePreview};
pub use shell::{ShellCommand, ShellLauncher};
pub use library::{SessionEntry, SessionLibrary};
pub use store::SessionStore;
pub use session::{Project, Session, SessionId, Turn};
