# 恢复审查：engine 生命周期与活动连接计数

日期：2026-09-20。按指定进度恢复，范围为停止、restart/retry、失败状态与资源计数；没有修改生产代码或共享测试文件。本轮确认 **1 个新根因**，复用 2 个已有根因的证据。异步事件归属由主代理独立审查，不在这里重复计数。

新增实验仅调用当前生产源码中的内存控制函数和虚拟时钟。连接组任务是假对象，只等待取消；没有启动或连接 SSH、agent、daemon、TCP/Unix socket，没有执行远端 helper、信号、进程回收或安全测试。

## RE01 — P2：显式 restart 后永久保留旧代次的 active_connections

**触发**：规则有活动业务连接时执行 `forward restart`。`Engine::request_generation` 为该规则增加 generation，但保留已有 `active_connections`；旧连接结束时，其 `ConnectionGuard::drop` 发现 generation 已不相等，跳过减数。

**实际观察**：使用真实 `Rule::connection_opened` 建立两个计数 guard，调用真实 `Engine::restart`，然后释放全部旧 guard：

| 时点 | active_connections |
|---|---:|
| restart 前，两个旧 guard 存活 | 2 |
| restart 后 | 2 |
| 两个旧 guard 全部释放 | 2 |
| 新代次的一个 guard 打开后 | 3 |
| 新 guard 也释放后 | 2 |
| engine shutdown 后 | 0 |

旧计数成为新代次的固定偏移；即使新监听状态已回到 established、业务连接全部关闭，状态接口仍报告 2。重复 restart 若又有活动流量，可继续累积错误计数。这里确认的是状态计数错误，**没有**把它描述成实际流量或 socket 未释放，也没有声称 256 连接容量因此被占用：容量使用独立 semaphore。

**定位**：

- [lifecycle.rs:136](../../crates/fwm-core/src/engine/lifecycle.rs#L136)：递增并替换 generation，重设 state/error/next_retry，却没有对旧代次活动计数做归零或迁移。
- [state.rs:128](../../crates/fwm-core/src/engine/state.rs#L128)：只在 guard 与当前 entry generation 相等时执行减数，这是保护新规则的正常要求。
- [mod.rs:149](../../crates/fwm-core/src/engine/mod.rs#L149)：配置驱动的代次替换会构建新的 `ForwardStatus`，其中 active_connections 为 0，因此问题特指显式操作走的另一条代次更新路径。

期望统一代次替换的计数语义，确保旧连接完全结束后新代次没有残留计数。显式 `reconnect_server` 也先调用同一个 request_generation，随后 reconcile 保留相同 spec/key 的 entry，因此是同根因的源码候选；本轮独立实验直接验证的是规则 restart，未另启动连接做 server restart 端到端实验。普通传输失败后的自动重连本身不递增规则 generation，旧 guard 正常释放仍可减数；不能凭 RE01 推断普通重连也一定残留计数，本轮没有对此做真实流量实验。

完整结果：[resume-engine-result.json](resume-engine-result.json)。复现程序：[Rust 内存 fixture](resume-engine-probe.rs)、[临时源码复制与编译脚本](resume-engine-probe.py)。脚本将当前 fwm-core 源码复制到 `/private/tmp`，只在临时副本追加 fixture；使用已有 debug 依赖编译，退出后删除临时目录。结果保存依赖路径与所有生产源文件的 SHA-256。

## 复用的已有根因与补充控制流核验

**SE08 / 排队连接不响应全部停止**：保留 [queue-cancel.json](evidence/queue-cancel.json) 的已有实测：停止前 4 个连接占用许可，第 5 个在 down 后仍产生 TCP accept。本轮没有重跑。重新阅读当前 supervise 确认：[connection.rs:199](../../crates/fwm-core/src/engine/connection.rs#L199) 先取旧规则快照，[connection.rs:238](../../crates/fwm-core/src/engine/connection.rs#L238) 等待许可和后续 connect 只消费 group cancel，未消费 desired 变化。取得许可后也没有重检最新运行集合。停止后是否仍发认证、是否创建监听不在该已存结果证明范围。

**SE02 / 无关配置唤醒 needs_attention 或提前结束 backoff**：保留 [unrelated-retry.json](evidence/unrelated-retry.json) 的已有实测：新增无规则的另一服务器，旧规则名称保持不变，认证请求计数由 1 增到 2。本轮没有重跑。当前 [mod.rs:198](../../crates/fwm-core/src/engine/mod.rs#L198) 仍对保留组无条件 send_replace；[connection.rs:314](../../crates/fwm-core/src/engine/connection.rs#L314) 与 [connection.rs:329](../../crates/fwm-core/src/engine/connection.rs#L329) 把任意 desired.changed 当作离开暂停/退避的原因。普通元数据编辑本身不换 rule generation，已由本轮内存对照确认；不能将该事实误解为不会唤醒连接级循环。

## 排除的候选与最后一轮局部零新增

已用同一纯内存 fixture 验证：

- unknown selection 在改任何 generation 前失败，没有部分 restart；重复选择同一规则只作用一次。
- established 规则的 retry 返回 already_established；desired_state 为 stopped 的 retry 与 restart 均返回 stopped，无 affected 项。
- 仅改 name/group 保留 generation，原 guard 可以正常减回 0；不是所有 reconcile 都存在 RE01。
- 配置驱动的 down 替换状态 entry，active_connections 归零；旧 guard 释放不会再减当前代次的数。
- 最后一轮使用生产 `forward::backoff` 与 Tokio 虚拟时钟：进入 backoff 后 down、取消旧任务，advance 60 秒，再尝试旧代次 established 更新，结果仍 stopped、retry_count 0、next_retry 无值。普通监听退避任务正确取消；不能把排队连接问题扩大成所有停止路径都无效。

最后一轮还复扫了 connected 对 worker generation 的取消、完成 worker 的退出、intentional shutdown 的有界等待、local 流任务取消、remote 请求超时后的所有权保留，以及 channel-open 许可持有与迟到 channel 关闭的源码。在这些限定范围内 **零新增**；RE01 是前一轮新增项。已有 remote 注册早到流量/端点门禁问题仍按原 SE01 计，不重新包装成资源计数新项。

**边界**：这是一轮 engine 生命周期/计数的局部复扫，不是全仓库或真实 SSH 端到端的“零缺陷”声明。未重新验证已开始握手期间的 down、真实远端取消响应时序、外部 SSH 配置双重 resolve 的时间窗口。没有为这些未执行场景补写成功结论。
