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
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

struct Args {
    config: Option<PathBuf>,
    source: String,
    seconds: u64,
}

fn parse_args() -> Args {
    let mut a = Args { config: None, source: "etw".into(), seconds: 0 };
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

    let started = Instant::now();
    let mut counters = Counters::default();
    match args.source.as_str() {
        "watch" => run_watch(&ledger, &enabled, args.seconds, started, &mut counters),
        "etw" => run_etw(&ledger, &enabled, args.seconds, started, &mut counters),
        other => Err(Error::Config(format!("未知来源: {other} (支持 watch|etw)"))),
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
