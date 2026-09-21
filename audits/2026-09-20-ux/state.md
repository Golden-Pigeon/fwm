# 配置、状态和日志的复查

使用现有 Release CLI、独立 `/private/tmp` 配置目录及测试私有 IPC；没有改实现、编译或连接真实 SSH。各实验目录/进程已清理。以下编号对应总表 U30–U36。

## U30 未知字段被静默忽略

手写 schema_version=3，服务器设置 `usr="intended-user"`，转发设置 `desired_sate="stopped"`（字段拼错）。其余字段有效，服务器是 127.0.0.1:1，规则未实际启动。

`config validate --json` 返回 `valid:true`；`config export --json` 的 user 为 null，desired_state 为 running；执行离线 config reload 后，文件里的错拼字段被删除，正规 desired_state 被写成 running。明确期望应是在保存/应用之前拒绝未知字段。

`ServerProfile` 与 `ForwardSpec` 没有拒绝未知字段，DesiredState 的缺省值又是 Running。顶层 Config 和 RetryPolicy 已有 deny_unknown_fields，嵌套对象的体验不一致。

证据：[第一轮 JSON](evidence/fwm-audit-state-results.json)。源码：[model.rs:141](../../crates/fwm-core/src/model.rs#L141)、[model.rs:235](../../crates/fwm-core/src/model.rs#L235)。优先修复此项。

## U31 手写反向转发的缺省恢复模式不同

手写当前 schema 3 的 remote 规则，提供 listen、target、server_id、desired_state=stopped，省略 remote_cleanup 和 connection_mode。validate 成功，export 得到 off/shared。CLI 创建相同方向时的缺省值却是 verified/dedicated。

这不是显式选择 off 的情况。用户从 CLI 转到配置文件管理时，默认恢复能力会随创建入口改变。

证据：[第二轮 JSON](evidence/fwm-audit-state-round2-results.json)。源码：[model.rs:133](../../crates/fwm-core/src/model.rs#L133)、[cli/cleanup.rs](../../crates/fwm/src/cli/cleanup.rs)。

## U32 日志组选择的名称空间和语义不一致

实际 CLI 操作：

```sh
fwm add task --server dev --local --port 31992 --group g --disabled
fwm edit task --rename archived
fwm add new --server dev --local --port 31993 --group task --disabled
fwm status task
fwm logs task
fwm logs --group task
```

status task 返回当前组里的 new；logs task 返回历史同名规则 task/archived，遗漏当前组；logs --group task 才返回 new。历史名称先命中之后，组解析被跳过。

同根因的第二个表现：将组 g 改名 h，logs h 会把该组当前成员在原 g 下的历史也列出，logs --group h 只列事件当时 group=h 的记录。显式组按历史标签过滤有合理依据；问题是 NAME 简写与显式 group 选择不一致，且没有说明。现有 history_cli 测试也期望两种组选择等价，但未覆盖这个变化过程。

证据：[第一轮 JSON](evidence/fwm-audit-state-results.json)。源码：[HistoryFilter::select](../../crates/fwm-core/src/history.rs#L161)。建议先决定一致的选择规则，再建立包含重命名与名称复用的回归。

## U33 服务器日志跟随在改名后静默失去匹配

启动 `logs --server dev --follow --tail 0`，修改规则目标后能收到 dev 标签事件；执行 `server edit dev --rename prod`，再次修改同一规则目标后，进程继续运行却没有新输出。新的事件可通过 logs --server prod 查到。

**这是语义/诊断缺口，不是日志数据丢失。** 目前服务器过滤按事件发生时的标签，源码有此约定；README 只明确保证规则 NAME 的历史 ID 跟踪。可选择开始时将当前服务器名解析成 ID，也可以保留标签过滤，但应说明改名后旧标签不会匹配、如何改用 ID 跟随。

证据：[第一轮 JSON](evidence/fwm-audit-state-results.json)。源码：[FollowFilter::select](../../crates/fwm/src/cli/query_selection.rs#L65)。P3。

## U34 已应用快照损坏后的恢复指引缺失

临时目录已有正常 config.toml 和 applied.toml。只把 applied.toml 改成坏 TOML，保留有效候选。config validate 返回成功，但 config reload 返回 applied 的底层 TOML 错误，没有恢复入口或修复指引。

**不能把拒绝覆盖损坏快照本身判为错误。** 快照还可能保存控制草稿的 stop/delete 意图，直接丢弃会破坏安全恢复语义。缺口是明确的自助修复流程：应保留坏文件备份，说明意图保护状态，并提供显式恢复选择。

证据：[第二轮 JSON](evidence/fwm-audit-state-round2-results.json)。源码：[Store::load](../../crates/fwm-core/src/store.rs#L41)。P3。

## U35 watch 不抵抗一次临时 IPC 断开

通过临时私有 IPC 对端精确控制时序，没有启动真实 daemon：先正确回复 ping、get_config、status，让 CLI 输出一条正常快照；下一轮回复 ping 后，在 get_config 上关闭连接。

CLI 随即 exit 5，stderr 为 io_error/daemon closed without a response，而不是继续观察后台恢复。启动期间发生同样断开的结果也已验证。运行时通信失败与初始无效选择器应区分，后者仍可立即报错。

证据：[启动期间](evidence/fwm-audit-state-round2-results.json)、[正常输出后的断开](evidence/fwm-audit-running-watch-result.json)。源码：[queries.rs:28](../../crates/fwm/src/cli/queries.rs#L28)。

## U36 服务器级事件缺少可筛选归属

执行 server add dev，然后 server edit dev --port 2。logs 能看到两条 server saved，然而两条记录的 server_id/server_name 均为 null，logs --server dev 返回空数组。未过滤的消息本身也不含被修改服务器的名字。

服务器信任和 SSH 连接事件也通过无 forward ID 的记录路径发送，属于相同标签缺失问题。前者为 CLI 实测，后者是源码确认。应记录服务器级身份，不能依赖从事件文案猜归属。

证据：[第三轮 JSON](evidence/fwm-audit-state-round3-results.json)。源码：[offline/history.rs](../../crates/fwm/src/offline/history.rs)、[daemon/probes.rs](../../crates/fwm/src/daemon/probes.rs)、[daemon/events.rs](../../crates/fwm/src/daemon/events.rs)。

## 分轮、交叉审查与停止

- 第一轮新增四个候选：未知字段、日志选择语义、server follow 改名、group watch 改名。
- 第二轮新增三个：手写 remote 默认值、坏快照恢复指引、watch IPC 断开。
- 第三轮新增一个：服务器级日志缺少标签。
- 独立交叉审查后，group watch 改名被排除：当前组仅为字符串标签集合，README 已区分规则/服务器 ID 跟踪；旧组变空且继续等待符合此语义。U33/U34 明确降为 P3 语义/诊断缺口，避免把合理的历史标签和 fail-closed 当成 bug。
- 第四轮完整复查无新增：检查初始只读无副作用、候选与应用快照分离、未应用草稿保护、启停覆盖、ID 持久化、日志坏记录处理、后台生命周期、删除后的 ID 查询。详见 [第四轮 JSON](evidence/fwm-audit-state-round4-results.json)。
- 此后补充 U35 的运行中实测，只提高证据强度，不增加条目。已确认内容归并为五项行为缺陷及两项诊断/语义缺口。
