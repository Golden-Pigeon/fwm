# 数据流复查：local / SOCKS / remote target / 通道取消

日期：2026-09-20。接续深入生命周期矩阵，转向尚未充分覆盖的数据路径。未修改生产代码、共享测试或 REVIEW.md。

本轮确认 **1 个较窄的取消顺序问题 WE01**：已经取消的 remote 数据 worker 仍会开始 poll 新的目标连接操作。按 P2 建议评估；证据严格限定在目标连接分支，不把它扩大为每次成功连接或仍有业务转发。另保留一项跨代 pending-open 容量观察 WE-C01，未作为新缺陷计数。其余普通数据路径对照通过，关联复扫零新增。

证据：[whole-engine-result.json](whole-engine-result.json)、[仅临时副本优先取消的对照](whole-engine-cancel-priority.json)、[Rust fixture](whole-engine-probe.rs)、[构建脚本](whole-engine-probe.py)。

## 实验边界

脚本将生产 `state.rs`、`socks.rs`、`retry.rs`、`channels.rs`、`forward.rs` 复制到临时目录。只替换 SSH channel 的类型接口为 `tokio::io::DuplexStream` 包装，并开放 local/serve_local 的模块可见性。用于计数的目标 connect 包装先增加一个 atomic，再原样 await 原 Tokio `TcpStream::connect`，参数不变。所有改动逐项记录在 JSON 的 temporary_source_adaptations；生产源文件 SHA-256 也随结果保存。

本轮使用真实本机 TCP 和内存 duplex，没有执行 SSH 协议或认证，没有 agent、远端 helper、远程服务器、进程信号、API 探针。真实 russh 只作为已有依赖提供错误类型；传输接口是假对象。因此它验证 engine 调度、协议解析、复制和所有权逻辑，不声称完成 russh 真实协议层端到端回归。`remote::run` 在 fixture 中是禁止调用的 stub，避免误触远端监听/helper；`forward::serve_remote` 的本机目标连接逻辑来自生产文件。

## WE01 — P2 建议：预先取消的 remote 数据 worker 仍开始目标连接

