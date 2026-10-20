# Minifilter 驱动：构建、签名、加载验证清单

> **状态：仅源码规划，未编译、未签名、未加载、未验证。** 本轮不作为阻塞条件。
> 等待独立的驱动测试环境（见文末）就绪后再执行本清单。

驱动源码位于 `driver/`（规划位置）。目标：Minifilter 捕获监视根的直接子目录创建事件，
通过 FilterManager 通信端口把 `(目录路径, PID, 进程映像路径, 结果)` 推给用户态服务。
用户态 `crates/capture` 的 `EventSource` 接缝已为此预留（`SourceKind::Minifilter`、
线格式见 `crates/capture/src/wire.rs`，`feed` 来源即按该格式回放测试）。

## 前置环境

1. 安装 Windows Driver Kit（WDK，与已装 Windows SDK/VS 版本匹配）。
2. 建议使用**虚拟机**（快照可回滚）作为加载测试机，不要在日常工作机上先加载：
   - VM：Windows 10/11 x64，关闭 Secure Boot 或准备好测试签名策略；
   - `bcdedit /set testsigning on`（VM 内，管理员）后重启；
   - VM 与主机之间可用网络共享/复制文件。
3. 主机安装 WDK 后即可交叉编译驱动（编译不需要 testsigning）。

## 构建

```bat
:: 在 WDK 环境中
cd driver
msbuild appdirfilter.vcxproj /p:Configuration=Release /p:Platform=x64
```

产出：`appdirfilter.sys` + `appdirfilter.inf` + `.cat`。

## 签名（VM 测试阶段）

```bat
signtool sign /fd SHA256 /a appdirfilter.sys
Inf2Cat /driver:. /os:10_x64   (如需 cat)
```

测试阶段用 testsigning + 自签证书即可；正式分发需要 EV 证书并过
Microsoft 硬件开发者中心 Attestation 签名。

## 加载与验证（VM 内）

```bat
sc create appdirfilter type= filesys binPath= C:\path\appdirfilter.sys
sc start appdirfilter
fltmc instances -f appdirfilter      :: 或手动 attach 到卷
```

验证顺序（对应任务书 §8）：

1. `fltmc load` 成功，`fltmc filters` 可见，无 BSOD；
2. VM 内新建直接子目录 → 用户态 `daemon --source feed`（先用回放验证端口）→
   换成真实通信端口来源后确认 `recorded` 且创建者 EXE 正确；
3. 打开已有目录 / 嵌套创建 / 文件创建 / 失败创建 → 不产生记录；
4. 反复 `sc stop` / `sc start` 50 次，确认无泄漏（Driver Verifier + pool 标签检查）；
5. 高频创建（1 万个目录脚本）压测，观察 `EventsLost=0` 与系统稳定性；
6. 卸载后系统日志无错误。

## 已知风险

- 本机 VBS/HVCI 开启 + 无 WDK：无法在本机编译或加载，因此**驱动代码中任何断言
  在加载验证前都属于未验证设计**；
- 通信端口断连重连语义已在用户态 `feed` 来源用测试覆盖，驱动侧仍需实测；
- 文件系统重解析点、网络盘上的行为需要专门用例。

## 恢复

VM 快照回滚即可完全恢复；`sc stop appdirfilter` + `sc delete appdirfilter` 后重启。
