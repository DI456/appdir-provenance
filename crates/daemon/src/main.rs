//! appdir-core.exe - 无窗口常驻后台采集进程。
//!
//! 职责: 读配置 JSON -> 打开 SQLite 账本 -> ETW 采集目录创建事件 -> 记账。
//! 管理员权限运行 (ETW 需要); Ctrl-C 或任务计划停止时优雅退出。
//!
//! 用法:
//!   appdir-core [--config <appdir.json 路径>] [--source watch|etw] [--seconds N]

use appdir_capture::{Counters, EtwSource, WatchSource};
use appdir_core::{config, timefmt, winfile, Db, Error, EventSource, IngestOutcome, Ledger, Result};
use std::path::PathBuf;
use std::time::{Duration, Instant};

struct Args {
    config: Option<PathBuf>,
    source: String,
    seconds: u64,
    reg_prefix: Vec<String>,
}

fn parse_args() -> Args {
    let mut a = Args { config: None, source: "etw".into(), seconds: 0, reg_prefix: Vec::new() };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--config" => a.config = it.next().map(PathBuf::from),
            "--source" => {
                if let Some(v) = it.next() {
                    a.source = v;
                }
            }
            "--seconds" => {
                if let Some(v) = it.next() {
                    a.seconds = v.parse().unwrap_or(0);
                }
            }
            "--reg-prefix" => {
                if let Some(v) = it.next() {
                    a.reg_prefix.push(v);
                }
            }
            _ => {}
        }
    }
    a
}

fn schedule_console_hide() {
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(4));
        unsafe {
            use windows::Win32::System::Console::GetConsoleWindow;
            use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};
            let hwnd = GetConsoleWindow();
            if !hwnd.is_invalid() {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        }
    });
}

fn main() {
    schedule_console_hide();
    let args = parse_args();
    let code = match run(&args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("appdir-core: {e}");
            1
        }
    };
    std::process::exit(code);
}

fn run(args: &Args) -> Result<()> {
    let config_path = match &args.config {
        Some(p) => p.clone(),
        None => config::config_path_for_exe(
            &std::env::current_exe().map_err(|e| Error::Config(format!("无法定位自身 exe: {e}")))?,
        ),
    };
    let cfg = config::load_or_create(&config_path)?;
    if args.source == "registry" {
        return run_registry(&cfg.ledger_path, args, Instant::now());
    }
    let db = Db::open(&cfg.ledger_path)?;
    let ledger = Ledger::new(&db)?;
    let enabled: Vec<String> = ledger
        .view
        .roots
        .iter()
        .filter(|r| r.enabled)
        .map(|r| r.path.clone())
        .collect();
    println!(
        "appdir-core 启动: 配置={}  账本={}  source={}",
        config_path.display(),
        cfg.ledger_path.display(),
        args.source
    );
    for r in &enabled {
        println!("  监视: {r}");
    }
    if enabled.is_empty() {
        return Err(Error::Config(
            "没有启用的监视根目录; 请用 appdir-gui 添加监视目录".to_string(),
        ));
    }

    // 配置开启注册表监视时, 附加一条采集线程: 事件 5 秒缓存后按目录账本过滤入库。
    // (修改配置后需重启 appdir-core 才会生效)
    let reg_mode = cfg.reg_mode_effective();
    if reg_mode != "off" {
        let h = spawn_registry_ingest(
            cfg.ledger_path.clone(),
            cfg.reg_prefixes.clone(),
            reg_mode == "instant",
        );
        std::mem::forget(h);
    }

    let started = Instant::now();
    let mut counters = Counters::default();
    match args.source.as_str() {
        "watch" => run_watch(&ledger, &enabled, args.seconds, started, &mut counters),
        "etw" => run_etw(&ledger, &enabled, args.seconds, started, &mut counters),
        other => Err(Error::Config(format!("未知来源: {other} (支持 watch|etw|registry)"))),
    }
}

