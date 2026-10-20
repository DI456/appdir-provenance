//! Capture sources. Nothing in this crate touches kernel mode: sources are
//! plain user-mode file-system notifications plus an ETW-based creator source.
//!
//! `EventSource` is deliberately the only seam between "how we learn about a created
//! directory" and "everything the ledger does with that knowledge", so a future
//! minifilter comms source can be dropped in without touching storage, query or cleanup.

pub mod etw;
pub mod watch;

pub use etw::EtwSource;
pub use watch::{WatchSource, WatchStatus};

use appdir_core::{IgnoreReason, RawCreateEvent};

/// Text for operators: what a source's attribution actually means for the ledger.
pub fn attribution_note(attr: appdir_core::Attribution) -> &'static str {
    match attr {
        appdir_core::Attribution::Exact => "creator EXE path is supplied by the source",
        appdir_core::Attribution::DirectoryOnly => {
            "directory appearance only; creator unknown, so records are ignored (never guessed)"
        }
    }
}

/// Summary counters a daemon keeps per source, used by `appdir status`.
#[derive(Debug, Default, Clone)]
pub struct Counters {
    pub received: u64,
    pub recorded: u64,
    pub reregistered: u64,
    pub duplicate: u64,
    pub ignored: u64,
    pub by_reason: Vec<(IgnoreReason, u64)>,
}

impl Counters {
    pub fn tally(outcome: &appdir_core::IngestOutcome, ev: &RawCreateEvent, c: &mut Counters) {
        c.received += 1;
        use appdir_core::IngestOutcome as I;
        match outcome {
            I::Recorded { .. } => c.recorded += 1,
            I::Reregistered { .. } => c.reregistered += 1,
            I::Duplicate { .. } => c.duplicate += 1,
            I::Ignored { reason, .. } => {
                c.ignored += 1;
                match c.by_reason.iter_mut().find(|(r, _)| r == reason) {
                    Some((_, n)) => *n += 1,
                    None => c.by_reason.push((*reason, 1)),
                }
            }
        }
        let _ = ev;
    }
}
