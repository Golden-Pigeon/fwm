# SSH / engine / cleanup 修复后复查：已完成证据与交接

日期：2026-09-20。本文件整理已完成的读取和隔离实验，不包含生产修复。交接后仅按主代理限定追加 C01/C02 的文档与本机 socket 核验，剩余 engine 范围由主代理接手。

## 当前进度与证据边界

- 首轮广查确认 7 个根因，限定后续核验再确认原 C01/C02 两项，本代理共确认 **9 个根因**。其中 keepalive 上界问题是对 store 范围发现的补充证据，汇总时应合并计数。
- 主代理随后独立补证了“全部 down 后排队连接仍发起 TCP”这一项，本文件另列 SE08 并注明证据归属。
- **没有完成最后一轮全范围零新增复扫，不能宣称 SSH/engine 范围已经审查完毕。** 后续候选及未完成清单列在文末。
- 配置实验使用原生 `ssh -G -F 临时文件` 对照已有 `fwm-core` 构建产物的只读 resolver。不是声称所有案例均经 Release CLI 或真实登录认证复现。
- engine 实验使用当前 `fwm-core` 与临时回环 russh 服务器，真实完成 SSH 协议、密钥认证及客户端回调；服务器按实验需要确认/拒绝转发请求，**不绑定真实反向监听端口**。错误路由案例另使用真实回环目标 listener 检查是否接入。
- 所有测试密钥、SSH 配置和信任库均为临时生成；未使用真实远程服务器、默认 daemon、真实 agent 或操作系统 SSH 服务。已运行的四个 engine 子进程均正常退出，均调用 engine shutdown 并终止自己的模拟服务器。

证据文件：

- [配置对照结果](ssh-parser-round1.json)：7 个输入案例的配置、原生输出、fwm resolver 输出与退出码。
- [只读 resolver](ssh_resolve_probe.rs)：只执行 `ssh::resolve`，不发起网络连接。
- [engine 结果](ssh-engine-round1.json)：四个模式的完整 stdout/stderr 与退出码。
- [engine 协议复现](ssh_engine_probe.rs)：模式 `dual_gate`、`mapped_cancel`、`metadata_attention`、`huge_keepalive`。
- 主代理的排队取消补证：[queue-cancel.json](evidence/queue-cancel.json)。
- 主代理的无关提交唤醒补证：[unrelated-retry.json](evidence/unrelated-retry.json)。
- macOS IPv6 wildcard：[本机 lsof/生产解析器结果](evidence/macos-ipv6-wildcard.json)、[隔离复现脚本](evidence/macos_ipv6_wildcard_probe.py)。

两个 Rust probe 使用工作区已有 debug dependency rlib 编译为临时可执行文件，没有替换或修改 fwm 生产模块、共享测试或 Release binary。复现时应使用同一次构建产物的 `fwm_core`、`tokio`、`russh` 和 `serde_json` 依赖；历史输出保留在 JSON，临时配置目录已释放。

## 已确认项

### SE01 — P1 / 协议实测：取消旧 remote 尚未确认，新规则已接收旧监听的流量

**触发**：同一 SSH 共享连接，remote cleanup 显式为 off。旧规则监听 `127.0.0.1:35001`；停止旧规则，同时创建新规则监听等价的 `[::ffff:127.0.0.1]:35001`，目标改为另一回环服务。服务器拒绝旧取消，并在处理新注册请求时发送旧监听的回调；新注册本身最终被拒绝。

**实际**：配置校验允许“旧 stopped、新 running”，但 engine 的旧端点门禁没有识别 mapped IPv6 等价地址，提前发出新注册。新规则的路由在注册确认前已安装；旧地址找不到精确路由后，按唯一端口回退到新规则。复现输出为：

```text
old_callback_reached_new_target=true
new: state=backoff, active_connections=1, last_error=remote listener ... refused
old: desired_state=stopped, state=stopping
```

这里的 `true` 来自真实回环目标 socket 的 `accept`，不是仅推断状态。SSH trace 甚至显示新 mapped 注册先于旧 cancel 发出。新注册失败后，已接入的新目标连接仍被计入活动连接。

**期望**：等价端点的旧取消没有确认前，不注册或开放新规则路由；旧回调不能接入新目标。请求失败时，必须处置其确认前接收的业务通道。

**源码链路**：

