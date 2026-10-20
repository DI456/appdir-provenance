# ETW 采集来源：实现、实测布局与能力边界

来源名 `etw`（`appdir daemon --source etw`）。真机验证平台：Windows x64（本机，2026-10-18），管理员权限。

## 架构：ETW 提供归属，监听器提供创建确认

Microsoft-Windows-Kernel-File 的 Create 事件**不携带**创建结果状态（本机 schema 为
`Irp / ThreadId / FileObject / CreateOptions / CreateAttributes / ShareAccess / FileName`，
没有 disposition/status 字段），无法单靠 ETW 区分"新建目录"与"打开已有目录"。

因此本来源从不单独相信 ETW 事件：

1. ETW 事件 → 提取目标路径 + PID → 立即解析进程完整映像路径（事件发生时进程必然存活）；
2. 非递归 ReadDirectoryChangesW 监听器确认"该路径确实作为直接子目录**新出现**"
   （`FILE_ACTION_ADDED`，只对真实新建的目录触发）；
3. 两者配对成功才生成账本记录（`etw-create-confirmed`）；配不上的一律丢弃并计数
   （`unconfirmed-expired`），15 秒确认窗口。

由此天然排除：打开已有目录、文件创建、深层目录、重命名进入的对象。**不做**进程树
追踪，不推测服务背后的原始请求者——`System`（PID 4）等进程的事件若被确认，将如实
记录为 PID 4；无法解析完整映像路径的候选直接丢弃（`no-creator`）。

## 实测事件形态（本机验证，代码按此实现，不匹配即跳过）

- 提供者 GUID：`EDD08927-9CC4-4E65-B970-C2560FB5C289`，仅启用关键字
  `FILENAME(0x10) | CREATE(0x80)`。
- **消费路径必须用经典 `EventCallback`（`EVENT_TRACE.MofData`）**：
  在本机上，`EventRecordCallback` 路径对该提供者给出的 `EVENT_RECORD.UserData`
  为空指针（仅 22 字节长度、无数据），而经典回调携带完整载荷。这是本次联调发现并
  用三个探针实验定位的关键事实。
- 带名字的 Create 事件：`Version=1`，载荷 = 32 字节头（`Irp u64`、`ThreadId u64`、
  `FileObject u32`、`CreateOptions u32`、`CreateAttributes u32`、`ShareAccess u32`）
  + 以 `\0` 结尾的 UTF-16 设备路径（`\Device\HarddiskVolumeX\...`，经 DOS 设备映射
  转换为盘符路径）。
- `Version=0`（20 字节无名字）与其他任何不匹配布局的事件：计入 `解析失败`，跳过。
- TDH 属性 API（`TdhGetPropertySize/TdhGetProperty`，按名与按索引）在本机上对该
  提供者一律返回 `ERROR_INVALID_DATA/ERROR_INVALID_PARAMETER`，因此不依赖 TDH。

实测数字（一次 25 秒真机运行）：kernel-file 事件 1784，带名字 Create 事件 1763，
其中 2 个与真实新建目录配对并成功记账（创建者 = 实际执行创建的 pwsh.exe 完整路径），
153 个候选因不属于确认范围被诚实丢弃，无事件丢失。

## 能力边界（务必阅读）

- **不保证历史完整**：只记录 ETW 会话运行期间的新建目录；会话停止/崩溃期间的事件丢失。
  会话停止时读取 `EventsLost/RealTimeBuffersLost`，非零时明确报告
  "采集不完整"，不会宣称记录完整。
- **15 秒确认窗口**：ETW 事件先到、监听确认后到；超过窗口未配对的候选被丢弃。
  监听器缓冲溢出（resync）同样计入丢失报告。
- **即时映像解析**：进程在创建后立即退出不影响归属（回调内同步解析）；
  但若 PID 复用发生在同一毫秒窗口内（理论可能），可能错误解析——概率极低，
  且路径+PID+时间都来自同一事件。
- **权限**：内核文件提供者要求管理员权限；无权限时明确报错（`StartTraceW` 失败码），
  不降级、不猜测。
- **性能**：仅启用 CREATE+FILENAME 关键字；回调内只做字符串解析与一次 `OpenProcess`
  查询，不做任何阻塞文件 I/O。25 秒实测 1763 个事件无缓冲丢失。
- **不做**：进程树/服务调用链归因、全盘扫描、对已存在目录的历史回溯。

## 诊断

`daemon` 结束时输出统计：
`kernel-file事件 / create事件 / 目录候选 / 解析失败 / 确认成功 / 未确认过期 / 无创建者 / 监听器可见 / 丢失报告`。
`丢失报告 > 0` 即表示本次记录不完整。
