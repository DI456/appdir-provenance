///! Backup and removal with mandatory re-validation.
///!
///! Deleting is the only destructive part of this tool. Every candidate is re-checked
///! against the *current* monitor configuration and the *current* file system state right
///! before anything is removed: a ledger row alone is never enough authority.

use crate::db::Db;
use crate::error::{Error, Result};
use crate::path::{self, WinPath};
use crate::types::{RecordStatus, DbPath};
use std::fs;
use std::path::{Path, PathBuf};

// Win32 FILE_ATTRIBUTE_* bits.
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// FILETIME epoch (1601-01-01) offset from the Unix epoch, in milliseconds.
pub const FILETIME_TO_UNIX_MS: i64 = 116_444_736_000_000;

/// How far the real directory creation time may drift from the ledger observation time
/// before we consider the path to be a different directory.
pub const CREATE_TIME_TOLERANCE_MS: i64 = 5 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockReason {
    NoLedgerRow,
    OwnerMismatch,
    NotInMonitorRoot,
    NotDirectChild,
    Missing,
    NotADirectory,
    ReparsePoint,
    WouldDeleteRoot,
    IdentityChanged,
    BackupFailed,
    /// The directory could not even be renamed away, which means an application is still
    /// holding something inside it open.
    InUse,
    SelfOwned,
    IoError,
}

impl BlockReason {
    pub fn code(self) -> &'static str {
        match self {
            BlockReason::NoLedgerRow => "no-ledger-row",
            BlockReason::OwnerMismatch => "owner-mismatch",
            BlockReason::NotInMonitorRoot => "not-in-monitor-root",
            BlockReason::NotDirectChild => "not-direct-child",
            BlockReason::Missing => "missing",
            BlockReason::NotADirectory => "not-a-directory",
            BlockReason::ReparsePoint => "reparse-point",
            BlockReason::WouldDeleteRoot => "would-delete-root",
            BlockReason::IdentityChanged => "identity-changed",
            BlockReason::BackupFailed => "backup-failed",
            BlockReason::InUse => "in-use",
            BlockReason::SelfOwned => "self-owned",
            BlockReason::IoError => "io-error",
        }
    }

    pub fn explain(self) -> &'static str {
        match self {
            BlockReason::NoLedgerRow => "no ledger record for this path",
            BlockReason::OwnerMismatch => "recorded owner EXE does not match the request",
            BlockReason::NotInMonitorRoot => "path is not under any enabled monitor root",
            BlockReason::NotDirectChild => "path is not a direct child of a monitor root",
            BlockReason::Missing => "directory no longer exists",
            BlockReason::NotADirectory => "path exists but is not a directory",
            BlockReason::ReparsePoint => "path is a reparse point (symlink or junction)",
            BlockReason::WouldDeleteRoot => "refusing to delete a monitor root or volume root",
            BlockReason::IdentityChanged => "directory identity differs from the ledger",
            BlockReason::BackupFailed => "backup did not complete; deletion aborted",
            BlockReason::InUse => "an application is still using this directory",
            BlockReason::SelfOwned => "path belongs to this tool's own data directory",
            BlockReason::IoError => "file system error",
        }
    }
}

/// Volume serial plus file record id plus creation time: enough to tell two same-named
/// directories apart, including "deleted then created again".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirIdentity {
    pub volume_serial: u64,
    pub file_index: u128,
    pub attributes: u32,
    pub creation_unix_ms: i64,
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub directory_path: DbPath,
    pub monitor_root: DbPath,
    pub directory_id: i64,
    pub app_exe_path: DbPath,
    pub created_at_ms: i64,
    pub record_status: RecordStatus,
    pub identity: Option<DirIdentity>,
    pub blocked: Option<(BlockReason, String)>,
}

impl Candidate {
    pub fn deletable(&self) -> bool {
        self.blocked.is_none()
    }

    pub fn block_text(&self) -> String {
        match &self.blocked {
            Some((r, detail)) => format!("{}: {detail}", r.code()),
            None => String::new(),
        }
    }

    /// Drift between the real creation time and the ledger observation time.
    pub fn creation_gap_ms(&self) -> Option<i64> {
        self.identity
            .as_ref()
            .map(|i| (i.creation_unix_ms - self.created_at_ms).abs())
    }
}

