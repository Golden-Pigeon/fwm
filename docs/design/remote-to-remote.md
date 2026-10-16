# 双服务器端口转发设计

日期：2026-10-16。状态：已按本提案实现（schema v4、API 协议版本 2、`Tunnel::Relay`）。使用说明见 [usage](../usage.md)；本文保留原始契约供评审对照。

[返回总体设计](../../DESIGN.md) · [现有命令](../usage.md) · [验证约定](../../AGENTS.md#validation)

本文用于独立实现与评审。第 1 节是已确认的需求；后续章节为实现契约。带 `--src-server`、`--tgt-server` 的命令自本版本起可用；交付版将目标主机并入 `--tgt [HOST:]PORT`（评审修订，见 3.1 节说明），未提供单独的 `--tgt-host`。

## 1. 已确认的需求与方向

fwm 运行在第三台机器 M，M 能通过 SSH 连接服务器 A 和 C。用户希望在 A 上访问 `localhost:B` 时，实际连接到 C:D。

- `src` 始终表示监听入口：服务器 A、端口 B。
- `tgt` 始终表示目标服务：服务器 C、端口 D。
- localhost 可以省略，沿用现有端口简写，不要求显式写地址。
- 新能力应表现为一条规则，统一保存、启停、编辑、等待和观察。
- 本提案不实现代码，也不创建或启动实际隧道。

例：`src=A:8080`、`tgt=C:9000` 表示访问 A 的 `localhost:8080`，连接到 C 的 `localhost:9000`。不要根据“把某端口转发到另一端口”的自然语言顺序反转监听端和服务端。

## 2. 拓扑、适用范围和现有基础

```mermaid
flowchart LR
    Client["A 上的应用"] <--> Listen["A：127.0.0.1:B"]
    Listen <-->|"SSH forwarded-tcpip"| Manager["M：fwm daemon"]
    Manager <-->|"SSH direct-tcpip"| SSH["C：SSH 服务"]
    SSH <--> Target["C：localhost:D"]
```

两条 SSH 连接都由 M 主动建立。A、C 无须相互访问，也无须相互持有登录凭证。数据经过 M；M 的网络、带宽和持续运行是可用性的条件。关闭 CLI 或终端不会停止 daemon，但停止 daemon 或关闭 M 会中断转发。

目标地址从 C 的网络环境解释；目标域名交给 C 解析，`localhost` 指 C。监听地址从 A 的网络环境解释。C 的 SSH 登录地址与目标服务地址是两个独立概念。

首版范围为固定目标 TCP 转发，支持现有 IPv4/IPv6 地址约定。不包含 UDP、双远端 SOCKS、应用层代理或断线后恢复已有 TCP 会话。若要求流量不经过 M，应改为在 A 或 C 部署 fwm；不是此模式的一个隐含优化。

截至本文分析时，代码已有以下基础：

| 位置 | 当前行为与扩展点 |
| --- | --- |
| [model.rs](../../crates/fwm-core/src/model.rs) | `ForwardSpec` 只有一个 `server_id`；`Tunnel` 有 Local、Remote、Dynamic、RemoteDynamic。 |
| [engine/mod.rs](../../crates/fwm-core/src/engine/mod.rs) | `Engine::reconcile` 按单服务器连接配置组织规则与运行代次。 |
| [engine/forward.rs](../../crates/fwm-core/src/engine/forward.rs) | Local 经 SSH 打开目标通道；`serve_remote` 从 M 使用 `TcpStream::connect` 连接目标。 |
| [engine/remote.rs](../../crates/fwm-core/src/engine/remote.rs) | 申请、取消远端监听并管理 verified cleanup。 |
| [engine/channels.rs](../../crates/fwm-core/src/engine/channels.rs) | `open_direct` 保留待确认请求的所有权，关闭超时或取消后迟到的通道。 |

现有两条 Local/Remote 规则可以通过 M 的本地端口串接，但引入额外监听且没有双端依赖状态。因此推荐进程内连接两条 SSH 通道，不生成两条隐藏规则或本地中转端口。

## 3. CLI 契约

### 3.1 创建与地址默认值

```sh
# A 的 localhost:8080 → C 的 localhost:9000
fwm add --name a-to-c --src-server A --src 8080 --tgt-server C --tgt 9000

# 两端同端口
fwm add --src-server A --tgt-server C --port 8080

# 显式指定 A 的监听地址
fwm add --src-server A --src 0.0.0.0:8080 --tgt-server C --tgt 9000

# 通过 C 访问 C 所在网络内的目标服务
fwm add --src-server A --src 15432 --tgt-server C --tgt db.internal:5432

# 仅保存，不启动 daemon 或 SSH 连接
fwm add --name a-to-c --src-server A --src 8080 --tgt-server C --tgt 9000 --disabled
```

默认监听地址为 `127.0.0.1`，默认目标主机为 `localhost`，与当前创建行为一致。目标 localhost 的 IPv4/IPv6 解析由 C 决定；需要固定地址族时使用 `--tgt 127.0.0.1:PORT` 或 `--tgt [::1]:PORT`。

`--src` 接受 `[监听IP:]端口/端口列表`，IPv6 监听地址加方括号；`--tgt` 接受 `[目标主机:]端口`，与 `--src` 形状对齐（评审修订：原提案新增 `--tgt-host` 以避免改动旧语法，评审认为绑定侧已有地址前缀而目标侧缺失主机槽不对称，故统一并入 `--tgt`，纯端口用法保持不变）。`--port` 沿用两侧同端口及范围展开规则，不能与 `--src/--tgt` 混用。端口必须为 1–65535，不自动换端口。

创建时必须同时提供 `--src-server`、`--tgt-server`；两者可以是保存的名称、ID 或 SSH alias。缺任一端时立即报错，不隐式使用 M。不需要再指定 `--remote`，并禁止与 `--server`、`--local`、`--remote`、`--dynamic`、`--remote-dynamic` 混用。

沿用自动命名、`--name`、`--group` 和端口范围的现有规则。批量创建一次验证、一次提交；网络就绪可以逐规则不同。两个新 alias 与规则必须在同一事务中保存，失败不能留下半套服务器记录。两端引用同一 profile 时允许，合并 profile 创建但保留两端角色。

复用现有 `--ssh-config PATH` 作为本次新 alias 的配置来源，应用到两端并保持已有 profile 的冲突检查。两端需要不同 SSH 配置时，先分别创建服务器 profile；不在首版增加两套 SSH 认证参数。

### 3.2 等待和返回结果

```sh
fwm add --name a-to-c --src-server A --src 8080 --tgt-server C --tgt 9000 --wait
fwm up a-to-c --timeout 30s
```

| 用法 | 契约 |
| --- | --- |
| 不加 `--wait` | 保存运行意图并发起后台协调，不等待隧道就绪；命令返回不表示连接成功。 |
| `--wait` | 默认最多等待 20 秒，沿用现有默认值。 |
| `--timeout 30s` | 指定等待期限，同时隐含 `--wait`。 |
| 超时或启动失败 | 保留已保存的规则与运行意图；可重试故障由后台继续处理，信任等问题等待用户修复。 |
| `--disabled` 与等待同时使用 | 参数冲突，保持现有行为。 |

双端就绪必须同时满足：A、C 的 SSH 都已认证并仍属当前运行代次，A 的监听申请成功，入站路由已安装，启用 verified cleanup 时所有权已确认。不得仅凭 A 的监听成功报告 `established`。

这不检查 C:D 的应用健康，不主动建立业务探测连接。每次访问才打开目标通道；目标拒绝连接时报告该数据连接失败，不把应用故障自动升级为 SSH 重连。等待覆盖一个批次时使用一个总期限。

保持一次性 JSON 命令只输出一个最终对象。等待失败返回失败结果，并保留现有 `data.saved:true` 表达已保存；不能先输出一个成功对象，再输出第二个失败对象。watch/follow 继续输出 JSON Lines。

### 3.3 编辑、选择与删除

```sh
fwm edit a-to-c --src 8081
fwm edit a-to-c --tgt 9001
fwm edit a-to-c --tgt-server another-target
fwm edit a-to-c --tgt db.internal:5432
fwm down a-to-c
fwm up a-to-c --wait
fwm restart a-to-c
fwm remove a-to-c
```

编辑已存在的双服务器规则时允许只提供一侧参数，另一侧保持不变。端口编辑保留地址；显式地址编辑替换地址；`--port` 修改两端端口。重命名或调整组名保留 ID 且不触发重连。端点或任一侧连接配置变化会关闭该规则现有数据连接。

首版不提供旧规则与双服务器规则之间的隐式类型转换；不适用参数应明确报错。group 编辑继续原子处理，混合规则类型中有任何成员不支持参数时整体拒绝。

`status/logs/up/down/retry/restart --server X` 均应识别任一侧引用 X 的规则，并按规则 ID 去重。两端同为 X 时不重复操作。`retry` 仍跳过已就绪和已停止规则。服务器连接配置变化或显式服务器重启应影响相关规则；仅服务器改名不得重连。

删除服务器的引用检查必须涵盖两端。保留现有删除/显式级联策略，不能留下悬空目标引用；若执行级联删除，仍按规则整体停止并清理 A 的监听。历史事件应保存两端 ID 和当时名称，使按任一服务器查询可跨重命名、停止和删除。

## 4. 持久化模型与兼容

推荐增加独立 `Tunnel::Relay` 变体，旧变体语义不变：

```rust
// 结构示意，非已实现代码
Relay {
    listen: SocketAddr,
    target: Endpoint,
    target_server_id: String,
}
```

`ForwardSpec.server_id` 在此类型中表示 src 服务器 A；`target_server_id` 表示 tgt 服务器 C。仍使用一个规则 ID、group、desired_state。序列化采用 `kind = "relay"`，拒绝未知或属于其他类型的字段。同步修改现有自定义 wire 解码层，不只给枚举增加分支。

提供统一的“规则引用哪些服务器”和“监听位于哪台服务器”查询，供校验、选择、删除、状态和调度使用。`is_remote()` 若承担“远端监听”语义则包含 Relay，但不能因此把它送入原有只连接 M 本地目标的分支。

监听冲突根据 src 的稳定服务器 ID 与监听地址判断，包括与既有 Remote/RemoteDynamic 规则的冲突；目标端口不是监听占用，不应参与冲突检查。保留运行规则/同组规则的现有冲突条件。不同 alias 指向同一物理主机的冲突仍以真实监听申请结果为准。

配置 schema 使用新的版本（当前文档记录为 3，应分配下一个可用版本）。保留备份、稳定 ID、旧运行意图、未应用草稿与停止/删除记录；不自动把两条既有规则识别为一条 Relay。旧程序读取新 schema 必须明确拒绝，不能忽略目标服务器字段后连接 M。

扩展 API 前明确协议版本/能力协商：旧客户端或旧 daemon 不支持 Relay 时应在变更提交前得到不兼容错误。新增 kind、双端状态和事件字段须纳入协议契约；不能假设给 JSON 加字段即与严格解码的旧客户端兼容。旧配置、旧规则与旧命令在新版本中保持行为。

## 5. 引擎与资源管理

### 5.1 规则级协调器

为 Relay 增加规则级协调器，管理两个 SSH 会话和一个逻辑监听。复用现有 SSH 解析、主机验证、认证、ProxyJump、心跳、重试和远端清理能力，不重写 SSH 客户端或建立第二套规则存储。

首版两侧均使用规则专用连接，以隔离重启和 A 端 verified cleanup；即使两端是同一 profile，也按角色隔离会话。配置默认 dedicated，显式 shared 暂时拒绝，不静默忽略。连接身份包含规则 ID、端点角色、服务器连接配置、SSH 路由指纹及相关重试配置；显示名称不影响身份。

两端连接可并行建立，但连接尝试分别遵守全局并发限制，每个角色最多一个进行中的尝试。不要在持有一个全局连接许可时等待另一端获取许可，避免并发规则形成死锁。等待另一端失败重试时可保留健康的 SSH 会话，不借故重启其他规则。

`Engine::reconcile` 需要同时跟踪两端配置与代次；不能只把 Relay 加到原有单 profile 的 `match` 中。停止 daemon 时协调器、通道任务和迟到协议操作都必须纳入现有有界关闭屏障。

### 5.2 转发与取消

1. 本地完成参数、引用、端口冲突和 SSH 配置校验后保存；启用操作按现有规则启动 daemon。
2. 建立 A、C 的 SSH 连接，分别验证主机密钥与认证；两侧独立支持已实现的跳板配置。
3. 两端可用后，在 A 建立 verified lease（若启用）、请求监听并确认所有权。处理注册前后早到回调，只有路由可用后才报告就绪。
4. A 的 `forwarded-tcpip` 到达时检查监听、规则代次、取消状态和容量；在异步任务中通过 C 打开 `direct-tcpip`，把两个 `ChannelStream` 双向复制。
5. 任一数据通道结束或规则取消时释放双方资源，保留半关闭、反压、超时和活动连接统计。一次业务连接计数一次，不因两个 SSH 通道计两次。

Handler 回调不等待目标连接或执行复制。现有每规则 256 个连接的限制应覆盖正在打开、正在传输及尚未确认的请求，避免超时后不断释放许可造成无限后台任务。复用 `open_direct` 的迟到结果关闭机制；不得把 drop Future 当作取消已发送的 SSH 请求。

远端 originator 信息仅作为 SSH 元数据传递，不能用它代替鉴权或声明目标应用看到原始客户端 IP。源监听匹配继续要求正确地址、端口及会话，不按“端口唯一”宽松路由。

### 5.3 故障与恢复契约

| 事件 | 必须发生的行为 |
| --- | --- |
| C 的 SSH 断开 | 立即撤销就绪、关闭相关数据连接、拒绝新业务通道，尝试取消 A 的监听；恢复 C 后重新协调监听。 |
| A 的 SSH 断开 | 撤销就绪、关闭相关数据通道；重连后按已有 verified 流程处理可能残留的监听。 |
| A 监听被占用/策略拒绝 | 报告 source/listen 阶段失败，按现有重试及清理策略处理，不报告就绪。 |
| C:D 拒绝连接或解析失败 | 只失败该业务连接，记录 target/connect 阶段；SSH 和监听健康时规则仍可接收后续连接。 |
| 主机密钥或认证失败 | 标记端点与失败阶段，进入需处理状态；不自动接受密钥，不降级校验。 |
| 用户停止/删除 | 持久化意图，停止接收和复制，取消 A 监听并回收专用会话；后续重试不能复活规则。 |
| 取消超时或结果未知 | 拒绝继续路由，保留恢复/占用证据并报告 stopping/unverified 等相应状态；不宣称端口已经释放。 |
| 旧代次结果迟到 | 不更新新代次状态；迟到监听成功必须补偿取消，迟到通道必须关闭。 |

监听取消未确认时禁止再次申请同一逻辑监听；依靠确认结果或 verified recovery 再继续。关闭 SSH 会话不能替代远端清理成功的证据。停止/删除后若用户可见规则已移除，仍按现有机制保留必要恢复记录和失败事件。

verified cleanup 只管理 A 的监听会话，继续要求同 manager、规则、generation 和进程身份的证据。不得按端口、进程名或裸 PID 杀进程。C 不创建远端监听，无需为目标连接另建清理 helper。A 不支持 helper 时明确报告，只有显式选择现有 cleanup off 策略才关闭该保障。

不承诺断线后续传已有业务连接；恢复后供新连接使用。健康 C 会话可在 A 恢复期间保留，但不能无限保留已失去 A 入站通道的业务资源。

## 6. 状态、诊断和安全边界

人类输出保持一条规则一行，源显示 `A:8080`，目标显示 `C:9000`，非默认地址保留。不要继续把两端都标为无主机上下文的 localhost。JSON 保留完整地址、两端稳定 ID 和名称、聚合状态以及可定位端点/阶段的错误信息；字段最终名称需在协议测试中固定。

聚合运行状态复用已有 starting、established、backoff、needs_attention、stopping、stopped、unverified。双端细节用于解释聚合状态；任一必要端点失效就不能保持 established。配置与运行快照使用相同 revision，离线保存的 Running 不能显示为已连接。实时状态、watch、历史日志和 daemon 重启恢复均应认识双端引用。

默认回环监听只供 A 本机访问。用户选择非回环绑定时仍受 A 的 SSH `GatewayPorts`、转发权限及防火墙约束。监听协议成功不证明外部可达或实际绑定范围。M 上保存两端凭证引用，沿用当前 OS 用户和 IPC 鉴权边界；不要把私钥复制到 A/C 或隐式启用 agent forwarding。

查询、诊断、补全、离线编辑不得启动 daemon。`--src-server`、`--tgt-server` 补全复用 `clap_complete` 与现有保存服务器候选，不发起 SSH 或 DNS 请求，不新增手写 shell 解析。

## 7. 实施分层与验收

实现顺序建议：模型与迁移/协议契约 → CLI 与共享在线离线校验 → 双会话协调与通道桥接 → 清理和故障恢复 → 状态/日志/选择器 → 集成验证及用户指南。每层保持旧模式兼容，直到数据面和生命周期均通过后再把语法加入当前功能说明。

| 验收面 | 必须覆盖的场景 |
| --- | --- |
| 方向和默认值 | A 上的 localhost:B 到达 C 的 localhost:D；省略地址、同端口、显式地址、IPv6、目标域名、批量端口。 |
| 端点归属 | M、A、C 的测试目标提供不同响应，证明目标没有误连 M；并断言 A 的回调与 C 的 direct-tcpip 目标。 |
| 参数/事务 | 缺少一端、混合模式、非法端口、不支持的编辑整体拒绝；两个新 alias 与规则原子保存；配置冲突不启动 daemon。 |
| 就绪/等待 | 任一 SSH 未连接不能就绪；监听失败、等待超时保留已保存结果；目标应用关闭不误判为 SSH 失败。 |
| 生命周期 | A/C 各自断线、同时断线、网络黑洞、重启 daemon、停止/删除期间重连与迟到成功；仅恢复新业务连接。 |
| 清理隔离 | 残留监听 verified 恢复、取消失败/无确认、helper 不支持、cleanup off；无关规则与服务不被终止。 |
| 资源 | 并发上限、慢目标、打开通道超时与迟到、双向半关闭、大流量反压、反复重连无通道/任务/许可泄漏。 |
| 配置与管理 | 两端服务器改名/修改/删除、按任一端选择、同 profile 双角色去重、组操作、草稿与持久停止/删除意图。 |
| 输出/兼容 | 一次性 JSON、watch/follow、历史双端筛选、旧配置迁移、旧客户端/daemon 拒绝不支持能力。 |

先使用两个隔离的 SSH 服务端夹具验证协议与故障，再做真实 SSH 集成。仅在同一网络命名空间开两个 sshd 不足以证明 localhost/DNS 归属；应使用隔离环境，或结合可观测协议断言明确证据边界。VM/容器端点必须为显式选择的隔离测试环境，不使用真实业务服务器。

实现后的检查按 [AGENTS.md](../../AGENTS.md) 和 [TESTING.md](../../TESTING.md) 执行：Rust 标准检查、SSH smoke、查询/历史、补全和安装相关回归。只有修改 native helper 才需要对应构建产物再生成；不能把交叉编译或模拟服务测试宣称为原生运行验证。实际执行环境和结果记入验证记录，不复制历史通过数量。

功能已交付：usage、configuration 指南与 README 已更新。实现采用进程内双会话桥接（`Tunnel::Relay`，schema v4），verified cleanup 仅管理源端监听会话，与第 5 节契约一致。

## 8. 协议参考

- [RFC 4254，第 7 节](https://www.rfc-editor.org/rfc/rfc4254.html#section-7)：远端监听、取消、forwarded-tcpip 和 direct-tcpip。
- [OpenSSH sshd_config：GatewayPorts](https://man.openbsd.org/sshd_config#GatewayPorts)：远端监听的回环/非回环策略。
