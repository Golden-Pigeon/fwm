# 深入审查：运行状态、监督任务存活与退出期限

日期：2026-09-20。本轮在 [resume-engine.md](resume-engine.md) 之后继续，保留 RE01、SE02、SE08、SE01 的原计数，未做生产修复，也未修改 REVIEW.md。

**结论**：更深入的正常控制流程没有确认新的独立根因；内存故障注入确认两类条件性恢复/退出缺口，见 DE-C01、DE-C02。它们的生产代码行为可重复，但本轮没有找到普通用户操作如何自然导致“监督任务异常结束”或“监督任务始终不响应取消”，因此不将这两项直接加入正常流程确认缺陷数量。

完整证据：[deep-engine-result.json](deep-engine-result.json)，程序：[deep-engine-probe.rs](deep-engine-probe.rs)、[deep-engine-probe.py](deep-engine-probe.py)。脚本复制当前 fwm-core 到临时目录，仅在副本追加 fixture，链接已有 debug 依赖；结果记录依赖路径、源码 SHA-256 与退出码。

所有实际 supervisor 都受 **0 许可 semaphore** 阻断，无法运行到 SSH resolve/connect。其余是内存 watch/cancel/pending 假任务。server restart 与 config reload 的只读解析使用临时空配置和未使用的临时 known_hosts 路径；没有访问真实凭证，没有 SSH/agent/socket、远端 helper、信号或 API 探针。

## DE-C01：监督任务已经结束时，规则级 retry/restart 仍只向失去消费者的 watch 写入

**条件**：组仍存在于 `Engine.groups`，但其 JoinHandle 已结束。测试通过取消内存监督任务注入这个状态；它不证明生产连接路径已经发生此异常。

28 个组合覆盖了 2 种运行意图 × 7 种运行状态 × alive/completed 两种任务状态。对已完成任务：

- desired running、state needs_attention/backoff/unverified 等可重试状态：retry 仍将规则列入 affected，状态改 starting，但没有新任务读取更新。
- desired running、state starting：retry 只返回 already_starting，即使该组已经没有活任务。
- desired running、state established：retry 只返回 already_established，同样不检查任务是否仍存在。
- 规则级 restart 将 running 规则列入 affected，但不重建已结束的组任务。
- 随后的 reconcile 对同一连接 key 继续保留相同 task ID，is_finished 仍为 true。