#[derive(Debug, Clone)]
pub struct CleanOptions<'a> {
    /// Where copies are written. `None` means "do not back up".
    pub backup_dir: Option<&'a Path>,
    /// Actual removal only when true; otherwise this is a plan/report run.
    pub commit: bool,
    /// Paths that must never be deleted, e.g. this tool's own data directory.
    pub protected_prefixes: Vec<DbPath>,
}

impl<'a> CleanOptions<'a> {
    pub fn dry_run() -> Self {
        CleanOptions {
            backup_dir: None,
            commit: false,
            protected_prefixes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Planned,
    Skipped,
    Failed,
}

#[derive(Debug, Clone)]
pub struct ActionItem {
    pub directory_path: DbPath,
    pub action: Action,
    pub detail: String,
    pub backup_path: Option<PathBuf>,
    /// Set when the directory really was removed.
    pub deleted: bool,
    /// Set when a backup copy was produced.
    pub backed_up: bool,
}

#[derive(Debug, Default, Clone)]
pub struct CleanReport {
    pub candidates: Vec<Candidate>,
    pub actions: Vec<ActionItem>,
    pub committed: bool,
    pub deleted: usize,
    pub backed_up: usize,
    pub skipped: usize,
    pub failed: usize,
}

impl CleanReport {
    pub fn deletable_count(&self) -> usize {
        self.candidates.iter().filter(|c| c.deletable()).count()
    }
    pub fn blocked_count(&self) -> usize {
        self.candidates.iter().filter(|c| !c.deletable()).count()
    }
    pub fn has_backup_failure(&self) -> bool {
        self.actions.iter().any(|a| a.detail.contains("backup"))
    }
}

/// Directory identity: volume serial + file record id + creation time + attributes.
///
/// `GetFileInformationByHandle` is used because a path name alone cannot tell "the same
/// directory" from "deleted and created again with the same name".
pub fn read_identity(dir: &Path) -> Option<DirIdentity> {
    #[cfg(windows)]
    {
        let info = crate::winfile::stat(dir).ok()?;
        Some(DirIdentity {
            volume_serial: info.volume_serial_number as u64,
            file_index: crate::winfile::file_index(&info),
            attributes: info.file_attributes,
            creation_unix_ms: info.creation_time.to_unix_ms(),
        })
    }
    #[cfg(not(windows))]
    {
        let md = fs::metadata(dir).ok()?;
        Some(DirIdentity {
            volume_serial: 0,
            file_index: 0,
            attributes: 0,
            creation_unix_ms: md
                .created()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
        })
    }
}

fn blocked(dir: &str, root: &str, directory_id: i64, exe: &str, reason: BlockReason, detail: &str) -> Candidate {
    Candidate {
        directory_path: dir.to_string(),
        monitor_root: root.to_string(),
        directory_id,
        app_exe_path: exe.to_string(),
        created_at_ms: 0,
        record_status: RecordStatus::NeedsReview,
        identity: None,
        blocked: Some((reason, detail.to_string())),
    }
}

/// Verify one path is safe to delete *right now*, not what the database once said.
pub fn verify(db: &Db, raw_path: &str, expect_exe: Option<&str>, protected: &[DbPath]) -> Result<Candidate> {
    let norm = match path::normalize(raw_path) {
        Some(p) => p,
        None => return Ok(blocked(raw_path, "", -1, "", BlockReason::NoLedgerRow, "path cannot be normalized")),
    };
    let target = PathBuf::from(&norm);

    // Structural refusal first: never consult the ledger for a path that must not be
    // deleted, and never treat "no record" as permission.
    let parsed_target = match path::parse(&norm) {
        Some(p) => p,
        None => {
            return Ok(blocked(&norm, "", -1, "", BlockReason::NotDirectChild, "unparsable path"));
        }
    };
    let roots = db.roots()?;
    let enabled: Vec<WinPath> = roots.iter().filter(|r| r.enabled).filter_map(|r| path::parse(&r.path)).collect();
    if parsed_target.is_root() {
        return Ok(blocked(&norm, "", -1, "", BlockReason::WouldDeleteRoot, "volume or share root"));
    }
    if enabled.iter().any(|rp| rp == &parsed_target) {
        return Ok(blocked(&norm, "", -1, "", BlockReason::WouldDeleteRoot, "this path is a monitor root itself"));
    }
    // A path inside this tool's own data directory is never a candidate either.
    if protected.iter().any(|p| path::is_within(p, &norm)) {
        return Ok(blocked(&norm, "", -1, "", BlockReason::SelfOwned, "inside this tool's data directory"));
    }
    let row = match db.dir_by_path(&norm)? {
        Some(r) => r,
        None => return Ok(blocked(&norm, "", -1, "", BlockReason::NoLedgerRow, "no row in directories table")),
    };
    let app = db.app_by_id(row.app_id)?;
    let exe = app.map(|a| a.exe_path).unwrap_or_default();
    let mut cand = Candidate {
        directory_path: norm.clone(),
        monitor_root: row.monitor_root.clone(),
        directory_id: row.directory_id,
        app_exe_path: exe.clone(),
        created_at_ms: row.created_at_ms,
        record_status: row.record_status,
        identity: None,
        blocked: None,
    };

    // 1. requested owner must match the recorded owner
    if let Some(want) = expect_exe {
        if !path::same_path(want, &exe) {
            cand.blocked = Some((BlockReason::OwnerMismatch, format!("ledger says {exe}, request says {want}")));
            return Ok(cand);
        }
    }

    // 2. never delete our own data directory (already checked before the ledger lookup)

    // 3. must still be covered by an *enabled* monitor root today
    // Multiple enabled roots may cover the target (e.g. both C:\Users\X and
    // C:\Users\X\AppData\Local). The most specific (deepest) root wins, so the
    // direct-child check is evaluated against the root the event actually came from.
    let root = match enabled
        .iter()
        .filter(|rp| parsed_target.is_within(rp))
        .max_by_key(|rp| rp.comps.len())
    {
        Some(rp) => rp,
        None => {
            cand.blocked = Some((
                BlockReason::NotInMonitorRoot,
                format!("no enabled monitor root covers {norm}"),
            ));
            return Ok(cand);
        }
    };
    if root.is_root() {
        cand.blocked = Some((BlockReason::WouldDeleteRoot, norm.clone()));
        return Ok(cand);
    }
    if !parsed_target.is_direct_child_of(&root) {
        cand.blocked = Some((
            BlockReason::NotDirectChild,
            format!("{norm} is not an immediate child of {}", root.as_str()),
        ));
        return Ok(cand);
    }
    cand.monitor_root = root.as_str();

    // 4. file system reality: exists, is a directory, is not a link or junction
    let meta = match fs::symlink_metadata(&target) {
        Ok(m) => m,
        Err(_) => {
            cand.blocked = Some((BlockReason::Missing, format!("{norm} does not exist")));
            return Ok(cand);
        }
    };
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        let attrs = meta.file_attributes();
        if attrs & FILE_ATTRIBUTE_DIRECTORY == 0 {
            cand.blocked = Some((BlockReason::NotADirectory, norm.clone()));
            return Ok(cand);
        }
        if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            cand.blocked = Some((BlockReason::ReparsePoint, norm.clone()));
            return Ok(cand);
        }
    }
    #[cfg(not(windows))]
    {
        if !meta.is_dir() {
            cand.blocked = Some((BlockReason::NotADirectory, norm.clone()));
            return Ok(cand);
        }
        if meta.file_type().is_symlink() {
            cand.blocked = Some((BlockReason::ReparsePoint, norm.clone()));
            return Ok(cand);
        }
    }
    let identity = match read_identity(&target) {
        Some(i) => i,
        None => {
            cand.blocked = Some((BlockReason::Missing, format!("{norm} cannot be opened")));
            return Ok(cand);
        }
    };

