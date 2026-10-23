# appdir-provenance 架构与功能源码地图（AI 维护版）

> 面向后续接手的 AI：**先在这里定位，再只打开相关文件**，不必通读全部源码。
> 最后更新：2026-10-21。本文以当前源码为准；README.md 的部分描述已过时（见文末「文档差异」）。

---

## 0. 快速定位表

| 想改什么 | 先看这些文件 |
| --- | --- |
| 目录事件怎么采集 | `crates/capture/src/etw.rs`、`crates/capture/src/watch.rs` |
| 哪些目录事件算数（过滤规则） | `crates/core/src/filter.rs`、`crates/core/src/ledger.rs` |
| 创建者(exe)怎么归属 | `crates/capture/src/etw.rs`、`crates/core/src/ledger.rs` |
| 账本表结构 / 增删查 | `crates/core/src/db.rs`、`crates/core/src/types.rs` |
| 按 exe 查询目录 | `crates/core/src/query.rs`、`crates/gui/src/main.rs`、`crates/cli/src/main.rs` |
| 删除 / 备份 / 安全拦截 | `crates/core/src/clean.rs`、`crates/core/src/winfile.rs` |
| 注册表事件解析 | `crates/capture/src/registry.rs` |
| 注册表事件归属与入库 | `crates/daemon/src/main.rs`（`spawn_registry_ingest`） |
| 注册表账本读写 / 失效检查 | `crates/core/src/db.rs`（`reg_*`）、`crates/core/src/regcheck.rs` |
| 注册表监视开关 / 三态 | `crates/core/src/config.rs`、`crates/gui/src/main.rs`（`ui_settings`） |
| 后台进程入口 | `crates/daemon/src/main.rs` |
| GUI 全部界面 | `crates/gui/src/main.rs` |
| 符号链接页签 (创建/移动并创建/历史记录) | `crates/gui/src/symlink_tab.rs` (依赖 rfd 文件对话框, `crates/gui/Cargo.toml`; 记录存 `symlink_records` 表, `crates/core/src/db.rs`) |
| 命令行 | `crates/cli/src/main.rs` |
| 配置读写 / 路径约定 | `crates/core/src/config.rs` |
| 开机自启 | `crates/gui/src/main.rs` 内联 `mod startup` |
| 自检流程 | `crates/core/src/selftest.rs` |
| ETW/清理的安全背景知识 | `docs/ETW.md`、`docs/GUI-CORE.md`、`docs/SECURITY.md`、`docs/MINIFILTER.md` |

---

## 1. 总体架构

```
crates/
  core/     业务库 appdir-core：路径/过滤/账本/查询/清理/配置/注册表检查
  capture/  事件源库 appdir-capture：etw.rs / watch.rs / registry.rs
  daemon/   appdir-core.exe   无窗口后台采集（ETW + 可选注册表监视）→ SQLite
  gui/      appdir-gui.exe    按需界面（eframe/egui，release 无控制台）
  cli/      appdir.exe        命令行（开发/诊断/自检）
```

- **所有业务逻辑只在 `core` 与 `capture`**；daemon/gui/cli 只是壳，直接调用 `Db / Ledger / ExeQuery / clean::*`。加功能优先在壳层或新文件，不要复制业务逻辑。
- 数据链路只有一条主干：**事件源 → RawCreateEvent → Ledger::ingest → SQLite**。
- 双 exe 部署：`deploy/` 下放 `appdir-core.exe` + `appdir-gui.exe` + `appdir.json` + `ledger.db`（含 `-wal`/`-shm`，不可拆删）。
- CLI 的数据目录约定独立：默认 `%LOCALAPPDATA%\appdir-provenance`，可用 `APPDIR_DATA_DIR` 或 `--data-dir` 覆盖，**不是** deploy 目录。

---

## 2. 目录采集主链路（目录账本）

### 2.1 事件源

