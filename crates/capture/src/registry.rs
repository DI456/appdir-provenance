//! ETW 注册表监视源 (初版 v1)。
//!
//! 复用 etw.rs 验证过的经典回调管线 (EVENT_TRACE_LOGFILEW + EventCallback):
//! 本机已验证 manifest provider 事件走 Header.Anonymous3.Guid + MofData。
//!
//! Provider: Microsoft-Windows-Kernel-Registry (内核 provider, 需管理员)。
//! 事件语义 (与 etw.rs 同一原则, 不猜):
//! * id 1 KcbCreate: KCB(8) + 键名 utf16。维护 KCB -> 键路径 映射。
//! * id 2 KcbDelete: KCB(8)。从映射中移除。
//! * id 6 SetValueKey / id 7 DeleteValueKey: KCB(8) + 值名 utf16 (前面有少量 u32
//!   字段, 各版本偏移不一, 故按候选偏移探测, 探测不到就计为未解析, 绝不猜测)。
//!   键路径由 KCB 映射解析; 解析不到的事件直接丢弃并计数。
//!
//! 输出: RegistryEvent 列表, daemon 落盘到 ledger 同目录的 registry.jsonl。
//! v1 不进账本/清理链路 —— 那需要先定义注册表记录的安全语义。

use windows::Win32::Foundation::WIN32_ERROR;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};

/// Microsoft-Windows-Kernel-Registry
const KERNEL_REGISTRY_GUID: windows::core::GUID =
    windows::core::GUID::from_u128(0x70EB4F03_C1DE_4F73_A051_33D13D5413BD);

const SESSION_NAME: &str = "appdir-reg";
// 实测本机 (经典回调 Class.Type): 经典类型 = manifest 事件ID + 32。
// ver0id33=KcbCreate(5521/6s) ver0id38=SetValue(5851/6s) ver0id35=QueryKey ver0id44=CreateKey
const EVT_KCB_CREATE: u8 = 1 + 32;
const EVT_KCB_DELETE: u8 = 2 + 32;
const EVT_SET_VALUE: u8 = 6 + 32;
const EVT_DELETE_VALUE: u8 = 7 + 32;

#[derive(Debug, Default)]
pub struct RegStats {
    pub total_events: AtomicU64,
    pub id_hist: Mutex<HashMap<u32, u64>>,
    pub errors: Mutex<Vec<String>>,
    pub received: AtomicU64,
    pub kcb_parsed: AtomicU64,
    pub kcb_unparsed: AtomicU64,
    pub value_events: AtomicU64,
    pub value_unresolved: AtomicU64,
    pub value_unparsed: AtomicU64,
    pub filtered_out: AtomicU64,
    pub emitted: AtomicU64,
}

impl RegStats {
    pub fn snapshot(&self) -> String {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let s = format!(
            "registry: 收到 {} kcb建图 {} kcb未解析 {} 值事件 {} 键未解析 {} 值名未解析 {} 前缀过滤 {} 输出 {}",
            g(&self.received),
            g(&self.kcb_parsed),
            g(&self.kcb_unparsed),
            g(&self.value_events),
            g(&self.value_unresolved),
            g(&self.value_unparsed),
            g(&self.filtered_out),
            g(&self.emitted),
        );
        let errs = self.errors.lock().map(|e| e.join("; ")).unwrap_or_default();
        let mut hist: Vec<String> = Vec::new();
        if let Ok(h) = self.id_hist.lock() {
            let mut v: Vec<_> = h.iter().collect();
            v.sort_by_key(|(_, c)| std::cmp::Reverse(**c));
            for (k, c) in v.iter().take(12) {
                hist.push(format!("ver{}id{}={}", **k >> 16, **k & 0xFFFF, **c));
            }
        }
        let mut out = s;
        if !hist.is_empty() { out = format!("{out} | id直方图: {}", hist.join(" ")); }
        if !errs.is_empty() { out = format!("{out} | {errs}"); }
        out
    }
}

/// 一条注册表事件。op: "set" | "delval"。
#[derive(Debug, Clone)]
pub struct RegistryEvent {
    pub ts_unix_ms: i64,
    pub pid: u32,
    pub op: &'static str,
    pub key: String,
    pub value: String,
}

struct RegShared {
    tx: mpsc::Sender<RegistryEvent>,
    stats: Arc<RegStats>,
    active: AtomicBool,
    kcbs: Mutex<HashMap<u64, String>>,
}

static REG_CALLBACK: OnceLock<Mutex<Option<RegShared>>> = OnceLock::new();

struct RegSessionCtl {
    handle: AtomicU64,
    session_name: Vec<u16>,
    props: Mutex<Vec<u8>>,
    stopped: AtomicBool,
}

