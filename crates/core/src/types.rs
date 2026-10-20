///! Shared data model for the directory provenance ledger.

/// A directory path as stored by us: normalized, absolute, Windows style.
pub type DbPath = String;

/// Where a raw create observation came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// Deterministic scripted events. Used by unit tests and `selftest`.
    Sim,
    /// User-mode ReadDirectoryChangesW watcher. Reports directory appearance only.
    Watch,
    /// Real-time ETW user-mode source: Microsoft-Windows-Kernel-File Create(12)
    /// events, confirmed by the non-recursive watcher before a record is written.
    Etw,
    /// Reserved for the future minifilter comms port. Never active in this build.
    Minifilter,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::Sim => "sim",
            SourceKind::Watch => "watch",
            SourceKind::Etw => "etw",
            SourceKind::Minifilter => "minifilter",
        }
    }

    /// Attribution confidence stored with every ledger row, per the source's nature.
    pub fn default_confidence(self) -> &'static str {
        match self {
            SourceKind::Sim => "scripted-exact",
            SourceKind::Watch => "watch-only",
            SourceKind::Etw => "etw-create-confirmed",
            SourceKind::Minifilter => "minifilter-exact",
        }
    }

    pub fn parse(s: &str) -> Option<SourceKind> {
        match s.to_ascii_lowercase().as_str() {
            "sim" | "simulation" => Some(SourceKind::Sim),
            "watch" | "rdcw" => Some(SourceKind::Watch),
            "etw" | "kernel-file" => Some(SourceKind::Etw),
            "minifilter" | "driver" => Some(SourceKind::Minifilter),
            _ => None,
        }
    }
}

/// Process identity as reported by a capture source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    /// Full image path, normalized. `None` means the source could not resolve it.
    pub exe_path: Option<DbPath>,
}

/// One candidate `IRP_MJ_CREATE`-style observation handed to the filter.
#[derive(Debug, Clone)]
pub struct RawCreateEvent {
    /// Directory that the request targeted, already mapped to a drive-letter path when possible.
    pub target_path: DbPath,
    pub request: crate::filter::CreateRequest,
    /// `IoStatus.Information` value from the completion.
    pub information: u32,
    /// False when the I/O completed with a failure status.
    pub success: bool,
    pub creator: ProcessIdentity,
    pub observed_at_ms: i64,
    pub source: SourceKind,
}

/// Reason an observation was not turned into a ledger record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoreReason {
    /// I/O did not complete successfully.
    Failed,
    /// Request did not ask for a directory object.
    NotDirectory,
    /// Object was opened, not created.
    OpenedExisting,
    /// Disposition cannot create a new object.
    DisallowedDisposition,
    /// Not under any enabled monitor root.
    OutsideMonitor,
    /// Deeper than the direct-child level.
    NotDirectChild,
    /// The created directory is a monitor root itself.
    IsMonitorRoot,
    /// No reliable full image path for the creator.
    CreatorUnknown,
    /// Already registered and still valid.
    AlreadyRegistered,
    /// Target path could not be normalized.
    BadPath,
}

impl IgnoreReason {
    pub fn code(self) -> &'static str {
        match self {
            IgnoreReason::Failed => "failed",
            IgnoreReason::NotDirectory => "not-directory",
            IgnoreReason::OpenedExisting => "opened-existing",
            IgnoreReason::DisallowedDisposition => "bad-disposition",
            IgnoreReason::OutsideMonitor => "outside-monitor",
            IgnoreReason::NotDirectChild => "not-direct-child",
            IgnoreReason::IsMonitorRoot => "is-monitor-root",
            IgnoreReason::CreatorUnknown => "creator-unknown",
            IgnoreReason::AlreadyRegistered => "already-registered",
            IgnoreReason::BadPath => "bad-path",
        }
    }
}

/// Lifecycle of a ledger row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordStatus {
    Active,
    Missing,
    NeedsReview,
}

impl RecordStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RecordStatus::Active => "active",
            RecordStatus::Missing => "missing",
            RecordStatus::NeedsReview => "needs-review",
        }
    }

    pub fn parse(s: &str) -> Option<RecordStatus> {
        match s {
            "active" => Some(RecordStatus::Active),
            "missing" => Some(RecordStatus::Missing),
            "needs-review" => Some(RecordStatus::NeedsReview),
            _ => None,
        }
    }
}

/// A confirmed directory creation, attributed to one executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirCreateRecord {
    pub directory_path: DbPath,
    pub monitor_root: DbPath,
    pub exe_path: DbPath,
    pub exe_name: String,
    pub creator_pid: u32,
    pub created_at_ms: i64,
    pub observed_at_ms: i64,
    /// Which capture source produced this record (`sim` / `watch` / `etw` / `minifilter`).
    pub capture_source: SourceKind,
    /// Attribution confidence label for the creator identity (see `SourceKind::default_confidence`).
    pub attribution_confidence: String,
}

/// A configured watch root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorRoot {
    pub path: DbPath,
    pub enabled: bool,
    pub added_at_ms: i64,
}
