# appdir-provenance

Windows 应用目录溯源与清理工具。监视指定根目录（表层**直接子目录**创建），通过 ETW
记录创建者 EXE 完整路径，写入 SQLite 账本；支持按 EXE 反查、备份+两步删除。

> 本 README 面向**未来要在此项目上继续开发功能的 AI/工程师**：先读完
> 「架构」和「已验证事实/坑」再动手，可以避免踩一遍已经踩过的雷。

## 架构（双 exe + 三个库）

```
crates/
  core/       appdir-core 库：路径规则(filter/path)、SQLite 账本(db/ledger)、
              安全清理(clean: 备份+两步删除)、查询(query)、共享配置(config)
  capture/    事件来源库：
              - etw.rs  ETW 来源（主来源，需管理员）
              - watch.rs ReadDirectoryChangesW 目录出现监听（确认器，无创建者信息）
  daemon/     appdir-core.exe  无窗口常驻后台采集进程（etw -> 账本）
  gui/        appdir-gui.exe  按需 GUI (eframe/egui, 纯 Rust, release 无控制台)
  cli/        appdir.exe      命令行前端（开发/诊断用，功能与 GUI 重叠）
driver/       Minifilter 驱动源码（未编译加载，见 docs/MINIFILTER.md）
deploy/       部署目录：两个 exe + appdir.json + ledger.db + backups/（运行数据，勿删）
```

- **不要改 core/capture 已验证的账本、删除、ETW 逻辑**；加功能优先在 daemon/gui/cli
  层做，或新增文件。
- 所有业务逻辑只在 core/capture；GUI 与 CLI 只是壳，直接调用
  `Db / ExeQuery / clean::*`，不重复实现。

## 路径约定（core 与 GUI 共享）

- 两个 exe 放同一目录；`appdir.json`（含 `ledger_path`）与 `ledger.db` 默认在 exe 同目录。
- GUI 修改账本路径 → 写 `appdir.json`；后台 core 重启后生效。
- 监视根目录列表存在账本 `monitor_roots` 表（不是 JSON）。

## 构建与运行

```powershell
$env:CARGO_HOME='D:\toss pc\rubbish-from-C\.cargo'
$env:RUSTUP_HOME='D:\toss pc\rubbish-from-C\.rustup'
cargo build --release --workspace     # 产物: target\release\{appdir-core,appdir-gui,appdir}.exe
cargo test --workspace                # 全部测试必须通过
```

- `appdir-core.exe` 必须管理员运行（ETW 需要）；`--config <appdir.json>` 可覆盖配置路径。
- 开机自启方案（未安装）：`schtasks /create /tn AppDirCore /sc onlogon /rl highest /tr "<exe 路径>"`。
  不要用 HKCU Run（UAC 下拿不到管理员令牌）。

## 已验证事实 / 大坑（2026-10-18，真机实测，勿凭文档推翻）

### ETW Kernel-File 会话（etw.rs）
1. Provider GUID `{EDD08927-9CC4-4E65-B970-C2560FB5C289}`，走**经典 EventCallback +
   MofData**（`EventRecordCallback` 在本机对内核 provider 的 UserData 为 null）。
2. Create(12) payload（version 0）= `Irp(8) ThreadId(8) FileObject(8) CreateOptions(u32)
   CreateAttributes(u32) ShareAccess(u32) FileName(utf16z 设备路径)`。TDH 在本机查不到
   schema（ERROR_NOT_FOUND），只能按此布局裸解析；解析失败计数丢弃，**绝不猜**。
3. Create 事件不能区分"新建/打开已有目录"，必须由 watch.rs 的非递归
   `FILE_ACTION_ADDED` 确认后才记账（确认窗口 15s）。
4. Kernel-File 事件只带 PID；创建者镜像路径解析顺序：进程句柄 → 进程缓存 → 3s 宽限重试
   → 仍无则丢弃（`no_creator`）。