| 文件 | 职责 | 关键接口 |
| --- | --- | --- |
| `crates/capture/src/etw.rs` | **主来源**。ETW 实时会话监听 `Microsoft-Windows-Kernel-File` 的 Create(12)，同时开 `Microsoft-Windows-Kernel-Process` 解析 pid→exe | `EtwSource::new`、`stats()`、`start_error()`、`watcher_mut()`、`drain()`、`shutdown()`；`EtwStats::snapshot`；`debug_dump()` |
| `crates/capture/src/watch.rs` | **确认器**。非递归 `ReadDirectoryChangesW`，只上报 `FILE_ACTION_ADDED`，用于把 ETW 的“打开已有目录”误报过滤掉 | `WatchSource::new/start/wait_until_ready/is_ready/drain`；`names_seen`、`resync_count`、`failures`、`statuses` |
| `crates/capture/src/lib.rs` | 导出事件源、`Counters`（daemon 诊断计数） | `EtwSource`、`WatchSource`、`RegistrySource`、`RegistryEvent`、`RegStats`、`Counters` |

ETW 关键事实（改动前必读，已在真机验证）：
- Kernel-File GUID `{EDD08927-9CC4-4E65-B970-C2560FB5C289}`，经典 `EVENT_TRACE_LOGFILEW + EventCallback + MofData` 管线。
- Create(12) version 0 payload：`Irp(8) ThreadId(8) FileObject(8) CreateOptions(u32) CreateAttributes(u32) ShareAccess(u32) ... 文件名`；布局不符就计数丢弃，**绝不猜**。
- Create 事件无法区分“新建/打开已有目录”→ 必须由 watch.rs 确认（确认窗口 15 秒）。
- Kernel-Process GUID `{22FB2CD6-0E7B-422B-A0C7-2FAD1FD0E716}`；`EnableTraceEx2` 必须 `level=5(VERBOSE)` + `keyword=0x10(WINEVENT_KEYWORD_PROCESS)`。
- 两个会话顺序无保证，所以有 3 秒宽限重试；进程缓存 + 句柄解析都失败才丢弃。
- 会话名带 pid：`appdir-etw-<pid>`、`appdir-proc-<pid>`；强杀会残留，用 `logman query -ets | findstr appdir` 检查。

### 2.2 过滤与记账

| 文件 | 职责 | 关键接口 |
| --- | --- | --- |
| `crates/core/src/filter.rs` | 只接受“监视根下的**直接子目录**新建”；定义 NtCreateFile 请求语义 | `CreateRequest::{dir_create,dir_open,...}`、`MonitorView::from_roots/matching_root`、`admit(ev, view) -> Verdict`，常量 `FILE_CREATE/FILE_DIRECTORY_FILE/...` |
| `crates/core/src/ledger.rs` | 记账编排：过滤 → 去重/重注册 → 写库；状态刷新 | `Ledger::new/with_view/ingest/reconcile`、`IngestOutcome`（`Recorded/Reregistered/Duplicate/Ignored`）、`raw()`（构造原始事件，测试用） |
| `crates/core/src/types.rs` | 共享数据模型 | `RawCreateEvent`、`ProcessIdentity{pid,exe_path}`、`DirCreateRecord`、`MonitorRoot`、`RecordStatus`、`SourceKind`、`Attribution` |
| `crates/core/src/source.rs` | 事件源抽象 | `EventSource` trait（`kind/attribution/drain/link_state/shutdown`）、`LinkState`、`SourceError` |
| `crates/core/src/path.rs` | Windows 路径规范化/比较/设备路径转换 | `normalize`、`WinPath::parse/key/parent/child`、`strip_extended`、`is_direct_child`、`is_within`、`same_path`、`device_to_dos` |
| `crates/core/src/db.rs` | SQLite 账本（唯一持久层）；SCHEMA_VERSION=3 | `Db::open/in_memory`、`add_root/remove_root/set_root_enabled/roots`、`insert_or_reregister`、`dirs_of_exe`、`all_dirs`、`set_status`、`log_ignored`、`ignored_counts/recent_ignored`、`reg_*`、`known_apps`、`add_symlink_record/symlink_records/delete_symlink_records`（GUI 符号链接页签用，表 `symlink_records`：link_path UNIQUE + target_path + created_at_ms） |

记账要点：
- 去重键是 `directories.dir_key`（规范化小写路径），重复出现走 `insert_or_reregister`。
- 未归属/被过滤事件写 `ignored_events`（`db.log_ignored`），可用 `appdir query ignored` 查看。
- `Ledger::reconcile` 按文件系统现状更新 `record_status`（`Present/Missing`）。

### 2.3 后台进程

`crates/daemon/src/main.rs`：

