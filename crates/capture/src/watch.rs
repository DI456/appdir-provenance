///! Real user-mode watcher: `ReadDirectoryChangesW`, non-recursive, so it reports exactly
///! one thing -- the appearance of a *direct child* of a monitor root. That is the same
///! scope the task book gives the driver, obtained with an ordinary documented Win32 API
///! that cannot crash the system.
///!
///! It cannot report which process created the directory. Events from this source
///! therefore carry no creator, and the filter drops them instead of guessing.

use appdir_core::filter::{CreateRequest, IO_FILE_CREATED};
use appdir_core::path;
use appdir_core::winfile::{
    self, error_name, close, long_path, stat, ByHandleFileInformation, Overlapped, CancelIoEx, CreateEventW,
    CreateFileW, GetOverlappedResult, GetLastError, ReadDirectoryChangesW, WaitForSingleObject,
    ERROR_OPERATION_ABORTED, FILE_ACTION_ADDED, FILE_ACTION_RENAMED_NEW_NAME, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_DIR_NAME, FILE_SHARE_ALL, INVALID_HANDLE, OPEN_EXISTING, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use appdir_core::{
    Attribution, EventSource, LinkState, ProcessIdentity, RawCreateEvent, SourceError, SourceKind,
};
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

const BUFFER_DWORDS: usize = 8 * 1024;
const POLL_MS: u32 = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchStatus {
    /// A name was added directly below the root.
    Added {
        root: String,
        name: String,
        is_dir: bool,
        is_reparse: bool,
    },
    /// Notification buffer overflowed: names since the last read are unknown.
    ResyncNeeded { root: String },
    /// The watch handle is open and the first request is pending: from now on additions
    /// below this root are reported. Needed so tests and callers never race startup.
    Ready { root: String },
    /// This root could not be watched; the code is the Win32 error.
    Failed { root: String, code: u32 },
}

#[derive(Debug)]
enum WatchMsg {
    Status(WatchStatus),
}

/// One thread per monitor root, results over a channel.
pub struct WatchSource {
    roots: Vec<String>,
    rx: Receiver<WatchMsg>,
    tx: Sender<WatchMsg>,
    /// Per-root receivers: one channel per root so a high-churn root (e.g.
    /// AppData\Local) can never starve events of a quiet root.
    rxs: Vec<Receiver<WatchMsg>>,
    threads: Vec<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    names_seen: Arc<AtomicU64>,
    resyncs: Arc<AtomicU64>,
    failures: HashMap<String, u32>,
    ready: std::collections::HashSet<String>,
    /// Most recent non-add statuses, for `appdir status` diagnostics.
    last_statuses: Vec<WatchStatus>,
    started: bool,
}

impl WatchSource {
    pub fn new<I, S>(roots: I) -> WatchSource
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let (tx, rx) = channel();
        let rxs: Vec<Receiver<WatchMsg>> = Vec::new();
        WatchSource {
            roots: roots.into_iter().map(|r| r.as_ref().to_string()).collect(),
            rx,
            tx,
            rxs,
            threads: Vec::new(),
            stop: Arc::new(AtomicBool::new(false)),
            names_seen: Arc::new(AtomicU64::new(0)),
            resyncs: Arc::new(AtomicU64::new(0)),
            failures: HashMap::new(),
            ready: std::collections::HashSet::new(),
            last_statuses: Vec::new(),
            started: false,
        }
    }

    pub fn roots(&self) -> &[String] {
        &self.roots
    }

    pub fn names_seen(&self) -> u64 {
        self.names_seen.load(Ordering::Relaxed)
    }

    pub fn resync_count(&self) -> u64 {
        self.resyncs.load(Ordering::Relaxed)
    }

    pub fn failures(&self) -> &HashMap<String, u32> {
        &self.failures
    }

    pub fn statuses(&self) -> &[WatchStatus] {
        &self.last_statuses
    }

    /// Every configured root has an open watch handle with a pending request.
    pub fn is_ready(&self) -> bool {
        !self.roots.is_empty() && self.ready.len() >= self.roots.len()
    }

    /// Read messages until all roots report ready, or the deadline passes.
    pub fn wait_until_ready(&mut self, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            let mut sink = Vec::new();
            let _ = self.drain(&mut sink);
            if self.is_ready() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        self.is_ready()
    }

    /// Open one handle per root and spawn the notify loops. Failing roots are reported,
    /// never silently skipped.
    pub fn start(&mut self) -> Result<(), SourceError> {
        if self.started {
            return Err(SourceError("watcher already started".to_string()));
        }
        self.rxs.clear();
        for root in self.roots.clone() {
            let (rtx, rrx) = channel();
            self.rxs.push(rrx);
            let tx = rtx;
            let stop = self.stop.clone();
            let seen = self.names_seen.clone();
            let resync = self.resyncs.clone();
            let h = std::thread::Builder::new()
                .name(format!("appdir-watch-{root}"))
                .spawn(move || run_watch(root, stop, tx, seen, resync))
                .map_err(|e| SourceError(format!("could not start watcher thread: {e}")))?;
            self.threads.push(h);
        }
        self.started = true;
        Ok(())
    }
}

