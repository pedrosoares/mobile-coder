//! Interactive PTY sessions.
//!
//! Separate from `mc-sandbox` on purpose. A tool call wants captured stdout,
//! captured stderr and an exit code; a terminal wants a live duplex stream with a
//! window size. Serving both from one abstraction produces a bad version of each,
//! so they stay apart and share only the command-building logic.
//!
//! # To verify on device
//!
//! bionic provides `openpty`/`forkpty`, and `/dev/ptmx` is reachable from an app
//! sandbox - Termux depends on both. Confirm it directly early rather than
//! discovering otherwise late; `tools/exec-probe` includes the check.

use std::{
    io::{Read, Write},
    sync::{Arc, Mutex},
};

use portable_pty::{CommandBuilder, NativePtySystem, PtyPair, PtySize, PtySystem};

#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("allocating a pty failed: {0}")]
    Open(String),
    #[error("spawning into the pty failed: {0}")]
    Spawn(String),
    #[error("pty io failed: {0}")]
    Io(#[from] std::io::Error),
}

/// A running interactive shell attached to a pseudo-terminal.
pub struct PtySession {
    pair: PtyPair,
    writer: Box<dyn Write + Send>,
    _child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl PtySession {
    /// Spawn `command` on a new PTY sized `cols` x `rows`.
    pub fn spawn(command: CommandBuilder, cols: u16, rows: u16) -> Result<Self, PtyError> {
        let pty = NativePtySystem::default();
        let pair = pty
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| PtyError::Open(e.to_string()))?;

        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|e| PtyError::Spawn(e.to_string()))?;

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| PtyError::Spawn(e.to_string()))?;

        Ok(Self {
            pair,
            writer,
            _child: child,
        })
    }

    /// Feed bytes to the terminal - keystrokes, pasted text.
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), PtyError> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Tell the program its window changed. Without this, full-screen programs
    /// draw to the wrong size after a rotation or a keyboard appearing - which on
    /// a phone is most of the time.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), PtyError> {
        self.pair
            .master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| PtyError::Open(e.to_string()))
    }

    /// Take a reader for the terminal's output.
    ///
    /// Blocking, so it belongs on its own thread; the returned bytes should be
    /// forwarded onto `mc-core`'s event bus as `Event::PtyOutput`.
    pub fn reader(&self) -> Result<Box<dyn Read + Send>, PtyError> {
        self.pair
            .master
            .try_clone_reader()
            .map_err(|e| PtyError::Open(e.to_string()))
    }
}

/// Convenience alias for a session shared between the reader thread and the UI.
pub type SharedPty = Arc<Mutex<PtySession>>;