定位：[lifecycle.rs:99](../../crates/fwm-core/src/engine/lifecycle.rs#L99) 的可重试判断只参考状态；[lifecycle.rs:127](../../crates/fwm-core/src/engine/lifecycle.rs#L127) 只更新现有组的 desired；[mod.rs:197](../../crates/fwm-core/src/engine/mod.rs#L197) 判断 key 存在就 send_replace，没有检查任务完成结果或重建结束任务。

**关联边界已追查**：显式 `reconnect_server` 会移除旧组再 reconcile。内存对照确认它替换 dead task，产生不同 task ID，替代任务保持存活并等待 0 许可 gate。因此问题限定在普通 reconcile 和规则级 retry/restart；不是所有恢复入口都失效。desired stopped 的所有矩阵组合仍正确跳过 retry/restart，不会凭这个条件改变运行意图。

期望若纳入韧性修复：组任务完成应可观察，需区分正常退休与异常结束；显式恢复不能在无人消费更新时报告已调度成功。当前按条件风险记录，不声称已找到真实 panic 或普通用户可触发的死组。

## DE-C02：超时强制中止的任务未 join，shutdown 的期限还会逐任务累加

**条件**：任务异步等待但不响应 cancellation token。使用内存 pending future 和 Drop 计数器注入，不使用系统进程或网络资源。

| 入口/条件 | 虚拟耗时 | 返回时已 Drop | 再 yield 后已 Drop |
|---|---:|---:|---:|
| shutdown，1 个不响应取消的 retired task | 5001 ms | 0/1 | 1/1 |
| shutdown，3 个不响应取消的 retired task | 15003 ms | 2/3 | 3/3 |
| reconnect_server，1 个不响应取消的旧组 task | 约 5001 ms | 0/1 | 1/1 |

Tokio 定时器的约 1ms 粒度差已包含在断言范围内。

源码根因：

- [mod.rs:245](../../crates/fwm-core/src/engine/mod.rs#L245) 在循环中为每个 task 重新设置 5 秒 timeout，而不是对整次退出设置一个共享期限。多个不响应取消的任务可以把总耗时扩大到约 `N × 5s`。
- [mod.rs:250](../../crates/fwm-core/src/engine/mod.rs#L250) timeout 后只 abort，不 await 被中止任务，随后就写 stopped/active_connections=0。
- [lifecycle.rs:62](../../crates/fwm-core/src/engine/lifecycle.rs#L62) 的服务器 restart 也只 abort；随后 reconcile 可以创建替代组，而旧 future 的析构尚未运行。

这里已确认的是“返回时旧 future 尚未析构”和“总期限逐项累加”，**不是实际 listener/socket 泄漏**。测试里的 Drop 在下一次调度就完成，未观察到永久占用。正常 production connected 已有 3 秒 worker 等待和 1 秒 disconnect 上限，正常响应取消的组本轮均能退出；不能把注入 pending future 的结果扩大成每次重启都需 5 秒或正常组都会遗留监听。

DESIGN.md:380 要求正常停止等待连接和搬运任务退出，并设置总退出期限；这一项是该故障条件下的边界缺口。若修复，需要在共享退出预算下取消/收集任务，并 await abort 的完成；调用方才有明确的资源释放屏障。与 DE-C01 分开记录，因为这是退休/退出路径的问题，并非活跃组的恢复调度。

## 正常控制流程及关联边界

本轮直接执行了以下生产方法，断言全部通过：

| 场景 | 已验证结果 |
|---|---|
| 共享组内只 retry 异常规则 | 仅 r1 generation 改变，健康同组 r0、停止 r2、其他服务器 r3 不变，组 task ID 不变 |
| 只 restart 一条健康规则 | 仅 r0 generation 改变，共享组与其他服务器 task ID 不变、未取消 |
| 服务器 restart，包含健康/异常/停止规则 | 影响该服务器的 running r0/r1，停止 r2 按 Engine API 合约跳过；其他服务器 task ID 和 generation 不变 |
| 20 次连续 down/up，监督任务未消费中间值 | 最终规则为最新 generation，旧 Rule 的 needs_attention/错误更新被忽略，组数保持 2 |
| defaults.retry.max_delay_secs 改变 | 原组 token 全取消，所有受新策略影响的规则换代，新组仍受 gate 阻断；不把策略变更需要更换组误报为 SE02 |
| SSH reload 后解析结果不变 | 组 task ID 保持不变；第一次将 fixture 的合成 fingerprint 替为真实临时解析结果后，再以第二次 reload 验证稳定性 |
| 所有规则初始为 stopped | 真实 supervise 把状态保持 stopped，组任务活着等待变化；不会因为存在空闲组就误报为任务泄漏 |
| 清空 forwards | 组和状态均移除；旧任务取消并完成，下一次 reconcile 会清空已完成 retired handles |
| 显式服务器 restart 恢复注入的 finished task | 旧 task 被替换，替代任务存活；这是 DE-C01 的恢复边界 |

服务器 restart 这里调用的是 Engine API，所以 stopped 规则按其注释跳过；CLI 的“restart 启动已停止规则”由上层先持久化 running 意图实现，不将两层不同前置条件混为缺陷。

再次检查了 request_generation 的全部选择校验、重复 ID 去重、generation 上界检查、runtime_equal 的名称/分组忽略规则、连接 key 对服务器与 defaults 的隔离、retired handle 清理、connected 的 worker 退休与代次判断。未发现 RE01/SE02/SE08/SE01 之外新的正常流程根因。

最后一轮增加了“服务器 restart 能替换 dead task”的反向对照，重跑全套 28 组合和正常操作矩阵；**本 engine 限定范围零新增**。这比上一轮只测普通 backoff 更广，但仍不是全仓库审查完成声明。真实传输成功/失败、真实资源释放和进程退出不在本轮实验范围。
