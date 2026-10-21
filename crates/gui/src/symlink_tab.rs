//! 符号链接页签: 批量创建 / 移动并创建符号链接 + 历史记录管理。
//!
//! - 待处理列表支持分多次添加, 文件/文件夹混合。
//! - "创建": 在目标文件夹 B 的下一级为每个源创建同名符号链接 (保留原 SymlinkCreator 功能)。
//! - "移动并创建": 先把源移动到 B 的下一级, 再在原位置创建指向新位置的符号链接。
//! - 相对目标: 同盘用相对路径, 跨盘自动回退绝对路径 (与原项目 SymlinkPlanner 一致)。
//! - 所有文件操作在后台线程执行, UI 通过 channel 轮询进度, 窗口不卡死; 支持取消 (按条粒度)。
//! - 占用解锁 (unlocker.rs, 移植自 UnlockDelete): "移动并创建"遇文件被占用时, 可用
//!   Restart Manager 找到占用进程并结束后重试; 仍失败则中止并把已移动的条目全部回滚到原位,
//!   同时弹出错误窗口。
//! - 创建成功的链接写入账本 symlink_records 表, 历史记录子页可勾选删除 (只删链接不碰本体)。

use appdir_core::timefmt;
use crate::unlocker;
use eframe::egui;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

pub struct SymlinkState {
    db: Option<Arc<appdir_core::Db>>,
    view: View,
    items: Vec<PathBuf>,
    dest: String,
    unlock: bool, // 遇占用自动结束进程并重试 (仅移动并创建)
    results: Vec<(bool, String)>, // (是否错误, 文本)
    busy: bool,
    done: usize,
    total: usize,
    current: String,
    cancel: Arc<AtomicBool>,
    rx: Option<mpsc::Receiver<Msg>>,
    popup: Option<Vec<String>>, // 回滚报错弹窗
    // 历史记录子页
    records: Vec<RecRow>,
    history_stale: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum View {
    Operate,
    History,
}

struct RecRow {
    rec: appdir_core::SymlinkRecord,
    checked: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Create,
    MoveAndCreate,
}

enum Msg {
    Progress(String),   // 当前正在处理的条目
    Info(String),       // 过程信息 (不占进度)
    Item(bool, String), // 单条结果
    Aborted(Vec<String>), // 移动失败已回滚, 弹窗内容
    Done,               // 全部结束
}

/// 后台线程的工作单元 (主线程预校验后生成)。
struct Entry {
    src: PathBuf,
    name: String,
    is_dir: bool,
}

impl SymlinkState {
    pub fn new(db: Option<Arc<appdir_core::Db>>) -> Self {
        Self {
            db,
            view: View::Operate,
            items: Vec::new(),
            dest: String::new(),
            unlock: true,
            results: Vec::new(),
            busy: false,
            done: 0,
            total: 0,
            current: String::new(),
            cancel: Arc::new(AtomicBool::new(false)),
            rx: None,
            popup: None,
            records: Vec::new(),
            history_stale: true,
        }
    }

