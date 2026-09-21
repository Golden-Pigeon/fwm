# 配置、持久化与服务事务续审

2026-09-20；从用户指定的运行状态机检查点继续。此子范围仅复核普通配置和事务行为，复用 [storage.md](storage.md)、[services.md](services.md) 及其已有证据；没有重跑旧复现脚本，也没有改动生产代码或原报告。

本轮 **0 项新增独立问题**。既有编号保持为 ST01–ST08、S01–S07，不重复计数；其中 S03 仍是条件性平台风险，不能把假管理器结果描述为 systemd 原生行为。与旧证据清单比较，25 个存储、模型、路径与服务相关源码文件均未改变，见 [源码核对结果](evidence/resume-storage-service-hashes.json)。

## 去重后的保留项

以下均为既有发现；严重性、证据强度和具体前提沿用原报告。本次源码检查没有得到可以撤销这些条目的修复证据。

| 编号 | 级别 | 根因及可观察后果 | 主要源码位置 |
|---|---|---|---|
| ST01 | P1 | 控制覆盖按 ID 未命中后退回名称；草稿重命名会把停止、启动或删除意图作用到另一稳定 ID。三种操作共用一个根因。 | `crates/fwm-core/src/store/control.rs:24` |
| ST02 | P2 | recover 只按候选 revision 加一，允许回退并复用旧 CAS 版本。 | `crates/fwm-core/src/store/recovery.rs:38` |
| ST03 | P2 | 读取已应用配置重新规范化 SSH 路径，ENOTDIR 等外部目录错误让停止、删除和清除故障字段一起失败。 | `crates/fwm-core/src/store.rs:86`、`crates/fwm-core/src/paths.rs:127` |
| ST04 | P2 | 旧 schema 分组推断把带前导零的端口后缀合并成同一组，制造原配置没有的监听冲突。 | `crates/fwm-core/src/model.rs:358`、`:542` |
| ST05 | P1 | recover 丢弃旧控制头且没有保存统一停止的覆盖；候选镜像失败后 reload 可复活已删规则并恢复 running。 | `crates/fwm-core/src/store/recovery.rs:81`、`crates/fwm-core/src/store/control.rs:180` |
| ST06 | P1 | 已初始化实例丢失 applied 文件被当成首次使用，普通启动直接采用未提交候选。 | `crates/fwm-core/src/store.rs:43`、`:142` |
| ST07 | P2 | `exists()` 把悬空 config.toml 链接当作缺失；初始化随后用普通文件覆盖该目录项。 | `crates/fwm-core/src/store.rs:54`、`:142` |
| ST08 | P1 | 全局 State 锁内 reconcile 对无规则服务器也同步读取配置；特殊文件可使整个管理接口等待文件 I/O。仅保留原报告证据，本轮未执行该实验。 | `crates/fwm/src/daemon/dispatch.rs:47`、`crates/fwm-core/src/ssh/config.rs:234` |
| S01 | P2 | rollback 用磁盘定义冒充 manager 已加载定义；两者不一致时恢复了错误程序。 | `crates/fwm/src/platform/service_macos.rs:55`、`service_linux.rs:88`、`service.rs:122` |
| S02 | P1 | Unix legacy 文件名回退重新采用已被内容归属检查排除的定义，导致针对 A 的停止/卸载影响 B。本轮没有运行该模拟。 | `crates/fwm/src/platform/service_linux.rs:159`、`service_macos.rs:188` |
| S03 | P2，条件性 | 首次安装失败恢复后删除 unit 却没有最终 daemon-reload；是否残留 loaded unit 取决于原生 manager 引用和回收状态。 | `crates/fwm/src/platform/service.rs:153`、`:62`，对照 `service_linux.rs:267` |
| S04 | P2 | stop 完成判断只看 Ping 和 lock，弱于 presence；无 lock 的无响应对端仍在监听时可误报已停止。 | `crates/fwm/src/cli/daemon.rs:101` |
| S05 | P2 | 启动错误通过 stderr 追加到没有轮转的 daemon.log，绕开 diagnostics.log 限额。 | `crates/fwm/src/platform/background.rs:9`、`main.rs:43`、`service_macos.rs:348` |
| S06 | P2 | Unix socket 路径直接继承合法配置目录长度，超出 socket 专用预算后后台无法启动。 | `crates/fwm/src/platform/ipc_unix.rs:27`、`crates/fwm-core/src/paths.rs:49` |
| S07 | P3 | Linux/Windows 托管启动失败统一指向 daemon.log，但相关启动方式没有该文件的写入者。 | `crates/fwm/src/client.rs:248`、`platform/service_linux.rs:193`、`platform/service_windows.rs:42` |

## 本轮检查与排除

通过 CodeGraph 检查了 `Overrides::apply/rebase`、`commit_control_for/commit_with_overrides`、显式恢复的验证/备份/提交顺序、服务 `checkpoint/validate_restore/restore`、macOS inactive/disabled 恢复分支，以及外层 `Native::restore` 的等待和恢复顺序。CodeGraph 未返回指定 fixture 源码时，仅补读了该已定位文件。

补充验证运行了 8 个仓库已有的纯假管理器测试，全部通过；执行命令为：

```sh
python3 audits/2026-09-20-postfix/evidence/resume-service-controls.py
```

见 [runner](evidence/resume-service-controls.py) 与 [逐项输出](evidence/resume-service-controls.json)。runner 只编译当前生产 adapter 和已有 fixture，并精确选择指定测试。服务命令全部由 `FakeExecutor` 接收；临时目录已清理。

这些对照支持排除以下候选，不能视作上述既有缺陷已修复：

- 外部编辑服务定义后，恢复的所有权检查在任何 manager 变更之前失败，不覆盖编辑结果。
- macOS 之前 inactive 的注册不会被恢复操作唤醒；之前已卸载且 disabled 的登录策略能够恢复。
- 不可恢复的 Linux 孤立注册在停止原服务之前被拒绝。
- 查询错误不会被当成注册不存在；已有定义且 manager 不可用时不会静默降级为非托管启动。
- 非法定义目标在 manager 变更前失败；普通升级在注册、启用或启动阶段失败时，外层 checkpoint 能恢复旧定义与运行程序。

存储正常对照复用 `storage-results.json` 的 `negative-controls-final-pass`：成功删除后的同名重建、完全缺失 identity 父目录、坏草稿下停止、修复草稿后保持停止、成功镜像后退休覆盖并允许后续显式编辑。没有将这些正常行为再次列成缺陷。

## 停止边界

本次有限续审的最后一轮为 **零新增**，覆盖控制覆盖与候选同步、恢复版本及提交边界、服务磁盘/注册 checkpoint、未运行/未启用状态、外部文件编辑、manager 错误与回滚。没有执行 SSH、认证 agent、远端 helper、进程回收、真实系统服务安装、旧特殊文件阻塞复现或安全漏洞实验。

仍未覆盖原生 launchd/systemd/Task Scheduler 全矩阵、真实断电和目录 fsync 故障。候选镜像已有 fsync 警告时是否应保留控制头仍是未验证的崩溃一致性候选，不新增为确认缺陷。零新增只描述此次范围和证据，不保证项目不存在其他 bug。
