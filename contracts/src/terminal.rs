//! The user's interactive terminal in a sandbox (`ctrl+t`).
//!
//! A terminal is the user, not the agent: what is typed there isn't approved, and nothing of it
//! reaches the agent unless the user shares it. It runs in the same place as the agent's
//! commands, with the same reach.

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use serde::{Deserialize, Serialize};

use crate::SandboxError;

/// A terminal's size in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TerminalSize {
    /// Columns.
    pub cols: u16,
    /// Rows.
    pub rows: u16,
}

/// What an open terminal sends, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    /// Bytes the shell wrote. Raw: they carry escape sequences, so they are for a terminal
    /// emulator to interpret, never to pass on to a real terminal.
    Output(Vec<u8>),
    /// The terminal has ended (the shell exited, it was closed, the sandbox was reset or
    /// stopped). Nothing follows it. The reason is in words for the user.
    Closed {
        /// Why it ended, e.g. "the shell exited with code 0" or "the sandbox was reset".
        reason: String,
    },
}

/// Input to an open terminal. Dropping it closes the terminal, like [`Terminal::close`] but
/// without waiting.
#[async_trait]
pub trait Terminal: Send + Sync {
    /// Sends bytes to the shell as if typed (`\r` is Enter, `\x03` is ctrl+c). Returns once the
    /// terminal has taken them; an error once it has ended.
    async fn write(&self, input: &[u8]) -> Result<(), SandboxError>;

    /// Resizes the terminal; the shell sees the new size (`SIGWINCH`). An error once it has
    /// ended.
    async fn resize(&self, size: TerminalSize) -> Result<(), SandboxError>;

    /// Ends the shell and everything it started, and returns once they are gone. Closing a
    /// terminal that has already ended does nothing.
    async fn close(&self) -> Result<(), SandboxError>;
}

/// An open terminal: its input, and the stream of what it sends.
pub struct OpenTerminal {
    /// Write, resize and close. Dropping it closes the terminal.
    pub terminal: Box<dyn Terminal>,
    /// The output, then one [`TerminalEvent::Closed`], then the end of the stream. A backend
    /// may stop reading the shell's output while this isn't read (the shell then waits), but
    /// never drops any of it.
    pub events: BoxStream<'static, TerminalEvent>,
}

impl std::fmt::Debug for OpenTerminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenTerminal").finish_non_exhaustive()
    }
}
