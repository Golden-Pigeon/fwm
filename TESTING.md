# 测试与验证

测试分为 Rust 单元测试、真实 CLI 进程测试、真实 OpenSSH 故障测试和远端 helper 模拟测试。命令成功退出不是唯一断言：配置、版本、监听、已有长连接、后台进程、错误输出和失败后的文件内容都属于验证范围。

## 回归矩阵

| 功能或边界 | 主要测试位置 | 断言 |
|---|---|---|
| 主机信任与自动恢复 | `tests/trust_recovery.py`、`daemon/probes.rs` | 首次信任后自动建立；错指纹不写信任；健康长连接和停止规则不受影响；未注册别名不创建空服务器；离线信任不启后台 |
| SSH 认证与签名 | `ssh/auth_tests.rs`、`ssh/auth_fixture.rs`、`ssh/auth_agent.rs` | 真实 agent 签名、IdentitiesOnly、加密私钥、重复身份、RSA SHA-256/512 协商与 SHA-1 拒绝、文件/agent 用户证书、ECDSA、错误签名及授权后新连接恢复；本地证书签名字节和密码学校验 |
| 交互信任 | `tests/trust_interactive.py` | 真实 PTY 中 yes/no/空输入、非 TTY、JSON 缺指纹、错误指纹、重复信任；验证 known_hosts 与后台状态 |
| 端口简写和部分编辑 | `cli/parse_patch_tests.rs`、`tests/port_group_regressions.rs` | IPv4/IPv6 地址保留、单端口语法等价、无效范围拒绝、显式地址替换 |
| 分组管理 | `cli/add_group_tests.rs`、`group_validation_tests.rs` | 单个/批量追加、独立命名、服务器必填、组内冲突原子拒绝、停止备选规则兼容 |
| SSH 配置继承 | `ssh/config_override_tests.rs`、`tests/server_overrides.rs` | 覆盖优先级、unset 持久化、禁用/继承跳板、字段冲突及空值在保存前拒绝 |
| 配置和诊断 | `tests/doctor_config.rs`、`tests/handwritten_config.rs`、`store/identity.rs` | 坏草稿输出失败、有效草稿提示、缺省 ID 稳定、只读不写文件 |
| 离线和并发操作 | `tests/offline_mutations.rs`、`offline/tests.rs`、`configuration/tests.rs` | 不唤醒后台、失败无变更、无操作不增版本、实例锁、并发写 revision 冲突、离线 reload 先保存 |
| 配置事务和迁移 | `store/`、`daemon/mutation_tests.rs`、`tests/control_workflows.rs` | 草稿字节保护、删除不复活、原子批量、幂等请求、版本迁移与备份 |
| 生命周期与弱网 | `engine/`、`tests/smoke.py`、`tests/ux_live.py` | 重连、黑洞、远端取消迟到、共享连接隔离、规则/组重启、退避上限 |
| 远端残留回收 | `cleanup/test_*.py`、`tests/remote_recovery.py` | 身份与代次核验、PID 复用、陌生进程保护、helper 崩溃、取消竞争 |
| 数据通道和 SOCKS | `engine/channel_tests.rs`、`engine/failure_tests.rs`、`engine/socks_protocol_tests.rs` | 本地端口占用后恢复/取消、目标通道拒绝和超时回复、拒绝和取消、容量释放、IPv4/IPv6/DNS、协议错误回复、分片、截断、双向半关闭 |
| helper 客户端与远端失败 | `engine/failure_tests.rs` | exec 拒绝、Python 不可用、断开/超时、超长/错误 JSON/版本/操作/身份/PID；确认失败补偿取消、释放失败、三次监听拒绝、迟到通道关闭、空闲异常输出 |
| 管理 API | `fwm-api/tests/` | 帧边界、截断、坏 JSON/UTF-8、版本与请求 ID、写失败不污染流、命令版本保护分类 |
| 输出与输入契约 | `cli/contracts_tests.rs`、`tests/cli_contracts.rs`、`tests/ux_commands.rs` | JSON 最终结果、退出码、时间单位与溢出、帮助零副作用、保存和就绪区分 |
| 持续查询 | `tests/cli_streaming.rs` | 真实 CLI watch/follow 子进程、规则/服务器重命名和删除、空组与新增成员、后台启停、轮转后旧名关联、警告去重、草稿修复、Ctrl-C |
| 动态 Shell 补全 | `tests/shell_completions.rs`、`tests/shell_completion_scripts.py` | 本地服务器/规则/组、ID、独立配置目录、坏草稿与无配置、零写入、路径和枚举；真实 Bash/Zsh Tab 引用、等号参数、光标位置与 source/autoload |
| 历史日志 | `history/`、`tests/history_cli.rs` | 重命名/删除后查询、轮转、有界存储、UTC、跨后台去重、离线读取；超大文件/元数据、UTF-8 截断、读/定位/写失败、部分写后恢复 |
| 配置提交故障 | `store/failure_tests.rs`、`store/io.rs` | 注入部分写、文件同步、原子替换、目录同步失败；提交前保持双文件不变，提交后报告已保存并保留停止保护；不会真实填满磁盘 |
| IPC 与启动故障 | `client_failure_tests.rs`、`cli/completion_tests.rs` | 测试私有 IPC 对端返回坏数据/断开/缺少错误/超时；虚拟时钟验证超时上限；启动失败保留 saved:true，等待失败保留最近快照 |
| 后台及平台适配 | `tests/daemon_restart.rs`、`platform/service_tests.rs`、`cli/service_lifecycle.rs`、`fwm-core/tests/paths.rs` | 配置隔离、实例替换、路径权限；三平台服务命令参数/顺序/重复安装/卸载/失败回滚、已安装服务与残留非托管后台接管；系统执行器使用模拟对象 |
| 分组与身份歧义回归 | `tests/audited_cli.rs`、`tests/port_group_regressions.rs`、`model.rs`、`store/migration_tests.rs` | 组改名保留成员名、拒绝隐式合并、显式入组退组、UUID 名称冲突、长自动组名区分、旧分组迁移不制造 ID 冲突 |
| SSH 路径、认证与刷新回归 | `ssh/path_options.rs`、`ssh/config_override_tests.rs`、`ssh/auth_tests.rs`、`tests/ssh_refresh.py` | 相对路径基准、配置符号链接切换、OpenSSH 布尔值、公钥选择 agent 身份、显式缺失私钥；CLI 服务器重启和 reload 读取新端点，同时保留其他服务器长连接 |
| 信任上下文和诊断回归 | `tests/trust_recovery.py`、`ssh_actions.rs`、`ssh/tests.rs`、`tests/ssh_path_ux.rs` | 已信任密钥无交互幂等、目标 known_hosts 内的 hop 信任、记录日志失败不否认已提交信任、实际认证进程/agent 来源、POSIX 和 PowerShell 恢复命令引用 |
| 服务事务与故障回归 | `platform/service_tests.rs`、`platform/service_windows_tests.rs`、`platform/service_definition.rs`、`platform/service_command.rs`、`cli/service_lifecycle_tests.rs` | 状态化三平台假服务管理器；旧定义与 OS 注册独立恢复、部分写失败、原子替换、回滚所有权、整个事务互斥、命令超时、旧路径发现、失配 marker、禁用任务、升级后可执行文件刷新、失败退出码 |
| 无响应后台与显式恢复 | `tests/daemon_presence_audit.rs`、`tests/audit_state_workflows.rs`、`store/recovery_tests.rs` | 持锁但无响应不误判停止、不另启实例、仍发送 shutdown；watch 失联后恢复；恢复前校验、独立备份、保留可读停删意图、损坏意图须显式放弃、全部规则停止 |
| 历史选择与服务器事件回归 | `history/selection_audit_tests.rs`、`cli/query_selection_tests.rs`、`tests/audit_state_workflows.rs` | 当前对象优先历史别名、组简写和显式组筛选等价、follow 固定规则/服务器 ID、删除重建不跳转、服务器配置/信任/连接事件保留归属 |

