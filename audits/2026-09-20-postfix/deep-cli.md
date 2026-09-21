# CLI 深入续审：离线编辑与组合语义

日期：2026-09-20。此次实际执行多步骤配置变更；不是复跑上一轮只读查询。**96 项确定性 CLI 检查通过，0 项失败；另有 5 次路径候选观察后排除。新增确认缺陷 0 项。** 所有规则始终为 stopped，未启动 daemon、IPC 测试服务、SSH、agent、系统服务或远端 helper。生产代码和既有汇总未修改。

证据为 [deep-cli-mutations.json](deep-cli-mutations.json)，完整可运行脚本为 [deep-cli-mutations.py](deep-cli-mutations.py)。矩阵首轮 75 项通过，随后针对名称/ID 边界、停止规则监听冲突、长 Unicode 自动命名和路径等价继续补查 21 项；最终执行完整 96 项均通过。新增数两轮均为 0，这只说明本子范围未发现新问题，不代表整个项目已找全。

## 检查方法

每个独立场景使用新的临时配置目录，命令仅包含 disabled add、edit、server add/edit/remove、group/config 只读、down/remove 等离线操作。每次成功变更均检查返回的配置 revision 与 TOML 落盘 revision 相同，随后重新 `config export`，逐字段比较其完整逻辑配置与变更响应。每次预期拒绝均比较操作前后的全部 TOML 字节，确认配置未发生部分更新。每条命令结束后检查没有产生 daemon socket；所有夹具最终清理。

输入是固定业务场景，不进行随机输入、API 畸形帧或容量压力。已通过的第一轮矩阵因加入后续边界而在最终完整轮中重跑，报告总数仍为 96 个唯一检查，而非累加两次运行。

## 组合流程与结果

| 流程 | 实测结果 |
| --- | --- |
| 批次与组编辑 | 端口去重后创建确定成员；统一改目标保留成员 ID/源端口；统一源端口导致冲突时整体拒绝；group rename 保留成员名称/ID；显式 move 合并组，rename 不隐式合并；ungroup 保留成员。 |
| 跨服务器组 | 同端口 remote 成员分属两服务器可保存；整组移到一个新服务器、或整体变为 local 时冲突原子拒绝，没有留下新服务器；先改一个成员源端口后可原子迁移到新 alias。 |
| 方向和默认 | local→remote 默认 verified/dedicated；显式 off/shared 保留映射；remote→local/dynamic 清除 cleanup；动态代理转 local 需明确目标，保留监听地址与 ID。 |
| 部分地址编辑 | IPv6 bind 与 IPv6/hostname target 在 --src/--tgt/--port 中按字段保留；三段旧式映射更新目标而保留既有 bind；四段显式映射按给定地址保存。 |
| 数字名称与参数次序 | numeric NAME 在带端口 flags 时保留；edit 的方向前置、NAME 后置、分离 scalar SPEC、附着 -L SPEC、global --json 混排均按确定语义保存；显式 spec 与 shorthand 混用拒绝。 |
| 服务器字段 | 改名保留 ID 和所有未指定字段；identity 列表替换而非追加；host/ssh 模式互相清除；各 --unset 与显式赋值互斥；批量 unset 恢复空覆盖；空 host/user/path/jump、port 0、none 与其他 jump 混用均拒绝。 |
| 名称、ID 和监听边界 | 以服务器 ID 添加不创建新 profile；规则名/组名不能遮蔽已有规则 ID；服务器名不能遮蔽服务器 ID；停止的无组监听替代项可共存，加入同组时仍检查 wildcard/local/dynamic 冲突；按规则 ID 编辑/删除保留其他成员。 |
| 手写配置与首笔提交 | 缺失规则 ID 的重复只读导出相同；remote 缺省模式被解释为 verified/dedicated；首次离线 rename 持久化同一个确定 ID；无变化 edit 不加 revision；后续按该 ID down 成功。 |
| 自动命名与路径 | 99-byte Unicode 服务器名生成的规则和组仍在 100-byte 范围内；后续批次复用同一自动组；相对路径以 CLI cwd 保存；同一绝对路径和 ./ 路径可复用；不同 config 路径明确拒绝且不替换原 profile。 |

## 追查后排除的候选

1. **手写配置缺失 ID 导致重复读取换身份**：初看 serde 默认使用随机 UUID，但 `store.rs:199` 在反序列化之前调用 `store/identity.rs:7–28`，按对象种类与名称生成确定 UUID。已用首次离线编辑及重复导出验证，未将其误报。
2. **空路径先规范化成 cwd 后误接受**：`ssh/path_options.rs:28–29` 在任何路径拼接前拒绝空值。server edit 的 identity、ssh-config、known-hosts 空值均实测失败且 TOML 不变。
3. **新 alias 在后续规则冲突时残留**：`cli/forwards.rs:69–78` 先验证完整候选，`configuration/batch.rs:104–121` 也在候选中插入并验证；跨服务器组移到单服务器的冲突和空/超长名称都未留下新 profile。
4. **组 rename 临时重名或合并**：`cli/forwards.rs:38–53` 拒绝有歧义的 rename+move 和 rename 到既有组；`:62–65` 恢复成员原名，只改组标签。组合测试验证原子性和稳定身份。
5. **停止规则可以通过分组绕过监听冲突**：`model.rs:539–553` 明确对同组成员检查冲突，与 desired state 无关。替代项仅在无同组运行契约时允许同端口；加入同组按预期拒绝。
6. **`%d/.ssh/config` 与 `~/.ssh/config` 等价却被拒绝**：做了 5 次离线 CLI 观察（[deep-cli-path-equivalence.json](deep-cli-path-equivalence.json)）。继续追踪发现 `ssh_config` 字段由 `ssh/config.rs:52–57` 调用 `expand_home`，后者 `:490–499` 只展开 ~；身份/known_hosts 文件路径才走 `%d` token 展开。因此此前提不能成立，没有把路径比较的拒绝提升为缺陷。未执行 SSH 解析器或连接验证，也未对 token 支持范围作新的产品契约结论。

## 去重和范围限制

本轮不重新计数 C1–C6、ST01–ST08、运行生命周期或日志专项的既有问题。尤其没有把错误写 stderr、停止的替代监听共存、未变化 edit 不提交、不同配置文件路径需显式 server edit、group rename 后成员名称保持不变列成缺陷。

覆盖的状态均为单进程命令完成后的离线 stopped 配置；没有证明在线控制、并发外部文件编辑、异常文件事务恢复或 Windows/Linux 原生行为正确。这些由其他专项证据另行评价。最后一轮局部零新增不抵消其他专项发现，也不是“没有剩余 bug”的结论。
