//! appdir-gui.exe - 按需打开的图形界面 (egui/eframe, 纯 Rust)。
#![windows_subsystem = "windows"]
//!
//! 职责:
//!   1. 管理监视目录列表 (账本 monitor_roots 表)
//!   2. 设置账本存放路径 (写 exe 同目录的 appdir.json, 与 appdir-core.exe 共享)
//!   3. 账本总览: 列出每个 EXE 及其创建的目录数; 选中后勾选删除 (备份+两步删除)
//!
//! 仅调用 appdir-core 现有 API (Db / clean), 不重复实现业务逻辑。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use appdir_core::{
    clean, config, path, timefmt, AppRow, CleanOptions, Candidate, Db, MonitorRoot, RecordStatus,
};
use eframe::egui;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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
}

struct DirRowUi {
    candidate: Candidate,
    checked: bool,
}

#[derive(Clone)]
struct AppSummary {
    pub app_id: i64,
    pub exe_path: String,
    pub exe_name: String,
    pub dir_count: usize,
}

struct App {
    tab: Tab,
    config_path: PathBuf,
    config: appdir_core::config::AppConfig,
    db: Option<Db>,
    status: String,

    // roots tab
    new_root: String,
    roots: Vec<MonitorRoot>,

    // settings tab
    ledger_edit: String,

    // ledger tab
    apps: Vec<AppSummary>,
    selected: Option<i64>,
    results: Vec<DirRowUi>,
    last_refresh: std::time::Instant,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_cjk_fonts(&cc.egui_ctx);
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
                },
                None,
                format!("配置加载失败: {e}"),
            ),
        };
        let ledger_edit = config.ledger_path.display().to_string();
        let mut app = Self {
            tab: Tab::Ledger,
            config_path,
            config,
            db,
            status,
            new_root: String::new(),
            roots: Vec::new(),
            ledger_edit,
            apps: Vec::new(),
            selected: None,
            results: Vec::new(),
            last_refresh: std::time::Instant::now(),
        };
        app.reload_roots();
        app.reload_apps();
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
        self.selected = None;
        self.results.clear();
        let Some(db) = &self.db else { return };
        // 按 app 聚合目录记录; 已删除 (Missing) 的不算进计数,
        // 与明细页一致: 总览数字 = 当前仍存在的目录数。
        let mut by_app: BTreeMap<i64, Vec<appdir_core::DirRow>> = BTreeMap::new();
        match db.all_dirs() {
            Ok(dirs) => {
                for d in dirs {
                    if d.record_status == RecordStatus::Missing {
                        continue;
                    }
                    by_app.entry(d.app_id).or_default().push(d);
                }
            }
            Err(e) => {
                self.status = format!("读取账本失败: {e}");
                return;
            }
        }
        for (app_id, dirs) in by_app {
            let app: Option<AppRow> = db.app_by_id(app_id).ok().flatten();
            let (exe_path, exe_name) = match &app {
                Some(a) => (a.exe_path.clone(), a.exe_name.clone()),
                None => (format!("#{}", app_id), format!("未知程序 #{}", app_id)),
            };
            self.apps.push(AppSummary {
                app_id,
                exe_path,
                exe_name,
                dir_count: dirs.len(),
            });
        }
        self.apps.sort_by(|a, b| b.dir_count.cmp(&a.dir_count));
    }

    fn open_app(&mut self, app_id: i64, preserve_checks: bool) {
        self.selected = Some(app_id);
        // 总是重建列表 (否则自动刷新会追加重复行); 勾选状态按 directory_id 保留。
        let old_checks: std::collections::HashMap<i64, bool> = self
            .results
            .iter()
            .map(|r| (r.candidate.directory_id, r.checked))
            .collect();
        self.results.clear();
        let Some(db) = &self.db else { return };
        let dirs: Vec<appdir_core::DirRow> = match db.all_dirs() {
            Ok(ds) => ds.into_iter().filter(|d| d.app_id == app_id).collect(),
            Err(e) => {
                self.status = format!("读取账本失败: {e}");
                return;
            }
        };
        let exe_path = self
            .apps
            .iter()
            .find(|a| a.app_id == app_id)
            .map(|a| a.exe_path.clone())
            .unwrap_or_default();
        let protected: Vec<String> = vec![
            self.config.ledger_path.display().to_string(),
            self.config_path.display().to_string(),
        ];
        for d in dirs {
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
            // 已删除的目录不再展示（账本状态 Missing，或 verify 确认目录不存在）。
            let gone = cand.record_status == RecordStatus::Missing
                || matches!(cand.blocked, Some((clean::BlockReason::Missing, _)));
            if gone {
                continue;
            }
            let checked = if preserve_checks {
                old_checks.get(&cand.directory_id).copied().unwrap_or_else(|| cand.deletable())
            } else {
                cand.deletable()
            };
            self.results.push(DirRowUi { candidate: cand, checked });
        }
    }

    fn back_to_list(&mut self) {
        self.selected = None;
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
                self.db = Some(db);
                self.status = format!("账本已切换: {}", self.config.ledger_path.display());
                self.reload_roots();
                self.reload_apps();
            }
            Err(e) => self.status = format!("账本打开失败: {e}"),
        }
    }

    fn delete_checked(&mut self) {
        let Some(db) = &self.db else { return };
        let selected: Vec<Candidate> = self
            .results
            .iter()
            .filter(|r| r.checked && r.candidate.deletable())
            .map(|r| r.candidate.clone())
            .collect();
        if selected.is_empty() {
            self.status = "没有可删除的选中目录".into();
            return;
        }
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
        match clean::execute(db, selected, &opts, timefmt::now_ms()) {
            Ok(report) => {
                self.status = format!(
                    "删除完成: 删除 {}  跳过 {}{}",
                    report.deletable_count(),
                    report.blocked_count(),
                    if self.config.backup {
                        format!("  备份于 {}", backup_dir.display())
                    } else {
                        "  (备份已关闭)".to_string()
                    }
                );
            }
            Err(e) => self.status = format!("删除失败: {e}"),
        }
        // 先刷新总览聚合（会重建 apps 列表），再重新打开当前程序详情：
        // 已删除的目录（账本状态 Missing）不再显示在明细里。
        let current = self.selected;
        self.reload_apps();
        if let Some(id) = current {
            self.open_app(id, false);
        }
    }
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
            match self.selected {
                Some(id) => self.open_app(id, true),
                None => self.reload_apps(),
            }
            self.reload_roots();
        }
        ctx.request_repaint_after(std::time::Duration::from_secs(2));
        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Ledger, "账本总览");
                ui.selectable_value(&mut self.tab, Tab::Roots, "监视目录");
                ui.selectable_value(&mut self.tab, Tab::Settings, "设置");
                if ui.button("刷新").clicked() {
                    self.reload_apps();
                    self.reload_roots();
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
        });
    }
}