- `main()`：`schedule_console_hide()`（4 秒后隐藏控制台）→ `parse_args` → `run`。
- `run()`：定位 `appdir.json` → 读配置 → 打开账本 → 读启用中的监视根 → 按 `--source` 分发。
- 参数：`--config <path>`、`--source watch|etw|registry`、`--seconds N`、`--reg-prefix <前缀>`（可重复）。
- `run_watch` / `run_etw`：循环 `src.drain()` → `ingest_all` → 100ms 睡眠；Ctrl-C 或 `--seconds` 到期退出。
- `ingest_all`：`ledger.ingest` + `Counters::tally` + 控制台行。
- 注册表监视：配置开启时在 `run()` 里 `spawn_registry_ingest(...)`，**与目录采集并行**。
- `run_registry`：仅 `--source registry` 诊断用，事件直接打印 stdout，**不落盘**。
- 空监视根时退出并提示用 GUI 添加。

---

## 3. 注册表监视与注册表账本

### 3.1 采集（`crates/capture/src/registry.rs`）

- Provider：`Microsoft-Windows-Kernel-Registry` GUID `{70EB4F03-C1DE-4F73-A051-33D13D5413BD}`，会话名 `appdir-reg`。
- 经典回调实测：`Class.Type = manifest 事件 ID + 32`。
  - `1+32` KcbCreate：KCB(8) + 变长头 + UTF-16 键名；维护 `KCB → 键路径`。
  - `2+32` KcbDelete：从映射移除。
  - `6+32` SetValue：`KCB(8)+Status(4)+Type(4)+DataSize(4)+填充(2)`，**值名在偏移 22**（写 24 会丢首字母）。
  - `7+32` DeleteValue。
- 键路径归一化为 `HKLM\...` / `HKU\...`；`KCB` 解析不到的事件丢弃并计数（冷启动前已存在的 KCB，约 15% 原理性损耗）。
- 输出 `RegistryEvent { ts_unix_ms, pid, op("set"|"delval"), key, value }`。
- **注意：文件顶部模块注释是 v1 时写的，已过时**。现在事件不写 `registry.jsonl`，由 daemon 归属后写 SQLite；daemon 启动时还会删除历史 `registry.jsonl` 临时文件。

### 3.2 归属与入库（`crates/daemon/src/main.rs`）

`spawn_registry_ingest(ledger_path, prefixes, instant)`：

1. 启动时删除账本同目录的旧 `registry.jsonl`。
2. `refresh_known()`：每 5 秒一次性 `Db::known_apps()` 读全部 `exe_key → app_id` 到内存 `HashMap`。**热路径不做 SQLite 查询。**
3. `pid_to_exe(pid)`：`OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` + `QueryFullProcessImageNameW`；每个 pid 缓存结果，每 60 秒清空一次（防 pid 复用误归属）。
4. `resolve_app()`：pid → exe → `known` 哈希查找；未命中返回 None（丢弃）。
5. 两种模式：
   - **cache**：事件进 5 秒缓存，窗口结束时统一判定。可覆盖“注册表先写、目录后建”的安装器。
   - **instant**：每条事件立即判定，未命中直接丢，内存几乎为零；不覆盖目标 exe 尚未建目录的场景。
6. 命中事件进入 `ready`，每 1 秒或满 256 条批量 `Db::reg_insert`；同 `app+op+key+value` 去重。

配置字段：`reg_mode` 三态；旧字段 `registry_monitor` 仅作兼容。**修改后需重启 appdir-core.exe。**

### 3.3 账本 API 与失效检查

`crates/core/src/db.rs`：

- `known_apps()`：`applications` 全量 `(exe_key, app_id)`。
- `reg_insert(app_id,pid,op,key,value,at_ms)`：`INSERT OR IGNORE`。
- `reg_apps()`：按 exe 聚合（供 GUI 列表）。
- `reg_events_of_app(app_id)`：明细。
- `reg_delete_ids(&[id])` / `reg_delete_app(app_id)`：删除账本记录（不动目录账本）。
- `reg_known_app(exe_path)`：单条查询（保留给其他调用方）。

`crates/core/src/regcheck.rs`：

- `value_exists(key_path, value)`：按 `HKLM/HKU/HKCU/\REGISTRY\...` 打开键并查询值是否存在。
- GUI 打开某 exe 明细时，对 `op="set"` 且值已不存在的条目**静默删除**；`delval` 无存在性语义，不参与清理。

