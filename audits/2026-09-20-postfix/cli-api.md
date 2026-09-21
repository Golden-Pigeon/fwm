# CLI / API 修复后复查

日期：2026-09-20。范围：CLI 参数与输出、status/watch、logs/follow、fwm-api 帧与请求边界、daemon 分发及事件历史。

本轮只复查，没有修改生产代码。最终确认 **6 个行为问题，均为 P2**。分轮新增数为 **3 → 1 → 2 → 0**；最后又运行一次基础矩阵，29 项通过、0 项失败。这里的“最后一轮无新增”只表示下述覆盖范围内没有新发现，不表示已确认的 6 项已经修复。

所有 CLI/IPC 复现均使用 macOS 上独立临时目录、私有后台或模拟 IPC 服务；EventJournal 复现直接导入当前生产实现。未连接真实 SSH 服务器，未操作实际登录服务或用户默认后台。测试所需进程和 socket 已清理。

## C1 / P2：探测类 RPC 绕过 request_id 校验

**复现与结果：** 给空配置的私有后台发送 `Ping` 和 `Doctor {server:null}`，分别使用空 request_id 和 129 字节的 request_id。Ping 均返回 `invalid_request: request_id must contain 1–128 bytes`，Doctor 均返回 `ok:true`。

**原因与影响：** `dispatch` 直接分派 Doctor/Inspect/Trust 系列命令，而请求 ID 校验只在普通命令的 `apply` 中执行，造成同一 API envelope 的合法性取决于命令类别。实测仅执行了 Doctor/Ping；其他探测分支的判断来自相同分派路径，未进行真实网络探测。