表中的 Rust 路径相对对应 crate 的 `src` 或根目录；Rust 集成测试位于各 crate 的 `tests/`。

此前 U01–U36 的修复行为与测试列在 [UX 修复核对表](audits/2026-09-20-ux/FIXES.md)。2026-09-21 对复审新增的 36 项确认问题及 4 项条件性风险实施修复，见 [复审修复核对表](audits/2026-09-20-postfix/FIXES.md)。原审查和证据保留为修复前快照。

## 本地运行

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
python3 -m unittest discover -s crates/fwm-core/src/cleanup -p 'test_*.py' -v
cargo build --release --locked
python3 tests/postfix_queries.py target/release/fwm
python3 tests/shell_completion_scripts.py
python3 tests/smoke.py target/release/fwm
```

真实 SSH 测试仅使用临时密钥、临时配置和回环监听。要求 Unix、sshd 和 ssh-keygen；macOS 的嵌套沙箱可能阻止 sshd 初始化，需在允许该测试的环境执行。测试清理其创建的后台和 SSH 服务，不读取或更改默认 fwm 配置。

## 覆盖率

```sh
rustup component add llvm-tools-preview
python3 tests/coverage.py --with-ssh
```

报告保存在 `target/coverage/html/index.html`，同时生成逐文件的 `report.txt`、`coverage.json` 和 `lcov.info`。可传 `--offline` 使用 Cargo 缓存，`--llvm-bin PATH` 使用外部匹配版本的 LLVM 工具。脚本汇总单元测试、CLI 子进程和后台进程的覆盖数据。

也可分步运行，方便 SSH 测试使用独立权限：

```sh
python3 tests/coverage.py
LLVM_PROFILE_FILE="$PWD/target/coverage/raw/fwm-%8m.profraw" \
  python3 tests/smoke.py target/coverage/build/debug/fwm
