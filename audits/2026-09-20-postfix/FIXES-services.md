# 服务与后台修复记录

2026-09-21，对应 REVIEW.md 的 S01–S07（S03 仍按条件性平台风险记录）。

| 编号 | 修复 | 回归验证 |
|---|---|---|
| S01 | checkpoint 不再用磁盘定义补替已注册 manager 的恢复定义。Linux 要求 `NeedDaemonReload=no` 才能使用磁盘定义；macOS 用 launchd 当前 program/profile 核对已知 FWM plist（兼容旧日志路径模板）。无法确认一致时，在停止旧服务前返回 `service_recovery_unavailable`。 | 两种 Unix adapter 均注入磁盘程序与实际运行程序不同，断言 checkpoint 拒绝、旧程序继续运行且未执行 stop/bootout/reload。既有升级失败恢复测试继续通过。 |
| S02 | Unix 发现流程对当前/legacy 文件名和已加载 job 均验证配置归属。文件名不能覆盖内容或 manager 的否定结果；发现的非标准名称也检查现存定义归属。冲突返回 `service_identity_conflict`。 | macOS/Linux 各覆盖磁盘和 manager 同属其它配置、仅磁盘被改、仅 manager 指向其它配置三种组合，断言无停止和文件改动。 |
| S03 | restore 在恢复最终文件状态后增加收尾步骤：Linux 之前未注册时执行最终 `daemon-reload`，删除定义后再查询确认已无注册，失败不会报告恢复成功。已注册的旧定义仍按原先顺序恢复。 | 假 systemd 首次安装后模拟 readiness 失败，恢复后磁盘定义、manager registration、installed 均不存在。 |
| S04 | `wait_stopped` 使用与 `daemon status` 相同的完整 presence 判断，只有 Stopped 才能成功。无锁文件但仍可连接的 IPC 对端会超时返回停止未确认。 | 私有 socket 接受连接但不回复、没有 lock pathname；停止等待返回 `daemon_stop_timeout`，对端退出后才成功。 |
| S05 | daemon 自己将启动错误和 tracing 写入可轮转的 `state/daemon.log`（2 MiB + 一个备份）。新后台进程和 LaunchAgent 不再持有 append-only 日志 stdio。daemon run 的错误展示限制约 8 KiB，保留头尾与 TOML 错误原因。 | 连续 300 次超长、多字节启动错误保持两个有界文件；保留错误头尾；既有单条超限和轮转测试通过。 |
| S06 | Unix socket 超过最小平台路径预算时，按规范配置目录身份生成 `/tmp/fwm-UID/HASH.sock`，目录必须为当前用户拥有的真实目录，权限 0700，socket 0600。短配置路径继续使用原 endpoint。 | IPC 传输、长路径身份稳定与配置间隔离、拒绝覆盖活 socket；实际私有后台在长路径下 start/status/restart/stop 完整通过。 |
| S07 | Linux/Windows/macOS 和独立后台均由 daemon 的启动错误处理写相同 `daemon.log`，readiness 的日志提示现在有统一写入者。 | 直接调用 daemon run，未做 stdout/stderr 重定向，坏配置错误仍写入提示的日志文件。 |

额外独立复核发现新 StatusView 缺少旧后台能力协商，已添加 `atomic_status_view` 检查：旧后台返回明确的升级指引，支持该能力的后台收到原始 selector；两例回归通过。daemon 的能力声明由主线修改。

验证结果：

- `cargo test -p fwm platform::service --offline --locked`：41 个服务相关测试通过（含三平台假管理器）。
- 最终 `cargo test -p fwm --bin fwm --offline --locked --quiet`：193/193 通过，包含补全的服务归属矩阵、日志、IPC 和能力协商测试。
- `cargo test -p fwm --test daemon_presence_audit --test daemon_restart --offline --locked`：最初 2 + 4 通过；随后增加长路径集成用例并重跑 `daemon_restart`，5/5 通过。

边界：未注册或变更真实 launchd/systemd/Task Scheduler 服务。S01 对不能证明可恢复的已加载定义选择在操作前拒绝，不声称能重建任意手改的 manager 配置。S03 的回归证明明确 reload/验证协议，未把假管理器行为视为真实 systemd 原生验收。Windows 日志路径使用同一 Rust 代码，但本机未运行原生 Windows task。未做断电、磁盘满或自然竞争概率实验。

## 追加真实 OpenSSH / PTY 回归

`cargo build -p fwm --offline --locked` 确认当前调试二进制构建完成后，运行标准 `python3 tests/smoke.py target/debug/fwm`。最终退出 0，日志 `/tmp/fwm-postfix-smoke.log` 有 17 项 PASS，包括 local/remote/SOCKS5、PTY 信任交互、停止/重试/重启隔离、远端陈旧 sshd 会话回收、helper 死亡恢复、无关监听保护、20 秒黑洞检测和恢复、并发 revision、坏草稿和手动停止持久性、认证 doctor。

测试使用临时 sshd、回环网络、临时密钥和配置，未安装系统登录服务。首次受 macOS 嵌套 sandbox 限制（`sandbox_init: Operation not permitted`），经自动批准在 sandbox 外运行，未修改生产 SSH 服务配置。

为保证标准 smoke 与用户环境隔离，给三个直接主机 fixture 加入临时 `--ssh-config`，避免继承用户的 SSH 配置。首次全功能通过后，测试退出时发现 fixture 仅关闭 proxy listener、仍保留已接受连接，远端 helper 可与临时目录删除竞争；现显式取消并 join 这些 fixture proxy 任务后再清理。重跑所有 17 项功能和退出清理均通过。三个 Python 文件的语法编译也通过。