**位置：** [dispatch.rs:26](../../crates/fwm/src/daemon/dispatch.rs#L26)、[dispatch.rs:60](../../crates/fwm/src/daemon/dispatch.rs#L60)。

**修复及回归方向：** 将 envelope 校验放到分派之前，对所有 Command 统一测试空 ID、合法边界和超长 ID；断言无效请求在执行探测前被拒绝。

**证据：** [cli-api-round1.json](cli-api-round1.json) 的 `request_ids`；复现脚本 [cli_api_probe.py](cli_api_probe.py)。

## C2 / P2：合法资源 ID 超过历史和事件页容量，导致记录静默丢失

这是同一资源元数据边界不一致的两种表现，合并为一项。

1. `PutServer` 接受 10,000 字节 ID，保存成功；实时 Events 有该服务器事件，但 `logs --server dev` 返回 0 条且没有警告。ID 本身已经超过历史单条记录的 8 KiB 预算，持久化失败只写内部 tracing。
2. 接受 260,500 字节的规则 ID，配置整体仍未超过允许的 256 KiB。通过本地 SSH 配置解析失败产生约 7.8 KiB 错误后，事件最新序号为 4，查询 `after:3` 却返回 `events:[]`、`next_sequence:4`、`has_more:false`、`resync_required:false`。第一条待返回事件超过 256 KiB 页预算，`take_while` 返回空列表，随后游标直接跳到最新位置。

**预期：** 被接受的资源应可正常记录事件和历史；无法返回的记录不能通过成功响应和游标前移被静默跳过。第二个复现只触发本地 SSH 配置解析，没有 SSH 网络连接。

**位置：** [model.rs:458](../../crates/fwm-core/src/model.rs#L458)、[model.rs:487](../../crates/fwm-core/src/model.rs#L487) 仅检查 ID 非空和重复；[history/storage.rs:100](../../crates/fwm-core/src/history/storage.rs#L100) 拒绝超预算元数据；[events.rs:166](../../crates/fwm/src/daemon/events.rs#L166) 仅内部记录持久化失败；[events.rs:180](../../crates/fwm/src/daemon/events.rs#L180) 分页及空页游标逻辑。

**修复及回归方向：** 统一资源元数据、JSON 转义后记录和 IPC 页的容量约束；明确处理单条事件过大及持久化失败，避免空页成功跳过未返回事件。加入最大合法 ID、多字节/转义字符、单条超页事件和分页续读回归；不能仅提高一个容量常量。

**证据：** [cli-api-round1.json](cli-api-round1.json) 的 `metadata_budgets`。

## C3 / P2：status 混用不同配置版本，返回不属于所选组的行

**确定性复现：** 模拟 IPC 依次返回：GetConfig revision 1；Status revision 2，规则属于 `old` 组；第二次 GetConfig revision 3，同一 ID 已移动至 `new` 组。运行 `fwm --json status --group new` 成功退出，却显示 `group:"old"`、`config_revision:2`，且 `warnings:[]`。

**原因与影响：** 初次配置版本不匹配时只刷新一次配置，没有再次确认刷新结果与运行时快照的 revision 一致。过滤器根据新配置选择 ID，最终输出旧快照中的行。并发编辑配置时，按服务器、组或名称查询都需要考虑这一边界。

**位置：** [status_watch.rs:16](../../crates/fwm/src/cli/status_watch.rs#L16)、[queries.rs:21](../../crates/fwm/src/cli/queries.rs#L21)。

**修复及回归方向：** 在服务端返回同一状态边界内的选择结果/配置快照，或有界重试到 revision 一致，并明确报告无法取得一致视图。加入多次连续修订、同一 ID 跨组或跨服务器移动的确定性 mock IPC 回归。

**证据：** [cli-api-round1.json](cli-api-round1.json) 的 `revision_race`。

## C4 / P2：排队中的旧引擎事件被归到规则的新服务器、名称和组

**生产实现复现：** 规则 ID 为 `rule`，起初属于 `server-a` / `old-rule` / `old-group`。先构造其引擎事件（`server_id:server-a`，`timestamp_ms:1234`），再用同一 ID 的新配置更新 journal：`server-b` / `new-rule` / `new-group`，最后 `record_engine` 旧事件。

**实际结果：** 历史记录内层 `event.server_id` 仍是 `server-a`，外层 `server_id` 已变为 `server-b`，名字和组也被换成新值；原时间戳被写入时的当前时间覆盖。按原服务器查询得到 0 条，按新服务器查询得到 1 条。

**原因与影响：** 引擎广播消费与 RPC 配置修改分别调度，排队的旧事件可能晚于配置更新进入 journal。记录时丢弃源时间戳，并使用当前 ID 的标签缓存重新补全上下文。用户会把旧服务器故障误判为新服务器故障。时间戳、名称和组的变化属于同一事件上下文丢失问题，不另行计数。

**位置：** [events.rs:116](../../crates/fwm/src/daemon/events.rs#L116)、[events.rs:135](../../crates/fwm/src/daemon/events.rs#L135)、[events.rs:153](../../crates/fwm/src/daemon/events.rs#L153)、[history.rs:55](../../crates/fwm-core/src/history.rs#L55)、[daemon/mod.rs:65](../../crates/fwm/src/daemon/mod.rs#L65)。

**修复及回归方向：** 在事件生成时保留来源时间和版本化上下文；至少不能用当前规则标签覆盖事件自带的另一服务器身份。覆盖事件入队后移动、改名、改组、删除再重建，以及延迟消费顺序。

**证据：** [event-journal-round2.json](event-journal-round2.json)；直接导入实现的 [event_journal_repro.rs](event_journal_repro.rs) 和 [run_event_journal_repro.py](run_event_journal_repro.py)。

## C5 / P2：在 IPC 故障期间首次开启 watch，不显示可读取的保存配置，也不拒绝无效选择器

**复现：** 私有模拟服务成功回复 Ping，但在 GetConfig 关闭连接。磁盘 `config.toml` 和 `state/applied.toml` 都是有效配置，含规则 `web`。分别运行 `status missing --watch`、`status web --watch` 和无选择器的 `status --watch`，连续观察两次更新。

**实际结果：** 三者都保持运行，每次输出 `daemon_state:"unavailable"`、`runtime_available:false`、`forwards:[]`。合法规则的保存意图被隐藏；不存在的 `missing` 在已有可读有效配置时仍不返回错误，持续故障期间可以一直等待。

**原因与预期：** fallback 已经读到配置并构造了 offline snapshot，但 `unavailable` 状态跳过初次选择器解析；尚未解析的 selection 又导致清空所有行。应在首份可读取的有效配置上验证选择器，同时以 unverified/unknown 状态显示已保存规则，避免把保存配置误作实时连接状态。

**位置：** [status_watch.rs:88](../../crates/fwm/src/cli/status_watch.rs#L88)、[status_watch.rs:120](../../crates/fwm/src/cli/status_watch.rs#L120)、[status_watch.rs:126](../../crates/fwm/src/cli/status_watch.rs#L126)。

**修复及回归方向：** 增加 watch 首次启动即 IPC 失败的用例，分别覆盖指定合法 ID/名称、缺失名称、组、服务器和无选择器；区别“没有可用配置”与“有保存配置但无运行时状态”。

**证据：** [watch-selector-round3.json](watch-selector-round3.json)；[watch_selector_probe.py](watch_selector_probe.py)。

## C6 / P2：支持的 512 条规则足以使全部 status 查询超出 IPC 上限，单条查询也失败

**复现：** 使用普通短 ID，一次保存 512 条规则，配置 JSON 仅 98,518 字节。共享 SSH profile 的本地配置含一个较长、不支持的指令，使每条规则在网络连接之前形成约 7.8 KiB 错误。批量保存成功，Status 返回 `response_too_large: response exceeds the IPC frame limit`；`fwm --json status web-0` 也退出 2，stdout 为空。

**原因与影响：** API 总是构造全量 StatusSnapshot，CLI 收到后才按单条规则过滤。512 条规则分别携带错误信息，聚合输出超过 1 MiB 帧上限。用户在最需要查看批量失败原因时无法查询状态，甚至不能逐条查看。

**与 C2 的区别：** 这里不使用超长资源 ID，问题是受支持的规则数量、运行时错误大小和全量快照聚合不兼容。只限制 ID 长度不能修复此项。

**位置：** [dispatch.rs:117](../../crates/fwm/src/daemon/dispatch.rs#L117)、[protocol.rs:10](../../crates/fwm-api/src/protocol.rs#L10)、[daemon/mod.rs:78](../../crates/fwm/src/daemon/mod.rs#L78)、[status_watch.rs:17](../../crates/fwm/src/cli/status_watch.rs#L17)。

**修复及回归方向：** 为状态提供服务端选择/分页和明确的大小约束；必要时有标记地缩短诊断摘要，并提供完整诊断的查询入口。测试最大规则数和最大错误体的组合，保证全量与单条状态、JSON 与 watch 的边界可用，而不是只测每个参数单独合法。

**证据：** [status-budget-round3.json](status-budget-round3.json)；[status_budget_probe.py](status_budget_probe.py)。

## 检查矩阵、分轮记录与排除项

| 轮次 | 重点 | 新增确认问题 |
| --- | --- | --- |
| 1 | 请求 envelope、输出容量、revision 一致性、CLI 基础行为 | C1、C2、C3 |
| 2 | 延迟事件与历史过滤归属 | C4 |
| 3 | watch 初始故障、受支持批量规模与运行时错误组合 | C5、C6 |
| 4 | watch 恢复、follow 固定身份、畸形帧后存活、基础矩阵重跑 | 0 |

基础矩阵 **29 项通过**，涵盖：禁用添加、端口范围、remote 默认值、部分编辑、组操作、改名保留 ID、取消组、服务器改名、历史日志、无效输入不更改配置且不启动后台、请求重放/冲突、revision 冲突及小事件分页。结果及脚本：[cli-api-round3-matrix.json](cli-api-round3-matrix.json)、[cli_api_matrix.py](cli_api_matrix.py)。最终重跑同一矩阵仍为 29 项通过。

最后一轮额外 **3 个流式/协议案例通过**：

- watch 在 IPC 故障后恢复，按稳定 ID 跟随规则改名，删除后继续运行。
- logs follow 在改名、删除、名字复用后仍固定原 ID，并接受其迟到事件。
- 畸形 JSON、未知 method、零长度和超长帧被关闭连接处理，不修改配置、不导致后台崩溃，随后 Ping 正常。

结果及脚本：[cli-api-round4-streams.json](cli-api-round4-streams.json)、[cli_stream_matrix.py](cli_stream_matrix.py)。

以下候选未计入发现：名称与其他资源 ID 冲突已在当前版本修复；Reload 能力门控已存在；未注册临时 alias 与后来新建 profile 的不同 ID 是既定身份语义；极大 timeout 未在本机复现异常；组 watch 按动态组标签跟随属于已文档化行为；历史标签、删除后固定 ID 和空历史结果本身不是问题；畸形帧关闭连接是安全拒绝；32 条当前后台请求幂等缓存已有文档说明；离线人类输出已显示 active connections unknown。没有把尚未验证的 Windows 差异或错误码偏好列成缺陷。

## 单列的接口设计差距，不计入六项行为问题

[DESIGN.md:390](../../DESIGN.md#L390) 描述状态快照与事件序号同一边界，以及连接/规则 generation。当前 StatusSnapshot 不携带事件序号，EngineEvent 也没有 generation 或结构化变更。README 对现状的描述较收敛，当前 CLI 使用轮询和实例/游标检查。后续实现增量 TUI/Web UI 前应统一设计契约和实现；不能仅据未来能力缺失断言今天的 CLI 查询失败。这一差距也解释了 C3/C4 为什么需要明确状态与事件上下文边界。