### 并行 Kernel-Process 会话（方案 1，短命进程归属的关键）
1. Provider GUID `{22FB2CD6-0E7B-422B-A0C7-2FAD1FD0E716}`，同样走 EventCallback + MofData。
2. **EnableTraceEx2 必须 level=5(VERBOSE) + keyword 0x10(WINEVENT_KEYWORD_PROCESS)**。
   实测：keyword=全量会收 ~1.4k 事件/秒（线程等噪音，延迟大到短命进程等不到）；
   level=4 + keyword 0x10 会收到 0 条事件。这两个组合都踩过坑。
3. ProcessStart(Class.Type=1) payload 实测布局：
   `u32 pid, u32 parent?, u32 0, FILETIME(8), u32 session, u32 parent2, ...,
   utf16z 镜像路径(\Device\HarddiskVolumeN\... 或 X:\...)`。
   **payload 里有完整镜像路径**——曾误判"没有路径"，实际是解析器只认盘符格式 + 一个
   大小写比较 bug（`is_word` 把 word 转小写却和 UPPER 常量比较，`'D'` 永远匹配不上）。
   解析用 `extract_proc_fields`（有真机 payload 回归测试）。
4. 两个 ETW 会话间事件到达顺序无保证：文件事件可能先于 ProcessStart 被处理，
   所以有 3 秒宽限期重试（drain 里 grace 列表）。宽限到期仍无归属才 `no_creator` 丢弃。
5. 强杀 core 会残留 `appdir-etw-<pid>` / `appdir-proc-<pid>` ETW 会话，残留会话会干扰
   新会话收事件。调试时用 `logman query -ets | findstr appdir` 检查，
   `logman stop <名字> -ets` 清理。

### 账本 (core/src/db.rs)
- schema v2，`capture_source` + `attribution_confidence` 列，旧库自动迁移。
- `attribution_confidence = etw-create-confirmed`（ETW+监听双确认）。

### 清理 (core/src/clean.rs)
- 默认 dry-run；commit 必须 `commit: true`。删除 = 先整树备份到 `backups/<名>-<时间戳>`
  再删（两步），占用/权限问题会 block（`Candidate.blocked`），被拦截项不可勾选。
- 保护路径通过 `protected_prefixes` 传入（GUI 传账本与配置文件路径）。

### GUI (gui/src/main.rs)
- eframe 0.28（glow, default-features=false）；**默认字体不含 CJK**，启动时必须
  `install_cjk_fonts` 加载 msyh/simhei/simsun（已实现，勿删）。
- release 有 `#![windows_subsystem = "windows"]`（debug 保留控制台）。
- 三个页签：账本总览（按 EXE 聚合→勾选删除）/ 监视目录 / 设置。

## 验证状态基线（改动前先知道哪些是真的）

| 功能 | 级别 |
| --- | --- |
| ETW 采集+短命进程归属（cmd /c mkdir 即退） | ✅ 真机 |
| 批量压测 20 短命进程/210ms，全部记账 ~1s，CPU≈0，私有内存 ~6MB | ✅ 真机 |
| 备份+两步删除 | ✅ 真机 |
| GUI 聚合视图/字体 | ✅ 真机（人工目视） |
| ETW 丢失报告 | 逻辑+计数可见，未注入真实丢失场景 |
| Minifilter | 仅源码，未编译未加载 |
| 开机自启 | 仅方案（docs/GUI-CORE.md），未安装 |

## 性能目标（常驻后台）

appdir-core.exe 空闲私有内存个位数 MB、CPU 接近 0%；事件洪峰（批量建目录）全部记账
秒级完成且 CPU 增量毫秒级。改动 capture 层后务必重跑类似压测
（20 个 `cmd /c mkdir` + 轮询 `query all` 直到全部入库，对比 `Get-Process` CPU 增量）。

## 文档

- `docs/ETW.md` — ETW 实现细节
- `docs/GUI-CORE.md` — 双 exe 路径约定与自启方案
- `docs/SECURITY.md` — 删除防护
- `docs/MINIFILTER.md` — 驱动路线

## 明确不做

- 服务调用链/RPC 归属、进程树追踪（超范围）
- 递归监视（只有根目录的直接子目录）
