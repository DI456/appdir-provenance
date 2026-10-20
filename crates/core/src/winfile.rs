///! Tiny hand-written kernel32 bindings, used only for file metadata, change
///! notifications and process image names. No driver, no hooking, nothing that can
///! destabilise the machine.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// FILETIME ticks (100 ns) between 1601-01-01 and 1970-01-01.
pub const FILETIME_UNIX_OFFSET_100NS: u64 = 116_444_736_000_000_000;

pub type Handle = *mut c_void;
pub const INVALID_HANDLE: Handle = !0usize as Handle;

pub const OPEN_EXISTING: u32 = 3;
pub const FILE_SHARE_ALL: u32 = 0x0000_0007;
pub const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
pub const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
pub const FILE_LIST_DIRECTORY: u32 = 0x0000_0001;
/// Correct values from winnt.h. Getting these wrong silently watches nothing,
/// so they are listed explicitly instead of derived.
pub const FILE_NOTIFY_CHANGE_FILE_NAME: u32 = 0x0000_0001;
pub const FILE_NOTIFY_CHANGE_DIR_NAME: u32 = 0x0000_0002;
pub const FILE_NOTIFY_CHANGE_ATTRIBUTES: u32 = 0x0000_0004;
pub const FILE_NOTIFY_CHANGE_SIZE: u32 = 0x0000_0008;
pub const FILE_NOTIFY_CHANGE_LAST_WRITE: u32 = 0x0000_0010;
pub const FILE_NOTIFY_CHANGE_CREATION: u32 = 0x0000_0020;
pub const FILE_NOTIFY_CHANGE_SECURITY: u32 = 0x0000_0080;
pub const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

pub const WAIT_TIMEOUT: u32 = 0x0000_0102;
pub const WAIT_OBJECT_0: u32 = 0;
pub const WAIT_FAILED: u32 = u32::MAX;

pub const FILE_ACTION_ADDED: u32 = 0x0000_0001;
pub const FILE_ACTION_REMOVED: u32 = 0x0000_0002;
pub const FILE_ACTION_MODIFIED: u32 = 0x0000_0003;
pub const FILE_ACTION_RENAMED_OLD_NAME: u32 = 0x0000_0004;
pub const FILE_ACTION_RENAMED_NEW_NAME: u32 = 0x0000_0005;

static CTRL_C: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// True once Ctrl-C was seen. Installed lazily by the daemon loop.
pub fn ctrl_c_requested() -> bool {
    CTRL_C.load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(windows)]
extern "system" {
    fn SetConsoleCtrlHandler(routine: Option<extern "system" fn(u32) -> i32>, add: i32) -> i32;
}

#[cfg(windows)]
pub const CTRL_C_EVENT: u32 = 0;
#[cfg(windows)]
pub const CTRL_CLOSE_EVENT: u32 = 2;

#[cfg(windows)]
extern "system" fn ctrl_handler(ctrl_type: u32) -> i32 {
    CTRL_C.store(true, std::sync::atomic::Ordering::SeqCst);
    // Handle the event ourselves so the daemon loop can close its handles first.
    let _ = ctrl_type;
    1
}

/// Install the Ctrl-C guard once. Returns false when the console refuses it.
#[cfg(windows)]
pub fn install_ctrl_c_guard() -> bool {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    let mut ok = true;
    ONCE.call_once(|| {
        ok = unsafe { SetConsoleCtrlHandler(Some(ctrl_handler), 1) != 0 };
        GUARD_OK.store(ok, std::sync::atomic::Ordering::SeqCst);
    });
    GUARD_OK.load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(windows)]
static GUARD_OK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(not(windows))]
pub fn install_ctrl_c_guard() -> bool {
    false
}

/// Error codes we translate for operators (winerror.h).
pub const ERROR_ACCESS_DENIED: u32 = 5;
pub const ERROR_DIR_NOT_EMPTY: u32 = 145;
pub const ERROR_SHARING_VIOLATION: u32 = 32;
pub const ERROR_FILE_NOT_FOUND: u32 = 2;
pub const ERROR_PATH_NOT_FOUND: u32 = 3;
pub const ERROR_NOTIFY_ENUM_DIR: u32 = 1077;
pub const ERROR_OPERATION_ABORTED: u32 = 995;
pub const ERROR_IO_INCOMPLETE: u32 = 997;
pub const ERROR_INVALID_PARAMETER: u32 = 87;

#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
pub struct FileTime {
    pub low: u32,
    pub high: u32,
}

impl FileTime {
    pub fn as_100ns(&self) -> u64 {
        ((self.high as u64) << 32) | self.low as u64
    }

