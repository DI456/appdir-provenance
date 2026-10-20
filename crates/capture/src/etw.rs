//! ETW capture backend: real-time session on `Microsoft-Windows-Kernel-File`.
//!
//! What we verified on a real machine before writing this:
//! * The provider's Create(12) event schema (version 0 here) is
//!   `Irp(8) ThreadId(8) FileObject(8) CreateOptions(u32) CreateAttributes(u32)
//!   ShareAccess(u32) FileName(utf16, null-terminated)`. TDH property lookup by name and
//!   by index both return ERROR_INVALID_DATA/ERROR_INVALID_PARAMETER on this machine, so
//!   the payload is parsed from the raw user data against that exact layout; any event
//!   whose schema differs (wrong version/property count) is counted as unparsed and
//!   ignored, never guessed.
//! * Create(12) does NOT carry a disposition or status: an open of an existing directory
//!   produces an event that is indistinguishable from a create request. Therefore an ETW
//!   event alone never proves a directory was newly created. This source confirms every
//!   candidate with the non-recursive `ReadDirectoryChangesW` appearance watcher, whose
//!   `FILE_ACTION_ADDED` only fires for objects that actually appeared. Only pairs
//!   (ETW directory-create candidate, watcher-confirmed appearance) become records;
//!   everything else is dropped with a diagnostic reason.
//! * The provider is a kernel provider: starting the session requires admin rights.

use crate::watch::WatchSource;
use windows::Win32::Foundation::WIN32_ERROR;
use appdir_core::filter::{CreateRequest, IO_FILE_CREATED};
use appdir_core::path::{self, DeviceMap};
use appdir_core::winfile::process_image_path;
use appdir_core::{
    Attribution, EventSource, LinkState, ProcessIdentity, RawCreateEvent, SourceError, SourceKind,
};
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Microsoft-Windows-Kernel-File
const KERNEL_FILE_GUID: windows::core::GUID =
    windows::core::GUID::from_u128(0xEDD08927_9CC4_4E65_B970_C2560FB5C289);
/// Microsoft-Windows-Kernel-Process (manifest provider)
const KERNEL_PROCESS_GUID: windows::core::GUID =
    windows::core::GUID::from_u128(0x22FB2CD6_0E7B_422B_A0C7_2FAD1FD0E716);
const EVENT_ID_CREATE: u16 = 12;
/// CreateOptions bit: the request targets a directory object.
const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
/// How long we keep an unconfirmed candidate before dropping it as unconfirmable.
const CONFIRM_WINDOW_MS: i64 = 15_000;

// ---------------------------------------------------------------------------
// Plan 1: PID -> image path cache fed by a parallel Kernel-Process session.
//
// Kernel-File events only carry a PID; resolving the full image path needs an
// open process handle, which fails for short-lived creators. The Kernel-Process
// provider's ProcessStart event carries the image path itself, so we keep a
// bounded cache written by that session; the file callback falls back to it.
// Only schema properties resolved by TDH (never guessed layouts) are used;
// failures are counted and the fallback simply stays empty.
// ---------------------------------------------------------------------------

const PROC_CACHE_CAP: usize = 8192;
static PROC_ACTIVE: OnceLock<AtomicBool> = OnceLock::new();
static PROC_STATS: OnceLock<Arc<EtwStats>> = OnceLock::new();
static PROC_CACHE: std::sync::LazyLock<Mutex<std::collections::HashMap<u32, (String, i64)>>> =
    std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

fn proc_cache_put(pid: u32, image: String) {
    let now = EtwSource::pending_now_ms();
    let mut cache = PROC_CACHE.lock().unwrap();
    if cache.len() >= PROC_CACHE_CAP && !cache.contains_key(&pid) {
        // drop the oldest entries (batch of 256) to stay bounded
        let mut oldest: Vec<(i64, u32)> = cache
            .iter()
            .map(|(k, (_, t))| (*t, *k))
            .collect();
        oldest.sort_unstable();
        for (_, k) in oldest.into_iter().take(256) {
            cache.remove(&k);
        }
    }
    cache.insert(pid, (image, now));
}

fn proc_cache_get(pid: u32) -> Option<String> {
    PROC_CACHE.lock().unwrap().get(&pid).map(|(p, _)| p.clone())
}

/// Extract (pid, image) from a Kernel-Process record via TDH by property name.
/// Returns None unless both properties resolve; no layout guessing.
static PROC_SCHEMA_DUMPED: AtomicBool = AtomicBool::new(false);

