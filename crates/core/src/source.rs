//! Capture abstraction. The pipeline only knows "a source produced a raw create
//! observation"; it does not care whether that came from a script, a directory watcher,
//! or later from a minifilter comms port.

use crate::types::{RawCreateEvent, SourceKind};

/// How trustworthy the creator identity of a source is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attribution {
    /// The source reports the creating process itself (driver, scripted test source).
    Exact,
    /// The source only knows that a directory appeared. Events from such a source cannot
    /// get a ledger row, because guessing an owner from a name is forbidden.
    DirectoryOnly,
}

#[derive(Debug, Clone)]
pub struct SourceError(pub String);

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "capture source error: {}", self.0)
    }
}

impl std::error::Error for SourceError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    Running,
    Idle,
    Disconnected,
}

impl LinkState {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkState::Running => "running",
            LinkState::Idle => "idle",
            LinkState::Disconnected => "disconnected",
        }
    }
}

pub trait EventSource: Send {
    fn kind(&self) -> SourceKind;
    fn attribution(&self) -> Attribution;

    /// Move everything currently available into `out`. Must not block.
    fn drain(&mut self, out: &mut Vec<RawCreateEvent>) -> Result<(), SourceError>;

    /// Whether the capture link is alive; surfaced by `appdir status`.
    fn link_state(&self) -> LinkState {
        LinkState::Running
    }

    /// Called when the daemon stops, so sources can release handles and threads.
    fn shutdown(&mut self) -> Result<(), SourceError> {
        Ok(())
    }
}
