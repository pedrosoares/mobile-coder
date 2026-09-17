//! Starting an interactive shell, in whatever sandbox the app runs.
//!
//! The terminal pane asks a [`ShellLauncher`] for a command and runs it on a
//! PTY. It never learns whether that is the host shell or proot over a rootfs,
//! the same boundary the agent's tools keep.

use std::{ffi::OsString, path::PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Added to the inherited environment.
    pub env: Vec<(String, OsString)>,
    pub cwd: Option<PathBuf>,
}

pub trait ShellLauncher: Send + Sync {
    /// The command for a new interactive shell. `Err` is shown to the user -
    /// on Android, typically "still installing".
    fn command(&self) -> Result<ShellCommand, String>;
}