    /// Convert to Unix milliseconds; 0 when empty or before the epoch.
    pub fn to_unix_ms(&self) -> i64 {
        let t = self.as_100ns();
        if t < FILETIME_UNIX_OFFSET_100NS {
            return 0;
        }
        ((t - FILETIME_UNIX_OFFSET_100NS) / 10_000) as i64
    }
}

#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
pub struct ByHandleFileInformation {
    pub file_attributes: u32,
    pub creation_time: FileTime,
    pub last_access_time: FileTime,
    pub last_write_time: FileTime,
    pub volume_serial_number: u32,
    pub file_size_high: u32,
    pub file_size_low: u32,
    pub number_of_links: u32,
    pub file_index_high: u32,
    pub file_index_low: u32,
}

#[repr(C)]
pub struct Overlapped {
    pub internal: usize,
    pub internal_high: usize,
    pub offset: u32,
    pub offset_high: u32,
    pub event: Handle,
}

pub type IoCompletion = Option<extern "system" fn(u32, u32, *mut Overlapped)>;

#[link(name = "kernel32")]
extern "system" {
    pub fn CreateFileW(
        name: *const u16,
        access: u32,
        share_mode: u32,
        security: *mut c_void,
        disposition: u32,
        flags: u32,
        template: Handle,
    ) -> Handle;
    pub fn GetFileInformationByHandle(handle: Handle, info: *mut ByHandleFileInformation) -> i32;
    pub fn CloseHandle(handle: Handle) -> i32;
    pub fn GetLastError() -> u32;
    pub fn CreateEventW(sa: *mut c_void, manual_reset: i32, initial: i32, name: *const u16) -> Handle;
    pub fn ReadDirectoryChangesW(
        dir: Handle,
        buf: *mut c_void,
        buf_len: u32,
        watch_subtree: i32,
        filter: u32,
        returned: *mut u32,
        overlapped: *mut Overlapped,
        completion: IoCompletion,
    ) -> i32;
    pub fn GetOverlappedResult(dir: Handle, overlapped: *mut Overlapped, returned: *mut u32, wait: i32) -> i32;
    pub fn CancelIoEx(dir: Handle, overlapped: *mut Overlapped) -> i32;
    pub fn WaitForSingleObject(handle: Handle, ms: u32) -> u32;
    pub fn QueryDosDeviceW(name: *const u16, target: *mut u16, max: u32) -> u32;
    pub fn GetLogicalDrives() -> u32;
    pub fn QueryFullProcessImageNameW(process: Handle, flags: u32, exe: *mut u16, size: *mut u32) -> i32;
    pub fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
}

pub fn to_wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

pub fn to_wide_str(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Open a path for metadata only: no requested access, backup semantics so directories
/// are allowed, full sharing so we never block another process.
pub fn open_metadata(path: &Path) -> Result<Handle, u32> {
    let wide = to_wide(path);
    let h = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_ALL,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE || h.is_null() {
        Err(unsafe { GetLastError() })
    } else {
        Ok(h)
    }
}

pub fn info_of(handle: Handle) -> Result<ByHandleFileInformation, u32> {
    let mut info: ByHandleFileInformation = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0 {
        Err(unsafe { GetLastError() })
    } else {
        Ok(info)
    }
}

pub fn close(handle: Handle) {
    unsafe {
        CloseHandle(handle);
    }
}

pub fn stat(path: &Path) -> Result<ByHandleFileInformation, u32> {
    let h = open_metadata(path)?;
    let r = info_of(h);
    close(h);
    r
}

/// The sharing mode used when this tool opens a file it must not interfere with: read and
/// write are shared, delete is not, exactly like a running application holding its own
/// data file open.
pub const FILE_SHARE_READ_WRITE: u32 = 0x0000_0003;

pub fn file_index(info: &ByHandleFileInformation) -> u128 {
    ((info.file_index_high as u128) << 32) | info.file_index_low as u128
}

/// Full image path of a live process. `None` when the process already exited or when the
/// query is denied: we never guess an owner.
pub fn process_image_path(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h == INVALID_HANDLE || h.is_null() {
            return None;
        }
        let mut buf = vec![0u16; 4096];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len);
        let out = if ok == 0 || len == 0 {
            None
        } else {
            Some(String::from_utf16_lossy(&buf[..len as usize]))
        };
        CloseHandle(h);
        out
    }
}