    // 5. stale rows are never auto-deleted
    if row.record_status == RecordStatus::NeedsReview {
        cand.blocked = Some((BlockReason::IdentityChanged, "record is flagged needs-review".to_string()));
        return Ok(cand);
    }
    if row.record_status == RecordStatus::Missing {
        cand.blocked = Some((BlockReason::Missing, "record is already marked missing".to_string()));
        return Ok(cand);
    }

    // 6. same name is not the same directory: creation time must agree
    cand.identity = Some(identity);
    if let Some(gap) = cand.creation_gap_ms() {
        if row.created_at_ms > 0 && gap > CREATE_TIME_TOLERANCE_MS {
            cand.blocked = Some((
                BlockReason::IdentityChanged,
                format!("creation time differs from ledger by {} s", gap / 1000),
            ));
        }
    }
    Ok(cand)
}

/// Plan for every directory recorded against one EXE path.
pub fn plan_for_exe(db: &Db, exe_path: &str, protected: &[DbPath]) -> Result<Vec<Candidate>> {
    let rows = db.dirs_of_exe(exe_path, true)?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push(verify(db, &r.directory_path, Some(exe_path), protected)?);
    }
    Ok(out)
}

/// Recursively copy a directory tree; returns the number of copied files.
pub fn backup_tree(src: &Path, dst: &Path) -> Result<usize> {
    fs::create_dir_all(dst).map_err(|e| Error::Io {
        path: dst.display().to_string(),
        message: e.to_string(),
    })?;
    let mut copied = 0usize;
    let mut stack = vec![src.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rel = dir.strip_prefix(src).unwrap_or(Path::new("")).to_path_buf();
        let out_dir = dst.join(rel);
        fs::create_dir_all(&out_dir).map_err(|e| Error::Io {
            path: out_dir.display().to_string(),
            message: e.to_string(),
        })?;
        let rd = fs::read_dir(&dir).map_err(|e| Error::Io {
            path: dir.display().to_string(),
            message: e.to_string(),
        })?;
        for entry in rd {
            let entry = entry.map_err(|e| Error::Io {
                path: dir.display().to_string(),
                message: e.to_string(),
            })?;
            let p = entry.path();
            let ft = entry.file_type().map_err(|e| Error::Io {
                path: p.display().to_string(),
                message: e.to_string(),
            })?;
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file() {
                let to = out_dir.join(entry.file_name());
                fs::copy(&p, &to).map_err(|e| Error::Io {
                    path: p.display().to_string(),
                    message: e.to_string(),
                })?;
                copied += 1;
            } else {
                return Err(Error::Blocked(format!(
                    "backup aborted: {} is neither a plain file nor a directory",
                    p.display()
                )));
            }
        }
    }
    Ok(copied)
}

