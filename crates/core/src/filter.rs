///! Create-request classification: decides whether an observation is a directory that
///! really got created, and whether it belongs to a monitor root.

use crate::path::{self, RootKind, WinPath};
use crate::types::{DirCreateRecord, IgnoreReason, MonitorRoot, RawCreateEvent, DbPath};

// CreateDisposition values from ntioapi.h.
pub const FILE_SUPERSEDE: u32 = 0x0000_0000;
pub const FILE_OPEN: u32 = 0x0000_0001;
pub const FILE_CREATE: u32 = 0x0000_0002;
pub const FILE_OPEN_IF: u32 = 0x0000_0003;
pub const FILE_OVERWRITE: u32 = 0x0000_0004;
pub const FILE_OVERWRITE_IF: u32 = 0x0000_0005;

// CreateOptions bits (same values appear in FILE_ATTRIBUTE_* / options fields).
pub const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
pub const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
pub const FILE_COMPLETE_IF_OPLOCKED: u32 = 0x0000_0100;
pub const FILE_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

// IoStatus.Information values.
pub const IO_FILE_SUPERSEDED: u32 = 0x0000_0000;
pub const IO_FILE_OPENED: u32 = 0x0000_0001;
pub const IO_FILE_CREATED: u32 = 0x0000_0002;
pub const IO_FILE_OVERWRITTEN: u32 = 0x0000_0003;
pub const IO_FILE_EXISTS: u32 = 0x0000_0004;
pub const IO_FILE_DOES_NOT_EXIST: u32 = 0x0000_0005;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateRequest {
    pub disposition: u32,
    pub options: u32,
}

impl CreateRequest {
    pub fn dir_create() -> Self {
        CreateRequest {
            disposition: FILE_CREATE,
            options: FILE_DIRECTORY_FILE,
        }
    }

    pub fn dir_open() -> Self {
        CreateRequest {
            disposition: FILE_OPEN,
            options: FILE_DIRECTORY_FILE,
        }
    }

    pub fn dir_open_if() -> Self {
        CreateRequest {
            disposition: FILE_OPEN_IF,
            options: FILE_DIRECTORY_FILE,
        }
    }

    pub fn file_create() -> Self {
        CreateRequest {
            disposition: FILE_CREATE,
            options: 0,
        }
    }

    pub fn asks_directory(&self) -> bool {
        self.options & FILE_DIRECTORY_FILE != 0
    }

