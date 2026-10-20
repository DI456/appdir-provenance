///! Ingest pipeline: monitor view + filter verdict + SQLite ledger.

use crate::db::{Db, InsertOutcome};
use crate::filter::{self, admit, CreateRequest, MonitorView, Verdict};
use crate::types::{DirCreateRecord, IgnoreReason, RawCreateEvent, RecordStatus, SourceKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestOutcome {
    /// Newly written ledger row.
    Recorded { directory_id: i64, record: DirCreateRecord },
    /// The directory existed and had been deleted before; row refreshed.
    Reregistered { directory_id: i64, record: DirCreateRecord },
    /// Duplicate of an existing active record; nothing written.
    Duplicate { directory_id: i64, reason: IgnoreReason },
    /// Rejected by the create-semantics or path filter; only counted in diagnostics.
    Ignored { reason: IgnoreReason, record_hint: Option<DirCreateRecord> },
}

impl IngestOutcome {
    pub fn label(&self) -> &'static str {
        match self {
            IngestOutcome::Recorded { .. } => "recorded",
            IngestOutcome::Reregistered { .. } => "reregistered",
            IngestOutcome::Duplicate { .. } => "duplicate",
            IngestOutcome::Ignored { reason, .. } => IngestOutcome::reason_code(reason),
        }
    }

    fn reason_code(r: &IgnoreReason) -> &'static str {
        match r {
            IgnoreReason::Failed => "ignored:failed",
            IgnoreReason::NotDirectory => "ignored:not-directory",
            IgnoreReason::OpenedExisting => "ignored:opened-existing",
            IgnoreReason::DisallowedDisposition => "ignored:bad-disposition",
            IgnoreReason::OutsideMonitor => "ignored:outside-monitor",
            IgnoreReason::NotDirectChild => "ignored:not-direct-child",
            IgnoreReason::IsMonitorRoot => "ignored:is-monitor-root",
            IgnoreReason::CreatorUnknown => "ignored:creator-unknown",
            IgnoreReason::AlreadyRegistered => "ignored:already-registered",
            IgnoreReason::BadPath => "ignored:bad-path",
        }
    }
}

pub struct Ledger<'a> {
    pub db: &'a Db,
    pub view: MonitorView,
}

impl<'a> Ledger<'a> {
    /// Reload the monitor configuration from SQLite.
    pub fn new(db: &'a Db) -> crate::Result<Ledger<'a>> {
        let mut roots = db.roots()?;
        for r in &mut roots {
            if !crate::path::parse(&r.path).map(|p| p.is_root()).unwrap_or(false) {
                // store roots without trailing separator for readable output
                r.path = crate::path::normalize(&r.path).unwrap_or_else(|| r.path.clone());
            }
        }
        Ok(Ledger {
            db,
            view: MonitorView::from_roots(roots),
        })
    }

    pub fn with_view(db: &'a Db, view: MonitorView) -> Ledger<'a> {
        Ledger { db, view }
    }

    /// Refresh `record_status` for every row against the current file system: a ledger
    /// row whose directory disappeared becomes `missing` rather than pretending to be
    /// valid. A row whose directory reappeared is flagged `needs-review`, because we can
    /// no longer prove it is the same object. Returns how many rows changed.
    pub fn reconcile(&self, now_ms: i64) -> crate::Result<usize> {
        let mut changed = 0usize;
        for row in self.db.all_dirs()? {
            let exists = std::fs::symlink_metadata(&row.directory_path).is_ok();
            let want = if exists {
                match row.record_status {
                    RecordStatus::Missing => RecordStatus::NeedsReview,
                    other => other,
                }
            } else {
                RecordStatus::Missing
            };
            if want != row.record_status {
                self.db.set_status(row.directory_id, want, now_ms)?;
                changed += 1;
            }
        }
        Ok(changed)
    }

    /// Feed one observation. `CreatorUnknown` and other rejections are recorded as
    /// diagnostic counts, never as ledger rows.
    pub fn ingest(&self, ev: &RawCreateEvent) -> crate::Result<IngestOutcome> {
        match admit(ev, &self.view) {
            Verdict::Accept(record) => match self.db.insert_or_reregister(&record)? {
                InsertOutcome::Recorded { directory_id } => {
                    Ok(IngestOutcome::Recorded { directory_id, record })
                }
                InsertOutcome::Reregistered { directory_id } => {
                    Ok(IngestOutcome::Reregistered { directory_id, record })
                }
                InsertOutcome::AlreadyRegistered { directory_id } => {
                    Ok(IngestOutcome::Duplicate {
                        directory_id,
                        reason: IgnoreReason::AlreadyRegistered,
                    })
                }
            },
            Verdict::Ignore(reason) => {
                let hint = if reason == IgnoreReason::CreatorUnknown || reason == IgnoreReason::AlreadyRegistered {
                    None
                } else {
                    None
                };
                if should_persist_diagnostic(&reason) {
                    let pid = ev.creator.pid;
                    let exe = ev.creator.exe_path.clone();
                    let fake = DirCreateRecord {
                        directory_path: ev.target_path.clone(),
                        monitor_root: String::new(),
                        exe_path: exe.clone().unwrap_or_default(),
                        exe_name: String::new(),
                        creator_pid: pid,
                        created_at_ms: ev.observed_at_ms,
                        observed_at_ms: ev.observed_at_ms,
                        capture_source: ev.source,
                        attribution_confidence: ev.source.default_confidence().to_string(),
                    };
                    self.db
                        .log_ignored(reason.code(), &fake, pid, exe.as_deref())?;
                }
                Ok(IngestOutcome::Ignored { reason, record_hint: hint })
            }
        }
    }
}

/// Events worth keeping in the diagnostic table. Create-semantics noise (regular file
/// creates) is skipped because it would flood the log.
fn should_persist_diagnostic(reason: &IgnoreReason) -> bool {
    matches!(
        reason,
        IgnoreReason::CreatorUnknown | IgnoreReason::Failed | IgnoreReason::IsMonitorRoot
    )
}

/// Convenience for tests and the simulator source.
pub fn raw(
    target_path: &str,
    request: CreateRequest,
    information: u32,
    success: bool,
    pid: u32,
    exe: Option<&str>,
    observed_at_ms: i64,
) -> RawCreateEvent {
    RawCreateEvent {
        target_path: target_path.to_string(),
        request,
        information,
        success,
        creator: crate::types::ProcessIdentity { pid, exe_path: exe.map(|s| s.to_string()) },
        observed_at_ms,
        source: SourceKind::Sim,
    }
}

pub use filter::FILE_DIRECTORY_FILE;
