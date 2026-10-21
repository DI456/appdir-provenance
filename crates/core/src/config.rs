//! 共享 JSON 配置: appdir-core.exe 与 appdir-gui.exe 使用同一路径约定。
//!
//! 默认约定: 配置文件 `appdir.json` 与账本 `ledger.db` 都放在 exe 同目录。
//! core 启动时可被 `--config <路径>` 覆盖; GUI 始终读写 exe 同目录的 appdir.json。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const CONFIG_FILE_NAME: &str = "appdir.json";
pub const LEDGER_FILE_NAME: &str = "ledger.db";
pub const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub version: u32,
    pub ledger_path: PathBuf,
    /// 删除前是否备份到 backups\ 目录。旧配置文件缺少该字段时默认 true。
    #[serde(default = "default_true")]
    pub backup: bool,
    /// 注册表监视开关。只记录目录账本中已知 exe 的注册表写入。
    /// 修改后需重启 appdir-core.exe 生效。旧配置文件缺少该字段时默认 false。
    #[serde(default)]
    pub registry_monitor: bool,
    /// 注册表监视模式: "off" | "cache" | "instant"。
    /// 空值表示沿用旧字段 registry_monitor (true -> cache)。
    /// 修改后需重启 appdir-core.exe 生效。
    #[serde(default)]
    pub reg_mode: String,
    /// 注册表事件键路径前缀过滤 (小写, 如 "hklm\\software")。空 = 不过滤。
    /// 过滤在 ETW 回调后、入账本前完成。
    #[serde(default)]
    pub reg_prefixes: Vec<String>,
}

impl AppConfig {
    /// 有效注册表模式: "off" | "cache" | "instant"。
    pub fn reg_mode_effective(&self) -> &'static str {
        match self.reg_mode.as_str() {
            "cache" => "cache",
            "instant" => "instant",
            "off" => "off",
            _ if self.registry_monitor => "cache",
            _ => "off",
        }
    }

    /// 设置模式并同步旧字段, 保证旧版本 core 也能读到合理值。
    pub fn set_reg_mode(&mut self, mode: &str) {
        self.reg_mode = mode.to_string();
        self.registry_monitor = mode != "off";
    }
}

fn default_true() -> bool {
    true
}

/// exe 同目录下的配置文件路径。
pub fn config_path_for_exe(exe_path: &Path) -> PathBuf {
    exe_path.parent().unwrap_or_else(|| Path::new(".")).join(CONFIG_FILE_NAME)
}

/// 配置文件同目录下的默认账本路径。
pub fn default_ledger_path(config_path: &Path) -> PathBuf {
    config_path.parent().unwrap_or_else(|| Path::new(".")).join(LEDGER_FILE_NAME)
}

/// 加载配置; 文件不存在时创建默认配置并落盘。
pub fn load_or_create(config_path: &Path) -> crate::Result<AppConfig> {
    if config_path.exists() {
        let text = std::fs::read_to_string(config_path)
            .map_err(|e| crate::Error::Config(format!("读取配置 {} 失败: {e}", config_path.display())))?;
        let cfg: AppConfig = serde_json::from_str(&text)
            .map_err(|e| crate::Error::Config(format!("解析配置 {} 失败: {e}", config_path.display())))?;
        if cfg.version != CONFIG_VERSION {
            return Err(crate::Error::Config(format!(
                "配置版本不支持: {} (当前支持 {CONFIG_VERSION})",
                cfg.version
            )));
        }
        Ok(cfg)
    } else {
        let cfg = AppConfig {
            version: CONFIG_VERSION,
            ledger_path: default_ledger_path(config_path),
            backup: true,
            registry_monitor: false,
            reg_mode: "off".into(),
            reg_prefixes: Vec::new(),
        };
        save(config_path, &cfg)?;
        Ok(cfg)
    }
}

pub fn save(config_path: &Path, cfg: &AppConfig) -> crate::Result<()> {
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| crate::Error::Config(format!("创建配置目录失败: {e}")))?;
    }
    let text = serde_json::to_string_pretty(cfg)
        .map_err(|e| crate::Error::Config(format!("序列化配置失败: {e}")))?;
    std::fs::write(config_path, text)
        .map_err(|e| crate::Error::Config(format!("写入配置 {} 失败: {e}", config_path.display())))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(reg_mode: &str, registry_monitor: bool) -> AppConfig {
        AppConfig {
            version: CONFIG_VERSION,
            ledger_path: PathBuf::from("ledger.db"),
            backup: true,
            registry_monitor,
            reg_mode: reg_mode.into(),
            reg_prefixes: Vec::new(),
        }
    }

    #[test]
    fn reg_mode_three_states() {
        assert_eq!(cfg("off", true).reg_mode_effective(), "off");
        assert_eq!(cfg("cache", false).reg_mode_effective(), "cache");
        assert_eq!(cfg("instant", false).reg_mode_effective(), "instant");
    }

    #[test]
    fn legacy_monitor_fallback() {
        assert_eq!(cfg("", true).reg_mode_effective(), "cache");
        assert_eq!(cfg("", false).reg_mode_effective(), "off");
    }

    #[test]
    fn set_mode_syncs_legacy_field() {
        let mut c = cfg("", false);
        c.set_reg_mode("instant");
        assert_eq!(c.reg_mode_effective(), "instant");
        assert!(c.registry_monitor);
        c.set_reg_mode("off");
        assert!(!c.registry_monitor);
        assert_eq!(c.reg_mode_effective(), "off");
    }

    #[test]
    fn old_json_without_mode_deserializes() {
        let text = r#"{"version":1,"ledger_path":"ledger.db","backup":true,"registry_monitor":true}"#;
        let c: AppConfig = serde_json::from_str(text).unwrap();
        assert_eq!(c.reg_mode_effective(), "cache");
    }
}