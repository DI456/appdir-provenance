//! appdir-gui.exe - 按需打开的图形界面 (egui/eframe, 纯 Rust)。
//!
//! 职责:
//!   1. 管理监视目录列表 (账本 monitor_roots 表)
//!   2. 设置账本存放路径 (写 exe 同目录的 appdir.json, 与 appdir-core.exe 共享)
//!   3. 账本总览: 可按 EXE 或按监视根两种视图分类; 选中后勾选删除 (备份+两步删除)
//!
//! 仅调用 appdir-core 现有 API (Db / clean), 不重复实现业务逻辑。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use appdir_core::{
    clean, config, path, regcheck, timefmt, AppRow, CleanOptions, Candidate, Db, MonitorRoot,
    RecordStatus, RegAppSummary, RegRow,
};
use eframe::egui;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

mod symlink_tab;
mod unlocker;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([900.0, 620.0])
            .with_title("appdir 目录溯源"),
        ..Default::default()
    };
    eframe::run_native("appdir-gui", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

#[derive(PartialEq)]
enum Tab {
    Roots,
    Settings,
    Ledger,
    RegLedger,
    Symlink,
}

/// 账本总览的两种分类方式。
#[derive(PartialEq, Clone, Copy)]
enum LedgerView {
    /// 按创建目录的程序 (exe) 聚合。
    ByApp,
    /// 按监视根聚合, 组内列出该根下捕获到的目录。
    ByRoot,
    /// 不分组, 全部记录按创建时间倒序平铺。
    ByTime,
}

struct DirRowUi {
    candidate: Candidate,
    checked: bool,
    /// 原目录已不存在: 只能删除账本记录, 不能删除文件。
    gone: bool,
    /// 该条目不可做文件删除, 但允许只删账本记录 (失效 / 符号链接等)。
    ledger_only: bool,
}

impl DirRowUi {
    /// 勾选框是否可点。
    fn checkable(&self) -> bool {
        self.ledger_only || self.candidate.deletable()
    }
}

#[derive(Clone)]
struct AppSummary {
    pub app_id: i64,
    pub exe_path: String,
    pub exe_name: String,
    /// 仍存在的目录数。
    pub dir_count: usize,
    /// 原目录已不在、只剩账本记录的条目数。
    pub stale_count: usize,
    /// 最近一次创建目录的时间 (排序用, 新的在上)。
    pub last_ms: i64,
}

#[derive(Clone)]
struct RootSummary {
    pub root: String,
    pub dir_count: usize,
    pub stale_count: usize,
    pub exe_count: usize,
    /// 该根下最近一次创建目录的时间。
    pub last_ms: i64,
}

struct App {
    tab: Tab,
    config_path: PathBuf,
    config: appdir_core::config::AppConfig,
    db: Option<std::sync::Arc<Db>>,
    status: String,

    // roots tab
    new_root: String,
    roots: Vec<MonitorRoot>,

    // settings tab
    ledger_edit: String,
    startup_checked: bool,

    // ledger tab
    apps: Vec<AppSummary>,
    root_groups: Vec<RootSummary>,
    ledger_view: LedgerView,
    selected: Option<i64>,
    selected_root: Option<String>,
    // registry ledger tab
    reg_apps: Vec<RegAppSummary>,
    reg_selected: Option<i64>,
    reg_rows: Vec<RegRow>,
    results: Vec<DirRowUi>,
    last_refresh: std::time::Instant,
    symlink: symlink_tab::SymlinkState,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_cjk_fonts(&cc.egui_ctx);
        // 全局字号放大 10%, 行距放宽: 明细里多行小字 (创建者/时间/状态) 太紧凑。
        {
            let mut style = (*cc.egui_ctx.style()).clone();
            for ts in [
                egui::TextStyle::Small,
                egui::TextStyle::Body,
                egui::TextStyle::Button,
                egui::TextStyle::Heading,
                egui::TextStyle::Monospace,
            ] {
                if let Some(f) = style.text_styles.get_mut(&ts) {
                    f.size *= 1.10;
                }
            }
            style.spacing.item_spacing.y = 6.0;
            cc.egui_ctx.set_style(style);
        }
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
        let config_path = config::config_path_for_exe(&exe);
        let (config, db, status) = match config::load_or_create(&config_path) {
            Ok(cfg) => match open_db(&cfg.ledger_path) {
                Ok(db) => (cfg, Some(db), String::new()),
                Err(e) => (cfg, None, format!("账本打开失败: {e}")),
            },
            Err(e) => (
                appdir_core::config::AppConfig {
                    version: config::CONFIG_VERSION,
                    ledger_path: config::default_ledger_path(&config_path),
                    backup: true,
                    registry_monitor: false,
                    reg_mode: "off".into(),
                    reg_prefixes: Vec::new(),
                },
                None,
                format!("配置加载失败: {e}"),
            ),
        };
        let db = db.map(std::sync::Arc::new);
        let ledger_edit = config.ledger_path.display().to_string();
        let mut app = Self {
            tab: Tab::Ledger,
            config_path,
            config,
            db: db.clone(),
            status,
            new_root: String::new(),
            roots: Vec::new(),
            ledger_edit,
            startup_checked: startup::is_installed(),
            apps: Vec::new(),
            root_groups: Vec::new(),
            ledger_view: LedgerView::ByApp,
            selected: None,
            selected_root: None,
            results: Vec::new(),
            reg_apps: Vec::new(),
            reg_selected: None,
            reg_rows: Vec::new(),
            last_refresh: std::time::Instant::now(),
            symlink: symlink_tab::SymlinkState::new(db.clone()),
        };
        app.reload_roots();
        app.reload_apps();
        app.reload_reg_apps();
        app
    }

    fn reload_roots(&mut self) {
        self.roots.clear();
        if let Some(db) = &self.db {
            match db.roots() {
                Ok(rs) => self.roots = rs,
                Err(e) => self.status = format!("读取监视目录失败: {e}"),
            }
        }
    }

    fn reload_apps(&mut self) {
        self.apps.clear();
        self.root_groups.clear();
        self.selected = None;
        self.selected_root = None;
        self.results.clear();
        let Some(db) = &self.db else { return };
        // 按 app / 按监视根 同时聚合目录记录。仍存在的记为 dir_count,
        // 原目录已不在 (Missing) 的单独计为 stale_count: 它们只能删账本记录,
        // 但程序仍会出现在列表里, 否则这些孤儿记录就无处可清。
        // last_ms = 该组内最近一次创建目录的时间, 一级列表按它倒序 (新的在上)。
        let mut by_app: BTreeMap<i64, (usize, usize, i64)> = BTreeMap::new();
        // key = 小写路径 (Windows 大小写不敏感),
        // value = (显示路径, 存在数, 失效数, 程序集合, 最近创建时间)。
        let mut by_root: BTreeMap<
            String,
            (String, usize, usize, std::collections::BTreeSet<i64>, i64),
        > = BTreeMap::new();
        match db.all_dirs() {
            Ok(dirs) => {
                for d in dirs {
                    let stale = d.record_status == RecordStatus::Missing;
                    let entry = by_root
                        .entry(d.monitor_root.to_ascii_lowercase())
                        .or_insert_with(|| {
                            (
                                d.monitor_root.clone(),
                                0,
                                0,
                                std::collections::BTreeSet::new(),
                                0,
                            )
                        });
                    if stale {
                        entry.2 += 1;
                    } else {
                        entry.1 += 1;
                    }
                    entry.3.insert(d.app_id);
                    entry.4 = entry.4.max(d.created_at_ms);
                    let a = by_app.entry(d.app_id).or_insert((0, 0, 0));
                    if stale {
                        a.1 += 1;
                    } else {
                        a.0 += 1;
                    }
                    a.2 = a.2.max(d.created_at_ms);
                }
            }
            Err(e) => {
                self.status = format!("读取账本失败: {e}");
                return;
            }
        }
        for (app_id, (live, stale, last_ms)) in by_app {
            let app: Option<AppRow> = db.app_by_id(app_id).ok().flatten();
            let (exe_path, exe_name) = match &app {
                Some(a) => (a.exe_path.clone(), a.exe_name.clone()),
                None => (format!("#{}", app_id), format!("未知程序 #{}", app_id)),
            };
            self.apps.push(AppSummary {
                app_id,
                exe_path,
                exe_name,
                dir_count: live,
                stale_count: stale,
                last_ms,
            });
        }
        self.apps.sort_by(|a, b| {
            b.last_ms
                .cmp(&a.last_ms)
                .then(b.dir_count.cmp(&a.dir_count))
                .then(b.stale_count.cmp(&a.stale_count))
                .then_with(|| {
                    a.exe_name
                        .to_ascii_lowercase()
                        .cmp(&b.exe_name.to_ascii_lowercase())
                })
        });
        // 监视根分组: 以配置中的根为准 (0 条也显示), 再补上账本里残留的旧根。
        for r in &self.roots {
            let (root, dir_count, stale_count, apps, last_ms) = by_root
                .remove(&r.path.to_ascii_lowercase())
                .unwrap_or_else(|| {
                    (
                        r.path.clone(),
                        0,
                        0,
                        std::collections::BTreeSet::new(),
                        0,
                    )
                });
            self.root_groups.push(RootSummary {
                root,
                dir_count,
                stale_count,
                exe_count: apps.len(),
                last_ms,
            });
        }
        for (_, (root, dir_count, stale_count, apps, last_ms)) in by_root {
            self.root_groups.push(RootSummary {
                root,
                dir_count,
                stale_count,
                exe_count: apps.len(),
                last_ms,
            });
        }
        // 一级列表也按最近创建时间倒序 (新的在上)。
        self.root_groups.sort_by(|a, b| b.last_ms.cmp(&a.last_ms));
    }

    fn open_app(&mut self, app_id: i64, preserve_checks: bool) {
        self.selected = Some(app_id);
        self.selected_root = None;
        let Some(db) = &self.db else { return };
        let dirs: Vec<appdir_core::DirRow> = match db.all_dirs() {
            Ok(ds) => ds.into_iter().filter(|d| d.app_id == app_id).collect(),
            Err(e) => {
                self.status = format!("读取账本失败: {e}");
                return;
            }
        };
        self.load_rows(dirs, preserve_checks);
    }

    /// 按监视根打开明细: 该根下捕获到的全部目录, 每行标注创建它的 exe。
    fn open_root(&mut self, root: &str, preserve_checks: bool) {
        self.selected = None;
        self.selected_root = Some(root.to_string());
        let Some(db) = &self.db else { return };
        let dirs: Vec<appdir_core::DirRow> = match db.all_dirs() {
            Ok(ds) => ds
                .into_iter()
                .filter(|d| d.monitor_root.eq_ignore_ascii_case(root))
                .collect(),
            Err(e) => {
                self.status = format!("读取账本失败: {e}");
                return;
            }
        };
        self.load_rows(dirs, preserve_checks);
    }

    /// 按时间视图: 不分组, 账本里的全部记录 (含已删除/符号链接) 平铺, 新的在上。
    fn open_time_view(&mut self, preserve_checks: bool) {
        self.selected = None;
        self.selected_root = None;
        let Some(db) = &self.db else { return };
        let dirs = match db.all_dirs() {
            Ok(ds) => ds,
            Err(e) => {
                self.status = format!("读取账本失败: {e}");
                return;
            }
        };
        self.load_rows(dirs, preserve_checks);
    }

    /// 两个明细视图共用的行构建: verify + 失效判定 + 勾选状态。
    /// 两种视图必须走同一份逻辑, 否则“失效条目可勾选”之类的新规则容易只改一处。
    fn load_rows(&mut self, dirs: Vec<appdir_core::DirRow>, preserve_checks: bool) {
        // 总是重建列表 (否则自动刷新会追加重复行); 勾选状态按 directory_id 保留。
        let old_checks: std::collections::HashMap<i64, bool> = self
            .results
            .iter()
            .map(|r| (r.candidate.directory_id, r.checked))
            .collect();
        self.results.clear();
        let Some(db) = &self.db else { return };
        let protected: Vec<String> = vec![
            self.config.ledger_path.display().to_string(),
            self.config_path.display().to_string(),
        ];
        // app_id -> exe_path 缓存, 避免同一程序重复查 SQLite。
        let mut exe_cache: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
        for d in dirs {
            let exe_path = match exe_cache.get(&d.app_id) {
                Some(p) => p.clone(),
                None => {
                    let p = db
                        .app_by_id(d.app_id)
                        .ok()
                        .flatten()
                        .map(|a| a.exe_path)
                        .unwrap_or_default();
                    exe_cache.insert(d.app_id, p.clone());
                    p
                }
            };
            let mut cand = Candidate {
                directory_path: d.directory_path.clone(),
                monitor_root: d.monitor_root,
                directory_id: d.directory_id,
                app_exe_path: exe_path.clone(),
                created_at_ms: d.created_at_ms,
                record_status: d.record_status,
                identity: None,
                blocked: None,
            };
            if let Ok(v) = clean::verify(db, &d.directory_path, Some(&exe_path), &protected) {
                cand = v;
            }
            // 失效条目 (只能删账本记录, 不能删文件) 的两种情形:
            //   1. 磁盘上确实已经不存在了;
            //   2. 账本已标记 missing (即使路径又出现也不可信, 交给下次重新注册)。
            // 注意: verify 在“存在但打不开”(如权限不足) 时也报 Missing, 那不是失效。
            let on_disk = path_exists(&cand.directory_path);
            let gone = !on_disk || cand.record_status == RecordStatus::Missing;
            if !on_disk && cand.record_status != RecordStatus::Missing && cand.directory_id >= 0 {
                // 顺手把账本状态标成 Missing, 让总览的“已失效”计数对得上。
                // 仅当父目录仍存在时才标记: 网络盘/移动硬盘暂时离线时整棵都不存在,
                // 不应把记录永久标成失效。
                let parent_ok = Path::new(&cand.directory_path)
                    .parent()
                    .map(|p| p.is_dir())
                    .unwrap_or(false);
                if parent_ok {
                    let _ =
                        db.set_status(cand.directory_id, RecordStatus::Missing, timefmt::now_ms());
                    cand.record_status = RecordStatus::Missing;
                }
            }
            let checked = if preserve_checks {
                old_checks
                    .get(&cand.directory_id)
                    .copied()
                    .unwrap_or(false)
            } else {
                false
            };
            // 这两类条目永远不能做文件删除, 但账本记录可以单独清理:
            //   - 原目录已不存在;
            //   - 是符号链接/重解析点 (工具出于安全不删链接本身)。
            let reparse = matches!(cand.blocked, Some((clean::BlockReason::ReparsePoint, _)));
            let ledger_only = gone || reparse;
            self.results.push(DirRowUi {
                candidate: cand,
                checked,
                gone,
                ledger_only,
            });
        }
        // 全部明细视图统一按创建时间倒序 (新的在上)。
        self.results
            .sort_by(|a, b| b.candidate.created_at_ms.cmp(&a.candidate.created_at_ms));
    }

    fn back_to_list(&mut self) {
        self.selected = None;
        self.selected_root = None;
        self.results.clear();
        self.reload_apps();
    }

    fn reopen_db(&mut self) {
        self.db = None;
        self.roots.clear();
        self.results.clear();
        self.apps.clear();
        match open_db(&self.config.ledger_path) {
            Ok(db) => {
                self.db = Some(std::sync::Arc::new(db));
                self.symlink.set_db(self.db.as_ref().unwrap().clone());
                self.status = format!("账本已切换: {}", self.config.ledger_path.display());
                self.reload_roots();
                self.reload_apps();
            }
            Err(e) => self.status = format!("账本打开失败: {e}"),
        }
    }

    fn delete_checked(&mut self) {
        let Some(db) = &self.db else { return };
        let live: Vec<Candidate> = self
            .results
            .iter()
            .filter(|r| r.checked && !r.ledger_only && r.candidate.deletable())
            .map(|r| r.candidate.clone())
            .collect();
        let gone: Vec<i64> = self
            .results
            .iter()
            .filter(|r| r.checked && r.ledger_only && r.candidate.directory_id >= 0)
            .map(|r| r.candidate.directory_id)
            .collect();
        if live.is_empty() && gone.is_empty() {
            self.status = "没有可处理的选中条目".into();
            return;
        }
        let mut parts: Vec<String> = Vec::new();
        if !gone.is_empty() {
            match db.delete_dir_records(&gone) {
                Ok(n) => parts.push(format!("删除账本记录 {n} 条（未动任何文件）")),
                Err(e) => parts.push(format!("删除账本记录失败: {e}")),
            }
        }
        if !live.is_empty() {
            let backup_dir = self.config_path.parent().unwrap_or(Path::new(".")).join("backups");
            let backup_opt = self.config.backup.then_some(backup_dir.as_path());
            let opts = CleanOptions {
                backup_dir: backup_opt,
                commit: true,
                protected_prefixes: vec![
                    self.config.ledger_path.display().to_string(),
                    self.config_path.display().to_string(),
                ],
            };
            match clean::execute(db, live, &opts, timefmt::now_ms()) {
                Ok(report) => parts.push(format!(
                    "删除目录 {}  跳过 {}{}",
                    report.deleted,
                    report.blocked_count(),
                    if self.config.backup {
                        format!("  备份于 {}", backup_dir.display())
                    } else {
                        "  (备份已关闭)".to_string()
                    }
                )),
                Err(e) => parts.push(format!("删除失败: {e}")),
            }
        }
        self.status = parts.join("; ");
        // 先刷新总览聚合（会重建 apps / root_groups 列表），再重新打开当前明细：
        // 已处理的条目（目录已删 / 记录已删）不再显示在明细里。
        let current_app = self.selected;
        let current_root = self.selected_root.clone();
        self.reload_apps();
        if let Some(id) = current_app {
            self.open_app(id, false);
            // 该程序的记录已全部处理完: 明细已空, 直接回到总览。
            if self.results.is_empty() {
                self.back_to_list();
                self.status.push_str(" (该程序已无剩余记录)");
            }
        } else if let Some(root) = current_root {
            self.open_root(&root, false);
            if self.results.is_empty() {
                self.back_to_list();
                self.status.push_str(" (该监视目录已无剩余记录)");
            }
        } else if self.ledger_view == LedgerView::ByTime {
            self.open_time_view(false);
        }
    }
}