/// Mounted drive letters with their device objects, so a driver path such as
/// `\Device\HarddiskVolume3\Users\a` can be compared with `C:\Users\a`.
pub fn device_map() -> crate::path::DeviceMap {
    let mut map = crate::path::DeviceMap::new();
    let mask = unsafe { GetLogicalDrives() };
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let name = format!("{letter}:");
        let wide = to_wide_str(&name);
        let mut buf = vec![0u16; 1024];
        let n = unsafe { QueryDosDeviceW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
        if n == 0 {
            continue;
        }
        let end = buf.iter().position(|c| *c == 0).unwrap_or(0);
        let dev = String::from_utf16_lossy(&buf[..end]);
        if !dev.is_empty() {
            map.push((name, dev));
        }
    }
    map
}

/// Long-path aware prefix, so deep trees are not silently truncated.
pub fn long_path(p: &str) -> String {
    if p.starts_with("\\??\\") {
        return format!("\\\\?\\{}", &p[4..]);
    }
    if p.starts_with("\\\\?\\") {
        return p.to_string();
    }
    if p.starts_with("\\\\") {
        return format!("\\\\?\\UNC{}", &p[1..]);
    }
    format!("\\\\?\\{p}")
}

/// Human-readable Win32 error name for the codes that matter here.
pub fn error_name(code: u32) -> &'static str {
    match code {
        ERROR_ACCESS_DENIED => "ACCESS_DENIED",
        ERROR_DIR_NOT_EMPTY => "DIR_NOT_EMPTY",
        ERROR_SHARING_VIOLATION => "SHARING_VIOLATION",
        ERROR_FILE_NOT_FOUND => "FILE_NOT_FOUND",
        ERROR_PATH_NOT_FOUND => "PATH_NOT_FOUND",
        ERROR_NOTIFY_ENUM_DIR => "NOTIFY_ENUM_DIR",
        ERROR_OPERATION_ABORTED => "OPERATION_ABORTED",
        ERROR_IO_INCOMPLETE => "IO_INCOMPLETE",
        ERROR_INVALID_PARAMETER => "INVALID_PARAMETER",
        _ => "UNKNOWN",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_conversion() {
        assert_eq!(FileTime { low: 0, high: 0 }.to_unix_ms(), 0);
        let epoch = FileTime {
            low: (FILETIME_UNIX_OFFSET_100NS / 10_000) as u32,
            high: ((FILETIME_UNIX_OFFSET_100NS / 10_000) >> 32) as u32,
        };
        assert_eq!(epoch.to_unix_ms(), 0);
    }

    #[test]
    fn stats_a_real_directory() {
        let dir = std::env::temp_dir();
        let info = match stat(&dir) {
            Ok(i) => i,
            Err(c) => panic!("temp dir must be queryable, got {c}"),
        };
        assert_ne!(info.volume_serial_number, 0);
        assert_ne!(file_index(&info), 0);
        assert_ne!(info.file_attributes & FILE_ATTRIBUTE_DIRECTORY, 0);
        assert!(info.creation_time.to_unix_ms() > 0);
    }

    #[test]
    fn missing_path_fails_with_code() {
        let e = stat(Path::new("C:\\definitely-not-here-42\\x")).unwrap_err();
        assert!(e == ERROR_FILE_NOT_FOUND || e == ERROR_PATH_NOT_FOUND, "got {e}");
    }

    #[test]
    fn device_map_finds_system_drive() {
        let map = device_map();
        assert!(!map.is_empty());
        let sys = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
        let hit = map.iter().find(|(d, _)| d.eq_ignore_ascii_case(&sys));
        assert!(hit.is_some(), "system drive {sys} missing from {map:?}");
        let dev = &hit.unwrap().1;
        let conv = crate::path::device_to_dos(&format!("{dev}\\Windows\\System32"), &map).unwrap();
        assert!(conv.to_ascii_lowercase().ends_with("\\windows\\system32"), "got {conv}");
    }

    #[test]
    fn long_path_prefix() {
        assert_eq!(long_path("C:\\a"), "\\\\?\\C:\\a");
        assert_eq!(long_path("\\\\srv\\sh\\a"), "\\\\?\\UNC\\srv\\sh\\a");
        assert_eq!(long_path("\\\\?\\C:\\a"), "\\\\?\\C:\\a");
        assert_eq!(long_path("\\??\\C:\\a"), "\\\\?\\C:\\a");
    }

    #[test]
    fn own_process_image_path_is_known() {
        let pid = std::process::id();
        let p = process_image_path(pid).expect("own image path must resolve");
        assert!(p.to_ascii_lowercase().ends_with(".exe"), "got {p}");
        assert!(crate::path::parse(&p).is_some(), "{p} must parse");
    }

    #[test]
    fn dead_process_has_no_image_path() {
        assert!(process_image_path(u32::MAX - 1).is_none());
    }
}