/// 注册表监视采集线程: 与目录账本联动。
///
/// 事件先进 5 秒缓存; flush 时按 pid 解析当前 exe 路径, 只有已出现在目录账本
/// (applications) 中的 exe 的注册表事件才入账本。这样 "先写注册表后建目录"
/// 的安装器 (如 python.exe) 也能对上账; 其余事件静默丢弃, 不落任何临时文件。
///
/// 性能: 账本中的 exe 每 5 秒一次性读入内存 HashMap, 每条事件只做哈希查找,
/// 热路径没有 SQLite 查询; pid -> exe 的 OpenProcess 每 60 秒才重解析一次。
/// 注册表监视采集线程: 与目录账本联动。
///
/// 只有已出现在目录账本 (applications) 中的 exe 的注册表事件才入账本。
/// * cache   (默认): 事件先进 5 秒缓存, 窗口结束时判定。可覆盖 "注册表先写、
///           目录后建" 的安装器 (如 python.exe)。
/// * instant (高性能): 每条事件立即判定, 未命中账本直接丢弃, 内存几乎为零。
///           代价是目标 exe 必须在目录账本中已经存在。
///
/// 性能: 账本 exe 索引每 5 秒一次性读入内存 HashMap, 热路径只做哈希查找;
/// pid -> exe 的 OpenProcess 每 60 秒才重解析一次; 命中事件每秒批量写库。
fn spawn_registry_ingest(
    ledger_path: PathBuf,
    prefixes: Vec<String>,
    instant: bool,
) -> std::thread::JoinHandle<()> {
    use std::collections::HashMap;
    std::thread::spawn(move || {
        // v1 诊断用的临时文件已废弃, 启动时顺手清理。
        let _ = std::fs::remove_file(ledger_path.with_file_name("registry.jsonl"));
        let mut src = match appdir_capture::RegistrySource::new(prefixes.iter()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("appdir-core: 注册表监视启动失败: {e}");
                return;
            }
        };
        let db = match Db::open(&ledger_path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("appdir-core: 注册表监视打开账本失败: {e}");
                return;
            }
        };
        eprintln!(
            "appdir-core: 注册表监视已启动 (前缀 {} 个, {} 模式, 只记目录账本已知 exe)",
            prefixes.len(),
            if instant { "instant 即时过滤" } else { "cache 5 秒缓存" }
        );
        let mut known: HashMap<String, i64> = HashMap::new();
        refresh_known(&db, &mut known);
        let mut pending: Vec<_> = Vec::new();
        let mut ready: Vec<(i64, appdir_capture::RegistryEvent)> = Vec::new();
        let mut pid_exe: HashMap<u32, Option<String>> = HashMap::new();
        let mut last_refresh = Instant::now();
        let mut last_write = Instant::now();
        let mut last_pid_clear = Instant::now();
        loop {
            pending.extend(src.drain());

            if instant {
                // 即时模式: 每条事件立刻判定, 未命中直接丢弃, 不保留任何缓存。
                for ev in pending.drain(..) {
                    if let Some(app_id) = resolve_app(ev.pid, &mut pid_exe, &known) {
                        ready.push((app_id, ev));
                    }
                }
            } else if last_refresh.elapsed() >= Duration::from_secs(5) {
                // 缓存模式: 5 秒窗口结束时统一判定。
                for ev in pending.drain(..) {
                    if let Some(app_id) = resolve_app(ev.pid, &mut pid_exe, &known) {
                        ready.push((app_id, ev));
                    }
                }
            }

            if last_refresh.elapsed() >= Duration::from_secs(5) {
                last_refresh = Instant::now();
                refresh_known(&db, &mut known);
            }

            if !ready.is_empty()
                && (ready.len() >= 256 || last_write.elapsed() >= Duration::from_secs(1))
            {
                last_write = Instant::now();
                let mut inserted = 0usize;
                for (app_id, ev) in ready.drain(..) {
                    match db.reg_insert(app_id, ev.pid, ev.op, &ev.key, &ev.value, ev.ts_unix_ms) {
                        Ok(true) => inserted += 1,
                        Ok(false) => {}
                        Err(e) => eprintln!("appdir-core: 注册表记账失败: {e}"),
                    }
                }
                if inserted > 0 {
                    eprintln!("appdir-core: 注册表账本新增 {inserted} 条");
                }
            }

            if last_pid_clear.elapsed() >= Duration::from_secs(60) {
                last_pid_clear = Instant::now();
                // 防止 pid 复用后把事件记到已经退出的旧进程 exe 上。
                pid_exe.clear();
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    })
}

/// 从账本全量刷新 exe 索引 (key 统一小写)。
fn refresh_known(db: &Db, known: &mut std::collections::HashMap<String, i64>) {
    match db.known_apps() {
        Ok(rows) => {
            known.clear();
            for (k, id) in rows {
                known.insert(k.to_ascii_lowercase(), id);
            }
        }
        Err(e) => eprintln!("appdir-core: 读取 exe 索引失败: {e}"),
    }
}

/// pid -> 当前 exe -> 账本 app_id。未命中返回 None (事件丢弃)。
fn resolve_app(
    pid: u32,
    pid_exe: &mut std::collections::HashMap<u32, Option<String>>,
    known: &std::collections::HashMap<String, i64>,
) -> Option<i64> {
    let exe = pid_exe.entry(pid).or_insert_with(|| pid_to_exe(pid));
    let exe = exe.as_ref()?;
    let key = exe.to_ascii_lowercase();
    known.get(&key).copied()
}

