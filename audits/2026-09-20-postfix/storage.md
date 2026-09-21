# 配置、持久化与恢复复查

审查对象为上轮修复后的 Release 和当前生产源码。Release SHA-256：`4b7e78e32fcdfc0b4b9cb55b3b4a8dca38b6f2b1059f972f3ca5b56b44d6b6db`。已核对它与本次 [source-sha256.json](evidence/source-sha256.json) 相同；下述涉及的 store、model、paths、path_options、offline 源文件也逐一匹配该清单。

本报告确认 **8 项新问题**，均有本次 Release 实测；控制意图的 remove/down/up 三种表现合为一个根因。没有将 U01–U36 中已经修复的选择、默认值或路径问题重新列报。所有实际命令使用独立临时配置，网络仅连接已拒绝的 `127.0.0.1:1`；未访问真实 SSH、默认后台或系统登录服务，未改生产源码/共享测试。

完整复现：

```sh
python3 audits/2026-09-20-postfix/storage_repro.py
```

脚本自动建立并清理实例，保存逐命令/IPC 结果到 [storage-results.json](storage-results.json)。文件镜像失败使用 macOS 用户文件 immutable flag 注入，只施加于临时 `config.toml`，并在 finally 中清除；其他平台会明确跳过该案例。以下简化命令均指向对应临时 `--config-dir`，完整可执行参数见脚本。

## ST01 — P1：草稿改名后，控制意图按旧名称误作用于另一稳定 ID

**最小触发：**

1. 添加两个已停止的规则 `one` 与 `two`，ID 分别为 A、B。
2. 手工草稿把 A 的名字改成 `three`、B 的名字改成 `one`；保留两个 ID，不改变其他字段。`config validate` 成功。
3. 对尚未 reload 的已应用配置执行 `remove one`，此时正确选择并删除 A。
4. `config reload` 成功，但 B 也消失，规则列表为空。

扩展实测：相同改名草稿下，`down one` 会在 reload 时把原本 running 的 B 停止；`up one` 会在 reload 时把原本 stopped 的 B 启动。B 从未被选择，两个 ID 从始至终都没有改变。

**实际与期望：** 控制记录先按 ID 查找，B 没有自己的覆盖记录，便退回名称匹配，命中 A 的旧名字。应只改变选中的稳定资源，不能把另一现存 ID 当成同一资源。若需要支持无 ID 的旧草稿，名称回退也必须识别候选对象是否是另一个已知资源。

