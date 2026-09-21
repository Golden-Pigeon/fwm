# daemon / service 只读 UX 审查

范围：daemon start/stop/restart/run/status，service install/uninstall，三平台服务定义、启动和失败路径、跨进程竞争、登录恢复及错误输出。审查使用当前源码、既有 mock 测试的控制流，以及 Release CLI 的隔离复现；未修改仓库源码，未编译，未安装或修改真实系统服务，未访问 SSH 服务器或默认 daemon。所有临时 IPC、锁和子进程均已结束，临时配置已清理。

证据分级：A = 隔离 CLI 实测；B = 源码在明确前提下可以确定的行为，未冒充平台原生实测；C = 尚需原生环境验证，未计入确认问题。

## 轮次

| 轮次 | 新确认 | 工作及结果 |
| --- | ---: | --- |
| 第一轮 | 3 | 从启动、停止、安装失败路径新增 N1–N3。关联检查了先停止后安装、定义提交点、无响应 owner 的识别。 |
| 第二轮 | 5 | 沿失败恢复和身份管理新增 N4–N8：等价配置路径、注册状态、外部命令等待、错误分类、并发回滚。 |
| 第三轮 | 0 | 按文末完整矩阵复查正常、失败、重复、竞争及三平台分支；未发现独立于既有 K1–K3 / N1–N8 的可靠新问题。排除项与原生验证边界列在矩阵后。 |

这里的“0”表示本轮实际检查范围未发现独立新增项，不是所有操作系统真实环境都已验收，也不是穷尽全部可能的外部故障。

## 先前已知项，不计本次新增

- **K1，B：安装路径变更后，daemon restart 仍启动服务定义里的旧 binary。** 当前执行文件 B 运行 restart，仅 stop + ensure_running；已安装服务只 start 既有定义，没有刷新 definition 中保存的 A 路径。原地覆盖 A 的情况正常，移动到新路径的升级可能循环“重启旧版本 → daemon_upgrade_required”。`crates/fwm/src/cli/daemon.rs:27`；`crates/fwm/src/client.rs:155`；`crates/fwm/src/platform/service_macos.rs:91`；Linux `service_linux.rs:89`；Windows `service_windows.rs:88`。README:171 的笼统升级指引未说明此条件。
- **K2，A+B：service uninstall 帮助只说移除登录自启，实际停止当前转发。** macOS bootout、Linux disable --now、Windows /End 都会停止服务；结果只输出“Login startup removed”。帮助实测没有此副作用说明。`crates/fwm/src/cli/daemon.rs:64`；`service_macos.rs:83`、`service_linux.rs:75`、`service_windows.rs:76`。
- **K3，A：仅支持用户服务，仍强制 --user。** `service install` / `service uninstall` 不带 --user 实测均 exit 2；当前 user 布尔值不参与模式选择，只有用户模式。`crates/fwm/src/cli/args.rs:388`。

## 本次确认问题

### N1 — 安装失败后，原来工作的后台保持停止（P2，B）

触发：后台原本运行，执行 service install；接管停止完成后，写服务定义、注册服务或启动失败。

`install_with` 先调用 `stop_with` 并等待旧实例退出，随后调用 `control.install()`；安装失败直接返回，没有恢复原来的普通后台或托管后台。适配器局部回滚只恢复文件/manager 定义，无法恢复完整 CLI 操作之前的运行状态。现有 `install_failure_is_returned_after_successful_takeover` mock 也明确记录最后两步为 wait_stopped、install，没有恢复步骤。

macOS 尤其明显：CLI 已先 bootout，适配器再次 bootout 得到失败，因此其 `booted_out` 为 false，即使原服务在本条 CLI 命令前运行，adapter 的恢复分支也不会重新 bootstrap。返回“previous definition was restored”只意味着文件，不意味着转发恢复。

源码：`crates/fwm/src/cli/service_lifecycle.rs:46`、`:58`、`:188`；`crates/fwm/src/platform/service_macos.rs:68`；`crates/fwm/src/platform/service.rs:135`。

