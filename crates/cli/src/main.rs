//! appdir - Windows 应用数据目录溯源与清理工具 (第一版, 全用户态)
//!
//! 命令行入口。所有破坏性操作默认 dry-run, 必须显式 --yes 才会删除。

use appdir_capture::WatchSource;
use appdir_core::{
    clean, path, timefmt, CleanOptions, Db, DirRow, EventSource, ExeQuery, Ledger, QueryItem, RawCreateEvent,
    RecordStatus,
};
use std::path::{Path, PathBuf};

const USAGE: &str = r#"appdir - 应用数据目录溯源与清理 (v1, 用户态)

用法:
  appdir monitor add <目录>            添加监视根目录 (绝对路径)
  appdir monitor list                   列出监视根目录
  appdir monitor remove <目录>          移除监视根目录
  appdir monitor enable|disable <目录>  启用/停用某个监视根目录

  appdir query exe <EXE完整路径> [--stale]     查询该程序创建的目录
  appdir query directory <目录路径>            查询某个目录的归属记录
  appdir query all [--stale]                   列出账本
  appdir query ignored [--limit N]             列出被丢弃的事件与原因

  appdir clean --exe <EXE完整路径> [选项]      清理该程序创建的目录
  appdir clean --directory <目录路径> [选项]   清理单个目录
      选项: --dry-run(默认) --backup --yes --max N

  appdir daemon [--source watch|etw] [--root <目录>] [--seconds N]
                [--seconds N]                   采集事件并写入账本
  appdir status                                 采集与账本状态
  appdir reconcile                              按当前文件系统刷新记录状态
  appdir selftest [--keep]                      在临时沙箱中自检全流程
  appdir version | help

数据目录: 默认 %LOCALAPPDATA%\appdir-provenance, 可用环境变量 APPDIR_DATA_DIR 覆盖。
全局选项: --data-dir <路径>
"#;

#[derive(Debug)]
struct Args {
    positional: Vec<String>,
    flags: Vec<String>,
    opts: Vec<(String, String)>,
}

impl Args {
    fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f == name)
    }
    fn opt(&self, name: &str) -> Option<&str> {
        self.opts.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// Every value given for an option, so `--root` can repeat.
    fn opt_all(&self, name: &str) -> Vec<String> {
        self.opts.iter().filter(|(k, _)| k == name).map(|(_, v)| v.clone()).collect()
    }
}

fn parse_args(argv: &[String]) -> Args {
    let mut a = Args { positional: Vec::new(), flags: Vec::new(), opts: Vec::new() };
    let mut it = argv.iter();
    while let Some(arg) = it.next() {
        if let Some(body) = arg.strip_prefix("--") {
            if let Some((k, v)) = body.split_once('=') {
                a.opts.push((k.to_string(), v.to_string()));
            } else if ["data-dir", "source", "root", "script", "feed", "seconds", "exe", "directory", "max", "limit"]
                .contains(&body)
            {
                match it.next() {
                    Some(v) => a.opts.push((body.to_string(), v.to_string())),
                    None => a.flags.push(body.to_string()),
                }
            } else {
                a.flags.push(body.to_string());
            }
        } else {
            a.positional.push(arg.clone());
        }
    }
    a
}

fn die(msg: impl AsRef<str>) -> ! {
    eprintln!("错误: {}", msg.as_ref());
    std::process::exit(1);
}