### 3.4 GUI「注册表账本」页（`crates/gui/src/main.rs` 末尾）

- `Tab::RegLedger`；`ui_reg_ledger` 分派列表/明细。
- `reload_reg_apps` / `open_reg_detail` / `reload_reg_rows` / `ui_reg_list` / `ui_reg_detail`。
- 明细支持单条「删除」与「清空该程序记录」；打开明细时自动静默清理失效条目。

---

## 4. 账本 schema（`crates/core/src/db.rs` 顶部 DDL）

`SCHEMA_VERSION = 2`；`CREATE TABLE IF NOT EXISTS` 保证新表自动补建；`migrate()` 只为旧库补 `directories.capture_source/confidence`。

| 表 | 列（重要） |
| --- | --- |
| `meta` | `key, value`（`schema_version`） |
| `monitor_roots` | `root_id, root_path UNIQUE, root_key UNIQUE, enabled, added_at_ms` |
| `applications` | `app_id, exe_path UNIQUE, exe_key UNIQUE, exe_name, first_seen_ms, last_seen_ms` |
| `directories` | `directory_id, app_id FK, directory_path UNIQUE, dir_key UNIQUE, monitor_root, creator_pid, capture_source, confidence, created_at_ms, updated_at_ms, record_status` + 索引 `idx_directories_app/status`、`idx_applications_name` |
| `ignored_events` | `ignored_id, observed_at_ms, reason, directory_path, creator_pid, exe_path` + `idx_ignored_reason` |
| `registry_events` | `reg_id, app_id FK ON DELETE CASCADE, pid, op, key_path, value_name, event_at_ms`；`idx_reg_app`；唯一索引 `(app_id, op, key_path, value_name)` 去重 |

- 账本用 WAL（`ledger.db-wal/-shm` 是运行数据，不要单独删除）。
- `SourceKind` 对应 `capture_source`：`sim/watch/etw/minifilter`；`confidence` 默认值见 `types.rs`。

---

## 5. 查询与清理

### 5.1 查询（`crates/core/src/query.rs`）

- `ExeQuery::run(db, exe_path, include_stale)`：某 exe 创建的目录；`QueryItem::run(db, dir_path)`：某目录的归属。
- `found()` 等辅助方法。GUI/CLI 都调用它，不重复 SQL。

### 5.2 清理（`crates/core/src/clean.rs` + `winfile.rs`）

安全模型：**默认 dry-run，显式 commit 才删；先整树备份，再两步删除。**

| 函数 | 作用 |
| --- | --- |
| `read_identity(dir)` | 读创建时间/属性（Windows FILETIME 换算） |
| `verify(db, path, expect_exe, protected)` | 生成 `Candidate`：校验存在性、归属、受保护路径、创建时间容差（5 分钟）、reparse point |
| `plan_for_exe(db, exe, protected)` | 某 exe 的全部可删候选 |
| `backup_tree(src, dst)` | 递归复制到 `backups/<名>-<时间戳>`，返回文件数 |
| `clean_one(...)` | 单个目录：备份 → 隔离重命名（`QUARANTINE_MARKER = ".appdir-todelete-"`）→ 删除 |
| `quarantine(target, stamp)` | 就地重命名隔离 |
| `execute(db, candidates, opts, now_ms)` | 批量执行，产出 `CleanReport` |
| `remove_tree(target)` | 递归删除（处理只读/权限） |
| `BlockReason` | 不可删原因：路径保护、身份不符、时间不符、reparse point、占用/权限等；`code()/explain()` 给 UI |

`CleanOptions<'a> { backup_dir: Option<&Path>, commit: bool, protected_prefixes: Vec<DbPath> }`；`dry_run()` 默认 `commit=false`。
GUI 传入的受保护路径包括账本文件与配置文件；`backup_dir` 为 `None` 表示不备份。

---

## 6. GUI（`crates/gui/src/main.rs`）