影响：一个失败的“安装自启”命令会中断原有转发；用户需要另行 daemon start。保存的规则 desired_state 没有被改掉，问题是运行状态与失败结果的预期。

建议：停止前完成可行的定义校验和 staging；保留接管前是否运行、是否托管的状态，失败后恢复，或在恢复不可行时明确报告后台已停止及恢复命令。

### N2 — 定义文件写失败时，回滚保护尚未建立（P2，B）

触发：重装已有服务，`fs::write` 打开并截断旧定义后发生写错误，例如磁盘写入失败。

`Definition::stage` 读取 previous 后直接 `fs::write(path, content)`，只有写成功才构造 Definition。写入途中报错时没有 RAII guard 可以恢复旧内容；回滚本身也用非原子的直接写。文件仍存在会继续被 is_installed 当成已安装，即使其中只有半份内容。

源码：`crates/fwm/src/platform/service.rs:105`，尤其 `:112` 在 guard 构造 `:114` 之前；回滚 `:123`；安装判定 `:27`。

证据边界：没有制造真实磁盘满；这是文件截断与错误传播顺序可以确定的路径，现有“注册失败回滚”mock 没有覆盖“stage 写到一半失败”。

建议：在同目录写临时定义并原子替换，失败前保持旧文件完整；回滚也使用原子替换。

### N3 — 无响应的锁持有者被当成后台未运行（P2，A）

隔离复现：临时 config-dir，Python 持 daemon.lock 并监听私有 daemon.sock，接收请求但不回复，模拟持锁而 IPC 无响应的实例。没有 SSH 规则，没有真实系统服务。

实际结果：

- daemon status：约 1.01 秒，exit 0，`daemon_running:false`。
- daemon start：约 6.28 秒后 `daemon_unavailable`；生成的 daemon.log 显示另一次 spawn 确实发生，但因“another daemon already owns this configuration directory”被锁拒绝。
- daemon stop：约 16.36 秒后返回 `invalid_request`，文案为“daemon is still shutting down”。
- 捕获到 22 条请求，全部为 ping，没有任何 shutdown。此前并未发出停止请求，却提示还在停止中。

源码：`crates/fwm/src/client.rs:143` 把超时/错误/不存在都压为 false；`:150` 不检查 owner 锁就决定 spawn；`crates/fwm/src/cli/service_lifecycle.rs:51` 只有 ping 成功才发 Shutdown；`cli/daemon.rs:71` 等锁超时。

影响：状态误导，start 白启动一个必然冲突的实例，stop 无法表达“后台存在但不响应”，且没有实际尝试停止 IPC owner。实例锁确实防止了两个 daemon 同时管理配置，没有发现双实例真正建立转发。

建议：区分 stopped / running / unresponsive / starting-or-stopping；结合锁与 IPC 状态决定下一步，错误提示实际发生的阶段。强制终止若未来提供，应有可核验进程身份，不能仅凭端口或 PID 猜测。

### N4 — 同一个配置目录的等价拼写产生不同服务身份（P2，A+B）

隔离 CLI 实测向同一临时目录传入 `p` 和 `p/.`：daemon status 返回的 config_dir 保留两种字符串，文件系统 canonical path 完全相同。按源码固定 hash 算法计算，示例服务 ID 分别为 `fwm-15845a6933e623b2` 与 `fwm-a037dbaaa8aa24eb`。

`Paths::new` 只将相对路径接到 cwd，没有统一目录身份；service identifier 与 Windows pipe_name 均对原始路径字符串取 hash。macOS `/var/...` 与 `/private/var/...` 也可能指向同一位置。

源码：`crates/fwm-core/src/paths.rs:17`、`:41`；`crates/fwm/src/platform/service.rs:80`。

影响：同目录换一种正常写法后找不到已安装自启；可能注册多个登录服务争抢同一个实例锁。Windows 下还会连接到不同 named pipe，以为原 daemon 不存在。此问题与 SSH config 相对文件路径问题不同。

建议：为配置目录建立稳定规范身份，并兼顾目录尚未创建的情况与现有服务名迁移。

### N5 — 本地定义文件被误当成系统注册状态的唯一依据（P2，B）

