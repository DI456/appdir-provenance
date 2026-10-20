# appdir-core.exe + appdir-gui.exe 双进程架构

## 路径约定（两 exe 共享）
- 两个 exe 放同一目录。
- `appdir.json`（配置）与 `ledger.db`（账本）默认在 exe 同目录。
- `appdir-core.exe` 启动时读 `appdir.json` 中的 `ledger_path`；GUI 修改后保存同一路径，core 重启后生效。

## appdir-core.exe（后台常驻）
- 无窗口，控制台可留作日志；以管理员身份运行（ETW 需要）。
- 启动：`appdir-core.exe [--config <appdir.json>] [--source watch|etw] [--seconds N]`
- 空闲时私有内存约 3 MB，CPU 接近 0%。
- 停止：Ctrl-C 或结束进程（守护循环内优雅退出）。

## appdir-gui.exe（按需）
- 普通权限即可（删除受保护目录时需相应权限）。
- 三个页签：监视目录 / 设置 / 查询-清理。
- 删除流程：按 EXE 查询 → 勾选（默认全选，被拦截项不可勾）→ 确认框输入 `DELETE` → 备份到 `backups\` 后两步删除。
- GUI 私有内存约 150 MB（egui 渲染栈），仅按需打开，不影响常驻内存目标。

## 开机自启（方案，未自动安装）
任务计划（推荐，开机登录后以最高权限启动后台核心）：
```
schtasks /create /tn AppDirCore /sc onlogon /rl highest /tr "D:\path\to\appdir-core.exe"
```
注册表方案（当前用户，无提权参数）：
```
reg add HKCU\Software\Microsoft\Windows\CurrentVersion\Run /v AppDirCore /t REG_SZ /d "D:\path\to\appdir-core.exe"
```
注意：ETW 需要管理员权限，注册表 Run 方式在 UAC 下可能拿不到管理员令牌，故推荐 schtasks + /rl highest。