**当前源码：** [store/control.rs:24](../../crates/fwm-core/src/store/control.rs#L24)，尤其 `get(&forward.id).or_else(...intent.name == forward.name)`；删除分支在同文件 32 行。

**设计依据：** README 170/272 行保证“针对所选资源”的控制意图在 reload 时保留；DESIGN 220 行要求选择范围受控。这里不是 U14 的名字碰 UUID：名字和 ID 均合法、名称也唯一，错对象发生在草稿覆盖合并阶段。

**证据 A：** `control-name-fallback-remove`、`control-name-fallback-down`、`control-name-fallback-up`。JSON 记录保留 B 的 expected_untouched_id、原状态、实际状态以及完整命令序列。

## ST02 — P2：recover 让持久 revision 回退并复用旧 CAS 版本

**最小触发：**

1. 保存服务器后连续编辑，已应用配置达到 revision 4；保存先前 revision 1 的视图。
2. 把候选文件的 revision 设为 0（恢复较旧副本也会出现此条件），执行 `config recover --from-candidate`。
3. 恢复后 revision 变成 1。启动临时后台，用先前 revision 1 及旧服务器字段提交 `PutServer`。

**实际：** 请求被接受，已保存的新端口 4 被旧视图中的端口 1 覆盖，没有 revision_conflict。恢复内容可以来自旧候选，但其提交版本不应与旧状态重新相等。

**期望：** 恢复应生成不能被旧视图误认为同一版本的新提交身份。能读到旧 revision 时至少不能倒退；旧版本不可读时需要能区分恢复世代的方案，不能仅信任候选里可编辑的 revision。

**当前源码：** [store/recovery.rs:38](../../crates/fwm-core/src/store/recovery.rs#L38) 仅执行 `candidate.revision + 1`。正常 offline 修改则以 before.revision 为基准，见 [offline.rs:107](../../crates/fwm/src/offline.rs#L107)。

**设计依据：** README 272 行和 DESIGN 212/222 行用 revision 防止旧视图覆盖。README 325 行另要求客户端在后台重启后重新对账；遵守此要求的客户端可避开演示中的旧请求，但不会改变 recover 自身复用了旧版本号这一事实。本条没有声称每个重启客户端都会发送旧请求。

**证据 A：** `recovery-reuses-stale-cas-revision`，同时记录 `before_revision:4`、`recovered_revision:1`、`stale_request_accepted:true` 及端口从 4 回到 1。原始请求通过仅该临时实例的 Unix IPC 提交。

## ST03 — P2：SSH 文件路径的目录故障被当成配置快照损坏，连停用/清除坏路径也被阻断

**最小触发：**

1. 保存 identity 路径 `TEMP/keys/sub/id`，启动一个规则；目标 SSH 仅为拒绝连接的回环端口。
2. 将 `keys` 目录替换成普通文件，制造可重复的 ENOTDIR 条件。
3. 在线执行 `down web`、`server edit dev --unset identity`。
4. 停止后台后执行 `config export`、`remove web`。

**实际：** 在线两个修复/控制命令都报 storage_error；规则保存意图仍为 running。离线导出/删除也失败，错误称 cannot read last committed configuration 并建议 config recover。没有改动任何 TOML，仅恢复原目录结构后，同一快照立刻可读，down 成功。

**原因与期望：** Store 每次读取已持久化配置都重新进行 SSH 路径的真实文件系统规范化；路径父级的 ENOTDIR/权限故障被传播成配置读失败。读取已保存的合法绝对路径不应要求其外部目录当前可用，更不能阻止停用规则或清掉该故障字段。认证可用性应由该服务器运行状态报告。

**当前源码：** [store.rs:86](../../crates/fwm-core/src/store.rs#L86) → [ssh/path_options.rs:48](../../crates/fwm-core/src/ssh/path_options.rs#L48) → [paths.rs:127](../../crates/fwm-core/src/paths.rs#L127)。后者只容忍 NotFound，对其他 canonicalize 错误立即失败。

**设计依据：** README 170/172 行、DESIGN 220 行保证停止/删除仍能执行；U03 的稳定路径修复不能让外部 SSH 路径故障破坏管理平面。这不是上轮的相对 cwd 问题：实测路径已经是正确的绝对路径。

**证据 A：** `ssh-path-enotdir-blocks-control-plane`，含在线状态仍 running、在线 unset 失败、离线失败和恢复目录后的成功对照。

## ST04 — P2：旧配置自动推断分组会凭空产生组内冲突，阻止合法配置升级

**最小触发：** schema 2 文件里有两个未分组、均 stopped 的替代规则：

- `bundle-3000`，监听 `127.0.0.1:3000`。
- `bundle-03000`，监听同一地址，目标不同。

这两个名称不同，显式 ID 不同；作为停止的替代规则，原配置合法。读取时后缀都被解析为整数 3000，两者被自动加入新组 `bundle`，随后校验报 `listen addresses conflict within group "bundle"`。只把版本号改为 schema 3、其他字段不变，则 validate 成功。

**期望：** 分组推断属于兼容迁移，不应把原来有效的独立规则变成无法读取的配置；推断产生冲突时应跳过该组，或只识别无歧义的规范化端口名称。

**当前源码：** [model.rs:358](../../crates/fwm-core/src/model.rs#L358)，367 行用 `suffix.parse::<u16>()`，其后直接设置 group；与 [model.rs:542](../../crates/fwm-core/src/model.rs#L542) 的组成员无论是否 running 都须无冲突规则组合后触发。

**设计依据：** DESIGN 220 行的旧版本分组迁移应保留成员身份与有效性；README 对停止的不同组/未分组替代规则允许复用端口。此问题与 U15 的长名称截断并组不同，发生在旧配置升级时。

**证据 A：** `legacy-group-inference-creates-new-conflict`，记录同一内容 schema 2 失败、schema 3 成功。

## ST05 — P1：恢复后的候选镜像写失败会丢掉停删保护，下一次 reload 复活已删规则并启动规则

**最小触发：**

1. 两个规则 `deleted`、`survivor`；候选草稿将两者写成 running。
2. 删除已应用的 `deleted`，留下受保护的删除意图；把 applied 的 TOML 正文损坏，保留可读取的控制记录头。
3. 对临时候选文件设置可逆 immutable flag，使读取和备份正常、替换失败。
4. 执行 `config recover --from-candidate`。
5. 清除该文件 flag，执行 `config reload`，不执行 up。

**实际：** 第 4 步成功返回，只剩 survivor/stopped，并宣称 `pending control intent remains protected`。但新 snapshot 没有控制记录头，旧候选仍含两个 running 规则；第 5 步把 deleted 重新加回，并让两个规则都恢复 running。

**期望：** 恢复前可读的删除意图和恢复时统一 stopped 的决定，必须在候选文件成功同步之前保持保护。若只能部分提交，警告不能声称并不存在的保护。

**当前源码：** [store/recovery.rs:29](../../crates/fwm-core/src/store/recovery.rs#L29) 应用旧意图，34 行统一 stopped，但 [同文件:81](../../crates/fwm-core/src/store/recovery.rs#L81) 用 `Overrides::default()` 提交，丢掉保护；[store/control.rs:180](../../crates/fwm-core/src/store/control.rs#L180) 仍使用“保护保留”的通用警告。

**设计依据：** README 285 行明确“可读取的删除/停用控制意图会继续应用”“需要显式 up 才运行”。这不是原 U34 缺少恢复入口，而是新恢复入口的部分提交失败路径。

**证据 A：** `recovery-mirror-failure-loses-stop-and-delete-protection`，实际使用 macOS 文件 flag 阻止替换，没有模拟成功/失败响应，也没有修改生产代码。fixture 完成后 flag 已清除、进程已结束。

## ST06 — P1：已有实例的 applied 文件丢失后，普通启动静默应用未提交草稿

**最小触发：**

1. 保存 stopped 的 web，启动并停止一次，确保已有 `state/recovery.json` 等实例状态。
2. 仅在候选 `config.toml` 中把 web 改为 running，未执行 reload。
3. 删除 `state/applied.toml` 模拟权威快照丢失。
4. 执行普通 `daemon start`。

**实际：** daemon 成功启动，web 的 desired_state 成为 running 并进入连接重试；status.warnings 为空。未经 reload 的草稿变成了新快照。

**期望：** 已初始化实例失去权威快照应与第一次启动区分。无法确认最后有效意图时，应明确失败并引导显式、停用状态下的恢复，不能静默授权运行候选规则。

**当前源码：** [store.rs:43](../../crates/fwm-core/src/store.rs#L43) 在 snapshot 不存在时直接采用 candidate；[store.rs:142](../../crates/fwm-core/src/store.rs#L142) 重新初始化快照；[daemon/state.rs:19](../../crates/fwm/src/daemon/state.rs#L19) 无此前初始化状态检查。

**设计依据：** README 272 行要求手工编辑显式 reload，并保证后台重启依据最后有效快照；README 285 行的显式恢复要求规则全部停用。实测是人为删除文件来模拟丢失，不宣称发生了真实磁盘事故。

**证据 A：** `missing-snapshot-silently-applies-draft-at-start`，保留 prior start/stop、candidate 修改、重新启动和无 warning 的运行意图结果。

## ST07 — P2：全新配置的 dangling config.toml 符号链接被当成不存在并覆盖删除

**最小触发：** 新配置目录里 `config.toml` 是指向不存在目标的符号链接；先 `config export`，再 `server add ...`。

**实际：** export 返回空默认配置；server add 成功，并把原符号链接直接替换为普通文件。若同一链接指向有效目标，解析路径会明确拒绝符号链接。输入是否被拒绝，不应取决于链接目标是否碰巧存在。

**期望：** 在判断首次初始化之前检查目录项本身，按已有策略拒绝不支持的配置文件链接；不要把已有但悬空的链接视作用户尚未创建配置并覆盖它。此处是 fwm 的 `config.toml`，不是 U03 特意允许保留的 SSH/identity 文件链接。

**当前源码：** [store.rs:54](../../crates/fwm-core/src/store.rs#L54) 的 `Path::exists()` 跳过 dangling link；[store.rs:142](../../crates/fwm-core/src/store.rs#L142) 的初始镜像写入没有调用 [reject_symlink:207](../../crates/fwm-core/src/store.rs#L207)。

**设计依据：** 当前 Store 明确拒绝 symlink 配置文件，其回归 `oversized_and_symlink_candidates_are_rejected_without_losing_snapshot` 也确认该策略；初始化必须维持同样的输入/文件保留边界。

**证据 A：** `dangling-config-link-treated-as-absent-and-replaced`，脚本断言修改前 is_symlink、修改后不是 symlink 且是普通文件。

## ST08 — P1：没有转发的服务器配置为 FIFO，也能阻塞整个后台管理接口

此项为主代理追加的有界定向验证，没有再次扩大此前广查范围。

**最小触发：**

1. 在临时目录创建 FIFO，启动本脚本拥有的前台 daemon。配置为空，没有任何 forward，connect_timeout_secs 设为 1。
2. 经该实例私有 IPC 提交 `PutServer`，让新服务器的 ssh_config 指向此 FIFO；暂不启动 writer。
3. 等待 mutation 响应 1 秒，再向另一个连接发送 Ping 并等待 1 秒。
4. 向 FIFO 写入几行有效 SSH 配置后关闭 writer。

**实际：** 两个请求都在各自 1 秒读取期限内无响应；直到约 2 秒后写 FIFO 才解除。之后原 mutation、已排队的 Ping 和新 Ping 全部成功。服务器没有 forward，因此这里没有任何 SSH 网络连接，不是远程超时或端口转发故障。

**原因与期望：** `PutServer` 在持有整个 daemon State 锁时提交并 reconcile；Engine 对所有服务器解析 SSH 配置，包括没有 forward 的服务器。解析器同步 `read_to_string`，遇 FIFO 会等待 writer/EOF；这一步不在 SSH connect timeout 内。应在解析前拒绝不支持的特殊文件，并让 SSH 文件 I/O 不占住全局管理状态锁；一个路径问题不能让 Ping、查询和停止请求都无法处理。

**当前源码：** [daemon/dispatch.rs:47](../../crates/fwm/src/daemon/dispatch.rs#L47) 持 State 锁执行 apply；[daemon/state.rs:138](../../crates/fwm/src/daemon/state.rs#L138) 调用 engine.reconcile；[engine/mod.rs:77](../../crates/fwm-core/src/engine/mod.rs#L77) 遍历所有 servers 并 resolved_route；[ssh/config.rs:234](../../crates/fwm-core/src/ssh/config.rs#L234) 同步读取且没有常规文件/大小/期限检查。

**设计依据：** DESIGN 48 行明确“一台服务器无法连接，不得阻塞其他服务器或 CLI 响应”；daemon/dispatch.rs:24 的注释也明确 SSH 探测不应占住应用锁。该缺陷与 ST03 不同：ST03 是规范化返回错误阻断配置控制，此项是同步文件读取不返回，锁住整个管理接口。

**证据 A：** [ssh-file-blocking.py](ssh-file-blocking.py)、[ssh-file-blocking.json](ssh-file-blocking.json)。记录 connect_timeout=1、forward_count=0、两个超时、解除后成功响应，以及自有前台进程 returncode=0/cleaned=true。脚本 finally 使用非阻塞 writer 解除 FIFO，再关闭连接与自有进程；没有残留 daemon 或 FIFO writer。此次 Release hash 仍与报告开头及证据清单一致。

## 分轮与完整复扫

| 轮次 | 检查与结果 |
|---|---|
| 第一轮广查 | Store 权威来源、提交/镜像/控制覆盖、revision、模型名称/ID、路径读取。确认 ST01/ST02/ST03；分别用 Release 验证。 |
| 第二轮关联扩展 | 将 ST01 扩展到 remove/down/up，证明 ID 不变的未选规则仍受影响；将 ST02 扩展为旧 CAS 请求实际覆盖；将 ST03 加入在线 unset 与恢复目录结构对照。迁移矩阵另发现 ST04。 |
| 第三轮故障与初始化 | 检查恢复提交失败及所有文件入口：实际镜像失败发现 ST05；已初始化 snapshot 缺失发现 ST06；dangling 候选链接发现 ST07。 |
| 广查最后一轮完整复扫 | 重新遍历控制覆盖退休、旧名字重用、正常/坏草稿、缺失身份文件父级、模型 ID/默认值/端口验证、schema 迁移、恢复锁/备份/停用语义、配置文件类型与实例路径。没有新增独立确认问题；正常对照见 `negative-controls-final-pass`。 |
| 后续有界复核 | 主代理指定 FIFO SSH 配置入口，确认 ST08；只验证该控制流及写入 FIFO 后的恢复，没有重新广查。 |

明确排除或归并：

- 普通 remove 成功同步后重新 add 同名规则，再 reload，不会误删；普通控制保护已在同步成功后退休。没有把已修复的“永久 tombstone”问题误报成本轮问题。ST01 仅指有草稿时，名字被另一保留 ID 接用。
- 完全缺失的 identity 文件/父目录不会阻断 Store 管理；ST03 是真实路径规范化遇到 ENOTDIR 等非 NotFound 错误的特定失败路径。
- 坏草稿期间 down 会保留原始字节，修好再 reload 会保留选中规则的 stop；同步完成后，后续新的显式手工修改仍可改变意图。以上均已作为正常对照执行。
- 服务器/规则名称与 ID 的校验、ID 优先选择、remote 默认恢复模式、IPv6 字面量/地址族校验已有修复；本轮未沿用旧审查文本重复报这些问题。
- 非 UUID 的显式规则 ID 不会直接违反 helper 协议：cleanup context 会派生 canonical UUID，已检查排除。
- `state` 目录 symlink 会让只读 status 看到被指向实例，但 stop 实测拒绝该目录，另一实例未停止；未将其误报为跨实例停止缺陷，也未计入上述确认项。
- 长显式 ID 的历史/IPC 容量问题、超大重试参数与引擎超时由其他范围审查，未重复计数。
- 候选目录 fsync 警告后清除控制头的崩溃一致性仍是值得故障注入的关联边界；本轮未做真实断电或该 fsync 故障注入，不把它当成已实测的新问题。

本轮只增加本目录下的复现脚本、结果与报告。上述问题都来自修复后的 Release；原审查存在相近模块条目，并不意味着这些新失败路径已经被其回归覆盖。