impl WatchSource {
    /// Capped round-robin batch across per-root channels: a high-churn root
    /// (e.g. AppData\Local) can never starve events of a quiet root.
    fn next_batch(&self) -> Vec<WatchMsg> {
        const PER_ROOT_CAP: usize = 8192;
        let mut batch = Vec::new();
        for rx in &self.rxs {
            for _ in 0..PER_ROOT_CAP {
                match rx.try_recv() {
                    Ok(msg) => batch.push(msg),
                    Err(_) => break,
                }
            }
        }
        batch
    }
}

impl EventSource for WatchSource {
    fn kind(&self) -> SourceKind {
        SourceKind::Watch
    }

    fn attribution(&self) -> Attribution {
        Attribution::DirectoryOnly
    }

    fn drain(&mut self, out: &mut Vec<RawCreateEvent>) -> Result<(), SourceError> {
        for msg in self.next_batch() {
            match msg {
                WatchMsg::Status(WatchStatus::Added { root, name, is_dir, is_reparse }) => {
                    self.last_statuses.push(WatchStatus::Added {
                        root: root.clone(),
                        name: name.clone(),
                        is_dir,
                        is_reparse,
                    });
                    if !is_dir || is_reparse {
                        continue;
                    }
                    let full = match path::normalize(&format!("{}\\{name}", root.trim_end_matches('\\'))) {
                        Some(p) => p,
                        None => continue,
                    };
                    out.push(RawCreateEvent {
                        target_path: full,
                        request: CreateRequest::dir_create(),
                        information: IO_FILE_CREATED,
                        success: true,
                        creator: ProcessIdentity { pid: 0, exe_path: None },
                        observed_at_ms: appdir_core::timefmt::now_ms(),
                        source: SourceKind::Watch,
                    });
                }
                WatchMsg::Status(WatchStatus::ResyncNeeded { root }) => {
                    self.last_statuses.push(WatchStatus::ResyncNeeded { root });
                }
                WatchMsg::Status(WatchStatus::Failed { root, code }) => {
                    self.failures.insert(root, code);
                }
                WatchMsg::Status(WatchStatus::Ready { root }) => {
                    self.ready.insert(root);
                }
            }
        }
        Ok(())
    }

    fn link_state(&self) -> LinkState {
        if !self.started {
            return LinkState::Idle;
        }
        if !self.failures.is_empty() {
            return LinkState::Disconnected;
        }
        LinkState::Running
    }

    fn shutdown(&mut self) -> Result<(), SourceError> {
        self.stop.store(true, Ordering::SeqCst);
        let mut errs = Vec::new();
        for t in self.threads.drain(..) {
            if let Err(_) = t.join() {
                errs.push("watcher thread panicked".to_string());
            }
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(SourceError(errs.join("; ")))
        }
    }
}