**触发链**：Handler 已接受回调并准备运行数据 worker，停止规则使 route.cancel 置位，然后尚未首次运行的 worker 得到调度。Handler 的先前检查不能覆盖这个时序：它在 [connection.rs:132](../../crates/fwm-core/src/engine/connection.rs#L132) 检查 cancel 后，还会 await reply.accept，再在 [connection.rs:146](../../crates/fwm-core/src/engine/connection.rs#L146) spawn worker；spawn 后至首次 poll 之间也可以发生取消。

**直接核验**：每次调用 `serve_remote` 前就设置取消 token。目标为同一临时回环 listener，24 次普通调用中，生产逻辑有 **10 次**进入并 poll 目标 `TcpStream::connect` future；仅在临时副本中将 select 设为优先处理已就绪的取消分支后，24 次是 **0 次**。两套全量数据路径对照均 exit 0。

| 项目 | 当前生产逻辑 | 临时取消优先对照 |
|---|---:|---:|
| 调用前 cancel 已置位 | 24/24 | 24/24 |
| 取消后仍开始 poll 新目标 connect | 10 | 0 |
| 本轮目标 accept | 0 | 0 |
| 应用数据字节 | 0 | 0 |
| 最终 active_connections | 0 | 0 |

这是无偏 select 的竞争：[forward.rs:190](../../crates/fwm-core/src/engine/forward.rs#L190) 在 cancel 已 ready 时仍可先 poll 建连分支；[forward.rs:181](../../crates/fwm-core/src/engine/forward.rs#L181) 会据此开始一次全新的目标连接。锁定 Tokio 1.53.1 对字面 IPv4 地址在就绪地址解析后进入 connect 调用，随后等待 socket 可写；已有取消无法保证阻止这一步。应在数据 worker 开始前重检并优先处理取消。

**边界**：最新可复查文件里目标 accept 是 0，不能说 10 次都已完成 TCP 连接。前期未加 poll 观察的运行曾在工具 stdout 中出现 3 次 accept/24 次，也有 0 次，说明 accept 可见性依赖时序；最终结论不依赖这个未保留完整结果的旧数值。已确认的是预先取消仍启动目标连接操作，不是业务数据继续转发，不是长期 socket/计数泄漏，也没有测试 DNS 副作用或真实远程环境。

**去重**：SE08 在连接监督层遗漏 desired 变化，涉及排队 SSH 建连；WE01 使用已经取消的 route token，发生在每条 remote 业务流的本机目标建连阶段。SE01 是 remote 注册/路由归属错误；RE01 是显式换代后的计数残留。这几项触发与修复点不同。本轮不重复计入它们。

## WE-C01：旧 pending open 与重启后的新监听并存，暂不单列缺陷

只用了两个普通客户端：第一个 direct-open 还未确认时停止 local worker，本地客户端关闭、active_connections 回到 0，但用于迟到确认补偿的 open task 仍存在。重启同一规则、保留共享 handle 后，第二个客户端可以产生新的 open task，pending 总数从 1 到 2；随后令假 transport closed，两个 pending 都退出。

这与 [channels.rs:12](../../crates/fwm-core/src/engine/channels.rs#L12) 保留未确认请求及其许可的设计一致。同时 [forward.rs:92](../../crates/fwm-core/src/engine/forward.rs#L92) 每次重建监听都会创建新 semaphore，旧请求持有旧许可集合，因此“每代监听的限制”与“同一规则跨代 pending 总数”不是同一个计数范围。

当前证据没有超过生产 256 容量，没有做数量压力实验，不能声称已实测容量越界或资源耗尽。把旧 pending 的有意保留单独报成泄漏也不正确。后续若明确要求跨代统一上限，应单独设计验证；本轮将这一点作为容量边界记录，不加入确认数量。

## 普通功能覆盖及具体排除

| 覆盖 | 已执行对照及结果 |
|---|---|
| local 双向半关闭与反压 | TCP↔内存 channel 双向各 8192 字节，duplex 容量仅 64 字节；客户端先半关闭和目标先半关闭两种顺序均保留反向剩余数据，最终许可归还 |
| SOCKS 地址解析 | IPv4、IPv6、域名三种格式；逐字节分片写入；域名仍以原 hostname 交给 channel 接口，没有本机 DNS 替换 |
| SOCKS 普通错误响应 | 不支持命令/地址格式、非零 RSV、零端口、空域名、截断请求、无可用认证方法；返回既有明确回复或关闭，不产生成功状态 |
| SOCKS 协商与业务数据紧接 | negotiation + CONNECT + payload 一次写入；payload 原样到达目标，成功回复位于返回 payload 前 |
| SOCKS 未完成协商超时 | 只发版本字节后超时，本地连接关闭、许可归还，没有创建 channel |
| 目标拒绝与打开超时 | 拒绝返回 SOCKS 5；超时返回 SOCKS 4。拒绝立即归还容量；超时按设计持有 pending-open 许可，transport closed 后归还 |
| 迟到 channel | 本地 receiver 已取消时保留许可；迟到成功结果变成 stream 后立即关闭，许可归还，共享假 handle 不关闭 |
| local 监听隔离 | 第一个目标打开失败后 listener worker 仍存活；下一个客户端正常双向通信，规则仍 established，active_count 最终为 0；stop 后同一监听地址可重新 bind |
| remote 本机目标拒绝 | 真实临时回环目标端口无 listener，连接失败只发 target error 事件、关闭该 channel，规则仍 established，active_count 回到 0 |
| remote 半关闭与活动流取消 | 本机目标正常处理半关闭后的回复；活动流取消同时关闭内存 channel 与目标 TCP，按 Handler 相同 permit 作用域归还容量，active_count 回到 0 |

worker 退出逻辑也做了源码关联检查：local 的绑定失败进入 backoff，accept 错误记录并继续；普通数据流错误只影响 JoinSet 内单个业务任务，不退出监听 worker。connected 对已完成 forward worker 的处理没有区分 JoinError；该异常退出观察没有新的正常用户触发链，本轮没有人为制造 panic 后把它升级为普通功能根因。已结束组监督任务的恢复缺口仍归 [deep-engine.md](deep-engine.md) 的 DE-C01。

## 最后一轮及剩余限制

确认取消分支问题后追加了 SOCKS pipelining/不完整协商超时、remote 正常半关闭/活动流取消，再比较当前逻辑和临时 cancellation-priority 副本。随后复扫相关 worker 退出、许可转移、通道迟到关闭和 guard 释放路径。最后一轮在 WE01 和已列容量观察之外 **零新增**。

这覆盖本次指定的数据路径，未重跑上一轮生命周期矩阵；也未验证真实 SSH channel window、真实协议 EOF/CLOSE、系统异常断网或远端 listener 注册。标准库/duplex 半关闭结果不能冒充这些尚未执行的端到端验证。