- 入口 `main()`；`#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`（release 无控制台）。
- `App::new`：加载配置/打开账本/加载 CJK 字体（`install_cjk_fonts`，**勿删**，默认字体不含中文）。
- 五个页签：`Tab::{Ledger, Roots, Settings, RegLedger, Symlink}`。
  - **账本总览**：顶部 `ui_view_switch` 切换 `ledger_view: LedgerView::{ByApp, ByRoot}`。`reload_apps` 一次性聚合出 `apps`（按 exe）与 `root_groups`（按 `monitor_root`，含 0 条记录的已配置根），每组带 `dir_count` 与 `stale_count`（`record_status=missing` 的孤儿记录）。**总数只统计仍存在的目录，失效记录单独标注**。按程序：`open_app` → `ui_app_detail`；按目录：`open_root` → `ui_root_detail`（每行标注创建者 exe）；两者共用 `ui_dir_detail`。已失效的条目仍然展示，但只能勾选「只删除账本记录」（`Db::delete_dir_records`），不能删文件；`delete_checked` 同时处理“删目录”（`clean::execute`）与“删记录”，按钮文案随选择变化。
  - **监视目录**：`ui_roots` → `Db::add_root/remove_root/set_root_enabled`。
  - **设置**：`ui_settings`——账本路径（写 `appdir.json`，提示重启 core）、删除前备份开关、注册表监视三态（`off/cache/instant`，`AppConfig::set_reg_mode`）、开机自启。
  - **注册表账本**：见 §3.4。
  - **符号链接**：见 §6.1。
- 顶部「刷新」同时刷新目录账本、监视根、注册表账本；底部状态栏 `self.status`。
- `reopen_db()`：改账本路径后重新打开连接；`App.db` 是 `Option<Arc<Db>>`（符号链接页签的后台线程也要持有一份 Arc）。

### 6.1 符号链接页签（`crates/gui/src/symlink_tab.rs` + `unlocker.rs`）

整合自外部项目 SymlinkCreator（多源→单目标→每源一个链接）与 UnlockDelete（Restart Manager 解锁）的核心语义，壳层实现，不动 core 业务。

**结构**：页签顶部二级切换 `View::{Operate, History}`；`SymlinkState` 由 `App` 持有，`poll()` 在每帧被调用（`update()` 里 busy 时 80ms 重绘一次）。

**操作页**：
- 待处理列表：「添加文件…/添加文件夹…」用 `rfd` 原生对话框（依赖 `crates/gui/Cargo.toml`），可分多次、混合；目标 B 可手输（去引号/空格）或弹窗选择。
- 「创建」：在 B 下一级为每源建同名链接（目录 `symlink_dir`，文件 `symlink_file`），原件不动。
- 「移动并创建」：先 move（同盘 rename；跨盘 copy+delete）到 B 下一级，再在原位建指向新位置的链接。
- 相对目标：同盘用相对路径，跨盘自动回退绝对路径（对齐 SymlinkPlanner 的 `SamePathRoot` 行为）。
- 所有文件操作在后台线程执行，经 `mpsc` 发 `Msg::{Progress, Info, Item, Aborted, Done}`，UI 轮询显示进度条；支持按条取消。
- 成功创建写入 `symlink_records` 表（`Db::add_symlink_record`，link_path UNIQUE，重复创建覆盖）。

**占用解锁（`unlocker.rs`，默认开关开，仅移动模式生效）**：
- `find_locking_processes`：rstrtmgr.dll 的 Restart Manager FFI（`RmStartSession/RmRegisterResources/RmGetList`）；目录先展开成自身+内部文件列表（上限 400）再注册。
- `kill_process`：`OpenProcess(PROCESS_TERMINATE)+TerminateProcess`，跳过自身 pid。
- 流程：rename 失败 → 枚举占用者 → 逐个结束 → 等 500ms → 重试一次。

**失败语义（已与用户确认）**：
| 情况 | 行为 |
| --- | --- |
| 解锁重试仍失败 / 链接建不上且移回失败 | 中止整批 → 逆序回滚已移动条目（删原位链接→文件搬回→清账本记录）→ 弹窗「⚠ 错误」逐条列出 |
| 移动前权限探测失败（`probe_privilege`，未做任何移动） | 弹窗报错 |
| 目标 B 已有同名项 / 源不存在 / 同批重名 / 源在目标内部 / 文件名无效 | 预校验红字列出，跳过该条，其余继续 |
| 「创建」模式单条失败 | 红字该条，其余继续（无移动，无回滚） |
| 发现占用进程 | 直接结束（过程写结果区），不弹确认 |