/// One-shot: dump actual TDH property names for diagnosis.
fn dump_proc_schema(record: &windows::Win32::System::Diagnostics::Etw::EVENT_RECORD) {
    use windows::Win32::System::Diagnostics::Etw::TdhGetEventInformation;
    if PROC_SCHEMA_DUMPED.swap(true, Ordering::Relaxed) {
        return;
    }
    let mut size = 0u32;
    let st = unsafe { TdhGetEventInformation(record, None, None, &mut size) };
    if st != 122 || size == 0 || size > 65536 {
        let g = record.EventHeader.ProviderId;
        let d = record.EventHeader.EventDescriptor;
        DEBUG_DUMP.lock().unwrap().push(format!(
            "proc schema: TdhGetEventInformation st={st} size={size} provider={:x}-{:x}-{:x} id={} version={} opcode={} kernel_time={:?}",
            g.data1, g.data2, g.data3, d.Id, d.Version, d.Opcode, record.EventHeader.TimeStamp
        ));
        return;
    }
    let mut buf = vec![0u8; size as usize];
    let st = unsafe {
        TdhGetEventInformation(
            record,
            None,
            Some(buf.as_mut_ptr() as *mut windows::Win32::System::Diagnostics::Etw::TRACE_EVENT_INFO),
            &mut size,
        )
    };
    if st != 0 {
        DEBUG_DUMP.lock().unwrap().push(format!("proc schema: retry st={st}"));
        return;
    }
    let u32_at = |off: usize| -> u32 {
        buf.get(off..off + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]])).unwrap_or(0)
    };
    let utf16_at = |off: usize| -> String {
        if off == 0 || off >= buf.len() {
            return String::new();
        }
        let rest = &buf[off..];
        let words: Vec<u16> = rest
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|w| *w != 0)
            .collect();
        String::from_utf16_lossy(&words)
    };
    let (property_count, top_level) = (u32_at(112) as usize, u32_at(116) as usize);
    let g = record.EventHeader.ProviderId;
    let mut out = format!(
        "proc schema: provider={:x}-{:x}-{:x} id={} PropertyCount={} TopLevel={}",
        g.data1, g.data2, g.data3,
        record.EventHeader.EventDescriptor.Id,
        property_count,
        top_level
    );
    for i in 0..top_level.min(32) {
        let info = 128 + i * 8; // EVENT_PROPERTY_INFO is 8 bytes for simple props
        let flags = u32_at(info);
        let name_offset = u32_at(info + 4) as usize;
        out.push_str(&format!(" [{} flags={:x} name={}]", i, flags, utf16_at(name_offset)));
    }
    DEBUG_DUMP.lock().unwrap().push(out);
}

static PROC_DEVICE_MAP: std::sync::LazyLock<appdir_core::path::DeviceMap> =
    std::sync::LazyLock::new(appdir_core::winfile::device_map);

/// Extract (pid, image dos path) from a classic Kernel-Process Start(1) payload.
///
/// Verified layout on this machine (real-machine dump):
///   u32 pid, u32 ?, u32 0, FILETIME(8), u32 session, u32 parent, ..., then a
///   null-terminated UTF-16 device image path, e.g.
///   `\Device\HarddiskVolume3\Windows\System32\cmd.exe`.
/// The path may alternatively be a `X:\...` drive path. Anything not matching a
/// strict absolute exe path is rejected (never guessed).
fn extract_proc_fields(bytes: &[u8]) -> Option<(u32, String)> {
    if bytes.len() < 8 {
        return None;
    }
    let pid = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    if pid < 4 {
        return None;
    }
    let words: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let is_word = |w: u16, c: u8| u16_lower(w) == u16_lower(c as u16);

    // locate the path start: "\Device\", "\??\", or "X:\"
    let mut start = None;
    for i in 0..words.len().saturating_sub(7) {
        if is_word(words[i], b'\\')
            && is_word(words[i + 1], b'D')
            && is_word(words[i + 2], b'e')
            && is_word(words[i + 3], b'v')
            && is_word(words[i + 4], b'i')
            && is_word(words[i + 5], b'c')
            && is_word(words[i + 6], b'e')
            && is_word(words[i + 7], b'\\')
        {
            start = Some(i);
            break;
        }
        if is_word(words[i], b'\\')
            && is_word(words[i + 1], b'?')
            && is_word(words[i + 2], b'?')
            && is_word(words[i + 3], b'\\')
        {
            start = Some(i);
            break;
        }
        if i > 0 && u16_is_alpha(words[i - 1]) && is_word(words[i], b':') && is_word(words[i + 1], b'\\')
        {
            start = Some(i - 1);
            break;
        }
    }
    let start = start?;
    if std::env::var("APPDIR_EXTRACT_DEBUG").as_deref() == Ok("1") {
        eprintln!("[extract] start={start}");
    }

    // path ends at the first ".exe" (case-insensitive)
    let mut end = None;
    for i in start..words.len().saturating_sub(4) {
        if is_word(words[i], b'.')
            && is_word(words[i + 1], b'e')
            && is_word(words[i + 2], b'x')
            && is_word(words[i + 3], b'e')
        {
            end = Some(i + 4);
            break;
        }
    }
    let end = end?;
    if std::env::var("APPDIR_EXTRACT_DEBUG").as_deref() == Ok("1") {
        eprintln!("[extract] end={end}");
    }
    let plen = end - start;
    if !(6..=300).contains(&plen) {
        return None;
    }
    let image_utf16 = String::from_utf16_lossy(&words[start..end]);
    let lower = image_utf16.to_ascii_lowercase();
    if !lower.ends_with(".exe") || image_utf16.bytes().any(|c| c < 0x20) {
        return None;
    }
    // device paths need a drive-letter mapping; drive paths are used as-is
    let dos = if lower.starts_with("\\device\\") {
        path::device_to_dos(&image_utf16, &PROC_DEVICE_MAP)?
    } else if lower.starts_with("\\??\\") {
        image_utf16.trim_start_matches("\\??\\").to_string()
    } else {
        image_utf16
    };
    let norm = path::normalize(&dos)?;
    let b = norm.as_bytes();
    if b.len() < 7 || !b[0].is_ascii_alphabetic() || b[1] != b':' || b[2] != b'\\' {
        return None;
    }
    Some((pid, norm))
}

fn u16_lower(w: u16) -> u16 {
    if (0x41..=0x5A).contains(&w) {
        w + 32
    } else {
        w
    }
}

fn u16_is_alpha(w: u16) -> bool {
    (0x41..=0x5A).contains(&w) || (0x61..=0x7A).contains(&w)
}