impl App {
    fn ui_ledger(&mut self, ui: &mut egui::Ui) {
        if let Some(app_id) = self.selected {
            self.ui_app_detail(ui, app_id);
        } else {
            self.ui_app_list(ui);
        }
    }

    fn ui_app_list(&mut self, ui: &mut egui::Ui) {
        let total: usize = self.apps.iter().map(|a| a.dir_count).sum();
        ui.heading(format!(
            "账本总览: {} 个程序, 累计 {} 条目录记录",
            self.apps.len(),
            total
        ));
        ui.label("点击某个程序查看它创建的目录并可删除。");
        ui.add_space(8.0);
        egui::ScrollArea::vertical().show(ui, |ui| {
            for a in self.apps.clone() {
                ui.horizontal(|ui| {
                    if ui.button("查看").clicked() {
                        self.open_app(a.app_id, false);
                    }
                    ui.vertical(|ui| {
                        ui.label(format!("{}  ({} 个目录)", a.exe_name, a.dir_count));
                        ui.small(&a.exe_path);
                    });
                });
                ui.separator();
            }
            if self.apps.is_empty() {
                ui.label("(账本为空; 等 appdir-core 采集到事件后点\"刷新\")");
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
        ui.horizontal(|ui| {
            if ui.button("← 返回总览").clicked() {
                self.back_to_list();
            }
        });
        ui.heading(format!("该程序创建的目录 (共 {} 条)", self.results.len()));
        ui.monospace(&exe);
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button("全选").clicked() {
                for r in &mut self.results {
                    r.checked = r.candidate.deletable();
                }
            }
            if ui.button("全不选").clicked() {
                for r in &mut self.results {
                    r.checked = false;
                }
            }
            let n = self.results.iter().filter(|r| r.checked).count();
            if ui.button(format!("删除选中 ({n})")).clicked() {
                self.delete_checked();
            }
        });
        ui.add_space(6.0);
        egui::ScrollArea::vertical().show(ui, |ui| {
            for row in &mut self.results {
                ui.horizontal(|ui| {
                    if !row.candidate.deletable() {
                        ui.label("⛔");
                    } else {
                        ui.checkbox(&mut row.checked, "");
                    }
                    ui.vertical(|ui| {
                        ui.label(&row.candidate.directory_path);
                        ui.small(format!(
                            "创建于 {}  状态: {}  {}",
                            timefmt::format_local(row.candidate.created_at_ms),
                            row.candidate.record_status.as_str(),
                            row.candidate.block_text()
                        ));
                    });
                });
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
        egui::ScrollArea::vertical().show(ui, |ui| {
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
        ui.label("开机自启 (方案, 未自动安装):");
        ui.monospace("schtasks /create /tn AppDirCore /sc onlogon /rl highest /tr \"<appdir-core.exe 完整路径>\"");
        ui.label("建议把 appdir-core.exe 与 appdir-gui.exe 放在同一目录; appdir-core.exe 需管理员权限运行 (ETW)。");
    }
}