python3 tests/coverage.py --report-only --fail-under-lines 80 --fail-under-functions 80
```

覆盖报告排除依赖、标准库、独立测试源码和测试 fixture；生产文件中的内联单元测试仍计入统计。稳定 Rust 的该配置不提供分支覆盖百分比，报告中的 branches=0 不代表分支全部覆盖。三平台服务的命令与流程逻辑可在本机通过模拟执行器测试；Linux/Windows 专用的原生系统入口仍不包含在 macOS 报告中。Python helper 和 PTY 测试本身不计入 Rust 百分比，但后者执行的 Rust 路径计入。

2026-09-20 修复 U01–U36 后验证：在上一轮 352 项基础上，本机测试总数增至 **438 项 Rust 测试**，全部通过，未忽略任何测试；**27 项 Python helper 测试**全部通过。Release 与插桩二进制均通过完整真实 OpenSSH 和 PTY 交互测试，包括服务器连接刷新、上下文跳板信任及服务器事件归属。合并后行覆盖率 **92.85%**（11283/12152），函数覆盖率 **90.99%**（1191/1309）。macOS 原生与 Windows 交叉目标的严格 Clippy 检查通过，格式检查通过。

LLVM 的 hash-mismatch 告警已核对为排除统计的外部依赖 `crypto_bigint::Limb::one` 和 `tracing_core::LevelFilter::cmp`，未涉及项目函数。

本轮针对 36 项审查逐项加入或加强行为断言，并保留上一轮身份认证、转发与恢复回归。组合测试还覆盖旧分组迁移中的 ID 冲突、等价路径的旧服务和 IPC 兼容、服务注册与磁盘定义不一致、配置符号链接切换、无服务管理器时的独立后台启动。存储故障通过私有 I/O 接口注入，系统服务通过状态化模拟命令执行器验证，测试不填满磁盘、不注册真实登录服务。

密集主机密钥检查会触发新版 OpenSSH 的源地址惩罚。SSH fixture 先探测选项支持，再仅为临时回环 sshd 关闭 `PerSourcePenalties`，避免认证流程测试受到自身流量限流干扰。

CI 配置为在 macOS、Linux、Windows 分别运行格式、严格 Clippy 和 Rust 测试；Linux 运行真实 OpenSSH/PTY 测试。独立覆盖率任务合并 Rust/CLI/SSH 数据，以行和函数覆盖率均不低于 80% 为下限，并上传 HTML/LCOV 报告。覆盖率是回归防线，不替代上面的行为断言；不能据此承诺所有故障组合均已验证。

真实登录/注销、系统休眠与 VPN 切换，以及 Windows 服务和 ACL 的原生运行验收仍需相应环境。常规测试不修改开发者的真实登录服务。

2026-09-21 复审修复验证：497 项 Rust 测试全部通过（0 忽略）、28 项 Python helper 测试、3 项 CLI/IPC 集成测试和17项真实 OpenSSH/PTY 检查全部通过；Release 构建、格式和全 workspace 严格 Clippy 通过。最后的 watch Ctrl-C 订阅调整另以14项 streaming/state-workflow及Release CLI测试复核。新增查询测试覆盖初次 IPC 失联的8种选择器组合、拒绝混合revision以及512条失败规则的全量/单条状态。验证日志与逐项边界见[复审修复记录](audits/2026-09-20-postfix/FIXES.md)。此前的覆盖率数字未重新测量，不代表这次修改后的覆盖率。