pub struct RegistrySource {
    prefixes: Vec<String>, // 小写规范化
    stats: Arc<RegStats>,
    rx: Option<mpsc::Receiver<RegistryEvent>>,
    ctl: Option<Arc<RegSessionCtl>>,
    start_error: Mutex<Option<String>>,
}

fn utf16_at(buf: &[u8], off: usize) -> Option<String> {
    if off == 0 || off >= buf.len() || buf.len() % 2 != 0 {
        return None;
    }
    let tail = &buf[off..];
    if tail.len() < 2 {
        return None;
    }
    let words: Vec<u16> = tail
        .chunks_exact(2)
        .take_while(|c| u16::from_le_bytes([c[0], c[1]]) != 0)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    if words.is_empty() {
        return None;
    }
    let s = String::from_utf16_lossy(&words);
    // 合理性检查: 注册表路径/值名不应含控制字符, 长度有上界
    if s.len() > 1024 || s.chars().any(|c| c.is_control()) {
        return None;
    }
    Some(s)
}

/// 键名必须形如 \REGISTRY\... 才可信。
fn find_key_name_offset(bytes: &[u8], from: usize) -> Option<usize> {
    const PAT: [u16; 9] = [0x5C, 0x52, 0x45, 0x47, 0x49, 0x53, 0x54, 0x52, 0x59]; // backslash-REGISTRY
    let mut i = from + (from & 1);
    while i + PAT.len() * 2 <= bytes.len() {
        let ok = (0..PAT.len()).all(|k| u16::from_le_bytes([bytes[i + k * 2], bytes[i + k * 2 + 1]]) == PAT[k]);
        if ok {
            return Some(i);
        }
        i += 2;
    }
    None
}
fn plausible_key(s: &str) -> bool {
    s.starts_with("\\REGISTRY\\") || s.starts_with("\\Registry\\")
}

fn normalize_key(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("\\registry\\machine\\") {
        format!("HKLM\\{rest}")
    } else if let Some(rest) = lower.strip_prefix("\\registry\\user\\") {
        format!("HKU\\{rest}")
    } else if let Some(rest) = lower.strip_prefix("\\registry\\") {
        format!("HK\\{rest}")
    } else {
        s.to_string()
    }
}