extern "system" fn proc_callback(event_ptr: *mut windows::Win32::System::Diagnostics::Etw::EVENT_TRACE) {
    if event_ptr.is_null() {
        return;
    }
    // SAFETY: ProcessTrace owns the EVENT_TRACE for the duration of the callback.
    let event = unsafe { &*event_ptr };
    let active = PROC_ACTIVE.get().map(|a| a.load(Ordering::Relaxed)).unwrap_or(false);
    if !active {
        return;
    }
    let stats = PROC_STATS.get();
    let guid = unsafe { event.Header.Anonymous3.Guid };
    if guid != KERNEL_PROCESS_GUID {
        return;
    }
    if let Some(s) = stats {
        s.proc_events.fetch_add(1, Ordering::Relaxed);
    }
    // Verified on this machine: EventRecordCallback delivers null UserData for kernel
    // providers, so use the classic EventCallback + MofData path (same as Kernel-File).
    if event.MofData.is_null() {
        return;
    }
    let (version, type_) = unsafe {
        (event.Header.Anonymous2.Class.Version, event.Header.Anonymous2.Class.Type)
    };
    let _ = version;
    if type_ != 1 {
        // only ProcessStart gives us the new PID while it is still (almost surely) alive
        return;
    }
    let bytes = unsafe {
        std::slice::from_raw_parts(event.MofData as *const u8, event.MofLength as usize)
    };
    if bytes.len() < 4 {
        return;
    }
    // Primary: the payload itself contains the image path (verified layout), so a
    // short-lived creator is attributed even if it exits before we see the event.
    // Fallback: resolve by PID via a process handle while it is still alive.
    let pid = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let mut resolved = false;
    if let Some((pid, image)) = extract_proc_fields(bytes) {
        proc_cache_put(pid, image);
        resolved = true;
    } else if pid >= 4 {
        if let Some(image) = process_image_path(pid) {
            proc_cache_put(pid, image);
            resolved = true;
        }
    }
    if resolved {
        if let Some(s) = stats {
            s.proc_parsed.fetch_add(1, Ordering::Relaxed);
        }
    }
    // One-shot diagnostics: dump the first few ProcessStart payloads with parse stages.
    {
        let mut dump = DEBUG_DUMP.lock().unwrap();
        if dump.len() < 4 {
            let words: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            let text: String = words.iter().map(|w| char::from_u32(*w as u32).filter(|c| !c.is_control()).unwrap_or('.')).collect();
            let dev_idx = words.windows(8).position(|w| {
                u16_lower(w[0]) == b'\\' as u16 && u16_lower(w[1]) == b'D' as u16
            });
            let raw_path = dev_idx.and_then(|i| {
                let mut end = None;
                for j in i..words.len().saturating_sub(4) {
                    if u16_lower(words[j]) == b'.' as u16
                        && u16_lower(words[j+1]) == b'e' as u16
                        && u16_lower(words[j+2]) == b'x' as u16
                        && u16_lower(words[j+3]) == b'e' as u16
                    { end = Some(j + 4); break; }
                }
                end.map(|e| String::from_utf16_lossy(&words[i..e]))
            });
            let dos = raw_path.as_ref().and_then(|p| path::device_to_dos(p, &PROC_DEVICE_MAP));
            let norm = dos.as_ref().and_then(|p| path::normalize(p));
            let map_len = PROC_DEVICE_MAP.len();
            let hex: String = bytes.iter().take(170).map(|b| format!("{:02x}", b)).collect();
            dump.push(format!(
                "procstart type={} len={} pid0={} ok={} dev_idx={:?} raw={:?} dos={:?} norm={:?} map_len={} hex={}",
                type_, bytes.len(), pid, resolved, dev_idx, raw_path, dos, norm, map_len, hex
            ));
        }
    }
}

#[derive(Debug, Clone)]
pub struct PendingCreate {
    /// Normalized drive-letter path of the requested directory.
    pub target: String,
    pub pid: u32,
    pub observed_at_ms: i64,
    pub exe_path: Option<String>,
}

static DEBUG_DUMP: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub fn debug_dump() -> Vec<String> {
    DEBUG_DUMP.lock().unwrap().clone()
}

#[derive(Debug, Default)]
pub struct EtwStats {
    pub kernel_file_events: AtomicU64,
    pub create_events: AtomicU64,
    pub dir_candidates: AtomicU64,
    pub unparsed: AtomicU64,
    pub schema_mismatch: AtomicU64,
    pub reject_version: AtomicU64,
    pub reject_short: AtomicU64,
    pub confirmed: AtomicU64,
    pub unconfirmed_expired: AtomicU64,
    pub no_creator: AtomicU64,
    pub watcher_seen: AtomicU64,
    pub event_loss_reports: AtomicU64,
    pub proc_cache_hits: AtomicU64,
    pub proc_events: AtomicU64,
    pub proc_parsed: AtomicU64,
    pub late_resolved: AtomicU64,
    pub stop_errors: Mutex<Vec<String>>,
}

impl EtwStats {
    pub fn snapshot(&self) -> String {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        format!(
            "kernel-file事件 {} create事件 {} 目录候选 {} 解析失败 {} 确认成功 {} 未确认过期 {} 无创建者 {} 监听器可见 {} 丢失报告 {} 缓存命中 {} 进程事件 {} 进程解析 {} 迟到归属 {}",
            g(&self.kernel_file_events),
            g(&self.create_events),
            g(&self.dir_candidates),
            g(&self.unparsed),
            g(&self.confirmed),
            g(&self.unconfirmed_expired),
            g(&self.no_creator),
            g(&self.watcher_seen),
            g(&self.event_loss_reports),
            g(&self.proc_cache_hits),
            g(&self.proc_events),
            g(&self.proc_parsed),
            g(&self.late_resolved),
        )
    }

