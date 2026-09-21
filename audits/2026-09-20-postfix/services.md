# 后台与平台服务复查

本轮仅审查。生产代码保持原样，复现只使用临时目录、私有 IPC 和已有状态化假服务管理器；没有注册、停止或改写用户真实登录服务。Rust harness 临时编译当前生产 adapter 和现有 fixture，断言用于确认缺陷存在，不能当作修复后的回归通过。

## S01 — P2：服务回滚把磁盘定义当成系统实际加载的定义

已安装且运行 `/actually-running/fwm`；用户或升级工具把磁盘定义改成 `/edited-on-disk/fwm`，尚未让服务管理器重新加载。随后升级/重启失败并执行恢复，当前实现返回恢复成功，却启动 `/edited-on-disk/fwm`，并非原先运行的程序。

macOS `inspect` 优先采用磁盘内容，仅在文件不存在时才看 launchd 的 program；Linux 直接把磁盘内容填进 `Registration.definition`。因此原本分开的 disk checkpoint 与 manager checkpoint 实际取到同一份错误来源。定位：[service_macos.rs:55](../../crates/fwm/src/platform/service_macos.rs#L55)、[service_linux.rs:88](../../crates/fwm/src/platform/service_linux.rs#L88)、[service.rs:122](../../crates/fwm/src/platform/service.rs#L122)。

证据：`audit_checkpoint_confuses_disk_definition_with_registered_program` 在 macOS/Linux adapter 上均复现；见 [harness](evidence/service_repros.rs) 与 [结果](evidence/service-repros.txt)。这是生产控制流配合假管理器实测，不是原生 launchd/systemd 实验。应捕获实际已注册配置，无法完整恢复时在停止旧后台前明确拒绝。

## S02 — P1：Unix 旧文件名回退绕过 profile 归属校验

原属于配置 A 的 `fwm-HASH.service` / `io.fwm.fwm-HASH.plist` 被改为启动配置 B，文件名未变。发现流程前半段正确识别其内容不属于 A；末尾 `legacy_ids` 却仅检查文件存在，又把它加回来。此后按 A 执行 uninstall/stop 可以停止 B，uninstall 还删除 B 正在使用的定义。

定位：[service_linux.rs:159](../../crates/fwm/src/platform/service_linux.rs#L159)、[service_macos.rs:188](../../crates/fwm/src/platform/service_macos.rs#L188)。无需 hash 碰撞，修改已存在服务的 `--config-dir` 即可。Windows 的 matching 分支会拒绝这类冲突，Unix 应同样校验实际文件及系统注册的 profile，不应以文件名覆盖否定证据。

证据：`audit_unix_legacy_filename_can_adopt_a_different_profile` 对两种 Unix adapter 都确认“uninstall A 移除了 B”；见 [结果](evidence/service-repros.txt)。未对真实系统服务执行该操作。

## S03 — P2，条件性平台风险：Linux 首次安装失败后的恢复遗漏 manager reload

安装前无注册；新服务安装后 readiness 失败。`Operation::restore` 调用 unregister（stop/disable），删除新 unit 文件，然后因为旧注册不存在而跳过 `register_saved`，直接返回成功。此路径没有在删除文件后执行 `daemon-reload`。正常 uninstall 则有该步骤。

定位：[service.rs:153](../../crates/fwm/src/platform/service.rs#L153)、[service.rs:62](../../crates/fwm/src/platform/service.rs#L62)，对照 [service_linux.rs:267](../../crates/fwm/src/platform/service_linux.rs#L267)。

假管理器证据：`audit_linux_failed_first_install_leaves_registered_unit_after_restore` 返回恢复成功、文件已消失，但 registration/installed 仍为 true。**原生表现取决于 unit 是否仍被引用、处于 failed 状态或被 IPC client 固定，不能声称每次都会残留**。systemd 会自动回收不再需要的 unit；仍加载的 unit 需显式刷新配置。[systemd 官方说明](https://github.com/systemd/systemd/blob/main/man/systemd.unit.xml#L520)。修复应在最终文件状态恢复后同步 manager，并验证结果，不能依赖垃圾回收时序。

## S04 — P2：无可见 lock 的无响应 IPC 对端被 stop 假报为已停止

私有 socket 仍监听并接受请求，但不回应，且没有可打开的 lock pathname（实际后台运行中 lock 文件被移走也可能产生这种状态）。`daemon status` 正确报告 unresponsive；`daemon stop` 发出 Shutdown 后收不到回复，却 exit 0 输出 `Daemon stopped`；紧接着 status 仍为 unresponsive。

`presence` 会额外检查可连接对端，[wait_stopped](../../crates/fwm/src/cli/daemon.rs#L101) 却只检查 Ping 与 lock。停止完成条件弱于状态查询判定条件。应确认完整 presence 为 Stopped，仍存在未确认对端时返回失败。

Release CLI 实测：[复现脚本](evidence/unresponsive_peer.py)、[请求及结果](evidence/unresponsive-peer.json)。没有启动真实 fwm 服务或终止未知进程。

## S05 — P2：启动失败日志绕过轮转，可持续放大错误占用磁盘

`diagnostics.log` 有 2 MiB 轮转，启动阶段错误却由 main 写到 stderr，并被独立后台及 LaunchAgent 追加到未限额的 `daemon.log`。坏 TOML 的错误展示会重复打印超长源代码行；200022 字节候选配置单次产生约 400161 字节启动错误，13 次已达到 5202093 字节，没有轮转文件。若由登录服务自动重启，会不断累积。

定位：[background.rs:9](../../crates/fwm/src/platform/background.rs#L9)、[main.rs:43](../../crates/fwm/src/main.rs#L43)、[service_macos.rs:348](../../crates/fwm/src/platform/service_macos.rs#L348)。设计第 10 节要求日志限额；应统一处理启动/运行日志，并限制单条解析错误的源文本展示。

证据：[脚本](evidence/runtime_limits.py)、[大小记录](evidence/startup-log.json)。实测执行临时配置的 Release `daemon run`，以与后台相同的 append sink 收集 stderr，未启动真实自动重启服务；实验文件已清理，没有进行磁盘满测试。

## S06 — P2：合法较长配置路径导致 Unix 后台完全无法启动

IPC 固定放在 `config-dir/state/daemon.sock`，未经处理地继承配置路径长度。合法目录产生 152 字节 socket 路径时，空有效配置的 `daemon start` 失败，日志为 `path must be shorter than SUN_LEN`；更改 SSH 或规则配置不能解决它。macOS/Linux 的 Unix socket 路径预算与普通文件路径预算不同，当前使用/配置接口没有给出这一约束。

定位：[ipc_unix.rs:27](../../crates/fwm/src/platform/ipc_unix.rs#L27)、[paths.rs:49](../../crates/fwm-core/src/paths.rs#L49)。应以规范配置身份生成短的私有 runtime endpoint，或至少在任何保存/启动前做明确预检并提供补救，不应等后台子进程退出后只返回泛化 daemon_unavailable。

证据：[脚本](evidence/runtime_limits.py)、[Release 结果](evidence/long-config-path.json)。未访问 SSH 服务器。

## S07 — P3：Linux/Windows 托管启动失败指向没有写入者的日志文件

readiness 超时统一要求查看 `state/daemon.log`，但 Linux unit 和 Windows task 都直接运行 `fwm daemon run`；只有独立后台 spawn 与 macOS LaunchAgent 把 stdout/stderr 接到该文件。daemon 内部的 tracing 写入另一个 `diagnostics.log`，启动加载错误则由 main 输出到 stderr。因此 Linux/Windows 托管失败时，给出的 daemon.log 可能不存在或是旧独立后台遗留内容。

定位：[client.rs:248](../../crates/fwm/src/client.rs#L248)、[service_linux.rs:193](../../crates/fwm/src/platform/service_linux.rs#L193)、[service_windows.rs:42](../../crates/fwm/src/platform/service_windows.rs#L42)。临时 Release `daemon run` 的坏配置结果见 [证据](evidence/managed-startup-diagnostic.json)：exit 2、stderr 有具体错误、state 中只有 daemon.lock，没有 daemon.log。应根据托管方式给出正确诊断入口，或让 daemon 自己统一记录启动错误。

这里确认的是 FWM 指定日志路径没有相应写入者；未声称 Task Scheduler 没有其他捕获渠道，也未执行原生 Windows task。

## 分轮与排除

新增审查项：2 → 2 → 2 → 1 → 0，其中 S03 保留条件性平台风险标签。最后复扫安装/重启/停止/卸载、注册有无、定义有无、enabled/disabled、错误和回滚、并发锁、子进程时限、目录/IPC 身份及日志路径，无新的可确认项。

已排除：服务 adapter 内部失败后 Drop 的弱错误报告可由外层事务恢复捕获；正常 fwm 并发服务变更受 operation lock 串行保护；Unix bind 会探测已有监听，不会仅因缺 lock 覆盖活 socket；Windows 禁用 task 的显式运行已有临时启用与恢复逻辑；PowerShell 查询没有插入用户输入。S03 保留条件性证据标签，不拿假管理器的缓存行为冒充 systemd 原生验收。
