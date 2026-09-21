# 修复后复审与指定断点续审

2026-09-20。审查对象为 U01–U36 修复后的当前源码。原任务停在重新 review 阶段，本次从“排队连接停止、无关配置唤醒 needs_attention、异步事件归属”继续。

**去重后累计 36 项确认问题，另列 4 项条件性风险。** 首次恢复审查确认 RE01，并补强 C4；用户追问后，继续扩大多步骤、失败组合和并发时序检查，又确认 DS01–DS03、DH01–DH02 五个独立根因。先前的局部零新增不能支持已经找全。生产实现尚未修复；新增执行限于本地内存状态机、临时配置/日志、文件故障注入和假服务管理器，没有重放原任务末尾 API 探针或开展安全测试。

## 用户追问后的新增确认项

| 编号 | 级别 | 触发与确认结果 | 位置与证据 |
|---|---|---|---|
| DS01 | P2 | down/reload 已读取草稿、尚未写镜像时，编辑器保存了更新版本；后续镜像仍覆盖该新草稿，而且没有 warning。 | [control.rs:164](../../crates/fwm-core/src/store/control.rs#L164)、[深入存储报告](deep-storage.md) |
| DS02 | P2 | 连续镜像失败使删除保护记录跨批次累积，提交没有校验完整 applied 文件大小。正常 UUID/短名称、每批最多 512 规则也能写出超过自身 1 MiB 读取上限的快照；下一次加载立即失败。 | [control.rs:138](../../crates/fwm-core/src/store/control.rs#L138)、[store.rs:192](../../crates/fwm-core/src/store.rs#L192)、[结果](deep-storage-results.json) |
| DS03 | P2 | revision 合法保存至 `i64::MAX` 后，下一次 down/recover 增为 `2^63`，序列化与提交返回成功，但读取先经只支持 i64 整数的 toml::Value，applied/candidate 均不能再解析。属于明确的极端数值边界。 | [store.rs:197](../../crates/fwm-core/src/store.rs#L197)、[深入存储报告](deep-storage.md) |
| DH01 | P2 | 已删除规则的稳定 ID A 恰为另一历史规则 B 的名称时，`logs A` 返回两者记录，无 warning；当前配置已没有任何规则，仍无法用 A 精确选择该历史对象。 | [query_selection.rs:95](../../crates/fwm/src/cli/query_selection.rs#L95)、[普通离线 CLI 证据](deep-history-cli.json) |
| DH02 | P2 | 读取 active 和 archive 的两次 open 之间发生两次轮转，旧 active 句柄被固定拼接到较新 archive 后，返回顺序逆转；包含最新序号 979 的结果，其 tail 1 却选出 490。 | [storage.rs:22](../../crates/fwm-core/src/history/storage.rs#L22)、[零/一/两次轮转对照](deep-history-rotation.json) |

上述五项均有确定性证据。DS01 是普通编辑器保存时序的注入；DS02 同时补证正常 UUID 和短名称，独立于既有 C2 超长 ID 问题；DH02 是调度窗口注入，未测量自然负载下发生频率。详细方法、触发限制、去重和正常对照见 [deep-storage.md](deep-storage.md) 与 [deep-history.md](deep-history.md)。

## 优先处理的结果

| 编号 | 级别 | 触发及用户可观察结果 | 位置 |
|---|---|---|---|
| ST01 | P1 | 草稿把旧名称分配给另一稳定 ID 后，再 down/remove/up 并 reload，控制意图会误作用于未选规则。 | [control.rs:24](../../crates/fwm-core/src/store/control.rs#L24) |
| ST05 | P1 | recover 提交后候选镜像写失败，停删保护丢失；后续 reload 会恢复已删除规则并重新启用规则。 | [recovery.rs:81](../../crates/fwm-core/src/store/recovery.rs#L81) |
| ST06 | P1 | 已初始化实例丢失 applied 快照后，普通启动会直接应用尚未 reload 的手工草稿。 | [store.rs:43](../../crates/fwm-core/src/store.rs#L43) |
| ST08 | P1 | 无规则服务器的 SSH 配置文件读取阻塞时，持有全局状态锁的 reconcile 会连带阻塞管理操作。 | [config.rs:234](../../crates/fwm-core/src/ssh/config.rs#L234) |
| S02 | P1 | Unix 旧服务文件名回退忽略已发现的配置归属不符，针对实例 A 的停用/卸载可能操作实例 B。 | [service_linux.rs:159](../../crates/fwm/src/platform/service_linux.rs#L159)、[service_macos.rs:188](../../crates/fwm/src/platform/service_macos.rs#L188) |
| SE01 | P1 | 等价监听端点的旧取消未确认，新规则提前接管路由，旧监听回调可能进入新目标；另有合法双栈组合被误判冲突。 | [connection.rs:406](../../crates/fwm-core/src/engine/connection.rs#L406)、[remote.rs:81](../../crates/fwm-core/src/engine/remote.rs#L81) |

上述六项沿用旧任务的隔离证据；本次没有重放这些实验。逐项触发限制和证据强度见文末分域报告。

## 本次继续检查的状态机结果

| 编号 | 级别 | 确认结果与证据 |
|---|---|---|
| SE08 | P2 | 全部 down 后排队任务仍可发起新 TCP 连接。等待许可和连接期间只监视组 cancel，不检查 desired 变化；全部 down 保留组。沿用 [queue-cancel.json](evidence/queue-cancel.json)，本次源码复核。证据没有证明停止后又完成认证或建立监听。 |
| SE02 | P2 | 任意配置提交向所有保留连接组发送 watch 更新，无关服务器新增或纯改名也能唤醒 needs_attention。沿用 [unrelated-retry.json](evidence/unrelated-retry.json)，不要把它扩写成健康连接都会断开。 |
| C4 | P2 | 排队旧事件在记录时套用最新服务器/分组/名称并重写时间；显式旧 server_id 与历史外层标签甚至互相冲突。[7 个补充案例](resume-events.json) 直接调用当前生产模块，同时确认删除和新 ID 同名重建的正常对照。 |
| **RE01（本次新增）** | **P2** | 显式 restart 换代时保留 active_connections；旧 guard 关闭后因 generation 不符无法扣减。内存测试中 2 条旧连接全关闭后仍显示 2，新代次连接打开再关闭也仍为 2。见 [生命周期续审](resume-engine.md) 和 [结果](resume-engine-result.json)。位置：[lifecycle.rs:136](../../crates/fwm-core/src/engine/lifecycle.rs#L136)、[state.rs:128](../../crates/fwm-core/src/engine/state.rs#L128)。这是状态统计错误，没有据此声称 socket 或许可泄漏。 |

RE01 修复时需定义换代的计数归属：可为新代初始化计数，或将旧代计数独立维护直至退出；同时验证显式 restart、允许 retry 的异常状态、元数据编辑和 shutdown。C4 应在事件产生时携带原上下文与时间，不能只在记录时回填当前标签。

## 其余确认问题

| 编号 | 级别 | 根因与结果 | 详情 |
|---|---|---|---|
| ST02 | P2 | recover 回退 revision，旧 CAS 写版本可能再次被接受。 | [存储报告](storage.md) |
| ST03 | P2 | 外部身份文件路径出现 ENOTDIR 等错误，使合法快照无法加载，连停止、删除和清除坏字段都失败。 | [存储报告](storage.md) |
| ST04 | P2 | 旧配置的自动组推断制造原本不存在的组内端口冲突，阻止升级。 | [存储报告](storage.md) |
| ST07 | P2 | 悬空 config.toml 符号链接被当成缺失，初始化覆盖该链接。 | [存储报告](storage.md) |
| S01 | P2 | 服务恢复把磁盘定义当成 manager 已加载定义，两者不一致时恢复到错误程序。 | [服务报告](services.md) |
| S04 | P2 | 没有 lock 路径的无响应后台仍在监听时，stop 可以误报已停止。 | [服务报告](services.md) |
| S05 | P2 | 启动错误追加到没有轮转的 daemon.log，反复启动失败会持续增长。 | [服务报告](services.md) |
| S06 | P2 | 合法配置路径超过 Unix socket 专用长度预算，后台启动失败。 | [服务报告](services.md) |
| S07 | P3 | Linux/Windows 托管启动失败指向可能没有写入者的 daemon.log。 | [服务报告](services.md) |
| SE03 | P2 | Include 内的 Host 条件状态泄漏到外层文件，改变后续配置是否生效。 | [SSH/engine 原报告](ssh-engine.md) |
| SE04 | P2 | 已被首值覆盖的后续选项仍被拒绝，有效配置无法解析。 | [SSH/engine 原报告](ssh-engine.md) |
| SE05 | P2 | Host 参数无条件忽略大小写，可能匹配另一别名配置。 | [SSH/engine 原报告](ssh-engine.md) |
| SE06 | P2 | lexer 去掉普通反斜杠，改写合法路径。 | [SSH/engine 原报告](ssh-engine.md) |
| SE07 | P2 | 超大 keepalive 数值通过校验，后续时间运算导致连接任务 panic。 | [SSH/engine 原报告](ssh-engine.md) |
| SE09 | P2 | IdentityAgent 的 `$变量` 形式被当作字面文件路径接受。 | [SSH/engine 原报告](ssh-engine.md) |
| SE10 | P2 | macOS lsof parser 把 IPv6 wildcard 当成 IPv4，后续监听归属确认错误。 | [SSH/engine 原报告](ssh-engine.md) |
| C1 | P3 | 探测命令与普通命令的 request_id 校验不一致；仅确认输入契约问题，原 P2 下调。 | [CLI 续审](resume-cli.md) |
| C2 | P2 | 外部配置接受的超长 ID 超过历史记录/事件页预算，可导致记录丢失或空页跳过。正常 CLI 新建 UUID 不触发此前提。 | [CLI 原报告](cli-api.md) |
| C3 | P2 | status 使用不同 revision 的配置和运行时快照，筛选后可能输出不属于所选组/服务器的行。 | [CLI 原报告](cli-api.md) |
| C5 | P2 | watch 首次遇到 IPC 故障时，隐藏已有保存配置且不拒绝无效选择器。 | [CLI 原报告](cli-api.md) |
| C6 | P2 | 支持的 512 条规则叠加长错误文本可使全量状态超过 IPC 上限，单条查询也因先取全量而失败。 | [CLI 原报告](cli-api.md) |

确认项级别合计：**6 项 P1、28 项 P2、2 项 P3**。同根因的 up/down/remove、改名/新增无关服务器、事件时间/服务器/组错配均只计一次。SE07 在原 SSH 报告中曾建议与 store 时间边界项合并，但最终 ST01–ST08 不含该问题，因此本汇总单列一次，没有重复计数。

## 条件性风险与未确认候选

以下四项另列，不计入 36 项确认数：

- **S03**：Linux 首次安装失败恢复后删除 unit，但没有最终 daemon-reload。假管理器出现残留注册；真实 systemd 是否仍保留 unit 取决于引用、failed 状态和回收时序，没有做原生验证。
- **DS-C01**：候选目录 fsync 返回错误后，仍清除 applied 中的停删保护。模拟仅丢失未同步候选的重命名后，reload 恢复 running；保留控制头的对照仍 stopped。这是持久化故障模型，不是真实断电实验，见 [存储深入报告](deep-storage.md)。
- **DE-C01**：注入已结束的监督任务后，规则级 retry/restart/reconcile 不重建它；服务器 restart 可以恢复。本轮未找到正常用户操作导致该死组的触发链，见 [生命周期深入报告](deep-engine.md)。
- **DE-C02**：注入不响应取消的内存任务后，shutdown 耗时按每任务 5 秒累加；shutdown/server restart 在 abort 后未 join。已证明返回时 future 未析构，未证明真实 socket 泄漏或普通连接会进入此前提，见 [生命周期深入报告](deep-engine.md)。

其他未确认候选包括两次 SSH 配置 resolve 的快照差异、已进入握手阶段的停止是否进一步影响认证。已有排队取消证据不能替代这些验证。本次按用户要求没有继续相关后续实验。

设计文档中的可续接事件订阅、事件代次和快照游标契约尚不完整，与现有查询接口有差距，见 [接口设计差距](cli-api.md)。不把所有未实现的未来 UI 能力计为今天 CLI 的独立缺陷。

## 本次验证与完成范围

- 开始与收尾均对照旧 [source-sha256.json](evidence/source-sha256.json) 核对源码、文档和 Release；164 个清单条目均一致。没有修改生产代码或既有共享测试。
- 追问后的 [CLI 组合矩阵](deep-cli.md)：96 个唯一检查全部通过，包含 39 个拒绝/原子性案例；所有规则 stopped。
- 追问后的 [存储故障与边界矩阵](deep-storage.md)：7 项测试全部完成，覆盖并发编辑、累计保护记录、revision/schema 边界和 fsync 模型；断言包括确认缺陷存在。
- 追问后的 [生命周期矩阵](deep-engine.md)：28 个意图/状态/任务存活组合，以及 20 次快速 down/up、服务器重启、重载和退出期限对照完成。
- 追问后的 [日志身份与轮转检查](deep-history.md)：普通离线 CLI 复现和零/一/两次轮转的时序对照完成。
- `cargo build -p fwm --offline --locked` 成功，`cargo fmt --all -- --check` 通过。
- [事件补充测试](resume-events.md)：7 个案例断言通过。
- [CLI 续审](resume-cli.md)：17 项纯本地只读检查通过，临时配置/日志原字节保持不变。
- [存储/服务续审](resume-storage-services.md)：8 个纯假服务管理器事务对照通过。
- [运行生命周期续审](resume-engine.md)：新增计数错误的内存复现及停止、健康 retry、未知选择原子性、元数据代次等对照完成。

这些断言有些用于确认缺陷存在；通过不等于生产缺陷已经修复。没有重跑此前全部 438 项 Rust / 27 项 Python / SSH 测试，也没有把旧覆盖率当作本次验证结果。

首次恢复后的局部复扫没有新增，后续扩大组合与时序后又发现五项，故不能用“最后一轮零新增”代表找全。当前报告记录已验证的缺陷和明确边界；真实网络交错、原生三平台服务、断电等未覆盖范围仍保留，不能保证项目不存在其他 bug。