**历史记录子页**：列出 `symlink_records`（链接位置/本体位置/创建时间/当前状态备注）；可勾选「删除选中」——`remove_symlink` 用 `symlink_metadata` 校验确实是符号链接后只删链接本身（`remove_dir` 失败回退 `remove_file`，不递归），并 `Db::delete_symlink_records` 清记录。

### 开机自启（`mod startup`，内联在 `gui/src/main.rs`）

- `is_installed()`：查计划任务 `AppDirCore` 或 HKCU Run 项。
- `set_enabled(true)`：优先 `schtasks /create /tn AppDirCore /sc onlogon /rl highest /f`；失败回退 `HKCU\...\Run`。
- `set_enabled(false)`：两种都清。
- 注册表回退在 UAC 下可能拿不到管理员令牌（ETW 可能失败），UI 有提示。

---

## 7. CLI（`crates/cli/src/main.rs`）

`USAGE` 常量即完整命令表。分发在 `main()` 的 `match`，各 `cmd_*` 函数：

| 命令 | 函数 |
| --- | --- |
| `monitor add/list/remove/enable/disable` | `cmd_monitor` |
| `query exe/directory/all/ignored` | `cmd_query` |
| `clean --exe|--directory [--dry-run --backup --yes --max N]` | `cmd_clean` |
| `daemon --source watch|etw [--root] [--seconds]` | `cmd_daemon` |
| `status` | `cmd_status` |
| `reconcile` | `cmd_reconcile` |
| `selftest [--keep]` | `cmd_selftest` |
| `version` / `help` | 内联 |

- 所有破坏性操作默认 dry-run，必须显式 `--yes`。
- 数据目录：`--data-dir` / `APPDIR_DATA_DIR` / 默认 `%LOCALAPPDATA%\appdir-provenance`。
- `Args` 是手写解析器（`positional/flags/opts`），不依赖 clap。

---

## 8. 配置（`crates/core/src/config.rs`）

`appdir.json`（与 exe 同目录；core 可用 `--config` 覆盖；GUI 始终读写 exe 同目录）：

| 字段 | 默认 | 说明 |
| --- | --- | --- |
| `version` | 1 | 必须为 `CONFIG_VERSION` |
| `ledger_path` | 同目录 `ledger.db` | GUI 修改后重启 core 生效 |
| `backup` | true | 删除前备份 |
| `registry_monitor` | false | **旧字段**，仅兼容；`reg_mode` 为空时回退到它（true→cache） |
| `reg_mode` | `"off"` | `off` / `cache` / `instant`；修改后重启 core |
| `reg_prefixes` | `[]` | 注册表键路径小写前缀过滤，空=全收 |

关键函数：`config_path_for_exe`、`default_ledger_path`、`load_or_create`、`save`、`AppConfig::reg_mode_effective`、`AppConfig::set_reg_mode`（同步旧字段）。

> 注意：Windows PowerShell 写 JSON 默认带 BOM，`serde_json` 会报 `expected value at line 1 column 1`。改配置请用无 BOM UTF-8。

---

## 9. 支撑模块

| 文件 | 作用 |
| --- | --- |
| `core/src/timefmt.rs` | 不依赖第三方库的 UTC 毫秒 ↔ 本地时间格式化（`now_ms`、`format_local`、`days_from_civil`/`civil_from_days`） |
| `core/src/winfile.rs` | Win32 文件操作辅助：Ctrl-C guard（`install_ctrl_c_guard`/`ctrl_c_requested`）、文件身份/属性、删除辅助 |
| `core/src/error.rs` | `Error` 枚举（`Config/Sqlite/Unsupported/InvalidPath` 等）与 `Result` |
| `core/src/selftest.rs` | 沙箱全流程自检（“创建→采集→记账→查询→清理”），不碰用户数据 |
| `core/src/lib.rs` | 模块导出；新增对外类型要在这里 `pub use` |

---

## 10. 构建、测试与部署

```powershell
cargo build --release --workspace
cargo test --workspace
```