/// 路径在磁盘上是否还存在 (不看权限/可打开性, 只看目录项)。
/// verify 的 Missing 有两种来源: 真的不存在, 或存在但打不开;
/// 后者不应把账本条目当成“失效”。
fn path_exists(p: &str) -> bool {
    Path::new(p).symlink_metadata().is_ok()
}

fn open_db(p: &std::path::Path) -> appdir_core::Result<Db> {
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Db::open(p)
}

/// egui 默认字体不含 CJK 字形 (显示为方框); 加载 Windows 系统中文字体。
fn install_cjk_fonts(ctx: &egui::Context) {
    const CANDIDATES: &[&str] = &[
        r"C:\Windows\Fonts\msyh.ttc",   // 微软雅黑
        r"C:\Windows\Fonts\msyh.ttf",
        r"C:\Windows\Fonts\simhei.ttf", // 黑体
        r"C:\Windows\Fonts\simsun.ttc", // 宋体
    ];
    for file in CANDIDATES {
        if let Ok(bytes) = std::fs::read(file) {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "cjk".into(),
                egui::FontData::from_owned(bytes).into(),
            );
            if let Some(fam) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                fam.insert(0, "cjk".into());
            }
            if let Some(fam) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
                fam.push("cjk".into());
            }
            ctx.set_fonts(fonts);
            return;
        }
    }
    eprintln!("警告: 未找到系统中文字体, 中文将显示为方框");
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 实时刷新: 每 2 秒从账本拉取最新数据 (WAL 支持与后台 core 并发读写)。
        if self.last_refresh.elapsed() >= std::time::Duration::from_secs(2) {
            self.last_refresh = std::time::Instant::now();
            if let Some(id) = self.selected {
                self.open_app(id, true);
            } else if let Some(root) = self.selected_root.clone() {
                self.open_root(&root, true);
            } else if self.ledger_view == LedgerView::ByTime {
                self.open_time_view(true);
            } else {
                self.reload_apps();
            }
            self.reload_roots();
        }
        ctx.request_repaint_after(if self.tab == Tab::Symlink && self.symlink.is_busy() {
            std::time::Duration::from_millis(80)
        } else {
            std::time::Duration::from_secs(2)
        });
        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Ledger, "账本总览");
                ui.selectable_value(&mut self.tab, Tab::Roots, "监视目录");
                ui.selectable_value(&mut self.tab, Tab::Settings, "设置");
                ui.selectable_value(&mut self.tab, Tab::RegLedger, "注册表账本");
                ui.selectable_value(&mut self.tab, Tab::Symlink, "符号链接");
                if ui.button("刷新").clicked() {
                    self.reload_apps();
                    self.reload_roots();
                    self.reload_reg_apps();
                    self.status = "已刷新".into();
                }
            });
        });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.colored_label(egui::Color32::LIGHT_BLUE, &self.status);
            ui.add_space(4.0);
        });
        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Ledger => self.ui_ledger(ui),
            Tab::Roots => self.ui_roots(ui),
            Tab::Settings => self.ui_settings(ui),
            Tab::RegLedger => self.ui_reg_ledger(ui),
            Tab::Symlink => {
                self.symlink.poll();
                self.symlink.ui(ui);
            }
        });
    }
}