    pub fn set_db(&mut self, db: Arc<appdir_core::Db>) {
        self.db = Some(db);
        self.history_stale = true;
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.view, View::Operate, "操作");
            ui.selectable_value(&mut self.view, View::History, "历史记录");
            ui.separator();
            ui.heading("符号链接");
        });
        ui.add_space(4.0);
        match self.view {
            View::Operate => self.ui_operate(ui),
            View::History => self.ui_history(ui),
        }
        self.ui_popup(ui.ctx());
    }

    fn ui_popup(&mut self, ctx: &egui::Context) {
        let Some(msgs) = self.popup.clone() else { return };
        let mut open = true;
        let mut close_clicked = false;
        egui::Window::new("⚠ 错误")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label("移动过程中出现无法恢复的错误, 已完成的条目已移回原位:");
                ui.separator();
                egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                    for m in &msgs {
                        ui.colored_label(egui::Color32::LIGHT_RED, m);
                    }
                });
                ui.add_space(4.0);
                if ui.button("关闭").clicked() {
                    close_clicked = true;
                }
            });
        if close_clicked || !open {
            self.popup = None;
        }
    }

    fn ui_operate(&mut self, ui: &mut egui::Ui) {
        ui.add_enabled_ui(!self.busy, |ui| {
            ui.horizontal(|ui| {
                if ui.button("添加文件...").clicked() {
                    if let Some(paths) = rfd::FileDialog::new().pick_files() {
                        self.add_items(paths);
                    }
                }
                if ui.button("添加文件夹...").clicked() {
                    if let Some(paths) = rfd::FileDialog::new().pick_folders() {
                        self.add_items(paths);
                    }
                }
                if ui.button("清空列表").clicked() {
                    self.items.clear();
                }
                ui.separator();
                ui.label(format!(
                    "待处理 {} 项 (可分多次添加, 文件/文件夹可混合)",
                    self.items.len()
                ));
            });
        });

        egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
            let mut remove: Option<usize> = None;
            if self.busy {
                for p in &self.items {
                    ui.horizontal(|ui| {
                        ui.label(if p.is_dir() { "📁" } else { "📄" });
                        ui.monospace(p.display().to_string());
                    });
                }
            } else {
                for (i, p) in self.items.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(if p.is_dir() { "📁" } else { "📄" });
                        ui.monospace(p.display().to_string());
                        if ui.small_button("✕").clicked() {
                            remove = Some(i);
                        }
                    });
                }
                if let Some(i) = remove {
                    self.items.remove(i);
                }
            }
            if self.items.is_empty() {
                ui.label("(列表为空)");
            }
        });

        ui.add_space(8.0);
        ui.add_enabled_ui(!self.busy, |ui| {
            ui.horizontal(|ui| {
                ui.label("目标文件夹 B:");
                let before = self.dest.clone();
                ui.add_sized([460.0, 20.0], egui::TextEdit::singleline(&mut self.dest));
                if self.dest != before && !self.dest.is_empty() {
                    // 手输路径容忍引号与首尾空格
                    self.dest = self.dest.trim().trim_matches('"').to_string();
                }
                if ui.button("选择目标文件夹...").clicked() {
                    if let Some(p) = rfd::FileDialog::new().pick_folder() {
                        self.dest = p.display().to_string();
                    }
                }
            });
            ui.small("同盘用相对目标, 跨盘自动改用绝对路径");
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                if ui.button("创建").clicked() {
                    self.run(Mode::Create);
                }
                ui.label("在 B 的下一级为每个源创建同名符号链接, 原件不动");
            });
            ui.horizontal(|ui| {
                if ui.button("移动并创建").clicked() {
                    self.run(Mode::MoveAndCreate);
                }
                ui.label("先把源移动到 B 的下一级, 再在原位置创建指向新位置的符号链接");
            });
            ui.checkbox(
                &mut self.unlock,
                "遇到占用自动解锁: 结束占用进程后重试; 仍失败则中止移动并回滚已移动的文件 (仅\"移动并创建\")",
            );
        });
        ui.add_space(8.0);

        // 进度区: 后台执行时显示进度条 + 取消按钮 (UI 不卡死)
        if self.busy {
            ui.horizontal(|ui| {
                ui.add_sized(
                    [380.0, 18.0],
                    egui::ProgressBar::new(self.done as f32 / self.total.max(1) as f32)
                        .show_percentage()
                        .text(format!("{}/{}", self.done, self.total)),
                );
                if ui.button("取消").clicked() {
                    self.cancel.store(true, Ordering::Relaxed);
                }
            });
            if !self.current.is_empty() {
                ui.monospace(format!("当前: {}", self.current));
            }
        }

        if !self.results.is_empty() {
            egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                for (is_err, msg) in &self.results {
                    ui.colored_label(
                        if *is_err {
                            egui::Color32::LIGHT_RED
                        } else {
                            egui::Color32::LIGHT_GREEN
                        },
                        msg,
                    );
                }
            });
        }
    }

    fn ui_history(&mut self, ui: &mut egui::Ui) {
        if self.history_stale {
            self.reload_records();
        }
        ui.label("账本中记录的、曾由本工具创建过的符号链接。删除只移除链接本身, 不碰文件本体。");
        ui.add_space(4.0);
        let n = self.records.iter().filter(|r| r.checked).count();
        ui.horizontal(|ui| {
            if ui.button("刷新").clicked() {
                self.reload_records();
            }
            if ui.button("全选").clicked() {
                for r in &mut self.records {
                    r.checked = true;
                }
            }
            if ui.button("全不选").clicked() {
                for r in &mut self.records {
                    r.checked = false;
                }
            }
            if ui.button(format!("删除选中 ({n})")).clicked() && n > 0 {
                self.delete_checked();
            }
        });
        ui.add_space(6.0);
        egui::ScrollArea::vertical().show(ui, |ui| {
            for row in &mut self.records {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut row.checked, "");
                    ui.vertical(|ui| {
                        ui.label(format!("🔗 链接位置: {}", row.rec.link_path));
                        ui.label(format!("   本体位置: {}", row.rec.target_path));
                        ui.small(format!(
                            "创建于 {}{}",
                            timefmt::format_local(row.rec.created_at_ms),
                            link_state_note(&row.rec.link_path)
                        ));
                    });
                });
                ui.separator();
            }
            if self.records.is_empty() {
                ui.label("(暂无记录; 在\"操作\"页创建符号链接后会自动记录)");
            }
        });
    }

    fn reload_records(&mut self) {
        self.records.clear();
        self.history_stale = false;
        let Some(db) = &self.db else { return };
        match db.symlink_records() {
            Ok(rs) => {
                self.records = rs.into_iter().map(|rec| RecRow { rec, checked: false }).collect();
            }
            Err(e) => self.results.push((true, format!("读取符号链接记录失败: {e}"))),
        }
    }

    fn delete_checked(&mut self) {
        let Some(db) = &self.db else { return };
        let mut removed = 0usize;
        let mut links: Vec<String> = Vec::new();
        for row in &self.records {
            if !row.checked {
                continue;
            }
            let p = PathBuf::from(&row.rec.link_path);
            match remove_symlink(&p) {
                Ok(()) => {
                    removed += 1;
                    self.results.push((false, format!("已删除链接: {}", row.rec.link_path)));
                }
                Err(e) => self.results.push((
                    true,
                    format!("删除链接失败 [{}]: {} (记录仍会移除)", row.rec.link_path, e),
                )),
            }
            links.push(row.rec.link_path.clone());
        }
        match db.delete_symlink_records(&links) {
            Ok(_) => self
                .results
                .push((false, format!("已移除 {removed} 个链接, {} 条记录", links.len()))),
            Err(e) => self.results.push((true, format!("移除账本记录失败: {e}"))),
        }
        self.reload_records();
    }

    /// 每帧轮询后台线程消息 (在 update 里调用)。
    pub fn is_busy(&self) -> bool {
        self.busy
    }

    pub fn poll(&mut self) {
        let Some(rx) = &self.rx else { return };
        let mut finished = false;
        while let Ok(m) = rx.try_recv() {
            match m {
                Msg::Progress(current) => self.current = current,
                Msg::Info(msg) => self.results.push((false, msg)),
                Msg::Item(ok, msg) => {
                    self.results.push((!ok, msg));
                    self.done += 1;
                }
                Msg::Aborted(msgs) => {
                    for m in &msgs {
                        self.results.push((true, m.clone()));
                    }
                    self.popup = Some(msgs);
                    self.history_stale = true;
                }
                Msg::Done => finished = true,
            }
        }
        if finished {
            self.busy = false;
            self.current.clear();
            self.rx = None;
            self.history_stale = true; // 本次创建产生的记录待刷新
        }
    }

    fn add_items(&mut self, paths: Vec<PathBuf>) {
        for p in paths {
            if !self.items.contains(&p) {
                self.items.push(p);
            }
        }
    }

    fn run(&mut self, mode: Mode) {
        self.results.clear();
        self.popup = None;
        let dest = PathBuf::from(&self.dest);

        let mut errs: Vec<String> = Vec::new();
        if self.items.is_empty() {
            errs.push("待处理列表为空".into());
        }
        if self.dest.trim().is_empty() || !dest.is_dir() {
            errs.push(format!("目标文件夹无效: {}", self.dest));
        }
        if !errs.is_empty() {
            self.results = errs.into_iter().map(|m| (true, m)).collect();
            return;
        }

        // 整批预校验 (主线程, 很快): 重名 / 目标已存在 / 源不存在 / (移动)源在目标内
        let mut seen: HashSet<String> = HashSet::new();
        let mut plan: Vec<Entry> = Vec::new();
        for it in self.items.drain(..).collect::<Vec<_>>() {
            let Some(name) = link_name(&it) else {
                errs.push(format!("无法取文件名: {}", it.display()));
                continue;
            };
            if !seen.insert(name.to_lowercase()) {
                errs.push(format!("重名的链接名: {name} (同批内不允许重复)"));
                continue;
            }
            if exists_any(&dest.join(&name)) {
                errs.push(format!("目标下已存在同名项: {}", dest.join(&name).display()));
                continue;
            }
            if !exists_any(&it) {
                errs.push(format!("源不存在: {}", it.display()));
                continue;
            }
            if mode == Mode::MoveAndCreate && it.starts_with(&dest) {
                errs.push(format!("源位于目标文件夹内部, 拒绝移动: {}", it.display()));
                continue;
            }
            let is_dir = it.is_dir();
            plan.push(Entry { src: it, is_dir, name });
        }
        self.items = plan.iter().map(|e| e.src.clone()).collect(); // 保留通过校验的项在列表里
        if plan.is_empty() {
            self.results = errs.into_iter().map(|m| (true, m)).collect();
            return;
        }
        for m in &errs {
            self.results.push((true, m.clone()));
        }

        // 移动前先探测符号链接权限, 避免搬走后链接建不上
        if mode == Mode::MoveAndCreate {
            if let Err(e) = probe_privilege(&dest) {
                let msg = format!(
                    "探测符号链接权限失败, 未做任何移动: {e}; 创建链接需要管理员权限或开发者模式"
                );
                self.results.push((true, msg.clone()));
                self.popup = Some(vec![msg]);
                return;
            }
        }

        // 后台线程执行, UI 通过 channel 拿进度
        self.busy = true;
        self.done = 0;
        self.total = plan.len();
        self.current = plan[0].name.clone();
        self.cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        let cancel = self.cancel.clone();
        let db = self.db.clone();
        let unlock = self.unlock;
        std::thread::spawn(move || {
            // 已成功完成的移动条目 (用于失败回滚): (原位置, 新位置)
            let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
            let mut abort_err: Option<String> = None;
            for e in &plan {
                if cancel.load(Ordering::Relaxed) {
                    let _ = tx.send(Msg::Item(false, format!("已取消: {} (未处理)", e.src.display())));
                    continue;
                }
                let _ = tx.send(Msg::Progress(e.src.display().to_string()));
                let mut ok_msg: Option<String> = None;
                let mut err_msg: Option<String> = None;
                match mode {
                    Mode::Create => {
                        let link = dest.join(&e.name);
                        // 跨盘自动回退绝对路径 (与原项目 SymlinkPlanner 一致)
                        let target = relative_target(&dest, &e.src).unwrap_or_else(|| e.src.clone());
                        match create_symlink(&link, &target, e.is_dir) {
                            Ok(()) => {
                                ok_msg = Some(format!("已创建链接: {} -> {}", link.display(), e.src.display()));
                                if let Some(d) = &db {
                                    if let Err(err) = d.add_symlink_record(
                                        &link.display().to_string(),
                                        &e.src.display().to_string(),
                                    ) {
                                        ok_msg = Some(format!("{} (账本记录写入失败: {err})", ok_msg.unwrap()));
                                    }
                                }
                            }
                            Err(err) => {
                                err_msg = Some(format!("失败 [{}]: {}", e.name, io_hint(err)));
                            }
                        }
                    }
                    Mode::MoveAndCreate => {
                        let new_loc = dest.join(&e.name);
                        let mut mv = move_path(&e.src, &new_loc);
                        if mv.is_err() && unlock {
                            // 占用解锁: Restart Manager 找占用者 → 结束 → 重试
                            let paths = collect_rm_paths(&e.src);
                            let lockers = unlocker::find_locking_processes(&paths);
                            if !lockers.is_empty() {
                                let names: Vec<String> = lockers
                                    .iter()
                                    .map(|l| format!("{}(pid {})", l.name, l.pid))
                                    .collect();
                                let _ = tx.send(Msg::Info(format!(
                                    "{} 被占用: {}; 尝试结束后重试",
                                    e.src.display(),
                                    names.join(", ")
                                )));
                                let mut killed = 0usize;
                                let me = std::process::id();
                                for l in &lockers {
                                    if l.pid != me && unlocker::kill_process(l.pid) {
                                        killed += 1;
                                    }
                                }
                                let _ = tx.send(Msg::Info(format!(
                                    "已结束 {}/{} 个占用进程, 等待后重试",
                                    killed,
                                    lockers.len()
                                )));
                                std::thread::sleep(std::time::Duration::from_millis(500));
                                mv = move_path(&e.src, &new_loc);
                            }
                        }
                        match mv {
                            Err(err) => {
                                let suffix = if unlock { " (尝试解锁后仍失败)" } else { "" };
                                err_msg = Some(format!(
                                    "失败 [{}]: {}{}",
                                    e.name,
                                    io_hint(err),
                                    suffix
                                ));
                            }
                            Ok(()) => {
                                let parent = e.src.parent().unwrap_or(Path::new("."));
                                let target =
                                    relative_target(parent, &new_loc).unwrap_or_else(|| new_loc.clone());
                                match create_symlink(&e.src, &target, e.is_dir) {
                                    Ok(()) => {
                                        moved.push((e.src.clone(), new_loc.clone()));
                                        ok_msg = Some(format!(
                                            "已移动 {} -> {} 并在原位置创建链接",
                                            e.src.display(),
                                            new_loc.display()
                                        ));
                                        if let Some(d) = &db {
                                            if let Err(err) = d.add_symlink_record(
                                                &e.src.display().to_string(),
                                                &new_loc.display().to_string(),
                                            ) {
                                                ok_msg = Some(format!(
                                                    "{} (账本记录写入失败: {err})",
                                                    ok_msg.unwrap()
                                                ));
                                            }
                                        }
                                    }
                                    Err(err) => {
                                        // 链接建不上: 把本条文件搬回去
                                        match move_path(&new_loc, &e.src) {
                                            Ok(()) => {
                                                err_msg = Some(format!(
                                                    "失败 [{}]: 链接创建失败 ({}), 文件已移回原位",
                                                    e.name,
                                                    io_hint(err)
                                                ));
                                            }
                                            Err(e2) => {
                                                err_msg = Some(format!(
                                                    "失败 [{}]: 链接创建失败 ({}) 且移回失败 ({}), 文件留在 {}",
                                                    e.name,
                                                    io_hint(err),
                                                    io_hint(e2),
                                                    new_loc.display()
                                                ));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if let Some(m) = ok_msg {
                    let _ = tx.send(Msg::Item(true, m));
                }
                if let Some(m) = err_msg {
                    let _ = tx.send(Msg::Item(false, m.clone()));
                    if mode == Mode::MoveAndCreate {
                        abort_err = Some(m);
                        break; // 中止后续条目
                    }
                }
            }
            // 移动模式失败 → 把已完成的条目全部回滚到原位
            if let Some(err) = abort_err {
                let mut msgs = vec![err, "已完成的条目正在回滚到原位:".into()];
                for (src, new_loc) in moved.iter().rev() {
                    let _ = remove_symlink(src); // 删掉原位置的链接 (如果有)
                    match move_path(new_loc, src) {
                        Ok(()) => msgs.push(format!("已移回: {}", src.display())),
                        Err(e) => msgs.push(format!(
                            "移回失败: {} 原因: {} (文件仍留在 {})",
                            src.display(),
                            io_hint(e),
                            new_loc.display()
                        )),
                    }
                    if let Some(d) = &db {
                        let _ = d.delete_symlink_records(&[src.display().to_string()]);
                    }
                }
                msgs.push("整个移动操作已中止, 回滚执行完毕。".into());
                let _ = tx.send(Msg::Aborted(msgs));
            }
            let _ = tx.send(Msg::Done);
        });
    }
}

/// Restart Manager 注册用的路径列表: 目录展开为自身 + 内部文件 (上限 400)。
fn collect_rm_paths(src: &Path) -> Vec<String> {
    let mut out = vec![src.display().to_string()];
    if src.is_dir() {
        let mut stack = vec![src.to_path_buf()];
        let mut n = 0usize;
        while let Some(d) = stack.pop() {
            if let Ok(rd) = std::fs::read_dir(&d) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        stack.push(p);
                    } else {
                        out.push(p.display().to_string());
                        n += 1;
                    }
                    if n >= 400 {
                        return out;
                    }
                }
            }
        }
    }
    out
}

/// 链接当前状态备注 (历史记录展示用)。
fn link_state_note(link: &str) -> String {
    match std::fs::symlink_metadata(link) {
        Ok(md) if md.file_type().is_symlink() => "  [链接存在]".to_string(),
        Ok(_) => "  [该位置已不是符号链接]".to_string(),
        Err(_) => "  [链接不存在 (已删除或目标丢失)]".to_string(),
    }
}

/// 只删除符号链接本身 (目录链接/文件链接都适用), 不递归进目标。
fn remove_symlink(p: &Path) -> std::io::Result<()> {
    let md = std::fs::symlink_metadata(p)?;
    if !md.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "该位置已不是符号链接, 为安全起见不删除",
        ));
    }
    match std::fs::remove_dir(p) {
        Ok(()) => Ok(()),
        Err(_) => std::fs::remove_file(p),
    }
}

fn link_name(p: &Path) -> Option<String> {
    p.file_name().map(|s| s.to_string_lossy().into_owned())
}

fn exists_any(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

#[cfg(windows)]
fn create_symlink(link: &Path, target: &Path, is_dir: bool) -> std::io::Result<()> {
    if is_dir {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

/// 同盘返回相对路径, 跨盘返回 None (调用方回退绝对路径)。
fn relative_target(from_dir: &Path, target: &Path) -> Option<PathBuf> {
    let from = std::fs::canonicalize(from_dir).unwrap_or_else(|_| from_dir.to_path_buf());
    let to = std::fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
    let fa: Vec<Component> = from.components().collect();
    let ta: Vec<Component> = to.components().collect();
    if fa.first()? != ta.first()? {
        return None;
    }
    let mut i = 0;
    while i < fa.len() && i < ta.len() && fa[i] == ta[i] {
        i += 1;
    }
    let mut out = PathBuf::new();
    for _ in i..fa.len() {
        out.push("..");
    }
    for c in &ta[i..] {
        out.push(c.as_os_str());
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    Some(out)
}

fn move_path(src: &Path, dst: &Path) -> std::io::Result<()> {
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    // 跨盘: 复制后删除源
    if src.is_dir() {
        copy_dir(src, dst)?;
        std::fs::remove_dir_all(src)
    } else {
        std::fs::copy(src, dst)?;
        std::fs::remove_file(src)
    }
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let sp = entry.path();
        let dp = dst.join(entry.file_name());
        if sp.is_dir() {
            copy_dir(&sp, &dp)?;
        } else {
            std::fs::copy(&sp, &dp)?;
        }
    }
    Ok(())
}

/// 在 dest 里建一个指向自身的临时符号链接再删除, 验证当前进程有权限。
fn probe_privilege(dest: &Path) -> Result<(), String> {
    let probe = dest.join(".appdir-symlink-probe");
    match create_symlink(&probe, Path::new("."), true) {
        Ok(()) => {
            let _ = std::fs::remove_dir(&probe);
            Ok(())
        }
        Err(e) => Err(io_hint(e)),
    }
}

fn io_hint(e: std::io::Error) -> String {
    if e.raw_os_error() == Some(1314) {
        "客户端没有所需的特权 (需以管理员运行 GUI, 或开启 Windows 开发者模式)".into()
    } else {
        e.to_string()
    }
}