- 产物：`target\release\{appdir-core.exe, appdir-gui.exe, appdir.exe}`。
- **部署约定：构建后把 `appdir-core.exe` / `appdir-gui.exe` 复制到 `deploy/` 再实测**（正在运行会锁文件，先停进程）。
- `deploy/` 是实机运行目录，不是构建产物目录；`target/` 可以随便删。
- 测试注册表链路：
  1. 配置 `reg_mode:"cache"` 或 `"instant"`。
  2. `deploy\appdir-core.exe --source etw --seconds 14`（控制台 4 秒后自动隐藏；诊断输出走 stderr，可重定向）。
  3. 用 SQLite 查 `registry_events`；系统若无 `python`，本项目机器上可用 `D:\Download\codexproject\etw-venv\Scripts\python.exe`。
- 注册表/ETW 会话残留：`logman query -ets | findstr appdir` → `logman stop <名字> -ets`。

---

## 11. 已验证的事实与坑（改动前先看）

1. **ETW 经典回调**：对内核 provider 必须用 `EventCallback + MofData`；`EventRecordCallback` 的 UserData 为 null（本机实测）。
2. **Kernel-Process**：`level=5 + keyword=0x10` 才有进程事件；`level=4 + keyword=0x10` 为 0 条；全量 keyword 噪音约 1.4k/s。
3. **事件顺序无保证**：文件事件可能先于 ProcessStart，3 秒宽限重试是必要的。
4. **watch 确认不可省**：ETW Create 无法区分新建/打开；非递归 `FILE_ACTION_ADDED` 是唯一确认来源。
5. **短命进程归属**：靠并行 Kernel-Process 会话 + 3 秒宽限；强杀 core 会残留会话，干扰下一次采集。
6. **注册表**：值名偏移是 22 不是 24；KCB 未解析约 15% 属原理性损耗；事件量全系统约数百~数千/秒，生产建议用 `reg_prefixes`。
7. **注册表模式**：`cache` 覆盖“先注册表后目录”；`instant` 内存最小但要求 exe 已在目录账本中。热路径已改为内存 HashMap，不逐条查 SQLite。
8. **PID 缓存**每 60 秒清空，防 pid 复用；不要加入永久缓存。
9. **删除安全**：默认 dry-run；备份+隔离+删除；reparse point / 时间不符 / 路径受保护都会被 block；`protected` 必须包含账本与配置文件。
10. **CJK**：egui 默认字体无中文，`install_cjk_fonts` 不能删。
11. **配置 BOM** 问题（见 §8）。
12. **deploy 的 WAL 三件套**（`ledger.db/-wal/-shm`）是一体的，不能拆删。
13. **GUI 下拉/中文**：release 无控制台，调试用 debug 构建；GUI 关闭后再覆盖 exe。

---

## 12. 已知限制 / 待办

- 注册表 `delval`（删除值）当前仍入库。它没有“痕迹”语义、无法做存在性清理；是否入库前丢弃**待产品确认**。
- 注册表冷启动 KCB 预热会丢约 15% 的键名解析（原理性）。
- 注册表监视是全系统 ETW；`reg_prefixes` 目前只有配置文件入口，GUI 未提供编辑。
- GUI 打开注册表明细时逐条 `RegOpenKeyExW` 检查失效，条目上千时会有可感知卡顿；量大可改为后台批量。
- `docs/MINIFILTER.md` 描述的是未实现的内核方案；当前版本全用户态。
- README.md 提到的 `driver/` 目录**当前不存在**（仅 `docs/MINIFILTER.md`）；README 的“三个页签”也已过时（现有四个，含注册表账本）。

---

## 13. 文档差异备忘

- `README.md`：架构部分仍基本准确；GUI 页签、注册表、构建环境描述可能落后于代码。
- `docs/ETW.md`：ETW 布局与坑，详细但以 `etw.rs` 内注释为准。
- `docs/GUI-CORE.md`：GUI 与 core 的职责边界。
- `docs/SECURITY.md`：清理安全模型。
- `docs/MINIFILTER.md`：未来方案，未启用。

---

## 14. 推荐修改流程

1. 在 §0 / §2–§7 找到对应文件与调用链。
2. 业务改动放 `core`/`capture`；GUI/CLI 只做调用与展示。
3. 改账本 schema：`db.rs` DDL + `migrate()` + 结构体 + 所有 SQL；旧库必须能直接打开。
4. 改完 `cargo test --workspace`，再按 §10 复制到 `deploy` 实测。
5. 若改动涉及 ETW/注册表事件布局，先读 `docs/ETW.md` 和对应 `.rs` 顶部注释，**不要凭猜测改偏移**。