/// Body of one watcher thread.
fn run_watch(
    root: String,
    stop: Arc<AtomicBool>,
    tx: Sender<WatchMsg>,
    seen: Arc<AtomicU64>,
    resync: Arc<AtomicU64>,
) {
    let verbose = std::env::var("APPDIR_DEBUG").is_ok();
    let dbg = |s: String| {
        if verbose {
            eprintln!("[watch {root}] {s}");
        }
    };
    let mut first_reported = false;
    let status = |s| {
        let _ = tx.send(WatchMsg::Status(s));
    };
    let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
    let dir = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_ALL,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
            std::ptr::null_mut(),
        )
    };
    if dir == INVALID_HANDLE || dir.is_null() {
        status(WatchStatus::Failed { root, code: unsafe { GetLastError() } });
        return;
    }
    let event = unsafe { CreateEventW(std::ptr::null_mut(), 0, 0, std::ptr::null()) };
    if event == INVALID_HANDLE || event.is_null() {
        let code = unsafe { GetLastError() };
        close(dir);
        status(WatchStatus::Failed { root, code });
        return;
    }
    let mut buffer = vec![0u32; BUFFER_DWORDS];

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let mut ov = Overlapped {
            internal: 0,
            internal_high: 0,
            offset: 0,
            offset_high: 0,
            event,
        };
        let mut returned: u32 = 0;
        let issued = unsafe {
            ReadDirectoryChangesW(
                dir,
                buffer.as_mut_ptr() as *mut c_void,
                (BUFFER_DWORDS * 4) as u32,
                0, // watch_subtree = FALSE: direct children only
                FILE_NOTIFY_CHANGE_DIR_NAME,
                &mut returned,
                &mut ov,
                None,
            )
        };
        if issued == 0 {
            let code = unsafe { GetLastError() };
            dbg(format!("ReadDirectoryChangesW failed: {code} {}", error_name(code)));
            close(event);
            close(dir);
            status(WatchStatus::Failed { root, code });
            return;
        }
        if !first_reported {
            first_reported = true;
            status(WatchStatus::Ready { root: root.clone() });
        }

        // Wait for the notification, but keep the stop flag responsive.
        loop {
            let w = unsafe { WaitForSingleObject(event, POLL_MS) };
            if w == WAIT_TIMEOUT {
                if stop.load(Ordering::SeqCst) {
                    unsafe {
                        CancelIoEx(dir, std::ptr::null_mut());
                        let mut dummy = 0u32;
                        GetOverlappedResult(dir, &mut ov, &mut dummy, 1);
                    }
                    close(event);
                    close(dir);
                    return;
                }
                continue;
            }
            if w != WAIT_OBJECT_0 {
                break;
            }
            let mut nread: u32 = 0;
            let ok = unsafe { GetOverlappedResult(dir, &mut ov, &mut nread, 0) };
            if ok == 0 {
                let code = unsafe { GetLastError() };
                if code == ERROR_OPERATION_ABORTED {
                    close(event);
                    close(dir);
                    return;
                }
                if code == winfile::ERROR_NOTIFY_ENUM_DIR {
                    resync.fetch_add(1, Ordering::Relaxed);
                    status(WatchStatus::ResyncNeeded { root: root.clone() });
                    break; // re-issue the request
                }
                close(event);
                close(dir);
                status(WatchStatus::Failed { root, code });
                return;
            }
            if nread > 0 {
                dbg(format!("notification: {nread} bytes"));
                let mut added = 0u64;
                parse_notifications(&root, &buffer, nread as usize, &mut |s| {
                    if matches!(s, WatchStatus::Added { .. }) {
                        added += 1;
                    }
                    status(s);
                });
                if added > 0 {
                    seen.fetch_add(added, Ordering::Relaxed);
                }
            }
            break; // completed, re-issue
        }
    }
    close(event);
    close(dir);
}

/// Walk the `FILE_NOTIFY_INFORMATION` chain. Each entry is
/// `u32 next_entry_offset, u32 action, u32 file_name_length, UTF-16 name`.
fn parse_notifications(
    root: &str,
    buffer: &[u32],
    used: usize,
    on_status: &mut dyn FnMut(WatchStatus),
) {
    if used > buffer.len() * 4 {
        return;
    }
    let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr() as *const u8, used) };
    let mut off = 0usize;
    loop {
        if off + 12 > bytes.len() {
            return;
        }
        let next = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
        let action = u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap());
        let name_len = u32::from_le_bytes(bytes[off + 8..off + 12].try_into().unwrap()) as usize;
        let name_at = off + 12;
        if name_at + name_len > bytes.len() {
            return;
        }
        let units: Vec<u16> = bytes[name_at..name_at + name_len]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let name = String::from_utf16_lossy(&units);
        if action == FILE_ACTION_ADDED || action == FILE_ACTION_RENAMED_NEW_NAME {
            let full = format!("{}\\{}", root.trim_end_matches('\\'), name);
            let (is_dir, is_reparse) = classify(&full);
            on_status(WatchStatus::Added { root: root.to_string(), name, is_dir, is_reparse });
        }
        if next == 0 {
            return;
        }
        off += next;
        if off <= name_at {
            return; // refuse a looping chain
        }
    }
}