    pub fn events_lost(&self) -> bool {
        self.event_loss_reports.load(Ordering::Relaxed) > 0
    }
}

/// State shared with the ETW callback. One capture session per process; the callback has
/// no user-context parameter, so the shared state lives in a process-global slot.
struct CallbackShared {
    tx: std::sync::mpsc::Sender<PendingCreate>,
    device_map: DeviceMap,
    stats: Arc<EtwStats>,
    active: AtomicBool,
}

static CALLBACK: OnceLock<Mutex<Option<CallbackShared>>> = OnceLock::new();

struct SessionCtl {
    /// CONTROLTRACE_HANDLE value; 0 until the session is started.
    handle: AtomicU64,
    session_name: Vec<u16>,
    /// EVENT_TRACE_PROPERTIES buffer, shared so ControlTraceW(stop) can reuse it.
    props: Mutex<Vec<u8>>,
    stop: AtomicBool,
}

pub struct EtwSource {
    roots: Vec<String>,
    watcher: WatchSource,
    pending: Vec<PendingCreate>,
    confirmed: HashSet<String>,
    queue: VecDeque<RawCreateEvent>,
    grace: Vec<PendingCreate>,
    device_map: DeviceMap,
    ctl: Option<Arc<SessionCtl>>,
    proc_ctl: Option<Arc<SessionCtl>>,
    stats: Arc<EtwStats>,
    start_error: Mutex<Option<String>>,
    session_started: bool,
    pending_rx: Option<std::sync::mpsc::Receiver<PendingCreate>>,
}