/// pid -> 当前 exe 路径 (进程已退出或无权限则 None)。
fn pid_to_exe(pid: u32) -> Option<String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            h,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(h);
        if ok.is_ok() && len > 0 {
            Some(String::from_utf16_lossy(&buf[..len as usize]))
        } else {
            None
        }
    }
}

fn should_stop(seconds: u64, started: &Instant) -> bool {
    if seconds != 0 && started.elapsed().as_secs() >= seconds {
        return true;
    }
    winfile::install_ctrl_c_guard() && winfile::ctrl_c_requested()
}

fn run_watch(
    ledger: &Ledger,
    roots: &[String],
    seconds: u64,
    started: Instant,
    counters: &mut Counters,
) -> Result<()> {
    let mut src = WatchSource::new(roots.to_vec());
    src.start().map_err(|e| Error::Unsupported(e.to_string()))?;
    if !src.wait_until_ready(Duration::from_secs(5)) {
        eprintln!("appdir-core: 警告: 目录监听未在 5 秒内就绪");
    }
    loop {
        let mut events = Vec::new();
        src.drain(&mut events).map_err(|e| Error::Unsupported(e.to_string()))?;
        ingest_all(ledger, &events, counters);
        if should_stop(seconds, &started) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    src.shutdown().map_err(|e| Error::Unsupported(e.to_string()))?;
    println!("appdir-core: watch 已停止");
    Ok(())
}

fn run_etw(
    ledger: &Ledger,
    roots: &[String],
    seconds: u64,
    started: Instant,
    counters: &mut Counters,
) -> Result<()> {
    let mut src = EtwSource::new(roots.to_vec()).map_err(|e| Error::Unsupported(e.to_string()))?;
    if let Some(err) = src.start_error() {
        return Err(Error::Unsupported(format!(
            "ETW 启动失败: {err} (appdir-core 必须以管理员身份运行)"
        )));
    }
    if !src.watcher_mut().wait_until_ready(Duration::from_secs(5)) {
        eprintln!("appdir-core: 警告: 目录监听未在 5 秒内就绪, 该时段事件可能无法确认");
    }
    loop {
        let mut events = Vec::new();
        src.drain(&mut events).map_err(|e| Error::Unsupported(e.to_string()))?;
        ingest_all(ledger, &events, counters);
        if should_stop(seconds, &started) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    src.shutdown().map_err(|e| Error::Unsupported(e.to_string()))?;
    println!("appdir-core: etw 诊断: {}  停止错误: {:?}", src.stats().snapshot(), src.stats().stop_errors.lock().unwrap());
    for line in appdir_capture::etw::debug_dump() {
        println!("appdir-core: {line}");
    }
    println!(
        "appdir-core: etw 已停止; counters={counters:?}"
    );
    Ok(())
}

fn ingest_all(ledger: &Ledger, events: &[appdir_core::RawCreateEvent], counters: &mut Counters) {
    for ev in events {
        match ledger.ingest(ev) {
            Ok(out) => {
                Counters::tally(&out, ev, counters);
                println!(
                    "{}  {}  {}  ({})",
                    timefmt::format_local(ev.observed_at_ms),
                    if matches!(out, IngestOutcome::Recorded { .. }) { "记录" } else { out.label() },
                    ev.target_path,
                    if ev.creator.exe_path.is_some() { "已归属" } else { "创建者未知" },
                );
            }
            Err(e) => eprintln!("appdir-core: 记账失败 {}: {e}", ev.target_path),
        }
    }
}

/// 注册表监视 (诊断模式 --source registry): 事件直接打印到 stdout, 不落临时文件。
fn run_registry(ledger_path: &std::path::Path, args: &Args, started: Instant) -> Result<()> {
    let _ = ledger_path;
    let mut src = appdir_capture::RegistrySource::new(args.reg_prefix.iter())
        .map_err(|e| Error::Unsupported(format!("注册表 ETW 会话启动失败: {e}")))?;
    eprintln!(
        "appdir-core: 注册表监视(诊断)已启动, 前缀 {} 个, 事件直接打印",
        args.reg_prefix.len()
    );
    let mut written: u64 = 0;
    loop {
        for ev in src.drain() {
            println!(
                "{}  pid {}  {}  {}\\{}",
                timefmt::format_local(ev.ts_unix_ms),
                ev.pid,
                ev.op,
                ev.key,
                ev.value,
            );
            written += 1;
        }
        if should_stop(args.seconds, &started) {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    src.shutdown();
    eprintln!(
        "appdir-core: 注册表监视已停止, 共收到 {written} 条; {}",
        src.stats().snapshot()
    );
    Ok(())
}