impl App {
    fn ui_ledger(&mut self, ui: &mut egui::Ui) {
        if let Some(app_id) = self.selected {
            self.ui_app_detail(ui, app_id);
        } else if let Some(root) = self.selected_root.clone() {
            self.ui_root_detail(ui, &root);
        } else {
            match self.ledger_view {
                LedgerView::ByApp => self.ui_app_list(ui),
                LedgerView::ByRoot => self.ui_root_list(ui),
                LedgerView::ByTime => self.ui_time_list(ui),
            }
        }
    }

    /// 按时间视图: 全部记录平铺, 无一级菜单。
    /// 注意: 视图切换开关必须先画, 否则切到「按时间」后顶部按钮就消失了。
    fn ui_time_list(&mut self, ui: &mut egui::Ui) {
        self.ui_view_switch(ui);
        ui.add_space(4.0);
        let title = format!(
            "全部目录记录 (按时间倒序, 共 {} 条{})",
            self.results.len(),
            self.stale_suffix()
        );
        self.ui_dir_detail(ui, title, "", true, false);
    }

    fn ui_app_list(&mut self, ui: &mut egui::Ui) {
        let total: usize = self.apps.iter().map(|a| a.dir_count).sum();
        let stale_total: usize = self.apps.iter().map(|a| a.stale_count).sum();
        ui.heading(format!(
            "账本总览: {} 个程序, 累计 {} 条目录记录{}",
            self.apps.len(),
            total,
            if stale_total > 0 {
                format!(" + {stale_total} 条已失效")
            } else {
                String::new()
            }
        ));
        self.ui_view_switch(ui);
        ui.label("按各程序最近一次捕获时间排列（新的在上）。点击某个程序查看它创建的目录并可删除。");
        ui.add_space(8.0);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
            for a in self.apps.clone() {
                ui.horizontal(|ui| {
                    if ui.button("查看").clicked() {
                        self.open_app(a.app_id, false);
                    }
                    ui.vertical(|ui| {
                        ui.label(format!(
                            "{}  ({} 个目录{})",
                            a.exe_name,
                            a.dir_count,
                            if a.stale_count > 0 {
                                format!(", {} 条已失效", a.stale_count)
                            } else {
                                String::new()
                            }
                        ));
                        ui.small(&a.exe_path);
                        if a.last_ms > 0 {
                            ui.small(format!("最近一次: {}", timefmt::format_local(a.last_ms)));
                        }
                    });
                });
                ui.separator();
            }
            if self.apps.is_empty() {
                ui.label("(账本为空; 等 appdir-core 采集到事件后点\"刷新\")");
            }
        });
    }

    /// 总览顶部的分类方式切换开关。切换时立即重建对应列表。
    fn ui_view_switch(&mut self, ui: &mut egui::Ui) {
        let mut want = self.ledger_view;
        ui.horizontal(|ui| {
            ui.label("分类方式:");
            ui.selectable_value(&mut want, LedgerView::ByApp, "按程序 (exe)");
            ui.selectable_value(&mut want, LedgerView::ByRoot, "按目录 (监视根)");
            ui.selectable_value(&mut want, LedgerView::ByTime, "按时间");
        });
        if want != self.ledger_view {
            self.ledger_view = want;
            self.selected = None;
            self.selected_root = None;
            self.results.clear();
            // 时间视图不经过一级菜单, 切过来时直接建好列表; 另两种视图下一帧用聚合数据。
            if want == LedgerView::ByTime {
                self.open_time_view(false);
            }
        }
    }

    /// 按监视根分类的总览列表。
    fn ui_root_list(&mut self, ui: &mut egui::Ui) {
        let total: usize = self.root_groups.iter().map(|g| g.dir_count).sum();
        let stale_total: usize = self.root_groups.iter().map(|g| g.stale_count).sum();
        ui.heading(format!(
            "账本总览: {} 个监视目录, 累计 {} 条目录记录{}",
            self.root_groups.len(),
            total,
            if stale_total > 0 {
                format!(" + {stale_total} 条已失效")
            } else {
                String::new()
            }
        ));
        self.ui_view_switch(ui);
        ui.label(
            "按监视目录下最近一次捕获时间排列（新的在上）。点击某个监视目录查看其下捕获到的目录（含创建它的 exe），可勾选删除。",
        );
        ui.add_space(8.0);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
            for g in self.root_groups.clone() {
                let rows = g.dir_count + g.stale_count;
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(rows > 0, egui::Button::new("查看"))
                        .clicked()
                    {
                        self.open_root(&g.root, false);
                    }
                    ui.vertical(|ui| {
                        ui.label(format!(
                            "{}  ({} 个目录{}, {} 个程序)",
                            g.root,
                            g.dir_count,
                            if g.stale_count > 0 {
                                format!(", {} 条已失效", g.stale_count)
                            } else {
                                String::new()
                            },
                            g.exe_count
                        ));
                        if g.last_ms > 0 {
                            ui.small(format!(
                                "最近一次: {}",
                                timefmt::format_local(g.last_ms)
                            ));
                        }
                    });
                });
                ui.separator();
            }
            if self.root_groups.is_empty() {
                ui.label("(尚未配置监视根目录; 到\"监视目录\"页添加)");
            }
        });
    }

    fn ui_app_detail(&mut self, ui: &mut egui::Ui, app_id: i64) {
        let exe = self
            .apps
            .iter()
            .find(|a| a.app_id == app_id)
            .map(|a| a.exe_path.clone())
            .unwrap_or_default();
        let title = format!(
            "该程序创建的目录 (共 {} 条{})",
            self.results.len(),
            self.stale_suffix()
        );
        self.ui_dir_detail(ui, title, &exe, false, true);
    }

    fn ui_root_detail(&mut self, ui: &mut egui::Ui, root: &str) {
        let title = format!(
            "该监视目录下的目录 (共 {} 条{})",
            self.results.len(),
            self.stale_suffix()
        );
        self.ui_dir_detail(ui, title, root, true, true);
    }

    /// 明细标题里的“其中 N 条原目录已不在”尾巴。
    fn stale_suffix(&self) -> String {
        let gone = self.results.iter().filter(|r| r.gone).count();
        let links = self.results.iter().filter(|r| !r.gone && r.ledger_only).count();
        let mut parts: Vec<String> = Vec::new();
        if gone > 0 {
            parts.push(format!("{gone} 条原目录已不在"));
        }
        if links > 0 {
            parts.push(format!("{links} 条为符号链接"));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!(", 其中 {}", parts.join("、"))
        }
    }

    /// 明细列表的共用界面; show_exe 为真时每行额外显示创建它的程序,
    /// show_back 为假时不显示「返回总览」按钮 (按时间视图没有上级)。
    fn ui_dir_detail(
        &mut self,
        ui: &mut egui::Ui,
        title: String,
        subtitle: &str,
        show_exe: bool,
        show_back: bool,
    ) {
        if show_back {
            ui.horizontal(|ui| {
                if ui.button("← 返回总览").clicked() {
                    self.back_to_list();
                }
            });
        }
        ui.heading(title);
        if !subtitle.is_empty() {
            ui.monospace(subtitle);
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button("全选").clicked() {
                // 全选包含失效/符号链接条目: 它们同样可以「只删除账本记录」。
                for r in &mut self.results {
                    r.checked = r.checkable();
                }
            }
            if ui.button("全不选").clicked() {
                for r in &mut self.results {
                    r.checked = false;
                }
            }
            let live = self
                .results
                .iter()
                .filter(|r| r.checked && !r.ledger_only && r.candidate.deletable())
                .count();
            let record: Vec<i64> = self
                .results
                .iter()
                .filter(|r| r.checked && r.ledger_only && r.candidate.directory_id >= 0)
                .map(|r| r.candidate.directory_id)
                .collect();
            let all_gone = self
                .results
                .iter()
                .filter(|r| r.checked && r.ledger_only)
                .all(|r| r.gone);
            let mut label = match (live, record.len()) {
                (0, 0) => "删除选中 (0)".to_string(),
                (0, g) if all_gone => format!("原目录已不在，只删除账本中的记录 ({g})"),
                (0, g) => format!("只删除账本中的记录 ({g})"),
                (l, 0) => format!("删除选中 ({l})"),
                (l, g) => format!("删除选中 (目录 {l} + 账本记录 {g})"),
            };
            if self.results.iter().any(|r| r.checked && r.ledger_only && r.candidate.directory_id < 0) {
                label.push_str("（另有记录行无法定位，已忽略）");
            }
            if ui.button(label).clicked() && (live > 0 || !record.is_empty()) {
                self.delete_checked();
            }
        });
        ui.add_space(6.0);
        // auto_shrink([false, false]): 让滚动区始终铺满窗口宽度。
        // 默认的 shrink-to-fit 会让滚动区宽度跟着内容走, 条目少或文字短时
        // 滚动条就贴在内容右侧而不是窗口最右边。
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for row in &mut self.results {
                    ui.horizontal(|ui| {
                        if row.checkable() {
                            ui.checkbox(&mut row.checked, "");
                        } else {
                            // 目录还在但不可删 (占用/身份不符/不在监视根等):
                            // 禁用勾选框, 原因写在行内和悬停提示里。
                            let mut off = false;
                            ui.add_enabled(false, egui::Checkbox::without_text(&mut off))
                                .on_disabled_hover_text(row.candidate.block_text());
                        }
                        ui.vertical(|ui| {
                            if row.gone {
                                ui.label(egui::RichText::new(&row.candidate.directory_path).weak());
                            } else {
                                ui.label(&row.candidate.directory_path);
                            }
                            if show_exe {
                                ui.small(format!("创建者: {}", row.candidate.app_exe_path));
                            }
                            let status_text = if row.gone {
                                "原目录已不在（仅剩账本记录）".to_string()
                            } else if row.ledger_only {
                                format!(
                                    "状态: {}  符号链接，只能清理账本记录",
                                    row.candidate.record_status.as_str()
                                )
                            } else if row.candidate.deletable() {
                                format!("状态: {}  可删除", row.candidate.record_status.as_str())
                            } else {
                                format!(
                                    "状态: {}  不可删除: {}",
                                    row.candidate.record_status.as_str(),
                                    row.candidate.block_text()
                                )
                            };
                            ui.small(format!(
                                "创建于 {}  {}",
                                timefmt::format_local(row.candidate.created_at_ms),
                                status_text
                            ));
                            if row.gone {
                                ui.small("勾选后只能删除账本中的记录，不会影响任何文件");
                            } else if row.ledger_only {
                                ui.small("勾选后只清掉账本记录；链接本身保留不动");
                            }
                        });
                    });
                    ui.separator();
                }
                if self.results.is_empty() {
                    ui.label("(无记录)");
                }
            });
    }

    fn ui_roots(&mut self, ui: &mut egui::Ui) {
        ui.heading("监视根目录");
        ui.label("后台 appdir-core.exe 只采集这些根目录下的子目录创建事件。");
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.new_root)
                    .hint_text("例如 D:\\toss pc")
                    .desired_width(480.0),
            );
            if ui.button("添加").clicked()
                || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
            {
                if let Some(db) = &self.db {
                    match path::normalize(self.new_root.trim()) {
                        Some(abs) if std::fs::metadata(&abs).map(|m| m.is_dir()).unwrap_or(false) => {
                            match db.add_root(&abs, timefmt::now_ms()) {
                                Ok((row, added)) => {
                                    self.status = if added {
                                        format!("已添加: {}", row.path)
                                    } else {
                                        format!("已存在: {}", row.path)
                                    };
                                    self.new_root.clear();
                                    self.reload_roots();
                                }
                                Err(e) => self.status = format!("添加失败: {e}"),
                            }
                        }
                        Some(abs) => self.status = format!("不是可用目录: {abs}"),
                        None => self.status = "请输入存在的绝对目录路径".into(),
                    }
                }
            }
        });
        ui.add_space(8.0);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
            let mut changed = false;
            for r in self.roots.clone() {
                ui.horizontal(|ui| {
                    let mut enabled = r.enabled;
                    if ui.checkbox(&mut enabled, &r.path).changed() {
                        if let Some(db) = &self.db {
                            let res = db.set_root_enabled(&r.path, enabled);
                            match res {
                                Ok(_) => changed = true,
                                Err(e) => self.status = format!("切换失败: {e}"),
                            }
                        }
                    }
                    if ui.button("移除").clicked() {
                        if let Some(db) = &self.db {
                            match db.remove_root(&r.path) {
                                Ok(_) => {
                                    changed = true;
                                    self.status = format!("已移除: {}", r.path);
                                }
                                Err(e) => self.status = format!("移除失败: {e}"),
                            }
                        }
                    }
                });
            }
            if changed {
                self.reload_roots();
            }
            if self.roots.is_empty() {
                ui.label("(尚未配置监视根目录)");
            }
        });
    }

    fn ui_settings(&mut self, ui: &mut egui::Ui) {
        ui.heading("设置");
        ui.label(format!("配置文件: {}", self.config_path.display()));
        ui.add_space(8.0);
        ui.label("账本 SQLite 路径 (appdir-core.exe 与本 GUI 共享同一配置):");
        ui.horizontal(|ui| {
            let resp = ui.add(egui::TextEdit::singleline(&mut self.ledger_edit).desired_width(600.0));
            if ui.button("保存并重新打开账本").clicked()
                || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
            {
                let p = PathBuf::from(self.ledger_edit.trim());
                if p.as_os_str().is_empty() {
                    self.status = "路径为空".into();
                } else {
                    self.config.ledger_path = p;
                    match config::save(&self.config_path, &self.config) {
                        Ok(()) => {
                            self.reopen_db();
                            self.status
                                .push_str(" (后台 appdir-core.exe 需重启后使用新路径)");
                        }
                        Err(e) => self.status = format!("保存配置失败: {e}"),
                    }
                }
            }
        });
        ui.add_space(12.0);
        if ui
            .checkbox(&mut self.config.backup, "删除前备份到 backups\\ 目录（默认开启）")
            .changed()
        {
            match config::save(&self.config_path, &self.config) {
                Ok(()) => {
                    self.status = if self.config.backup {
                        "已开启删除前备份".into()
                    } else {
                        "已关闭删除前备份（删除不再留备份）".into()
                    };
                }
                Err(e) => self.status = format!("保存配置失败: {e}"),
            }
        }
        ui.add_space(12.0);
        ui.label("注册表监视 (只记录目录账本中已知 exe 的注册表写入; 修改后需重启 appdir-core.exe 生效):");
        let cur = self.config.reg_mode_effective().to_string();
        let mut mode = cur.clone();
        ui.horizontal(|ui| {
            ui.selectable_value(&mut mode, "off".to_string(), "关");
            ui.selectable_value(&mut mode, "cache".to_string(), "cache (5 秒缓存)");
            ui.selectable_value(&mut mode, "instant".to_string(), "instant (高性能)");
        });
        ui.small(match mode.as_str() {
            "cache" => "cache: 事件缓存 5 秒后判定, 可覆盖“注册表先写、目录后建”的安装器。",
            "instant" => "instant: 每条事件立即判定, 未命中目录账本直接丢弃; 内存几乎为零, 但不覆盖目标 exe 尚未建目录的场景。",
            _ => "关: 不启动注册表 ETW 会话 (默认)。",
        });
        if mode != cur {
            self.config.set_reg_mode(&mode);
            match config::save(&self.config_path, &self.config) {
                Ok(()) => {
                    self.status = match mode.as_str() {
                        "cache" => "已开启 cache 模式 (重启 appdir-core.exe 后生效)".into(),
                        "instant" => "已开启 instant 模式 (重启 appdir-core.exe 后生效)".into(),
                        _ => "已关闭注册表监视 (重启 appdir-core.exe 后生效)".into(),
                    };
                }
                Err(e) => self.status = format!("保存配置失败: {e}"),
            }
        }
        ui.add_space(12.0);
        ui.label("开机自启 appdir-core.exe (登录后自动启动后台采集):");
        let mut want = self.startup_checked;
        if ui.checkbox(&mut want, "开机自启").changed() {
            match startup::set_enabled(want) {
                Ok(msg) => {
                    self.startup_checked = startup::is_installed();
                    self.status = msg;
                }
                Err(e) => self.status = format!("设置开机自启失败: {e}"),
            }
        }
        if !self.startup_checked {
            ui.label("提示: ETW 采集需要管理员权限; 若开关以注册表方式启用, ETW 可能拿不到管理员令牌。建议将两个 exe 放同一目录。");
        }
    }
}