fn data_dir(args: &Args) -> PathBuf {
    if let Some(d) = args.opt("data-dir") {
        return PathBuf::from(d);
    }
    if let Ok(d) = std::env::var("APPDIR_DATA_DIR") {
        return PathBuf::from(d);
    }
    let local = std::env::var("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    local.join("appdir-provenance")
}

fn open_db(dir: &Path) -> Db {
    if let Err(e) = std::fs::create_dir_all(dir) {
        die(format!("无法创建数据目录 {}: {e}", dir.display()));
    }
    match Db::open(&dir.join("ledger.db")) {
        Ok(db) => db,
        Err(e) => die(format!("无法打开账本数据库: {e}")),
    }
}

fn ts(ms: i64) -> String {
    timefmt::format_local(ms)
}

fn rel(ms: i64) -> String {
    let d = (timefmt::now_ms() - ms).max(0);
    if d < 60_000 {
        format!("{} 秒前", d / 1000)
    } else if d < 3_600_000 {
        format!("{} 分钟前", d / 60_000)
    } else {
        format!("{} 小时前", d / 3_600_000)
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = parse_args(&argv);
    let dir = data_dir(&args);
    let cmd = args.positional.first().map(|s| s.as_str()).unwrap_or("help");

    let code = match cmd {
        "monitor" => cmd_monitor(&args, &dir),
        "query" => cmd_query(&args, &dir),
        "clean" => cmd_clean(&args, &dir),
        "daemon" => cmd_daemon(&args, &dir),
        "status" => cmd_status(&args, &dir),
        "reconcile" => cmd_reconcile(&args, &dir),
        "selftest" => cmd_selftest(&args, &dir),
        "version" => {
            println!("appdir 0.1.0 (用户态第一版, 无内核驱动)");
            Ok(0)
        }
        "help" | "" => {
            println!("{USAGE}");
            Ok(0)
        }
        other => {
            eprintln!("未知命令: {other}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    match code {
        Ok(0) => {}
        Ok(c) => std::process::exit(c),
        Err(e) => die(e.to_string()),
    }
}

fn need<'a>(args: &'a Args, i: usize, what: &str) -> Result<&'a String, appdir_core::Error> {
    args.positional
        .get(i)
        .ok_or_else(|| appdir_core::Error::Config(format!("缺少参数: {what}")))
}

fn cmd_monitor(args: &Args, dir: &Path) -> Result<i32, appdir_core::Error> {
    let db = open_db(dir);
    let sub = args.positional.get(1).map(|s| s.as_str()).unwrap_or("list");
    match sub {
        "add" => {
            let raw = need(args, 2, "要监视的目录")?;
            let abs = path::normalize(raw).ok_or_else(|| appdir_core::Error::InvalidPath(raw.clone()))?;
            let meta = std::fs::metadata(&abs)
                .map_err(|_| appdir_core::Error::InvalidPath(format!("{abs} 不存在或不可访问")))?;
            if !meta.is_dir() {
                return Err(appdir_core::Error::InvalidPath(format!("{abs} 不是目录")));
            }
            let (row, added) = db.add_root(&abs, timefmt::now_ms())?;
            if added {
                println!("已添加监视根目录: {}", row.path);
            } else {
                println!("该根目录已在监视列表中: {}", row.path);
            }
            println!("数据目录: {}", dir.display());
            println!("提示: 运行 appdir daemon --source watch --root \"{}\" 开始监听", row.path);
            Ok(0)
        }
        "list" => {
            let roots = db.roots()?;
            if roots.is_empty() {
                println!("(尚未配置监视根目录)");
                return Ok(0);
            }
            println!("{:<9} {:<24} 路径", "状态", "添加于");
            for r in roots {
                println!(
                    "{:<9} {:<24} {}",
                    if r.enabled { "启用" } else { "停用" },
                    ts(r.added_at_ms),
                    r.path
                );
            }
            Ok(0)
        }
        "remove" => {
            let raw = need(args, 2, "要移除的目录")?;
            if db.remove_root(raw)? {
                println!("已移除监视根目录: {raw}");
                println!("提示: 已登记的目录记录会保留历史。");
                Ok(0)
            } else {
                Err(appdir_core::Error::Config(format!("未在监视配置中找到: {raw}")))
            }
        }
        "enable" | "disable" => {
            let raw = need(args, 2, "目录")?;
            let on = sub == "enable";
            if db.set_root_enabled(raw, on)? {
                println!("已{}: {raw}", if on { "启用" } else { "停用" });
                Ok(0)
            } else {
                Err(appdir_core::Error::Config(format!("未在监视配置中找到: {raw}")))
            }
        }
        other => Err(appdir_core::Error::Config(format!("未知子命令 monitor {other}"))),
    }
}

fn print_dir_row(db: &Db, row: &DirRow, show_app: bool) -> Result<(), appdir_core::Error> {
    let app = db.app_by_id(row.app_id)?;
    let status = match row.record_status {
        RecordStatus::Active => "有效",
        RecordStatus::Missing => "已不存在",
        RecordStatus::NeedsReview => "待复核",
    };
    println!("{:<10} {}", status, row.directory_path);
    println!("{:<10} {}", "监视根", row.monitor_root);
    if show_app {
        if let Some(a) = app {
            println!("{:<10} {}  ({})", "创建程序", a.exe_path, a.exe_name);
        }
    }
    println!("{:<10} PID {}  创建记录 {}", "", row.creator_pid, ts(row.created_at_ms));
    Ok(())
}

fn cmd_query(args: &Args, dir: &Path) -> Result<i32, appdir_core::Error> {
    let db = open_db(dir);
    match args.positional.get(1).map(|s| s.as_str()).unwrap_or("") {
        "exe" => {
            let exe = need(args, 2, "EXE 完整路径")?;
            let q = ExeQuery::run(&db, exe, args.flag("stale"))?;
            if !q.found() {
                println!("账本中没有 {exe} 创建的目录");
                println!("提示: --stale 可显示目录已被删除的历史记录");
                return Ok(3);
            }
            let n = q.directories.len();
            if let Some(a) = &q.app {
                println!("程序: {} (首次见于 {}, 最近 {}) ", a.exe_path, ts(a.first_seen_ms), rel(a.last_seen_ms));
            }
            println!("共 {n} 个目录:");
            for d in &q.directories {
                println!("  {}", d.directory_path);
                println!("      PID {}  {}  记录于 {}", d.creator_pid, ts(d.created_at_ms), d.record_status.as_str());
            }
            Ok(0)
        }
        "directory" => {
            let p = need(args, 2, "目录路径")?;
            match QueryItem::run(&db, p)? {
                None => {
                    println!("账本中没有该目录的记录: {p}");
                    Ok(3)
                }
                Some(item) => {
                    print_dir_row(&db, &item.directory, true)?;
                    Ok(0)
                }
            }
        }
        "all" => {
            let rows = db.all_dirs()?;
            let rows: Vec<_> = rows
                .iter()
                .filter(|r| args.flag("stale") || r.record_status == RecordStatus::Active)
                .cloned()
                .collect();
            if rows.is_empty() {
                println!("账本为空 (还没有捕获到可归属的目录创建事件)");
                return Ok(0);
            }
            println!("账本共 {} 条记录:", rows.len());
            for r in &rows {
                print_dir_row(&db, r, false)?;
                println!();
            }
            Ok(0)
        }
        "ignored" => {
            let limit: i64 = args.opt("limit").and_then(|v| v.parse().ok()).unwrap_or(20);
            let counts = db.ignored_counts()?;
            if counts.is_empty() {
                println!("没有被丢弃的事件");
                return Ok(0);
            }
            println!("按原因统计:");
            for (reason, n) in &counts {
                println!("  {:<24} {n}", reason);
            }
            println!("\n最近 {} 条:", limit);
            for row in db.recent_ignored(limit)? {
                println!(
                    "  {}  {:<18} {}  (PID {}{})",
                    ts(row.observed_at_ms),
                    row.reason,
                    row.directory_path,
                    row.creator_pid,
                    row.exe_path.map(|e| format!(", {e}")).unwrap_or_default()
                );
            }
            Ok(0)
        }
        other => Err(appdir_core::Error::Config(format!("未知子命令 query {other}"))),
    }
}

fn cmd_clean(args: &Args, dir: &Path) -> Result<i32, appdir_core::Error> {
    let db = open_db(dir);
    let now = timefmt::now_ms();
    let by_exe = args.opt("exe").map(|s| s.to_string());
    let by_dir = args.opt("directory").map(|s| s.to_string());
    if by_exe.is_none() && by_dir.is_none() {
        return Err(appdir_core::Error::Config("clean 需要 --exe <路径> 或 --directory <路径>".to_string()));
    }
    if by_exe.is_some() && by_dir.is_some() {
        return Err(appdir_core::Error::Config("--exe 与 --directory 只能二选一".to_string()));
    }

    let mut candidates = Vec::new();
    match (&by_exe, &by_dir) {
        (Some(exe), None) => candidates = clean::plan_for_exe(&db, exe, &[dir.display().to_string()])?,
        (None, Some(p)) => candidates = vec![clean::verify(&db, p, None, &[dir.display().to_string()])?],
        _ => {}
    }
    if let Some(max) = args.opt("max").and_then(|v| v.parse::<usize>().ok()) {
        candidates.truncate(max);
    }
    if candidates.is_empty() {
        println!("没有匹配的账本记录");
        return Ok(3);
    }

    let deletable = candidates.iter().filter(|c| c.deletable()).count();
    let blocked = candidates.len() - deletable;
    println!("候选目录 {} 个, 可删除 {deletable} 个, 被拦截 {blocked} 个", candidates.len());
    for c in &candidates {
        if c.deletable() {
            println!("  [可删除] {}", c.directory_path);
            println!("            归属程序 {}  监视根 {}", c.app_exe_path, c.monitor_root);
        } else {
            println!("  [拦截]   {}  -> {}", c.directory_path, c.block_text());
        }
    }

    let commit = args.flag("yes") && !args.flag("dry-run");
    if !commit {
        println!("\n这是预览, 未删除任何目录。要真正删除请加 --yes, 并建议同时加 --backup。");
        return Ok(0);
    }
    if args.flag("backup") && !args.flag("no-backup") {
        // keep
    }
    let backup_root = if args.flag("no-backup") {
        None
    } else {
        Some(dir.join("backups"))
    };
    if backup_root.is_none() {
        println!("\n警告: 指定了 --no-backup, 将不产生备份。");
    } else if let Some(b) = &backup_root {
        println!("\n备份目录: {}", b.display());
    }

    let opts = CleanOptions {
        backup_dir: backup_root.as_deref(),
        commit: true,
        protected_prefixes: vec![dir.display().to_string()],
    };
    let report = clean::execute(&db, candidates.clone(), &opts, now)?;
    println!();
    for item in &report.actions {
        let mark = if item.deleted {
            "已删除"
        } else if item.action == clean::Action::Skipped {
            "已跳过"
        } else if item.action == clean::Action::Failed {
            "失败"
        } else {
            "计划"
        };
        println!("  [{mark}] {}  {}", item.directory_path, item.detail);
    }
    println!(
        "\n结果: 删除 {} 个, 备份 {} 个, 跳过 {} 个, 失败 {} 个",
        report.deleted, report.backed_up, report.skipped, report.failed
    );
    if report.failed > 0 {
        return Ok(4);
    }
    Ok(0)
}

fn cmd_daemon(args: &Args, dir: &Path) -> Result<i32, appdir_core::Error> {
    let db = open_db(dir);
    let source = args.opt("source").unwrap_or("watch");
    let mut extra_roots: Vec<String> = args.opt_all("root");
    if extra_roots.is_empty() {
        extra_roots = args.positional.iter().skip(1).cloned().collect();
    }
    for r in &extra_roots {
        let (row, added) = db.add_root(r, timefmt::now_ms())?;
        println!("{}监视根目录: {}", if added { "已添加" } else { "已有" }, row.path);
    }

    let ledger = Ledger::new(&db)?;
    if ledger.view.roots.iter().filter(|r| r.enabled).count() == 0 {
        return Err(appdir_core::Error::Config(
            "没有启用的监视根目录; 先运行 appdir monitor add <目录>".to_string(),
        ));
    }

    let seconds: u64 = args.opt("seconds").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut counters = appdir_capture::Counters::default();
    let started = std::time::Instant::now();

    let run_how = if seconds == 0 { "直到 Ctrl-C".to_string() } else { format!("{seconds} 秒") };
    println!(
        "采集器启动: source={source}  账本={}  运行方式={run_how}",
        dir.join("ledger.db").display()
    );
    for r in &ledger.view.roots {
        println!("  {:<6} {}", if r.enabled { "[启用]" } else { "[停用]" }, r.path);
    }

    match source {
        "watch" => {
            let roots: Vec<String> = ledger.view.roots.iter().filter(|r| r.enabled).map(|r| r.path.clone()).collect();
            let mut src = WatchSource::new(roots.clone());
            println!("\n模式说明: 用户态目录监听(ReadDirectoryChangesW)只能确认\"直接子目录出现了\",");
            println!("          无法得知创建进程, 因此这些事件会被记账为 creator-unknown 并丢弃,");
            println!("          不会凭目录名猜测归属。要得到创建者, 需要驱动或 ETW 来源。");
            src.start().map_err(|e| appdir_core::Error::Unsupported(e.to_string()))?;
            if !src.wait_until_ready(std::time::Duration::from_secs(5)) {
                println!("警告: 监听未在 5 秒内就绪");
            }
            loop {
                let mut events = Vec::new();
                src.drain(&mut events).map_err(|e| appdir_core::Error::Unsupported(e.to_string()))?;
                for ev in &events {
                    let out = ledger.ingest(ev)?;
                    appdir_capture::Counters::tally(&out, ev, &mut counters);
                    println!(
                        "{}  {:<14} {}  ({})",
                        ts(ev.observed_at_ms),
                        out.label(),
                        ev.target_path,
                        if ev.creator.exe_path.is_some() { "已归属" } else { "创建者未知" }
                    );
                }
                if seconds != 0 && started.elapsed().as_secs() >= seconds {
                    break;
                }
                if appdir_core::winfile::install_ctrl_c_guard() && appdir_core::winfile::ctrl_c_requested() {
                    println!("\n收到 Ctrl-C, 正在关闭监听句柄...");
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            src.shutdown().map_err(|e| appdir_core::Error::Unsupported(e.to_string()))?;
            println!(
                "watch诊断: names_seen={} resync={} failures={:?} statuses={:?}",
                src.names_seen(),
                src.resync_count(),
                src.failures(),
                src.statuses().iter().take(8).collect::<Vec<_>>()
            );
        }
        "etw" => {
            let roots: Vec<String> = ledger.view.roots.iter().filter(|r| r.enabled).map(|r| r.path.clone()).collect();
            let mut src = appdir_capture::EtwSource::new(roots.clone()).map_err(|e| appdir_core::Error::Unsupported(e.to_string()))?;
            println!("\n模式说明: ETW 内核文件事件(Microsoft-Windows-Kernel-File)提供创建进程,");
            println!("          但 Create 事件本身不能区分\"新建\"与\"打开已有目录\", 因此每条候选");
            println!("          都会由非递归目录监听确认\"目录确实出现\"后才记账; 配不上的一律丢弃。");
            if let Some(err) = src.start_error() {
                println!("错误: {err}");
                println!("提示: 以管理员身份运行, 或改用 --source watch。");
                return Ok(2);
            }
            if !src.watcher_mut().wait_until_ready(std::time::Duration::from_secs(5)) {
                println!("警告: 目录监听未在 5 秒内就绪, 该时段事件可能无法确认");
            }
            loop {
                let mut events = Vec::new();
                src.drain(&mut events).map_err(|e| appdir_core::Error::Unsupported(e.to_string()))?;
                for ev in &events {
                    let out = ledger.ingest(ev)?;
                    appdir_capture::Counters::tally(&out, ev, &mut counters);
                    println!(
                        "{}  {:<14} {}  ({})",
                        ts(ev.observed_at_ms),
                        out.label(),
                        ev.target_path,
                        describe_creator(ev)
                    );
                }
                if seconds != 0 && started.elapsed().as_secs() >= seconds {
                    break;
                }
                if appdir_core::winfile::install_ctrl_c_guard() && appdir_core::winfile::ctrl_c_requested() {
                    println!("\n收到 Ctrl-C, 正在停止 ETW 会话...");
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            src.shutdown().map_err(|e| appdir_core::Error::Unsupported(e.to_string()))?;
            println!("ETW: {}", src.stats().snapshot());
            for l in appdir_capture::etw::debug_dump() { println!("  dump: {l}"); }
            if src.stats().events_lost() {
                println!("警告: 本次运行发生过事件丢失(缓冲区溢出或会话中断), 记录不完整!");
            }
            if let Some(err) = src.start_error() {
                println!("警告: ETW 会话异常: {err}");
            }
        }
        other => {
            return Err(appdir_core::Error::Config(format!("未知来源: {other} (可用 watch|etw)")))
        }
    }

    println!(
        "\n统计: 收到 {}  登记 {}  重新登记 {}  重复 {}  丢弃 {}",
        counters.received, counters.recorded, counters.reregistered, counters.duplicate, counters.ignored
    );
    for (reason, n) in &counters.by_reason {
        println!("  丢弃原因 {:<22} {n}", reason.code());
    }
    Ok(0)
}

fn describe_creator(ev: &RawCreateEvent) -> String {
    match &ev.creator.exe_path {
        Some(p) => format!("PID {}  {}", ev.creator.pid, p),
        None => format!("PID {}  创建者未知", ev.creator.pid),
    }
}

fn cmd_status(args: &Args, dir: &Path) -> Result<i32, appdir_core::Error> {
    let db = open_db(dir);
    let roots = db.roots()?;
    println!("数据目录 : {}", dir.display());
    println!("数据库   : {}", dir.join("ledger.db").display());
    println!("监视根   : {} 个 (启用 {} 个)", roots.len(), roots.iter().filter(|r| r.enabled).count());
    for r in &roots {
        println!("  {:<6} {}", if r.enabled { "[启用]" } else { "[停用]" }, r.path);
    }
    println!("应用程序 : {} 条", db.app_count()?);
    println!(
        "目录记录 : {} 条 (有效 {} / 已不存在 {} / 待复核 {})",
        db.dir_count(None)?,
        db.dir_count(Some(RecordStatus::Active))?,
        db.dir_count(Some(RecordStatus::Missing))?,
        db.dir_count(Some(RecordStatus::NeedsReview))?
    );
    let counts = db.ignored_counts()?;
    if counts.is_empty() {
        println!("丢弃事件 : 无");
    } else {
        let total: i64 = counts.iter().map(|(_, n)| n).sum();
        println!("丢弃事件 : {total} 条 -> {}", counts.iter().map(|(r, n)| format!("{r}:{n}")).collect::<Vec<_>>().join(", "));
    }
    println!(
        "采集状态 : {} (当前构建不含内核驱动; 用户态监听无创建者信息)",
        if roots.iter().any(|r| r.enabled) { "配置就绪" } else { "未配置" }
    );
    let _ = args;
    Ok(0)
}

/// Refresh every ledger row against the current file system.
fn cmd_reconcile(_args: &Args, dir: &Path) -> Result<i32, appdir_core::Error> {
    let db = open_db(dir);
    let total = db.dir_count(None)?;
    let ledger = Ledger::new(&db)?;
    let changed = ledger.reconcile(timefmt::now_ms())?;
    println!("已核对 {total} 条记录, 状态变化 {changed} 条");
    println!("  有效 {}  已不存在 {}  待复核 {}",
        db.dir_count(Some(RecordStatus::Active))?,
        db.dir_count(Some(RecordStatus::Missing))?,
        db.dir_count(Some(RecordStatus::NeedsReview))?);
    Ok(0)
}

fn cmd_selftest(args: &Args, _dir: &Path) -> Result<i32, appdir_core::Error> {
    let stamp = timefmt::stamp_ms(timefmt::now_ms(), 0);
    let sandbox = std::env::temp_dir().join(format!("appdir-selftest-{stamp}-{}", std::process::id()));
    std::fs::create_dir_all(&sandbox)
        .map_err(|e| appdir_core::Error::Io { path: sandbox.display().to_string(), message: e.to_string() })?;
    println!("沙箱目录: {} (自检结束后会清理)", sandbox.display());
    let db_path = sandbox.join("selftest.db");
    let steps = appdir_core::selftest_run(&db_path, &sandbox)?;
    let mut failed = 0usize;
    for s in &steps {
        let mark = if s.passed { "通过" } else { "失败" };
        if !s.passed {
            failed += 1;
        }
        println!("[{mark}] {:<64} {}", s.name, s.detail);
    }
    println!("\n共 {} 项, 失败 {failed} 项", steps.len());

    if !args.flag("keep") {
        let _ = std::fs::remove_dir_all(&sandbox);
    } else {
        println!("沙箱已保留: {}", sandbox.display());
    }
    if failed == 0 {
        println!("真实账本未被本次自检触碰。");
        Ok(0)
    } else {
        Ok(5)
    }
}