/// Report, and optionally perform, removal of one already-verified candidate.
pub fn clean_one(db: &Db, cand: &Candidate, opts: &CleanOptions, now_ms: i64) -> Result<ActionItem> {
    let fail = |action: Action, detail: String, backed_up: bool, backup_path: Option<PathBuf>| ActionItem {
        directory_path: cand.directory_path.clone(),
        action,
        detail,
        backup_path,
        deleted: false,
        backed_up,
    };

    if let Some((r, detail)) = &cand.blocked {
        return Ok(fail(Action::Skipped, format!("{}: {detail}", r.code()), false, None));
    }
    let target = PathBuf::from(&cand.directory_path);
    if !opts.commit {
        return Ok(fail(
            Action::Planned,
            format!(
                "would {}delete {}",
                if opts.backup_dir.is_some() { "back up and " } else { "" },
                cand.directory_path
            ),
            false,
            None,
        ));
    }

    // Re-verify immediately before any destructive action: the monitor configuration or
    // the file system may have changed while the plan was shown to the user.
    let fresh = verify(db, &cand.directory_path, Some(&cand.app_exe_path), &opts.protected_prefixes)?;
    if let Some((r, detail)) = &fresh.blocked {
        return Ok(fail(Action::Skipped, format!("re-check failed {r:?}: {detail}"), false, None));
    }
    if fresh.identity != cand.identity {
        return Ok(fail(Action::Skipped, "re-check failed: identity-changed".to_string(), false, None));
    }

    let mut backup_path = None;
    let mut backed_up = false;
    if let Some(broot) = opts.backup_dir {
        let name = path::file_name(&cand.directory_path).unwrap_or_else(|| "directory".to_string());
        let stamp = crate::timefmt::stamp_ms(now_ms, crate::timefmt::local_offset_ms());
        let dest = broot.join(format!("{name}-{stamp}"));
        match backup_tree(&target, &dest) {
            Ok(files) => {
                backed_up = true;
                backup_path = Some(dest);
                let _ = files;
            }
            Err(e) => {
                // A failed backup must never be followed by a deletion.
                return Ok(fail(Action::Failed, format!("backup failed, deletion aborted: {e}"), false, None));
            }
        }
    }

    // Two-step delete: rename first (detects "in use"), then remove the renamed tree.
    let stamp_q = crate::timefmt::stamp_ms(now_ms, 0);
    let quarantined = match quarantine(&target, &stamp_q) {
        Ok(p) => p,
        Err(e) => {
            return Ok(fail(
                Action::Failed,
                format!("in use, nothing deleted: could not rename {} ({e})", cand.directory_path),
                backed_up,
                backup_path,
            ));
        }
    };
    // The renamed directory must still be the same object we verified.
    match read_identity(&quarantined) {
        Some(id) if cand.identity.map(|c| c == id).unwrap_or(true) => {}
        other => {
            return Ok(fail(
                Action::Failed,
                format!("identity-changed after rename, left in place: {} ({other:?})", quarantined.display()),
                backed_up,
                backup_path,
            ));
        }
    }

    match remove_tree(&quarantined) {
        Ok(()) => {
            if cand.directory_id >= 0 {
                let _ = db.set_status(cand.directory_id, RecordStatus::Missing, now_ms);
            }
            Ok(ActionItem {
                directory_path: cand.directory_path.clone(),
                action: Action::Planned,
                detail: match &backup_path {
                    Some(p) => format!("deleted, backup at {}", p.display()),
                    None => "deleted without backup".to_string(),
                },
                backup_path,
                deleted: true,
                backed_up,
            })
        }
        Err(e) => Ok(fail(
            Action::Failed,
            format!(
                "delete failed after rename, the tree now sits at {}: {e}",
                quarantined.display()
            ),
            backed_up,
            backup_path,
        )),
    }
}