/// 开机自启: 优先用计划任务 AppDirCore (schtasks, /rl highest), 失败则回退到
/// HKCU Run 注册表项。开关状态 is_installed() 每次真实查询系统。
mod startup {
    use std::process::Command;

    const TASK_NAME: &str = "AppDirCore";
    const RUN_KEY: &str = "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run";
    const RUN_VALUE: &str = "AppDirCore";

    fn core_exe() -> Option<String> {
        let exe = std::env::current_exe().ok()?;
        let core = exe.parent()?.join("appdir-core.exe");
        if core.is_file() {
            Some(core.display().to_string())
        } else {
            None
        }
    }

    fn command_output(cmd: &mut Command) -> std::io::Result<std::process::Output> {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        cmd.output()
    }

    /// 当前是否已存在任一自启方案。
    pub fn is_installed() -> bool {
        task_exists().unwrap_or(false) || reg_value_exists().unwrap_or(false)
    }

    fn task_exists() -> std::io::Result<bool> {
        let out = command_output(
            Command::new("schtasks").args(["/query", "/tn", TASK_NAME]),
        )?;
        Ok(out.status.success())
    }

    fn reg_value_exists() -> std::io::Result<bool> {
        let out = command_output(
            Command::new("reg").args(["query", RUN_KEY, "/v", RUN_VALUE]),
        )?;
        Ok(out.status.success())
    }