`is_installed()` 只判断文件 exists，所有 uninstall 都先据此短路。

- 如果本地 marker 被删除，但 Windows Task Scheduler 内导入的任务仍存在，uninstall 直接返回成功，不执行 /Delete，实际登录自启仍存在。
- 反过来，本地 marker 还在、系统任务已由其他工具删除，uninstall 先 /End 并在其报错时退出，无法删除 marker；daemon start 仍因 marker 而选择 /Run，继续失败。
- macOS 已注册进程与 plist 文件、Linux 内存中的 unit 与磁盘文件，也不是同一个状态。当地文件缺失时，当前运行服务不会因为 exists=false 就自动消失。

源码：`crates/fwm/src/platform/service.rs:27`；`service_windows.rs:76`、`:88`、`:94`；`service_macos.rs:83`；`service_linux.rs:75`。

证据边界：没有真实删除系统任务；这里确认的是文件/OS 注册不一致时的控制流。Windows 不运行状态的 /End 具体退出表现未做原生验证，不将其扩大声称为所有正常重复 stop 都有问题。

建议：分别查询本地定义和 manager 的注册/运行状态；卸载对“已不存在”幂等，仍尝试清理另一侧，而不是仅靠本地缓存判断成功。

### N6 — 服务管理器卡住时，CLI 的等待时限不起作用（P2，B）

所有 launchctl/systemctl/schtasks 调用均通过同步 `std::process::Command::output()`，没有时间限制。启动的 5 秒 readiness 检查在服务命令返回后才开始；停止的 15 秒等待也在服务 stop 返回后才开始。因此 manager 或相关任务卡住时，CLI 可以长期没有最终结果，既有超时参数保护不了这一阶段。

源码：`crates/fwm/src/platform/service.rs:48`；`crates/fwm/src/client.rs:155`–`:160`；`crates/fwm/src/cli/service_lifecycle.rs:48`；`cli/daemon.rs:71`。

证据边界：源码确认没有时限，未实际卡住 OS manager。不是断言每个平台命令日常都会无期限等待。

建议：给外部命令单独设置有界等待，终止超时命令并报告“注册/停止结果尚未确认”，不能把超时直接当成操作未发生。

### N7 — 服务运行故障被编码为参数错误（P2，A+B）

manager 进程成功启动但以非零 exit 结束时，`run` 使用普通 `bail!`，没有 domain error type。最终 `error_code` 将其归类为 `invalid_request` / exit 2。正常参数下的 daemon stop 超时也走相同路径；N3 实测已得到此结果。

源码：`crates/fwm/src/platform/service.rs:89`；`crates/fwm/src/cli/mod.rs:114`；`cli/daemon.rs:74`。

影响：脚本无法可靠区分命令参数有误、系统服务不可用和等待超时，与设计约定的退出码分类不一致。系统命令根本无法启动时仍会因保留 io::Error 被归为 io_error/5；问题是非零退出和手写超时分支。

建议：使用明确的 service_command_failed / service_timeout / daemon_stop_timeout 等错误码，并保留部分成功状态。

### N8 — 并发服务操作没有串行化，失败回滚可覆盖另一个成功安装（P2，B）

service install/uninstall 没有配置 mutation 所具有的锁或 revision 检查。Definition 持有的 previous 字节也没有版本保护。

可行交错：A stage 新定义（保存旧内容 O）；B stage 并成功注册/commit 更新 B；A 随后注册失败，rollback 无条件把文件改回 O。B 已成功返回，其定义却被更旧操作的失败回滚覆盖。卸载与安装交错也可能移除另一方刚生成的文件。

源码：`crates/fwm/src/cli/service_lifecycle.rs:58`；`crates/fwm/src/platform/service.rs:105`、`:123`；三平台 uninstall 的 remove_file 分支。与 config revision 不同，这些服务操作没有经过 offline::mutate 的实例锁。

证据边界：源码交错分析，没有用真实系统服务复现竞争。现有 fake executor 测试是单操作顺序测试，并不排除此问题。