/// Marker used by the two-step delete. Any path containing it is the tool's own transient
/// quarantine directory and must never be registered in the ledger.
pub const QUARANTINE_MARKER: &str = ".appdir-todelete-";

/// Rename the directory out of the way, inside its own parent.
///
/// This is the point of the two-step delete: a recursive delete can appear to succeed even
/// while an application holds a file inside open (Windows then leaves a dangling, already
/// deleted object). Renaming, in contrast, fails hard when something is in use, so we learn
/// about it *before* any data is destroyed.
pub fn quarantine(target: &Path, stamp: &str) -> std::io::Result<PathBuf> {
    let parent = target
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "target has no parent directory"))?;
    let name = target
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "target has no file name"))?
        .to_string_lossy()
        .to_string();
    let q = parent.join(format!("{name}{QUARANTINE_MARKER}{stamp}"));
    std::fs::rename(target, &q)?;
    Ok(q)
}

/// Run a whole plan.
pub fn execute(db: &Db, candidates: Vec<Candidate>, opts: &CleanOptions, now_ms: i64) -> Result<CleanReport> {
    let mut rep = CleanReport {
        committed: opts.commit,
        candidates,
        ..Default::default()
    };
    let mut actions = Vec::new();
    for cand in &rep.candidates {
        let item = clean_one(db, cand, opts, now_ms)?;
        if item.deleted {
            rep.deleted += 1;
        }
        if item.backed_up {
            rep.backed_up += 1;
        }
        if item.action == Action::Skipped {
            rep.skipped += 1;
        }
        if item.action == Action::Failed {
            rep.failed += 1;
        }
        actions.push(item);
    }
    rep.actions = actions;
    Ok(rep)
}

/// Removal with Windows error codes translated into readable causes.
pub fn remove_tree(target: &Path) -> std::io::Result<()> {
    match fs::remove_dir_all(target) {
        Ok(()) => Ok(()),
        Err(e) => {
            if let Some(code) = e.raw_os_error() {
                if code == 32 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "the directory is held open by another process (sharing violation)",
                    ));
                }
                if code == 5 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "access denied, or the directory is held open by another process",
                    ));
                }
                if code == 145 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "directory became non-empty during removal",
                    ));
                }
            }
            Err(e)
        }
    }
}