    /// 开/关自启。返回给 UI 的提示文本。
    pub fn set_enabled(enable: bool) -> Result<String, String> {
        if enable {
            let Some(target) = core_exe() else {
                return Err("未找到同目录的 appdir-core.exe, 无法设置自启".into());
            };
            // 方案 A: 计划任务 (最高权限, ETW 需要)。非管理员会失败 -> 回退注册表。
            let out = command_output(Command::new("schtasks").args([
                "/create", "/tn", TASK_NAME, "/sc", "onlogon", "/rl", "highest", "/f",
                "/tr", &format!("\\\"{target}\\\""),
            ]))
            .map_err(|e| e.to_string())?;
            if out.status.success() {
                return Ok("已创建计划任务 AppDirCore (开机自启, 最高权限)".into());
            }
            let out = command_output(Command::new("reg").args([
                "add", RUN_KEY, "/v", RUN_VALUE, "/t", "REG_SZ", "/d", &target, "/f",
            ]))
            .map_err(|e| e.to_string())?;
            if out.status.success() {
                Ok("已写入 HKCU Run 注册表项 (注意: 此方式启动可能无管理员权限, ETW 不可用)".into())
            } else {
                Err(format!(
                    "schtasks 与 reg add 均失败; schtasks: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ))
            }
        } else {
            let mut msgs: Vec<String> = Vec::new();
            if let Ok(out) = command_output(
                Command::new("schtasks").args(["/delete", "/tn", TASK_NAME, "/f"]),
            ) {
                if out.status.success() {
                    msgs.push("已删除计划任务 AppDirCore".into());
                }
            }
            if let Ok(out) = command_output(
                Command::new("reg").args(["delete", RUN_KEY, "/v", RUN_VALUE, "/f"]),
            ) {
                if out.status.success() {
                    msgs.push("已删除 HKCU Run 注册表项".into());
                }
            }
            if msgs.is_empty() {
                Ok("未发现已安装的自启项".into())
            } else {
                Ok(msgs.join(", "))
            }
        }
    }
}

// ---- 注册表账本 ----

impl App {
    fn ui_reg_ledger(&mut self, ui: &mut egui::Ui) {
        if let Some(app_id) = self.reg_selected {
            self.ui_reg_detail(ui, app_id);
        } else {
            self.ui_reg_list(ui);
        }
    }

    fn reload_reg_apps(&mut self) {
        self.reg_apps.clear();
        self.reg_selected = None;
        self.reg_rows.clear();
        let Some(db) = &self.db else { return };
        match db.reg_apps() {
            Ok(a) => self.reg_apps = a,
            Err(e) => self.status = format!("读取注册表账本失败: {e}"),
        }
    }

    /// 打开某 exe 的注册表明细; 顺带静默清理注册表中已不存在的值条目
    /// (op=set 且当前查不到该键值 -> 视为失效, 直接从账本删除)。
    fn open_reg_detail(&mut self, app_id: i64) {
        self.reg_selected = Some(app_id);
        self.reload_reg_rows(app_id);
    }

    fn reload_reg_rows(&mut self, app_id: i64) {
        self.reg_rows.clear();
        let Some(db) = &self.db else { return };
        let rows = match db.reg_events_of_app(app_id) {
            Ok(r) => r,
            Err(e) => {
                self.status = format!("读取注册表记录失败: {e}");
                return;
            }
        };
        let stale: Vec<i64> = rows
            .iter()
            .filter(|r| r.op == "set" && !regcheck::value_exists(&r.key_path, &r.value_name))
            .map(|r| r.reg_id)
            .collect();
        if !stale.is_empty() {
            match db.reg_delete_ids(&stale) {
                Ok(n) if n > 0 => self.status = format!("已静默清理 {n} 条注册表中已不存在的条目"),
                Ok(_) => {}
                Err(e) => self.status = format!("清理失效条目失败: {e}"),
            }
        }
        self.reg_rows = rows.into_iter().filter(|r| !stale.contains(&r.reg_id)).collect();
    }

    fn ui_reg_list(&mut self, ui: &mut egui::Ui) {
        let total: i64 = self.reg_apps.iter().map(|a| a.event_count).sum();
        ui.heading(format!(
            "注册表账本: {} 个程序, 共 {total} 条记录",
            self.reg_apps.len()
        ));
        ui.label("只收录目录账本中已知 exe 的注册表写入 (需在设置中开启注册表监视并重启 core)。点击查看可静默清理已失效条目。");
        ui.add_space(8.0);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
            for a in self.reg_apps.clone() {
                ui.horizontal(|ui| {
                    if ui.button("查看").clicked() {
                        self.open_reg_detail(a.app_id);
                    }
                    ui.label(format!("{}  ({} 条)", a.exe_name, a.event_count));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.small(&a.exe_path);
                    });
                });
                ui.separator();
            }
        });
    }

    fn ui_reg_detail(&mut self, ui: &mut egui::Ui, app_id: i64) {
        if ui.button("← 返回").clicked() {
            self.reg_selected = None;
            return;
        }
        ui.add_space(4.0);
        if let Some(a) = self.reg_apps.iter().find(|a| a.app_id == app_id) {
            ui.heading(format!("{} 的注册表记录 ({} 条)", a.exe_name, a.event_count));
            ui.small(&a.exe_path);
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button("刷新").clicked() {
                self.open_reg_detail(app_id);
            }
            if ui.button("清空该程序记录").clicked() {
                let res = self.db.as_ref().map(|db| db.reg_delete_app(app_id));
                match res {
                    Some(Ok(n)) => self.status = format!("已清空 {n} 条记录"),
                    Some(Err(e)) => self.status = format!("清空失败: {e}"),
                    None => {}
                }
                self.reload_reg_apps();
                return;
            }
        });
        ui.add_space(4.0);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
            for r in self.reg_rows.clone() {
                ui.horizontal(|ui| {
                    ui.monospace(format!(
                        "[{}] {}{}",
                        if r.op == "set" { "写" } else { "删值" },
                        r.key_path,
                        if r.value_name.is_empty() { String::new() } else { format!("\\{}", r.value_name) },
                    ));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small("删除").clicked() {
                            let res = self.db.as_ref().map(|db| db.reg_delete_ids(&[r.reg_id]));
                            match res {
                                Some(Ok(n)) if n > 0 => {
                                    self.reg_rows.retain(|x| x.reg_id != r.reg_id);
                                    let _ = n;
                                }
                                Some(Ok(_)) => {}
                                Some(Err(e)) => self.status = format!("删除失败: {e}"),
                                None => {}
                            }
                        }
                        ui.small(timefmt::format_local(r.event_at_ms));
                    });
                });
            }
        });
    }
}