impl EtwSource {
    /// Create the backend and start the ETW session + appearance watcher. Fails softly:
    /// a session start failure (e.g. no admin rights) is surfaced by `start_error()` and
    /// the source then behaves like an honest "nothing captured" source.
    pub fn new<I, S>(roots: I) -> Result<EtwSource, SourceError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let roots: Vec<String> = roots.into_iter().map(|r| r.as_ref().to_string()).collect();
        let watcher = WatchSource::new(roots.iter().cloned());
        let stats = Arc::new(EtwStats::default());
        let mut src = EtwSource {
            roots,
            watcher,
            pending: Vec::new(),
            confirmed: HashSet::new(),
            queue: VecDeque::new(),
            grace: Vec::new(),
            device_map: appdir_core::winfile::device_map(),
            ctl: None,
            proc_ctl: None,
            stats,
            start_error: Mutex::new(None),
            session_started: false,
            pending_rx: None,
        };
        // the appearance watcher must be open before the session, or early creates
        // cannot be confirmed
        src.watcher_mut().start()?;
        src.start_session();
        src.start_process_session();
        Ok(src)
    }

    pub fn stats(&self) -> &Arc<EtwStats> {
        &self.stats
    }

    pub fn start_error(&self) -> Option<String> {
        self.start_error.lock().unwrap().clone()
    }

    pub fn watcher_mut(&mut self) -> &mut WatchSource {
        &mut self.watcher
    }

    pub fn watcher(&self) -> &WatchSource {
        &self.watcher
    }

    /// Stop a leftover session with the same name (from a force-killed previous run).
    fn stop_existing_session(session: &str) {
        use windows::Win32::System::Diagnostics::Etw::*;
        let mut session_w: Vec<u16> = session.encode_utf16().collect();
        session_w.push(0);
        let props_size = std::mem::size_of::<EVENT_TRACE_PROPERTIES>();
        let name_space = (session_w.len() + 8) * 2 + 128;
        let mut prop_bytes: Vec<u8> = vec![0; props_size + name_space];
        let props = unsafe { &mut *(prop_bytes.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES) };
        props.Wnode.BufferSize = (props_size + name_space) as u32;
        props.LoggerNameOffset = props_size as u32;
        let session_name = windows::core::PCWSTR(session_w.as_ptr());
        unsafe {
            let _ = ControlTraceW(
                CONTROLTRACE_HANDLE::default(),
                session_name,
                props,
                EVENT_TRACE_CONTROL_STOP,
            );
        }
    }

    fn start_session(&mut self) {
        let session = "appdir-etw".to_string();
        Self::stop_existing_session(&session);
        let mut session_w: Vec<u16> = session.encode_utf16().collect();
        session_w.push(0);

        let props_size = std::mem::size_of::<windows::Win32::System::Diagnostics::Etw::EVENT_TRACE_PROPERTIES>();
        let name_space = (session_w.len() + 8) * 2 + 128;
        let mut prop_bytes: Vec<u8> = vec![0; props_size + name_space];
        // SAFETY: prop_bytes is sized for the struct plus name space and outlives the
        // session (kept in SessionCtl).
        let props = unsafe {
            &mut *(prop_bytes.as_mut_ptr()
                as *mut windows::Win32::System::Diagnostics::Etw::EVENT_TRACE_PROPERTIES)
        };
        use windows::Win32::System::Diagnostics::Etw::*;
        props.Wnode.BufferSize = (props_size + name_space) as u32;
        props.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        props.Wnode.ClientContext = 1; // QPC timestamps
        props.BufferSize = 64;
        props.MinimumBuffers = 4;
        props.MaximumBuffers = 16;
        props.LogFileMode = EVENT_TRACE_REAL_TIME_MODE | EVENT_TRACE_NO_PER_PROCESSOR_BUFFERING | EVENT_TRACE_SYSTEM_LOGGER_MODE;
        props.LoggerNameOffset = props_size as u32;
        props.LogFileNameOffset = 0;

        let session_name = windows::core::PCWSTR(session_w.as_ptr());
        let mut handle = CONTROLTRACE_HANDLE::default();
        let start = unsafe { StartTraceW(&mut handle, session_name, props) };
        if start != WIN32_ERROR(0) {
            *self.start_error.lock().unwrap() = Some(format!(
                "StartTraceW 失败 (0x{:x}): 内核提供者需要管理员权限",
                start.0
            ));
            return;
        }

        let enable = unsafe {
            EnableTraceEx2(
                handle,
                &KERNEL_FILE_GUID,
                EVENT_CONTROL_CODE_ENABLE_PROVIDER.0,
                5, // TRACE_LEVEL_VERBOSE
                0x10 | 0x80,
                0,
                0,
                None,
            )
        };
        if enable != WIN32_ERROR(0) {
            let _ = unsafe { ControlTraceW(handle, session_name, props, EVENT_TRACE_CONTROL_STOP) };
            *self.start_error.lock().unwrap() =
                Some(format!("EnableTraceEx2 失败 (0x{:x})", enable.0));
            return;
        }
        self.session_started = true;

        let ctl = Arc::new(SessionCtl {
            handle: AtomicU64::new(handle.Value),
            session_name: session_w,
            props: Mutex::new(prop_bytes),
            stop: AtomicBool::new(false),
        });
        self.ctl = Some(ctl.clone());

        // consumer thread: OpenTrace + blocking ProcessTrace
        let consumer_ctl = ctl.clone();
        let consumer_stats = self.stats.clone();
        let (tx, rx) = std::sync::mpsc::channel::<PendingCreate>();
        let device_map = self.device_map.clone();
        let slot = CALLBACK.get_or_init(|| Mutex::new(None));
        {
            let mut guard = slot.lock().unwrap();
            if guard.is_some() {
                // One session per process; a second source instance would fight over the
                // callback slot.
                *self.start_error.lock().unwrap() =
                    Some("本进程已有一个 ETW 采集会话在运行".to_string());
                self.stop_session(&consumer_ctl, &KERNEL_FILE_GUID);
                return;
            }
            *guard = Some(CallbackShared {
                tx,
                device_map,
                stats: consumer_stats,
                active: AtomicBool::new(true),
            });
        }

        let ctl_for_thread = ctl.clone();
        std::thread::spawn(move || {
            let mut logger_w = consumer_ctl.session_name.clone();
            logger_w.push(0);
            let mut logfile = EVENT_TRACE_LOGFILEW::default();
            logfile.LoggerName = windows::core::PWSTR(logger_w.as_mut_ptr());
            logfile.Anonymous1.ProcessTraceMode = EVENT_TRACE_REAL_TIME_MODE;
            logfile.Anonymous2.EventCallback = Some(etw_callback);

            let open = unsafe { OpenTraceW(&mut logfile) };
            if open.Value == u64::MAX {
                if let Some(shared) = CALLBACK.get().and_then(|s| s.lock().unwrap().take()) {
                    shared.active.store(false, Ordering::Relaxed);
                }
                return;
            }
            let trace_handle = open;
            let handles = [trace_handle];
            let _ = unsafe { ProcessTrace(&handles, None, None) };
            let _ = unsafe { CloseTrace(trace_handle) };
            // release the callback slot
            if let Some(shared) = CALLBACK.get().and_then(|s| s.lock().unwrap().take()) {
                shared.active.store(false, Ordering::Relaxed);
            }
            let _ = ctl_for_thread;
        });

        // keep the receiver alive for drain()
        self.pending_rx = Some(rx);
    }

    /// Plan 1: parallel Kernel-Process session feeding the PID->image cache.
    fn start_process_session(&mut self) {
        use windows::Win32::System::Diagnostics::Etw::*;
        let session = "appdir-proc".to_string();
        Self::stop_existing_session(&session);
        let mut session_w: Vec<u16> = session.encode_utf16().collect();
        session_w.push(0);

        let props_size = std::mem::size_of::<EVENT_TRACE_PROPERTIES>();
        let name_space = (session_w.len() + 8) * 2 + 128;
        let mut prop_bytes: Vec<u8> = vec![0; props_size + name_space];
        let props = unsafe {
            &mut *(prop_bytes.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES)
        };
        props.Wnode.BufferSize = (props_size + name_space) as u32;
        props.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        props.Wnode.ClientContext = 1;
        props.BufferSize = 64;
        props.MinimumBuffers = 4;
        props.MaximumBuffers = 16;
        props.LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
        props.LoggerNameOffset = props_size as u32;
        props.LogFileNameOffset = 0;

        let session_name = windows::core::PCWSTR(session_w.as_ptr());
        let mut handle = CONTROLTRACE_HANDLE::default();
        let start = unsafe { StartTraceW(&mut handle, session_name, props) };
        if start != WIN32_ERROR(0) {
            // non-fatal: file events still resolve normally via process handles
            self.stats
                .stop_errors
                .lock()
                .unwrap()
                .push(format!("proc StartTraceW 0x{:x}", start.0));
            return;
        }
        let enable = unsafe {
            EnableTraceEx2(
                handle,
                &KERNEL_PROCESS_GUID,
                EVENT_CONTROL_CODE_ENABLE_PROVIDER.0,
                5, // TRACE_LEVEL_VERBOSE
                0x10, // WINEVENT_KEYWORD_PROCESS only
                0,
                0,
                None,
            )
        };
        if enable != WIN32_ERROR(0) {
            let _ = unsafe { ControlTraceW(handle, session_name, props, EVENT_TRACE_CONTROL_STOP) };
            self.stats
                .stop_errors
                .lock()
                .unwrap()
                .push(format!("proc EnableTraceEx2 0x{:x}", enable.0));
            return;
        }

        let ctl = Arc::new(SessionCtl {
            handle: AtomicU64::new(handle.Value),
            session_name: session_w,
            props: Mutex::new(prop_bytes),
            stop: AtomicBool::new(false),
        });
        self.proc_ctl = Some(ctl.clone());

        let active = PROC_ACTIVE.get_or_init(|| AtomicBool::new(false));
        active.store(true, Ordering::Relaxed);
        let _ = PROC_STATS.set(self.stats.clone());

        let ctl_for_thread = ctl;
        std::thread::spawn(move || {
            let mut logger_w = ctl_for_thread.session_name.clone();
            logger_w.push(0);
            let mut logfile = EVENT_TRACE_LOGFILEW::default();
            logfile.LoggerName = windows::core::PWSTR(logger_w.as_mut_ptr());
            logfile.Anonymous1.ProcessTraceMode = EVENT_TRACE_REAL_TIME_MODE;
            logfile.Anonymous2.EventCallback = Some(proc_callback);
            let open = unsafe { OpenTraceW(&mut logfile) };
            if open.Value == u64::MAX {
                let err = unsafe { windows::Win32::Foundation::GetLastError() };
                if let Some(s) = PROC_STATS.get() {
                    s.stop_errors.lock().unwrap().push(format!(
                        "proc OpenTraceW 失败 0x{:x}", err.0
                    ));
                }
                PROC_ACTIVE.get().unwrap().store(false, Ordering::Relaxed);
                return;
            }
            let handles = [open];
            let pt = unsafe { ProcessTrace(&handles, None, None) };
            if let Some(s) = PROC_STATS.get() {
                s.stop_errors.lock().unwrap().push(format!(
                    "proc ProcessTrace 返回 0x{:x}", pt.0
                ));
            }
            let _ = unsafe { CloseTrace(open) };
            PROC_ACTIVE.get().unwrap().store(false, Ordering::Relaxed);
            let _ = ctl_for_thread;
        });
    }

    fn stop_session(&self, ctl: &Arc<SessionCtl>, provider: &windows::core::GUID) {
        use windows::Win32::System::Diagnostics::Etw::*;
        if ctl.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        let handle = CONTROLTRACE_HANDLE {
            Value: ctl.handle.load(Ordering::SeqCst),
        };
        if handle.Value != 0 {
            let _ = unsafe {
                EnableTraceEx2(
                    handle,
                    provider,
                    EVENT_CONTROL_CODE_DISABLE_PROVIDER.0,
                    0,
                    0,
                    0,
                    0,
                    None,
                )
            };
            let props_bytes = ctl.props.lock().unwrap();
            let props = unsafe {
                &*(props_bytes.as_ptr() as *const EVENT_TRACE_PROPERTIES)
            };
            let session_name = windows::core::PCWSTR(ctl.session_name.as_ptr());
            let stop = unsafe { ControlTraceW(handle, session_name, props as *const _ as *mut _, EVENT_TRACE_CONTROL_STOP) };
            if stop != WIN32_ERROR(0) {
                self.stats.stop_errors.lock().unwrap().push(format!(
                    "ControlTraceW(stop) 返回 0x{:x}",
                    stop.0
                ));
            }
            // EventsLost / RealTimeBuffersLost tell us whether capture was incomplete.
            let lost = props.EventsLost;
            let rt_lost = props.RealTimeBuffersLost;
            if lost > 0 || rt_lost > 0 {
                self.stats
                    .event_loss_reports
                    .fetch_add(lost as u64 + rt_lost as u64, Ordering::Relaxed);
            }
        }
    }
}