建议：用规范化 profile 身份对应的独立服务操作锁覆盖 stage、manager 操作、commit/rollback；回滚前核对当前定义仍属于本操作。

## 第三轮完整复查矩阵

| 范围 | 已检查状态/边界 | 结果 |
| --- | --- | --- |
| daemon start | 已响应、真正停止、持锁无响应、启动后 readiness、启动日志打不开 | 正常幂等及日志错误路径有既有测试；N3/N6/N7。未发现真实双 daemon 越过同一实例锁。 |
| daemon stop | 已停止、响应中、锁未释放、无响应 owner、托管重启抑制 | 正常路径同时等 IPC 和锁，未发现提前假报退出；N3/N6/N7。 |
| daemon restart | 正在运行、原本停止、多 profile、升级换路径、停止失败 | 原本停止时启动和保留规则意图有明确约定与测试；K1，停止失败不会继续启动替代者。 |
| 后台脱离终端 | Unix setsid / null stdio，Windows detached process / new process group | 源码符合脱离终端的实现；未把原生 logout 环境声称为已测。 |
| service install | 首装、重装、普通后台接管、已有 marker 但普通后台仍运行、定义失败、注册失败、启动失败 | 正常接管顺序和局部回滚有 fake executor 测试；N1/N2/N6/N7/N8。 |
| service uninstall | 有定义、无定义、已停止、停止失败、删除失败、删除后 manager reload 失败、外部注册状态不同步 | K2、N5/N6/N7/N8；没有把部分失败说成完全未发生。 |
| service 身份 | 不同 profile、相同字符串、等价路径字符串、可执行文件新路径 | 正常隔离有测试；N4/K1。 |
| Linux | user scope、enable/start/stop/disable --now、daemon-reload、参数转义 | mock 顺序和错误路径可确定；没有实际调用 systemctl。 |
| macOS | bootstrap/print/kickstart/bootout、KeepAlive 抑制、已停止服务回滚 | mock 可确定；整体 CLI 接管失败恢复缺口为 N1，不能仅以 adapter rollback 测试宣称完整事务恢复。 |
| Windows | /Create /Run /End /Delete、当前 SID、InteractiveToken、LeastPrivilege、marker | mock 可确定；OS 注册状态不同步为 N5，原生 scheduler 和 ACL 仍属验收边界。 |
| 登录恢复 | daemon stop 保留 saved running intent，定义仍安装，下一次登录恢复 | 这是已明示语义，不列新增问题；不承诺未登录系统服务。 |
| 错误结果 | 定义已安装但启动失败、rollback 自身失败、manager 非零 exit、启动/停止 timeout | 前两类已有明确文案；N7 分类错误、N1 运行状态信息不足。 |
| 命令/路径安全边界 | 无 shell、XML 转义、systemd 参数转义、Windows argv 转义、服务操作仅 profile 自己的名字 | 既有 mock/单元测试覆盖；本轮未发现新的独立转义问题。 |

## 排除与交接

- 不重复 server --ssh-config 相对路径与等价字符串比较问题：另一个代理已负责。
- server --ssh/--host 切换保留未指定覆盖字段，符合此前部分编辑约定，不列为新问题。
- config reload 保留草稿期间明确启停/删除意图是已约定功能，主代理检查存储与展示；不把该保护机制本身列为 bug。
- service install 返回“installed”不等于所有 forward 已 established；只要注册已成功，保留定义并报告启动失败是合理的部分完成状态，不要求回滚该成功注册。
- Windows /End 对“任务已停止”的原生结果、RestartOnFailure 对手动终止的行为、各系统真实注销/登录后的 agent 环境都没有原生证据，作为 C 类边界，不计确认问题。
- engine.shutdown 的逐任务 5 秒上限乍看可能累计，但 connected 的 worker 退出有组级 3 秒界限，未证明现实故障路径可以制造累计等待；不以极端假设另列问题。
- 没有服务状态子命令可以改善发现性，但它属于功能建议，不与上面的明确行为缺陷混计。

最终：先前已知 3 项，本次新增 8 项；第三轮完整矩阵复查新增 0 项。