impl RegistrySource {
    /// prefixes: 只保留键路径以这些前缀开头的事件; 为空则收全部 (噪声很大, 生产建议给前缀)。
    pub fn new<I, S>(prefixes: I) -> Result<RegistrySource, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut src = RegistrySource {
            prefixes: prefixes
                .into_iter()
                .map(|p| normalize_key(p.as_ref()))
                .collect(),
            stats: Arc::new(RegStats::default()),
            rx: None,
            ctl: None,
            start_error: Mutex::new(None),
        };
        src.start_session()?;
        Ok(src)
    }

    pub fn stats(&self) -> &Arc<RegStats> {
        &self.stats
    }

    pub fn start_error(&self) -> Option<String> {
        self.start_error.lock().unwrap().clone()
    }

    fn stop_existing_session() {
        use windows::Win32::System::Diagnostics::Etw::*;
        let mut session_w: Vec<u16> = SESSION_NAME.encode_utf16().collect();
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

    fn start_session(&mut self) -> Result<(), String> {
        use windows::Win32::System::Diagnostics::Etw::*;
        Self::stop_existing_session();
        let mut session_w: Vec<u16> = SESSION_NAME.encode_utf16().collect();
        session_w.push(0);

        let props_size = std::mem::size_of::<EVENT_TRACE_PROPERTIES>();
        let name_space = (session_w.len() + 8) * 2 + 128;
        let mut prop_bytes: Vec<u8> = vec![0; props_size + name_space];
        let props = unsafe { &mut *(prop_bytes.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES) };
        props.Wnode.BufferSize = (props_size + name_space) as u32;
        props.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        props.Wnode.ClientContext = 1;
        props.BufferSize = 256; // 注册表事件速率高, 缓冲给大一些
        props.MinimumBuffers = 16;
        props.MaximumBuffers = 64;
        props.LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
        props.LoggerNameOffset = props_size as u32;
        props.LogFileNameOffset = 0;

        let session_name = windows::core::PCWSTR(session_w.as_ptr());
        let mut handle = CONTROLTRACE_HANDLE::default();
        let start = unsafe { StartTraceW(&mut handle, session_name, props) };
        if start != WIN32_ERROR(0) {
            return Err(format!("StartTraceW 0x{:x} (需要管理员权限)", start.0));
        }
        let enable = unsafe {
            EnableTraceEx2(
                handle,
                &KERNEL_REGISTRY_GUID,
                EVENT_CONTROL_CODE_ENABLE_PROVIDER.0,
                5, // TRACE_LEVEL_VERBOSE
                0xFFFFFFFFFFFFFFFF, // 全部 keyword
                0,
                0,
                None,
            )
        };
        if enable != WIN32_ERROR(0) {
            let _ = unsafe { ControlTraceW(handle, session_name, props, EVENT_TRACE_CONTROL_STOP) };
            return Err(format!("EnableTraceEx2 0x{:x}", enable.0));
        }

        let ctl = Arc::new(RegSessionCtl {
            handle: AtomicU64::new(handle.Value),
            session_name: session_w,
            props: Mutex::new(prop_bytes),
            stopped: AtomicBool::new(false),
        });
        self.ctl = Some(ctl.clone());

        let (tx, rx) = mpsc::channel::<RegistryEvent>();
        self.rx = Some(rx);
        let shared = RegShared {
            tx,
            stats: self.stats.clone(),
            active: AtomicBool::new(true),
            kcbs: Mutex::new(HashMap::new()),
        };
        let slot = REG_CALLBACK.get_or_init(|| Mutex::new(None));
        *slot.lock().unwrap() = Some(shared);

        let ctl_for_thread = ctl;
        std::thread::spawn(move || {
            let mut logger_w = ctl_for_thread.session_name.clone();
            logger_w.push(0);
            let mut logfile = EVENT_TRACE_LOGFILEW::default();
            logfile.LoggerName = windows::core::PWSTR(logger_w.as_mut_ptr());
            logfile.Anonymous1.ProcessTraceMode = EVENT_TRACE_REAL_TIME_MODE;
            logfile.Anonymous2.EventCallback = Some(reg_callback);
            let open = unsafe { OpenTraceW(&mut logfile) };
            if open.Value == u64::MAX {
                let err = unsafe { windows::Win32::Foundation::GetLastError() };
                if let Some(slot) = REG_CALLBACK.get() {
                    if let Ok(mut g) = slot.lock() {
                        if let Some(s) = g.as_ref() {
                            s.stats.errors.lock().unwrap().push(format!("OpenTraceW 失败 0x{:x}", err.0));
                        }
                    }
                }
                let _ = ctl_for_thread;
                return;
            }
            let handles = [open];
            let pt = unsafe { ProcessTrace(&handles, None, None) };
            if let Some(slot) = REG_CALLBACK.get() {
                if let Ok(mut g) = slot.lock() {
                    if let Some(s) = g.as_ref() {
                        s.stats.errors.lock().unwrap().push(format!("ProcessTrace 返回 0x{:x} 总事件 {}", pt.0, s.stats.total_events.load(Ordering::Relaxed)));
                    }
                }
            }
            let _ = unsafe { CloseTrace(open) };
        });
        Ok(())
    }

    /// 取出累积的事件 (非阻塞)。
    pub fn drain(&mut self) -> Vec<RegistryEvent> {
        let mut out = Vec::new();
        if let Some(rx) = &self.rx {
            for ev in rx.try_iter() {
                if !self.prefixes.is_empty() {
                    let kl = ev.key.to_ascii_lowercase();
                    if !self.prefixes.iter().any(|p| kl.starts_with(p)) {
                        self.stats.filtered_out.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                }
                self.stats.emitted.fetch_add(1, Ordering::Relaxed);
                out.push(ev);
            }
        }
        out
    }

    pub fn shutdown(&mut self) {
        use windows::Win32::System::Diagnostics::Etw::*;
        let Some(ctl) = self.ctl.take() else { return };
        if ctl.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(slot) = REG_CALLBACK.get() {
            if let Ok(mut g) = slot.lock() {
                if let Some(s) = g.as_ref() {
                    s.active.store(false, Ordering::Relaxed);
                }
            }
        }
        let handle = CONTROLTRACE_HANDLE {
            Value: ctl.handle.load(Ordering::SeqCst),
        };
        if handle.Value != 0 {
            let _ = unsafe {
                EnableTraceEx2(
                    handle,
                    &KERNEL_REGISTRY_GUID,
                    EVENT_CONTROL_CODE_DISABLE_PROVIDER.0,
                    0,
                    0,
                    0,
                    0,
                    None,
                )
            };
            let mut props_bytes = ctl.props.lock().unwrap();
            let props = unsafe { &mut *(props_bytes.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES) };
            let session_name = windows::core::PCWSTR(ctl.session_name.as_ptr());
            unsafe {
                let _ = ControlTraceW(handle, session_name, props, EVENT_TRACE_CONTROL_STOP);
            }
        }
    }
}

impl Drop for RegistrySource {
    fn drop(&mut self) {
        self.shutdown();
    }
}

extern "system" fn reg_callback(event_ptr: *mut windows::Win32::System::Diagnostics::Etw::EVENT_TRACE) {
    if event_ptr.is_null() {
        return;
    }
    let event = unsafe { &*event_ptr };
    let shared = REG_CALLBACK
        .get()
        .and_then(|s| s.lock().ok())
        .and_then(|g| match g.as_ref() {
            Some(s) if s.active.load(Ordering::Relaxed) => Some(unsafe { &*(s as *const RegShared) }),
            _ => None,
        });
    let shared = match shared {
        Some(s) => s,
        None => return,
    };
    shared.stats.total_events.fetch_add(1, Ordering::Relaxed);
    {
        let id = unsafe { event.Header.Anonymous2.Class.Type } as u32;
        let ver = unsafe { event.Header.Anonymous2.Class.Version } as u32;
        if let Ok(mut h) = shared.stats.id_hist.lock() {
            *h.entry((ver << 16) | id).or_insert(0) += 1;
        }
    }
    let guid = unsafe { event.Header.Anonymous3.Guid };
    if guid != KERNEL_REGISTRY_GUID {
        let mut errs = shared.stats.errors.lock().unwrap();
        let g = format!("{:08x}-{:04x}-{:04x}-{:02x}{:02x}", guid.data1, guid.data2, guid.data3, guid.data4[0], guid.data4[1]);
        if errs.iter().filter(|e| e.starts_with("guid:")).count() < 8 && !errs.iter().any(|e| *e == format!("guid:{g}")) {
            errs.push(format!("guid:{g} type={}", unsafe { event.Header.Anonymous2.Class.Type }));
        }
        return;
    }
    shared.stats.received.fetch_add(1, Ordering::Relaxed);
    if event.MofData.is_null() {
        return;
    }
    let bytes = unsafe {
        std::slice::from_raw_parts(event.MofData as *const u8, event.MofLength as usize)
    };
    let id = unsafe { event.Header.Anonymous2.Class.Type };
    match id {
        EVT_KCB_CREATE => {
            if bytes.len() < 8 {
                shared.stats.kcb_unparsed.fetch_add(1, Ordering::Relaxed);
                return;
            }
            let kcb = u64::from_le_bytes(bytes[..8].try_into().unwrap());
            let name = find_key_name_offset(bytes, 8).and_then(|off| utf16_at(bytes, off));
            match name {
                Some(n) => {
                    shared.stats.kcb_parsed.fetch_add(1, Ordering::Relaxed);
                    if let Ok(mut kcbs) = shared.kcbs.lock() {
                        kcbs.insert(kcb, n);
                    }
                }
                None => {
                    shared.stats.kcb_unparsed.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        EVT_KCB_DELETE => {
            if bytes.len() < 8 {
                return;
            }
            let kcb = u64::from_le_bytes(bytes[..8].try_into().unwrap());
            if let Ok(mut kcbs) = shared.kcbs.lock() {
                kcbs.remove(&kcb);
            }
        }
        EVT_SET_VALUE | EVT_DELETE_VALUE => {
            shared.stats.value_events.fetch_add(1, Ordering::Relaxed);
            if bytes.len() < 8 {
                shared.stats.value_unparsed.fetch_add(1, Ordering::Relaxed);
                return;
            }
            let kcb = u64::from_le_bytes(bytes[..8].try_into().unwrap());
            let key = shared
                .kcbs
                .lock()
                .ok()
                .and_then(|m| m.get(&kcb).cloned());
            let Some(key) = key else {
                shared.stats.value_unresolved.fetch_add(1, Ordering::Relaxed);
                return;
            };
            // 实测本机 id38 布局: KCB(8)+Status(4)+Type(4)+DataSize(4)+填充(2), 值名在 22。
            // id39 (DeleteValue) 未实测, 按候选偏移探测。
            let value = match id {
                EVT_SET_VALUE => utf16_at(bytes, 22),
                _ => [12usize, 16, 20, 22].iter().find_map(|&off| utf16_at(bytes, off)),
            };
            let op = if id == EVT_SET_VALUE { "set" } else { "delval" };
            let ts = unsafe { event.Header.TimeStamp } as i64;
            let ev = RegistryEvent {
                ts_unix_ms: ts / 10_000 - 11_644_473_600_000,
                pid: unsafe { event.Header.ProcessId },
                op,
                key: normalize_key(&key),
                value: value.unwrap_or_default(),
            };
            let _ = shared.tx.send(ev);
        }
        _ => {}
    }
}