- [connection.rs:406](../../crates/fwm-core/src/engine/connection.rs#L406)：运行时门禁仍直接比较 IP，未同步模型层的 IPv4-mapped 归一化及地址族规则。
- [remote.rs:81](../../crates/fwm-core/src/engine/remote.rs#L81)：发送 tcpip-forward 前插入业务路由。
- [connection.rs:114](../../crates/fwm-core/src/engine/connection.rs#L114)：未知地址在同端口唯一时回退到该规则。
- [remote.rs:105](../../crates/fwm-core/src/engine/remote.rs#L105)：注册失败移除路由，但本次早到业务连接使用的 traffic token 没有在该分支取消。

**同根因的另一表现，合并计数**：`dual_gate` 使用 `0.0.0.0:35000` 与 `[::1]:35000` 两条合法 remote/off/shared 规则。第一条 established，第二条永久 stopping，提示等待“previous owner”取消；删除第一条后第二条才 established。模型已经允许合法双栈组合，但运行时门禁仍误拒。

**边界**：实测是回环 russh 协议故障注入，不是宣称已在真实 OpenSSH 上诱发此取消时序；真实 SSH 的合法旧回调、失败响应和本地目标连接均通过生产 engine 路径处理。

### SE02 — P2 / 实测与控制流：任意配置提交可能重新触发无关失败连接

**已实测触发**：一条规则因公钥被拒绝进入 needs_attention；只修改规则名字，没有变更任何 SSH 参数或运行意图，再 reconcile。

**实际**：服务器观察到的公钥尝试从 1 次变为 2 次，状态经历新一次连接/认证后又回到 needs_attention。输出见 `metadata_attention` 的 `requests_before_rename=1`、`requests_after_rename=2`。

**根因不局限于重命名**：[engine/mod.rs:198](../../crates/fwm-core/src/engine/mod.rs#L198) 对所有保留的连接组无条件 `send_replace`；[connection.rs:314](../../crates/fwm-core/src/engine/connection.rs#L314) 遇到任何 desired 变化就离开永久错误暂停，[connection.rs:329](../../crates/fwm-core/src/engine/connection.rs#L329) 也会提前结束退避。主代理已补证“新增一个没有规则的无关服务器”：旧规则名字保持 attention，认证尝试仍从 1 次变为 2 次，见 [unrelated-retry.json](evidence/unrelated-retry.json)。勿与重命名样例重复计数。

**期望**：只对运行集合、连接参数、需要替换的规则代次或显式 retry/restart 作出重试反应。纯名称、标签或其他服务器的配置提交不应触发无关认证，也不应绕过其退避节奏。

**边界**：本代理实测的是 needs_attention 下重命名，无关服务器提交由主代理独立补证；backoff 提前结束已有确定调用链，尚未另外跑计时对照。健康 SSH 连接没有因这个案例断开，不应夸大为所有健康流都会重连。

### SE03 — P2 / 解析实测：Include 中的 Host 状态泄漏到外层文件

外层配置：

```sshconfig
Host audit
 Include /临时目录/included
 Port 23456
 User fixture
```

被包含文件：

```sshconfig
Host other
 Port 19999
```

原生 `ssh -G -F ... audit` 得到 `port 23456`、`user fixture`；fwm resolver 得到 `port 22`、当前环境用户名。被包含文件结尾的 inactive 状态错误地继续控制外层剩余行。

根因：[config.rs:300](../../crates/fwm-core/src/ssh/config.rs#L300) 递归传同一个 `active` 引用，返回时没有恢复外层 Host 条件。期望保留外层条件作用域，同时保留已经累积的有效配置值。它会影响端口、用户、身份文件或信任库，不只是输出格式。

### SE04 — P2 / 解析实测：已被更具体首值覆盖的选项仍被后续默认块拒绝

已对照三种案例，同根因合并：

```sshconfig
Host audit
 HostName 127.0.0.1
 ForwardAgent no
Host *
 ForwardAgent yes
```

将该选项替换为 `Compression no/yes` 或 `StrictHostKeyChecking yes/no`，结果相同：原生 `ssh -G` 采用安全/支持的第一个值；fwm 却报后一个值 unsupported 或可能关闭信任检查而拒绝整个配置。

根因：[config.rs:343](../../crates/fwm-core/src/ssh/config.rs#L343) 和 [config.rs:350](../../crates/fwm-core/src/ssh/config.rs#L350) 逐行立即拒绝，没有按这些选项的已取得首值决定实际生效值。

期望遵守 first-value 规则后，才判断最终有效值是否受支持；这里不是要求实现 ForwardAgent 或 Compression，而是不要拒绝已明确关闭它们的有效配置。[OpenSSH 配置规则](https://man.openbsd.org/ssh_config#DESCRIPTION)

### SE05 — P2 / 解析实测：Host 参数被无条件忽略大小写

配置 `Host AUDIT` 下写 `HostName 127.0.0.1`、`User fixture`、`Port 23456`，查询小写 `audit`。原生 `ssh -G` 不应用该块；fwm 却应用，得到另一套主机、用户和端口。

根因：[config.rs:425](../../crates/fwm-core/src/ssh/config.rs#L425) 的共享 wildcard matcher 使用 `eq_ignore_ascii_case`，同一函数同时用于 Host 和 known_hosts。修复应区分用途，不可为了 Host 直接改坏 known_hosts 匹配。

期望 Host 参数匹配与 OpenSSH 一致，避免配置条目意外覆盖另一别名。关键字本身不区分大小写，不等于其参数也不区分。[OpenSSH 参数规则](https://man.openbsd.org/ssh_config#DESCRIPTION)

### SE06 — P2 / 解析实测：合法反斜杠路径被 lexer 改写

`IdentityFile C:\Users\fixture\key` 在原生 `ssh -G` 输出中保持反斜杠；fwm resolver 输出的是配置目录下 `C:Usersfixturekey`。

根因：[config.rs:455](../../crates/fwm-core/src/ssh/config.rs#L455) 将任意反斜杠无条件当作去掉自身的转义前缀。这个行为会破坏普通 Windows SSH 配置路径，也会改变 POSIX 上合法的反斜杠文件名。

期望按 OpenSSH 的词法规则处理反斜杠，支持的路径应保留其字节含义。当前证据是原生 OpenSSH 解析对照；没有宣称已做 Windows 本机连接实验，也尚未扩展验证所有单双引号/注释组合。

### SE07 — P2 / engine 实测：合法保存的超大 keepalive 数值让 SSH 任务反复 panic

将 `keepalive_interval_secs` 设为 `i64::MAX`。`Config::validate()` 成功，但启动回环连接后 300ms 内捕获两次：

```text
overflow when adding duration to instant
```

规则进入 backoff，错误只剩 `JoinError`，未建立监听。不是整个审查程序崩溃，但该配置下每次新 SSH 尝试都不可用。

定位：[model.rs:448](../../crates/fwm-core/src/model.rs#L448) 只检查大于零；[ssh/connection.rs:143](../../crates/fwm-core/src/ssh/connection.rs#L143) 将数值直接构造 keepalive duration。锁定 russh 0.63.3 的客户端后续按 `Instant::now() + d` 重置计时，触发溢出。

期望保存时拒绝超出支持范围的时间值，并给明确字段错误；实现内部还需避免时间计算 panic。**这是 store 参数边界项的 engine 补证，整体报告只计一次。** 没有将 connect_timeout/max_delay 全部称作已复现 panic：读取 Tokio timeout 源码发现其使用 checked_add，相关其他大值行为仍需单独区分。

### SE08 — P2 / 主代理补证：全部停止后，排队旧任务仍发起新连接

主代理已独立运行 4 个占满全局许可的回环 SSH 握手加第 5 个排队连接；将 Engine 配置全部改为 stopped 后，第 5 个仍在许可释放时建立 TCP。证据见 [queue-cancel.json](evidence/queue-cancel.json)，独立 probe 由主代理维护，本代理未重复运行。

定位：[connection.rs:188](../../crates/fwm-core/src/engine/connection.rs#L188) 的 supervise 在等待许可前取得旧 rules；等待许可和 [connection.rs:257](../../crates/fwm-core/src/engine/connection.rs#L257) 的 connect 期间只监听连接组 cancel，不监听 desired 变化。全部 down 仍保留该组并 send_replace，不会取消组 token。

期望全部规则停止后取消排队与在途连接，或取得许可后重检最新运行集合。主代理当前实测证明了多余 TCP 连接；是否继续发送认证、是否产生后续监听，应以进一步证据说明，不能仅凭这次观测夸大。

### SE09 — P2 / 官方语义与解析实测：IdentityAgent 的 `$变量` 被接受后变成错误文件路径

原 C01 已确认。官方明确规定：`IdentityAgent SSH_AUTH_SOCK` 使用该环境变量；以 `$` 开头的值也表示从对应环境变量取得 socket 路径。[OpenSSH IdentityAgent](https://man.openbsd.org/ssh_config#IdentityAgent)

已有实验设置 `SSH_AUTH_SOCK` 为临时目录中的 `fixture-agent.sock`，配置为 `IdentityAgent $SSH_AUTH_SOCK`。原生 `ssh -G` 接受此配置；fwm resolver 也返回成功，却把 socket 变为 SSH 配置目录下名叫 `$SSH_AUTH_SOCK` 的字面文件，完整输出见 `ssh-parser-round1.json` 的 `agent_env_keyword`。

根因：[config.rs:389](../../crates/fwm-core/src/ssh/config.rs#L389) 的 source_path 只保留不带 `$` 的 `SSH_AUTH_SOCK` 特殊字符串，其他值先当相对路径规范化；[config.rs:179](../../crates/fwm-core/src/ssh/config.rs#L179) 也只处理不带 `$` 的形式，已经被改成绝对文件路径的值不会再进行环境变量选择。

期望支持这个合法形式，或者在解析阶段明确拒绝未实现的 `$变量` 形式；不能成功解析后选择另一文件路径。这不等于要求实现所有 OpenSSH 环境扩展。本次未启动或连接任何 agent，确认范围是官方语义及实际 resolver 结果，不声称已经完成签名认证复现。

### SE10 — P2 / 本机 socket 实测与确认链：macOS helper 把 IPv6 wildcard 误当 IPv4

原 C02 已确认。本机创建一只 `AF_INET6`、`IPV6_V6ONLY=1` 的临时 socket，实际 `getsockname()` 为 `::`，未接受任何连接。只查询当前 Python 进程、该临时端口的 lsof；其 `-F pfnT` 输出为 `n*:60042`。生产 `MacPlatform.parse_lsof` 将该记录变成 `local=["0.0.0.0",60042]`。端口由系统分配，数值仅为本次记录。

结果及完整命令在 [macos-ipv6-wildcard.json](evidence/macos-ipv6-wildcard.json)。该 socket 已关闭；没有执行远端 helper、回收、信号或服务操作。

根因：[remote_helper.py:263](../../crates/fwm-core/src/cleanup/remote_helper.py#L263) 对 `*:` 无条件替换为 IPv4 wildcard，没有保留地址族。默认 verified remote 请求 `::` 时，claim 记录仍为 `::`，但后续 [remote_helper.py:687](../../crates/fwm-core/src/cleanup/remote_helper.py#L687) 比较解析出的绑定地址与请求地址，必然不等，误报 `ownership_mismatch` / “sshd changed the requested bind address; check GatewayPorts”。remote.run 对确认失败会补偿取消并进入需要处理的状态。

期望读取并保留 lsof 的 IPv4/IPv6 类型信息，正确区分 `0.0.0.0` 与 `::`，而不是把正确的 IPv6 监听当成服务端改写地址。这里已实测本机 socket→真实 lsof→生产 parser，后面的 helper confirm 结果由明确比较链证明；没有另跑真实 sshd 的端到端 verified 转发。

## 已观察但未完成验证的候选

| 候选 | 已有事实与定位 | 尚缺的验证 / 边界 |
|---|---|---|
| C03 握手期间 down | 等待许可与 connect 均不消费 desired.changed 的代码已确认；排队部分由主代理证实并列 SE08。 | 建连已开始但握手/认证尚未结束时，down 能否阻止认证仍需独立验证；并入同一根因，不重复计数。 |
| C04 两次 resolve 的快照不一致 | engine/connection.rs:244 先 resolve 构造校验 handler；ssh/connection.rs:26 在 connect 内再次 resolve 选择实际端点/用户。外部 SSH 文件在两读之间变化时，信任上下文与实际端点可能不同。 | 只有明确代码顺序，尚未构造可重复的文件切换/握手对照。不是已确认安全绕过，也未计入缺陷数。 |

## 本次已读范围与未完成项

已广读：SSH resolver、Include/Host/路径与布尔选项、跳板展开与逐跳校验、私钥/公钥/证书/agent 认证分支、known_hosts 校验与写入；engine reconcile、server restart/reload、连接监督、规则代次、local/SOCKS 搬运、remote 注册/取消/早到回调、channel-open 补偿；cleanup context 代次与 helper 身份、锁、核验和 TERM/KILL 路径。

本轮读取后没有确认新缺陷的部分（不代表重新运行了原测试）：明确指纹的比较与 changed/revoked 阻止；RSA 禁用 SHA-1 的分支；plain key 与 agent 重复签名去重；迟到 direct-channel 的容量保留与关闭；远端 helper 对 owner/rule/generation、PID 出生身份及外部占用的核验。

已排除：手写非 UUID rule ID 并不会直接送远端 helper，cleanup/context.rs 会派生固定 UUID；不能把该输入误报成 helper UUID 校验失败。也未将 DESIGN 中尚属后续增强的健康探测、网络事件通知或独立 control_degraded 状态当成本轮缺陷。

主代理仍需接手：取消/刷新/断开交错下完整复扫；已开始握手的停止行为；规则切换服务器或连接模式的退役确认；资源上限和活动通道计数在失败/早到/取消时的一致性；上面 C04 的验证；最终按完整范围重扫一轮并记录是否零新增。

整理阶段只读取已有证据与源码。后续经主代理限定追加了一次本机 IPv6 socket/lsof 检查和官方 IdentityAgent 文档核验，没有启动 SSH 服务器、agent、daemon，没有发信号或修改真实服务。
