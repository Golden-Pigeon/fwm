# 存储事务组合深入审查

2026-09-20。本轮针对上一轮没有执行的事务组合补充检查，确认 **3 个新根因，均为 P2**；另列 1 个条件性持久化模型风险。没有修改生产代码或 `REVIEW.md`。

验证使用当前 `Store` 源码的临时副本、仓库已有的私有 `FileOps` 注入接口及真实临时普通文件。模型、路径和路径规范化类型复用本地编译的 `fwm_core`；未启动后台、发送 API 请求、连接 SSH、执行认证 agent、创建 FIFO、调用系统服务或进行真实磁盘满/断电实验。

执行入口：[deep-storage-run.py](deep-storage-run.py)；测试源码：[deep-storage.rs](deep-storage.rs)；最终逐项结果与源码哈希：[deep-storage-results.json](deep-storage-results.json)。最终 **7 个测试通过，0 个失败，耗时 10.89 秒**。这些审查测试包括对当前缺陷的断言，通过不代表缺陷已经修复。编译目录及所有临时配置已清理。

## DS01 — P2：提交期间较晚保存的草稿会被候选镜像静默覆盖

**触发条件：** 用户在控制命令或 reload 已读取候选之后、候选镜像写回之前保存新的普通配置编辑。

纯文件测试在权威 snapshot 替换完成时，通过 `FileOps::replace` 插入一次普通编辑器保存：将候选服务器端口改为 3333；随后继续执行原生产控制流。没有模拟 Store 的返回值，没有多个 daemon，也没有修改配置身份。

| 操作 | 命令读取的端口 | 较晚保存的端口 | 最终候选端口 | 返回 |
|---|---:|---:|---:|---|
| `commit_control_for`，停止一条规则 | 22 | 3333 | 22 | `Ok(None)`，无 warning |
| `read_candidate` 后 `commit_reload` | 2222 | 3333 | 2222 | `Ok(None)`，无 warning |