    pub fn allows_creation(&self) -> bool {
        matches!(
            self.disposition,
            FILE_SUPERSEDE | FILE_CREATE | FILE_OPEN_IF | FILE_OVERWRITE | FILE_OVERWRITE_IF
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// A directory was newly created inside an enabled monitor root by a known EXE.
    Accept(DirCreateRecord),
    /// Deliberately dropped. Reported so the daemon can count and log it.
    Ignore(IgnoreReason),
}

/// The monitor view used by the filter.
#[derive(Debug, Clone, Default)]
pub struct MonitorView {
    pub roots: Vec<MonitorRoot>,
    /// Directories the tool itself owns; never ledger candidates.
    pub self_exclusions: Vec<DbPath>,
}

impl MonitorView {
    pub fn from_roots(roots: Vec<MonitorRoot>) -> Self {
        MonitorView {
            roots,
            self_exclusions: Vec::new(),
        }
    }

    /// The enabled root that `target` is a direct child of, if any.
    pub fn matching_root(&self, target: &WinPath) -> Option<&MonitorRoot> {
        self.roots
            .iter()
            .filter(|r| r.enabled)
            .filter_map(|r| path::parse(&r.path).map(|rp| (r, rp)))
            .find(|(_, rp)| target.is_direct_child_of(rp))
            .map(|(r, _)| r)
    }

    fn is_self_owned(&self, target: &WinPath) -> bool {
        let t = target.as_str();
        self.self_exclusions
            .iter()
            .filter_map(|p| path::parse(p))
            .any(|rp| t.eq(&rp.as_str()) || target.is_within(&rp) || rp.is_within(target))
    }
}

/// Decide whether one observation becomes a ledger record.
///
/// Pre-conditions follow the task book: only a *successful* create whose completion says
/// the object was *newly created*, whose request asked for a directory, whose path is a
/// direct child of an enabled monitor root, and whose creator EXE path is known.
pub fn admit(ev: &RawCreateEvent, view: &MonitorView) -> Verdict {
    if !ev.success {
        return Verdict::Ignore(IgnoreReason::Failed);
    }
    if ev.information != IO_FILE_CREATED {
        // FILE_OPENED / FILE_OVERWRITTEN / FILE_SUPERSEDED are not new directories.
        return Verdict::Ignore(IgnoreReason::OpenedExisting);
    }
    if !ev.request.allows_creation() {
        return Verdict::Ignore(IgnoreReason::DisallowedDisposition);
    }
    if !ev.request.asks_directory() {
        return Verdict::Ignore(IgnoreReason::NotDirectory);
    }
    let target = match path::parse(&ev.target_path) {
        Some(p) => p,
        None => return Verdict::Ignore(IgnoreReason::BadPath),
    };
    if target.kind == RootKind::Device {
        // A driver must hand us a drive-mapped path; unmapped device paths cannot be
        // compared with user-configured roots reliably.
        return Verdict::Ignore(IgnoreReason::BadPath);
    }
    if target.is_root() {
        return Verdict::Ignore(IgnoreReason::IsMonitorRoot);
    }
    // Creating/opening a configured root itself is never a child event.
    if view
        .roots
        .iter()
        .filter(|r| r.enabled)
        .filter_map(|r| path::parse(&r.path))
        .any(|rp| rp == target)
    {
        return Verdict::Ignore(IgnoreReason::IsMonitorRoot);
    }
    if view.is_self_owned(&target) {
        return Verdict::Ignore(IgnoreReason::OutsideMonitor);
    }
    // Our own transient quarantine directory is never a candidate.
    if target.name().map(|n| n.contains(crate::clean::QUARANTINE_MARKER)).unwrap_or(false) {
        return Verdict::Ignore(IgnoreReason::OutsideMonitor);
    }
    let root = match view.matching_root(&target) {
        Some(r) => r,
        None => {
            return Verdict::Ignore(if view.roots.iter().any(|r| {
                r.enabled
                    && path::parse(&r.path)
                        .map(|rp| target.is_within(&rp))
                        .unwrap_or(false)
            }) {
                IgnoreReason::NotDirectChild
            } else {
                IgnoreReason::OutsideMonitor
            })
        }
    };
    let exe_path = match ev.creator.exe_path.as_ref().and_then(|p| path::normalize(p)) {
        Some(p) => p,
        None => return Verdict::Ignore(IgnoreReason::CreatorUnknown),
    };
    let exe_name = match path::file_name(&exe_path) {
        Some(n) => n,
        None => return Verdict::Ignore(IgnoreReason::CreatorUnknown),
    };
    Verdict::Accept(DirCreateRecord {
        directory_path: target.as_str(),
        monitor_root: root.path.clone(),
        exe_path,
        exe_name,
        creator_pid: ev.creator.pid,
        created_at_ms: ev.observed_at_ms,
        observed_at_ms: ev.observed_at_ms,
        capture_source: ev.source,
        attribution_confidence: ev.source.default_confidence().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ProcessIdentity, SourceKind};

    fn root(p: &str) -> MonitorRoot {
        MonitorRoot {
            path: p.to_string(),
            enabled: true,
            added_at_ms: 0,
        }
    }

    fn ev(path_in: &str, req: CreateRequest, info: u32, success: bool, exe: Option<&str>) -> RawCreateEvent {
        RawCreateEvent {
            target_path: path_in.to_string(),
            request: req,
            information: info,
            success,
            creator: ProcessIdentity {
                pid: 4242,
                exe_path: exe.map(|s| s.to_string()),
            },
            observed_at_ms: 1000,
            source: SourceKind::Sim,
        }
    }

    #[test]
    fn accepts_direct_child_create() {
        let view = MonitorView::from_roots(vec![root("C:\\Users\\Administrator\\AppData\\Local")]);
        let v = admit(
            &ev(
                "C:\\Users\\Administrator\\AppData\\Local\\AppA",
                CreateRequest::dir_create(),
                IO_FILE_CREATED,
                true,
                Some("C:\\Program Files\\AppA\\AppA.exe"),
            ),
            &view,
        );
        match v {
            Verdict::Accept(r) => {
                assert_eq!(r.directory_path, "C:\\Users\\Administrator\\AppData\\Local\\AppA");
                assert_eq!(r.monitor_root, "C:\\Users\\Administrator\\AppData\\Local");
                assert_eq!(r.exe_name, "AppA.exe");
                assert_eq!(r.creator_pid, 4242);
            }
            other => panic!("expected accept, got {other:?}"),
        }
    }

    #[test]
    fn ignores_open_existing_and_failure() {
        let view = MonitorView::from_roots(vec![root("C:\\Local")]);
        assert_eq!(
            admit(&ev("C:\\Local\\A", CreateRequest::dir_open(), IO_FILE_OPENED, true, Some("C:\\a.exe")), &view),
            Verdict::Ignore(IgnoreReason::OpenedExisting)
        );
        assert_eq!(
            admit(&ev("C:\\Local\\A", CreateRequest::dir_create(), IO_FILE_CREATED, false, Some("C:\\a.exe")), &view),
            Verdict::Ignore(IgnoreReason::Failed)
        );
        // FILE_OPEN_IF that ended up opening an existing directory
        assert_eq!(
            admit(&ev("C:\\Local\\A", CreateRequest::dir_open_if(), IO_FILE_OPENED, true, Some("C:\\a.exe")), &view),
            Verdict::Ignore(IgnoreReason::OpenedExisting)
        );
        // FILE_OPEN_IF that really created it
        assert!(matches!(
            admit(&ev("C:\\Local\\A", CreateRequest::dir_open_if(), IO_FILE_CREATED, true, Some("C:\\a.exe")), &view),
            Verdict::Accept(_)
        ));
    }

    #[test]
    fn ignores_files_nested_and_unattributable() {
        let view = MonitorView::from_roots(vec![root("C:\\Local")]);
        assert_eq!(
            admit(&ev("C:\\Local\\file.txt", CreateRequest::file_create(), IO_FILE_CREATED, true, Some("C:\\a.exe")), &view),
            Verdict::Ignore(IgnoreReason::NotDirectory)
        );
        assert_eq!(
            admit(&ev("C:\\Local\\A\\sub", CreateRequest::dir_create(), IO_FILE_CREATED, true, Some("C:\\a.exe")), &view),
            Verdict::Ignore(IgnoreReason::NotDirectChild)
        );
        assert_eq!(
            admit(&ev("C:\\Other\\A", CreateRequest::dir_create(), IO_FILE_CREATED, true, Some("C:\\a.exe")), &view),
            Verdict::Ignore(IgnoreReason::OutsideMonitor)
        );
        assert_eq!(
            admit(&ev("C:\\Local\\A", CreateRequest::dir_create(), IO_FILE_CREATED, true, None), &view),
            Verdict::Ignore(IgnoreReason::CreatorUnknown)
        );
        assert_eq!(
            admit(&ev("C:\\Local", CreateRequest::dir_create(), IO_FILE_CREATED, true, Some("C:\\a.exe")), &view),
            Verdict::Ignore(IgnoreReason::IsMonitorRoot)
        );
    }

    #[test]
    fn prefix_name_trap_and_device_paths() {
        let view = MonitorView::from_roots(vec![root("C:\\Users\\Administrator\\AppData\\Local")]);
        assert_eq!(
            admit(
                &ev("C:\\Users\\Administrator\\AppData\\LocalBackup\\App", CreateRequest::dir_create(), IO_FILE_CREATED, true, Some("C:\\a.exe")),
                &view,
            ),
            Verdict::Ignore(IgnoreReason::OutsideMonitor)
        );
        assert_eq!(
            admit(&ev("\\Device\\HarddiskVolume3\\Local\\A", CreateRequest::dir_create(), IO_FILE_CREATED, true, Some("C:\\a.exe")), &view),
            Verdict::Ignore(IgnoreReason::BadPath)
        );
    }

    #[test]
    fn disabled_root_is_not_matched() {
        let mut r = root("C:\\Local");
        r.enabled = false;
        let view = MonitorView::from_roots(vec![r]);
        assert_eq!(
            admit(&ev("C:\\Local\\A", CreateRequest::dir_create(), IO_FILE_CREATED, true, Some("C:\\a.exe")), &view),
            Verdict::Ignore(IgnoreReason::OutsideMonitor)
        );
    }

    #[test]
    fn self_exclusion() {
        let mut view = MonitorView::from_roots(vec![root("C:\\Local")]);
        view.self_exclusions = vec!["C:\\Local\\appdir-provenance".to_string()];
        assert_eq!(
            admit(&ev("C:\\Local\\appdir-provenance\\x", CreateRequest::dir_create(), IO_FILE_CREATED, true, Some("C:\\a.exe")), &view),
            Verdict::Ignore(IgnoreReason::OutsideMonitor)
        );
    }
}