// pending_rx lives outside the struct literal above; declare it with the other fields.
impl EtwSource {
    fn pending_now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    /// Pure decision logic, unit-testable without a session.
    fn match_pending(
        pending: &mut Vec<PendingCreate>,
        confirmed: &mut HashSet<String>,
        stats: &EtwStats,
    ) -> Vec<PendingCreate> {
        let now = Self::pending_now_ms();
        let mut matched = Vec::new();
        let mut kept = Vec::new();
        for p in pending.drain(..) {
            let key = p.target.to_ascii_lowercase();
            if confirmed.contains(&key) {
                stats.confirmed.fetch_add(1, Ordering::Relaxed);
                confirmed.remove(&key);
                matched.push(p);
            } else if now - p.observed_at_ms > CONFIRM_WINDOW_MS {
                stats.unconfirmed_expired.fetch_add(1, Ordering::Relaxed);
            } else {
                kept.push(p);
            }
        }
        *pending = kept;
        matched
    }
}

extern "system" fn etw_callback(event_ptr: *mut windows::Win32::System::Diagnostics::Etw::EVENT_TRACE) {
    if event_ptr.is_null() {
        return;
    }
    // SAFETY: ProcessTrace owns the EVENT_TRACE for the duration of the callback.
    let event = unsafe { &*event_ptr };
    let shared = CALLBACK
        .get()
        .and_then(|s| s.lock().ok())
        .and_then(|g| match g.as_ref() {
            Some(s) if s.active.load(Ordering::Relaxed) => {
                Some(unsafe { &*(s as *const CallbackShared) })
            }
            _ => None,
        });
    let shared = match shared {
        Some(p) => p,
        None => return,
    };
    // Verified on this machine: manifest provider events arrive through the classic
    // callback with the provider GUID in Header.Anonymous3.Guid and a valid MofData
    // payload, while the EventRecordCallback path delivers null UserData. Trust only
    // the layout we actually verified here.
    let guid = unsafe { event.Header.Anonymous3.Guid };
    if guid != KERNEL_FILE_GUID {
        return;
    }
    shared.stats.kernel_file_events.fetch_add(1, Ordering::Relaxed);
    let (version, type_) = unsafe {
        (event.Header.Anonymous2.Class.Version, event.Header.Anonymous2.Class.Type)
    };
    let _ = type_;
    if event.MofData.is_null() {
        return;
    }
    let bytes = unsafe {
        std::slice::from_raw_parts(event.MofData as *const u8, event.MofLength as usize)
    };
    // Named Create(12) payload, verified: 32-byte header (Irp u64, ThreadId u64,
    // FileObject u32, CreateOptions u32, CreateAttributes u32, ShareAccess u32) followed
    // by a null-terminated UTF-16 file name. Version 0 events carry no name and are
    // ignored; anything else is counted as unparsed, never guessed.
    if version != 1 || bytes.len() < 34 || bytes.len() % 2 != 0 {
        shared.stats.unparsed.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let tail = &bytes[32..];
    let words: Vec<u16> = tail
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let end = match words.iter().position(|w| *w == 0) {
        Some(e) if e > 0 => e,
        _ => {
            shared.stats.unparsed.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    shared.stats.create_events.fetch_add(1, Ordering::Relaxed);

    let dev = String::from_utf16_lossy(&words[..end]);
    let target = match path::device_to_dos(&dev, &shared.device_map)
        .and_then(|p| path::normalize(&p))
    {
        Some(t) => t,
        None => {
            // unmapped device path: cannot be compared with configured roots reliably
            shared.stats.unparsed.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    let pid = event.Header.ProcessId;
    let exe_path = match process_image_path(pid) {
        Some(p) => Some(p),
        None => {
            let hit = proc_cache_get(pid);
            if hit.is_some() {
                shared.stats.proc_cache_hits.fetch_add(1, Ordering::Relaxed);
            }
            hit
        }
    };
    let observed_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    shared.stats.dir_candidates.fetch_add(1, Ordering::Relaxed);
    let _ = shared.tx.send(PendingCreate {
        target,
        pid,
        observed_at_ms,
        exe_path,
    });
}

/// Testable payload helper: parse a named Create payload (verified layout: 32-byte
/// header + null-terminated UTF-16 device path). Returns the device path.
fn parse_named_create(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 34 || bytes.len() % 2 != 0 {
        return None;
    }
    let tail = &bytes[32..];
    let words: Vec<u16> = tail
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let end = words.iter().position(|w| *w == 0)?;
    if end == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&words[..end]))
}

impl EventSource for EtwSource {
    fn kind(&self) -> SourceKind {
        SourceKind::Etw
    }

    fn attribution(&self) -> Attribution {
        // Exact only after confirmation: the pipeline receives events that carry both the
        // creator PID (ETW) and a witnessed appearance (watcher).
        Attribution::Exact
    }

    fn drain(&mut self, out: &mut Vec<RawCreateEvent>) -> Result<(), SourceError> {
        // 1) absorb watcher appearances into the confirmation set
        let mut watcher_events = Vec::new();
        self.watcher.drain(&mut watcher_events)?;
        self.stats
            .watcher_seen
            .fetch_add(watcher_events.len() as u64, Ordering::Relaxed);
        for ev in watcher_events {
            if let Some(norm) = path::normalize(&ev.target_path) {
                self.confirmed.insert(norm.to_ascii_lowercase());
            }
        }
        // watcher resync reports = lost names; surface as capture-incompleteness
        if self.watcher.resync_count() > 0 {
            self.stats
                .event_loss_reports
                .store(self.watcher.resync_count(), Ordering::Relaxed);
        }

        // 2) pull new ETW candidates
        if let Some(rx) = &self.pending_rx {
            while let Ok(p) = rx.try_recv() {
                self.pending.push(p);
            }
        }

        // 3) pair candidates with confirmed appearances
        let matched = Self::match_pending(&mut self.pending, &mut self.confirmed, &self.stats);

        // 4) build pipeline events. Creators that are not yet resolvable go into a
        //    short grace list and are retried on every drain: the parallel
        //    Kernel-Process session may deliver/cache the image path a few
        //    milliseconds after the file event. After GRACE_WINDOW_MS the candidate
        //    is dropped as no-creator (never guessed).
        const GRACE_WINDOW_MS: i64 = 3_000;
        let now = Self::pending_now_ms();
        let mut still_grace: Vec<PendingCreate> = Vec::new();
        let mut grace_done: Vec<PendingCreate> = Vec::new();
        for mut p in std::mem::take(&mut self.grace) {
            if p.exe_path.is_none() {
                p.exe_path = process_image_path(p.pid).or_else(|| proc_cache_get(p.pid));
            }
            if p.exe_path.is_some() {
                grace_done.push(p);
            } else if now - p.observed_at_ms >= GRACE_WINDOW_MS {
                self.stats.no_creator.fetch_add(1, Ordering::Relaxed);
            } else {
                still_grace.push(p);
            }
        }
        self.grace = still_grace;
        let mut matched_all = matched;
        matched_all.extend(grace_done);
        for p in matched_all {
            let exe = match p.exe_path.clone() {
                Some(e) => Some(e),
                None => {
                    let late = process_image_path(p.pid).or_else(|| proc_cache_get(p.pid));
                    if late.is_some() {
                        self.stats.late_resolved.fetch_add(1, Ordering::Relaxed);
                    }
                    late
                }
            };
            let exe = match exe {
                Some(e) => Some(e),
                None => {
                    self.grace.push(p);
                    continue;
                }
            };
            self.queue.push_back(RawCreateEvent {
                target_path: p.target,
                request: CreateRequest::dir_create(),
                information: IO_FILE_CREATED,
                success: true,
                creator: ProcessIdentity {
                    pid: p.pid,
                    exe_path: exe,
                },
                observed_at_ms: p.observed_at_ms,
                source: SourceKind::Etw,
            });
        }
        out.extend(self.queue.drain(..));
        Ok(())
    }

    fn link_state(&self) -> LinkState {
        if self.session_started {
            LinkState::Running
        } else {
            LinkState::Disconnected
        }
    }

    fn shutdown(&mut self) -> Result<(), SourceError> {
        if let Some(ctl) = self.proc_ctl.take() {
            self.stop_session(&ctl, &KERNEL_PROCESS_GUID);
        }
        if let Some(ctl) = self.ctl.take() {
            self.stop_session(&ctl, &KERNEL_FILE_GUID);
        }
        self.watcher.shutdown()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_cmd_exe_from_real_payload() {
        let hex = "ac350000691100000000000015e18207c85edd01184000006611000000000000010000000000000001000000010000000101000000000010003000005c004400650076006900630065005c0048006100720064006400690073006b0056006f006c0075006d00650033005c00570069006e0064006f00770073005c00530079007300740065006d00330032005c0063006d0064002e0065007800650000000c36050000d457d500";
        let mut bytes = Vec::new();
        for i in (0..hex.len()).step_by(2) {
            bytes.push(u8::from_str_radix(&hex[i..i + 2], 16).unwrap());
        }
        let r = extract_proc_fields(&bytes);
        // debug: walk the same steps with prints
        let words: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let idx: Vec<usize> = (0..words.len()).filter(|&i| words[i] == 0x005c).collect();
        eprintln!("backslash word idx: {idx:?}");
        eprintln!("words[26..40]: {:?}", &words[26..40]);
        // step-by-step replication to find the failing stage
        let start = 30usize;
        let mut end = None;
        for i in start..words.len().saturating_sub(4) {
            if u16_lower(words[i]) == b'.' as u16
                && u16_lower(words[i + 1]) == b'e' as u16
                && u16_lower(words[i + 2]) == b'x' as u16
                && u16_lower(words[i + 3]) == b'e' as u16
            { end = Some(i + 4); break; }
        }
        eprintln!("end={end:?}");
        let img = String::from_utf16_lossy(&words[start..end.unwrap()]);
        eprintln!("img={img:?}");
        let dos = appdir_core::path::device_to_dos(&img, &PROC_DEVICE_MAP);
        eprintln!("dos={dos:?} map={:?}", &*PROC_DEVICE_MAP);
        let norm = dos.as_ref().and_then(|p| appdir_core::path::normalize(p));
        eprintln!("norm={norm:?}");
        assert!(r.is_some(), "extract failed for real cmd.exe payload");
        let (pid, img) = r.unwrap();
        assert_eq!(pid, 0x35ac);
        assert!(img.to_ascii_lowercase().ends_with("cmd.exe"), "img={img}");
    }

    /// Verified layout: 32-byte header + UTF-16 null-terminated device path.
    fn named_payload(name: &str) -> Vec<u8> {
        let mut data = vec![0u8; 32];
        for w in name.encode_utf16().chain(std::iter::once(0)) {
            data.extend_from_slice(&w.to_le_bytes());
        }
        data
    }

    #[test]
    fn parses_named_create_payload() {
        let bytes = named_payload(r"\Device\HarddiskVolume3\Temp\Alpha");
        let path = parse_named_create(&bytes).unwrap();
        assert!(path.ends_with("Alpha"));
        assert!(path.starts_with(r"\Device\"));
    }

    #[test]
    fn rejects_short_and_nameless_payloads() {
        assert!(parse_named_create(&[0u8; 20]).is_none(), "version-0 nameless event");
        assert!(parse_named_create(&[0u8; 30]).is_none(), "too short");
        // name starts immediately with the terminator: no path
        assert!(parse_named_create(&named_payload("")).is_none());
    }

    #[test]
    fn match_pending_pairs_confirms_and_expires() {
        let stats = EtwStats::default();
        let mut confirmed: HashSet<String> = HashSet::new();
        confirmed.insert(r"c:\watched\alpha".to_ascii_lowercase());
        let mut pending = vec![PendingCreate {
            target: r"C:\Watched\Alpha".to_string(),
            pid: 1,
            observed_at_ms: EtwSource::pending_now_ms(),
            exe_path: Some("C:\\app.exe".to_string()),
        }];
        let matched = EtwSource::match_pending(&mut pending, &mut confirmed, &stats);
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].target, r"C:\Watched\Alpha");
        assert!(pending.is_empty() && confirmed.is_empty());

        // expired, never confirmed
        let mut pending = vec![PendingCreate {
            target: r"C:\Watched\Gone".to_string(),
            pid: 1,
            observed_at_ms: EtwSource::pending_now_ms() - CONFIRM_WINDOW_MS - 1,
            exe_path: Some("C:\\app.exe".to_string()),
        }];
        let matched = EtwSource::match_pending(&mut pending, &mut confirmed, &stats);
        assert!(matched.is_empty());
        assert_eq!(stats.unconfirmed_expired.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn source_kind_and_confidence_are_wired() {
        assert_eq!(SourceKind::Etw.as_str(), "etw");
        assert_eq!(SourceKind::parse("etw"), Some(SourceKind::Etw));
        assert_eq!(SourceKind::Etw.default_confidence(), "etw-create-confirmed");
    }
}
