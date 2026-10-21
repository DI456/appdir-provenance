//! 检测并结束占用文件的进程 (核心逻辑移植自 UnlockDelete 项目的 Restart Manager 封装)。
//!
//! - 用 rstrtmgr.dll 的 Restart Manager API 枚举占用指定路径 (文件/目录) 的进程。
//! - 目录会展开成自身 + 内部文件列表后注册 (Restart Manager 只认文件)。
//! - kill_process 用 OpenProcess(PROCESS_TERMINATE) + TerminateProcess 结束单个进程。

#[derive(Debug, Clone)]
pub struct LockingProcess {
    pub pid: u32,
    pub name: String,
}

pub use imp::{find_locking_processes, kill_process};

#[cfg(windows)]
mod imp {
    use super::LockingProcess;
    use std::mem;

    type DWORD = u32;
    type UINT = u32;
    type BOOL = i32;
    type HANDLE = isize;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct RmUniqueProcess {
        dw_process_id: DWORD,
        process_start_time: [DWORD; 2], // FILETIME
    }

    const CCH_RM_MAX_APP_NAME: usize = 255;
    const CCH_RM_MAX_SVC_NAME: usize = 63;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct RmProcessInfo {
        process: RmUniqueProcess,
        app_name: [u16; CCH_RM_MAX_APP_NAME + 1],
        service_short_name: [u16; CCH_RM_MAX_SVC_NAME + 1],
        application_type: i32,
        app_status: u32,
        ts_session_id: DWORD,
        b_restartable: BOOL,
    }

    extern "system" {
        fn RmStartSession(session: *mut u32, flags: DWORD, key: *const u16) -> u32;
        fn RmEndSession(session: u32) -> u32;
        fn RmRegisterResources(
            session: u32,
            nfiles: UINT,
            filenames: *const *const u16,
            napps: UINT,
            apps: *const RmUniqueProcess,
            nservices: UINT,
            services: *const u16,
        ) -> u32;
        fn RmGetList(
            session: u32,
            needed: *mut u32,
            count: *mut u32,
            infos: *mut RmProcessInfo,
            reboot_reasons: *mut u32,
        ) -> u32;

        fn OpenProcess(access: DWORD, inherit: BOOL, pid: DWORD) -> HANDLE;
        fn TerminateProcess(h: HANDLE, exit: u32) -> BOOL;
        fn CloseHandle(h: HANDLE) -> BOOL;
    }

    const ERROR_MORE_DATA: u32 = 234;
    const PROCESS_TERMINATE: DWORD = 0x0001;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn utf16_to_string(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    /// 枚举占用给定路径列表的进程。路径应为文件 (目录请先展开成文件列表)。
    pub fn find_locking_processes(paths: &[String]) -> Vec<LockingProcess> {
        let mut out = Vec::new();
        if paths.is_empty() {
            return out;
        }
        unsafe {
            let mut session: u32 = 0;
            // 会话 key 要求 GUID 形式的字符串
            let key = wide("9DF52FA2-1E8C-4A2B-9C1D-3E7F5A6B8C9D");
            if RmStartSession(&mut session, 0, key.as_ptr()) != 0 {
                return out;
            }
            let wpaths: Vec<Vec<u16>> = paths.iter().map(|p| wide(p)).collect();
            let ptrs: Vec<*const u16> = wpaths.iter().map(|p| p.as_ptr()).collect();
            let rc = RmRegisterResources(
                session,
                ptrs.len() as UINT,
                ptrs.as_ptr(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
            );
            if rc == 0 {
                let mut needed: u32 = 0;
                let mut count: u32 = 0;
                let mut reasons: u32 = 0;
                let rc = RmGetList(session, &mut needed, &mut count, std::ptr::null_mut(), &mut reasons);
                if rc == ERROR_MORE_DATA && needed > 0 {
                    let mut infos: Vec<RmProcessInfo> = vec![mem::zeroed(); needed as usize];
                    let mut count = needed;
                    if RmGetList(session, &mut needed, &mut count, infos.as_mut_ptr(), &mut reasons) == 0 {
                        for info in &infos[..count as usize] {
                            out.push(LockingProcess {
                                pid: info.process.dw_process_id,
                                name: utf16_to_string(&info.app_name),
                            });
                        }
                    }
                }
            }
            RmEndSession(session);
        }
        out
    }

    pub fn kill_process(pid: u32) -> bool {
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if h == 0 {
                return false;
            }
            let ok = TerminateProcess(h, 1) != 0;
            CloseHandle(h);
            ok
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::LockingProcess;
    pub fn find_locking_processes(_paths: &[String]) -> Vec<LockingProcess> {
        Vec::new()
    }
    pub fn kill_process(_pid: u32) -> bool {
        false
    }
}
