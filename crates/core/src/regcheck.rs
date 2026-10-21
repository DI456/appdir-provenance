//! 检查注册表值当前是否仍存在。
//!
//! GUI 打开某 exe 的注册表明细时, 用它静默清理 "账本里还有、注册表里已没有"
//! 的失效条目 (只对 op="set" 有意义; delval 是删除动作的历史记录, 无存在性语义)。

use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE,
    HKEY_USERS, KEY_QUERY_VALUE, REG_VALUE_TYPE,
};

/// key_path 与捕获时归一化一致: "HKLM\\..." / "HKU\\..." / "HKCU\\...",
/// 也兼容原始 "\\REGISTRY\\MACHINE\\..." / "\\REGISTRY\\USER\\..."。
/// value 为空串表示默认值。
pub fn value_exists(key_path: &str, value: &str) -> bool {
    let Some((hive, sub)) = split_hive(key_path) else {
        return false;
    };
    let sub_w: Vec<u16> = sub.encode_utf16().chain(std::iter::once(0)).collect();
    let mut hkey = HKEY::default();
    let opened = unsafe {
        RegOpenKeyExW(hive, PCWSTR(sub_w.as_ptr()), 0, KEY_QUERY_VALUE, &mut hkey)
    };
    if opened != ERROR_SUCCESS || hkey.is_invalid() {
        return false;
    }
    let val_w: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    let mut ty = REG_VALUE_TYPE(0);
    let mut cb = 0u32;
    let q = unsafe {
        RegQueryValueExW(
            hkey,
            PCWSTR(val_w.as_ptr()),
            None,
            Some(&mut ty),
            None,
            Some(&mut cb),
        )
    };
    let _ = unsafe { RegCloseKey(hkey) };
    q == ERROR_SUCCESS
}

fn split_hive(key: &str) -> Option<(HKEY, String)> {
    let lower = key.to_ascii_lowercase();
    let (hive, rest) = if let Some(r) = lower.strip_prefix("hklm\\") {
        (HKEY_LOCAL_MACHINE, r)
    } else if let Some(r) = lower.strip_prefix("hku\\") {
        (HKEY_USERS, r)
    } else if let Some(r) = lower.strip_prefix("hkcu\\") {
        (HKEY_CURRENT_USER, r)
    } else if let Some(r) = lower.strip_prefix("\\registry\\machine\\") {
        (HKEY_LOCAL_MACHINE, r)
    } else if let Some(r) = lower.strip_prefix("\\registry\\user\\") {
        (HKEY_USERS, r)
    } else {
        return None;
    };
    Some((hive, rest.to_string()))
}