**根因：** `commit_control_for` 在 [control.rs:107](../../crates/fwm-core/src/store/control.rs#L107) 仅检查一次 pending 状态；[control.rs:159](../../crates/fwm-core/src/store/control.rs#L159) 后的镜像阶段没有验证候选是否仍是当初读取的文件内容，直接执行替换。reload 同样未把读取时的候选版本带到镜像阶段。daemon 实例锁只能串行化遵循它的程序，编辑器保存不会取得该锁。

**期望：** 新草稿应保留，并返回已提交但草稿未镜像的明确警告；或在提交前发现冲突并拒绝操作。镜像不能在无提示的成功响应中丢掉用户较晚保存的无关字段。修复需要用读取时的候选身份/内容校验及相应同步设计处理该窗口，不能仅再添加一次存在性检查。

证据测试：`deep_later_editor_save_is_lost_during_control_and_reload`。关联对照中，在控制命令开始前已经存在的不同草稿会被保留，连续 down/up/down 也保留最后意图及无关字段；因此这不是原 ST01 的名称回退问题。

## DS02 — P2：累计控制记录可让成功提交的 applied 超过自身读取上限

**触发条件：** 候选镜像持续失败，用户分批编辑候选、reload、删除旧批次；旧删除保护因此累计。每份配置都在当前资源数和配置预算以内。

主证据使用普通 36 字节 UUID 和 29 字节规则名，每轮最多 512 条已停止规则，单份 Config JSON 为 **142,211 字节**。累计 18 批、9,216 条历史删除保护后，第 19 批 reload：

1. 当前候选仍只有 512 条规则，合法且可读取。
2. `commit_reload` 返回 `Ok(Some(...))`；唯一警告称配置已提交、候选镜像失败、控制意图仍受保护。
3. 实际写出的 `applied.toml` 为 **1,079,125 字节**，超过 1,048,576 字节上限。
4. 紧接着 `Store::load` 失败：`configuration file exceeds 1 MiB`。
5. 普通 `recover_from_candidate(false)` 也将整个 snapshot 判为过大，拒绝读取控制意图，要求额外显式舍弃意图。

**根因：** [control.rs:48](../../crates/fwm-core/src/store/control.rs#L48) 的 rebase 保留历史删除记录；镜像未成功时这样做原本是保护措施。但 [control.rs:134](../../crates/fwm-core/src/store/control.rs#L134) 只验证当前 Config，随后 [control.rs:139](../../crates/fwm-core/src/store/control.rs#L139) 将不受该预算约束的控制头拼接到正文并直接提交。读取端 [store.rs:192](../../crates/fwm-core/src/store.rs#L192) 却限制整个文件，恢复端 [recovery.rs:21](../../crates/fwm-core/src/store/recovery.rs#L21) 也使用同一上限。写入前没有检查完整 snapshot 是否仍能被自己读取。

**期望：** 在权威快照替换之前保证完整持久化表示满足读取预算。无法容纳时应保留上一个可读快照并明确返回未提交错误，或采用可以有界维护历史意图的存储设计；不能成功保存一个让管理操作随后失效的快照，也不能直接丢弃停删保护来减小文件。

主证据测试：`deep_control_record_growth_also_occurs_with_standard_uuid_ids`。早期使用较长但合法 ID 的简短探测也保留在测试中，**本条结论不依赖长 ID**，与已有 CLI/事件的大 ID 预算问题不同。成功镜像后的覆盖退休已另作正常对照；这里只在旧保护因镜像失败持续保留时累计。

## DS03 — P2：revision 跨过 i64 上限时，正常递增可成功保存无法读取的配置

这是**极端数值边界**，不表示正常使用频率下容易发生。前提是已保存/候选 revision 被设置到允许读取的 `9,223,372,036,854,775,807`，例如手工编辑计数字段。

测试先保存并读取 `i64::MAX - 1`，再正常递增、保存、读取 `i64::MAX`，确认边界前的配置完全有效。随后分别执行：

- 与离线 down 相同的 `checked_add(1)` 加 `commit_control_for` 保存顺序。
- 生产 `recover_from_candidate(false)`，其内部会将规则停用并将候选 revision 加一。

两条路径都成功返回，`warning=None`，revision 为 **9,223,372,036,854,775,808**。之后 applied 和候选都无法读取；错误为 `u64 value was too large`。

**根因：** Config 的 revision 是 u64，离线递增 [offline.rs:107](../../crates/fwm/src/offline.rs#L107) 及恢复递增 [recovery.rs:38](../../crates/fwm-core/src/store/recovery.rs#L38) 只检查 u64 溢出。[control.rs:137](../../crates/fwm-core/src/store/control.rs#L137) 使用的 TOML serializer 接受这个值，但读取端 [store.rs:197](../../crates/fwm-core/src/store.rs#L197) 先解析成 `toml::Value`，该中间表示拒绝超出 i64 的整数。写入与读取接受的值域不同。

**期望：** 在任何持久化替换之前拒绝无法往返解析的 revision，保留旧快照并明确报告版本耗尽；或让写入和读取的整数表示保持一致。恢复入口也必须满足同一约束。

证据测试：`deep_next_revision_after_i64_max_is_successfully_saved_but_unreadable`。该问题与 ST02 的 recover 版本回退/CAS 复用独立。另测 schema 0、4 和 u32::MAX：候选读取及恢复都提前失败，原 applied 字节不变，未发现新的 schema 替换问题。

## DS-C01 — 条件性风险：候选目录同步失败后仍清除控制保护

候选镜像的 rename 成功、但目录 fsync 注入失败时，[io.rs:63](../../crates/fwm-core/src/store/io.rs#L63) 返回 `Ok(Some(durability warning))`。[control.rs:168](../../crates/fwm-core/src/store/control.rs#L168) 将它与完全成功归入同一分支，继续同步写出不含控制头的 applied。

测试确认操作返回成功并带 durability 警告，同时控制头已经清除。随后**仅以文件恢复操作模拟未同步候选 rename 在崩溃后丢失**，保留已同步的新 applied：load 仍显示 stopped，但 `read_candidate` 得到 running，下一次 reload 可恢复运行。若同时令最后的控制头清除失败，则同一模拟仍读出 stopped，说明保护记录正是差异来源。

这是代码顺序与故障模型证据，**没有执行真实断电，也没有证明具体文件系统必然以该组合恢复；不计入 3 个确认问题**。期望镜像耐久性未确认时继续保留保护。该边界是原报告留下的 fsync 候选，本轮提供了显式模型与保留保护对照。

## 覆盖与停止边界

本轮实质新增 3 项后，又检查了各自相邻边界：并发编辑与预存草稿、普通 UUID 与短名称、成功镜像后的记录退休、revision 上限前后、unsupported schema 原文件不变、多轮 down/up/down、镜像失败后的重试、候选同步失败与控制头清除失败组合。这些关联检查没有再产生第 4 个独立确认问题。

未验证原生文件系统崩溃恢复、真实磁盘满、不同平台的目录 fsync 实现，也没有将 synthetic 旧 schema 控制头迁移场景计为新发现。本轮结果仍不保证不存在其他 bug。

最初的数值往返不变量测试确实失败并揭示 DS03，原始失败结果保留于 [deep-storage-first-invariant-failure.json](deep-storage-first-invariant-failure.json)。随后把它收紧成上述边界前正常控制及 down/recover 两条路径，最终结果文件为 7/7 通过的可重复审查断言。