fn classify(p: &str) -> (bool, bool) {
    match stat(&PathBuf::from(long_path(p))) {
        Ok(ByHandleFileInformation { file_attributes, .. }) => (
            file_attributes & FILE_ATTRIBUTE_DIRECTORY != 0,
            file_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0,
        ),
        Err(code) => {
            // Gone already, or not openable: never invent an event for it.
            let _ = error_name(code);
            (false, false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_a_synthetic_notification_chain() {
        // two entries: "AA" added, then "BB" removed
        let mut buf = vec![0u32; 16];
        let bytes = unsafe { std::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut u8, 64) };
        let e1_len = 12 + 4;
        bytes[0..4].copy_from_slice(&(e1_len as u32).to_le_bytes());
        bytes[4..8].copy_from_slice(&FILE_ACTION_ADDED.to_le_bytes());
        bytes[8..12].copy_from_slice(&4u32.to_le_bytes());
        bytes[12..14].copy_from_slice(&0x0041u16.to_le_bytes()); // 'A'
        bytes[14..16].copy_from_slice(&0x0041u16.to_le_bytes()); // 'A'
        bytes[16..20].copy_from_slice(&0u32.to_le_bytes());
        bytes[20..24].copy_from_slice(&winfile::FILE_ACTION_REMOVED.to_le_bytes());
        bytes[24..28].copy_from_slice(&4u32.to_le_bytes());
        let mut seen = Vec::new();
        parse_notifications("C:\\no-such-root-appdir", &buf, 28, &mut |s| seen.push(s));
        assert_eq!(seen.len(), 1, "only the ADDED entry is reported");
        match &seen[0] {
            WatchStatus::Added { name, is_dir, .. } => {
                assert_eq!(name, "AA");
                assert!(!is_dir, "a path that cannot be opened is not a directory");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn refuses_to_watch_a_missing_root() {
        let root = "C:\\definitely-not-a-real-root-42";
        let mut src = WatchSource::new(vec![root]);
        src.start().unwrap();
        let mut out = Vec::new();
        let mut state = LinkState::Running;
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(25));
            src.drain(&mut out).unwrap();
            state = src.link_state();
            if state == LinkState::Disconnected {
                break;
            }
        }
        assert_eq!(state, LinkState::Disconnected, "bad root must be reported, not hidden");
        assert!(out.is_empty());
        src.shutdown().unwrap();
    }

    #[test]
    fn reports_real_directory_creation_without_creator() {
        let root = std::env::temp_dir().join(format!("appdir-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut src = WatchSource::new(vec![root.to_string_lossy().to_string()]);
        src.start().unwrap();
        assert!(src.wait_until_ready(std::time::Duration::from_secs(5)), "watcher must open its handle");
        std::fs::create_dir(root.join("MadeByTest")).unwrap();
        let mut events = Vec::new();
        for _ in 0..60 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            src.drain(&mut events).unwrap();
            if !events.is_empty() {
                break;
            }
        }
        eprintln!(
            "statuses={:?} ready={:?} names_seen={} failures={:?}",
            src.statuses(),
            src.is_ready(),
            src.names_seen(),
            src.failures()
        );
        assert_eq!(events.len(), 1, "the new direct child must be reported once; failures={:?}", src.failures());
        assert_eq!(events[0].source, SourceKind::Watch);
        assert!(events[0].creator.exe_path.is_none(), "a watcher cannot know the creator");
        assert_eq!(src.attribution(), Attribution::DirectoryOnly);
        assert_eq!(src.names_seen(), 1);
        src.shutdown().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn nested_directories_are_not_reported_as_direct_children() {
        let root = std::env::temp_dir().join(format!("appdir-watch-nest-{}", std::process::id()));
        let root_str = root.to_string_lossy().to_string();
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("Child")).unwrap();
        let mut src = WatchSource::new(vec![root.to_string_lossy().to_string()]);
        src.start().unwrap();
        assert!(src.wait_until_ready(std::time::Duration::from_secs(5)));
        std::fs::create_dir(root.join("Child").join("Deep")).unwrap();
        let mut events = Vec::new();
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            src.drain(&mut events).unwrap();
        }
        assert!(
            events
                .iter()
                .all(|e| path::parent_path(&e.target_path).map(|p| path::same_path(&p, &root_str)).unwrap_or(false)),
            "non-recursive watch must never surface a nested path: {events:?}"
        );
        src.shutdown().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }
}
