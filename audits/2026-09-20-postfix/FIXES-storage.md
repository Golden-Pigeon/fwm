# 存储审查项修复

2026-09-21。覆盖 `REVIEW.md` 的 ST01–ST07、DS01–DS03，以及条件风险 DS-C01。ST08 的 SSH 文件读取由 SSH/engine 修复记录覆盖。

| 项目 | 最终行为 | 回归覆盖 |
|---|---|---|
| ST01 | 控制覆盖仅按稳定 ID 应用；候选解析时原本缺少 ID 的对象仍获得确定性 ID。删除旧名称回退，避免草稿把旧名交给另一个 ID 后误停、误启或误删。 | 同一草稿改名/名字重用下分别执行 down、up、remove，未选 ID 内容保持不变，再 reload 验证。 |
| ST02 | `state/committed-revision` 保存独立的 revision 高水位；提交前先确认该文件耐久性。恢复取候选、可读取的旧快照 revision、高水位三者最大值再加一。旧正文损坏或整个 applied 丢失不再复用旧 CAS 版本。 | revision 42 配旧候选 0 恢复为 43；再丢失整个 applied、使用旧候选恢复为 44；高水位写入/同步/替换/目录同步故障不改 applied/candidate。 |
| ST03 | 已保存的绝对 SSH 路径在配置读取、控制及编辑时保持原样，不再次访问外部目录做 canonicalize；认证阶段仍负责实际文件可用性。 | identity 的父目录由目录变为普通文件后，加载、down、清空 identity 和删除规则均可完成。既有路径、token 和可切换符号链接测试通过。 |
| ST04 | 旧组推断只识别规范的十进制端口后缀，并在合并到已有同名组前检查监听冲突；不安全的推断跳过。 | schema 1/2 的 `bundle-3000`、`bundle-03000` 停止替代规则继续合法；推断成员与显式已有成员冲突时保留原分组。 |
| ST05 | 恢复保留原有删除保护，并为全部恢复后存活规则保存 stopped 覆盖。只有候选镜像确认持久化后才退休这些记录。 | applied 正文损坏，恢复镜像替换失败，重新打开 Store 并 reload，已删 ID 不复活，存活规则保持 stopped。 |
| ST06 | 高水位同时作为初始化标记；旧版本已有 `recovery.json` 也视为已初始化。applied 缺失时 load、initialize、普通 commit/reload 明确失败，要求显式恢复。 | 新格式丢失 applied 后四个入口拒绝；显式恢复全部 stopped；旧实例 recovery.json 对照；首次空实例仍可初始化。 |
| ST07 | 在 exists 判断之前检查候选和 applied 的目录项，悬空符号链接不会被当成缺失文件。首次初始化、普通提交均拒绝替换此类候选。 | 悬空 config.toml 下 load/initialize/commit 全部拒绝，原符号链接与缺失目标保持原状。 |
| DS01 | read_candidate 保存实际读到的原始内容版本；控制操作也在开始时记录候选版本。镜像把原候选原子移到同文件系统暂存目录，校验移走的原文件，然后以 `persist_noclobber` 发布，避免检查后的覆盖型 rename。发生冲突时无覆盖恢复原文件；如果编辑器已经又保存另一版本，保留当前草稿和暂存副本并返回副本路径。首次初始化和升级也使用相同发布逻辑。 | 候选读取后编辑、applied 替换后编辑、恰在最终版本检查后/新镜像发布前编辑器原子 rename 均被保留；已打开旧文件句柄在发布期间原地写入的内容保留于报告的备份；首次 load 后才创建的候选不被初始化覆盖。 |
| DS02 | 在任何权威快照替换及 revision 预留前，校验完整 TOML（含控制记录头）不超过读取端 1 MiB 预算；读取也按该预算限制实际字节。不会为缩减大小而丢弃保护。 | 用正常 UUID、短名称构造 15,000 条累计删除保护；超限提交失败，旧 applied 原始字节与可加载性保持不变。 |
| DS03 | Config 统一限制 revision 不超过 TOML 的 i64 整数上限；恢复递增也使用同一上界，initialize 同样先校验。 | i64::MAX−1、i64::MAX 正常写入/读取；下一次控制提交和 recover 拒绝，applied/candidate 原始字节不变。 |
| DS-C01 | 候选目录 fsync 的 durability warning 不再被当作完整镜像成功；保留 applied 中的控制头。 | 注入候选目录同步失败，再仅恢复旧候选字节模拟未同步 rename 丢失；重开 Store 后候选的有效意图仍 stopped。 |

## 验证

- `cargo test -p fwm-core --offline --locked`：186 个库单元测试、7 个 paths 集成测试、4 个 selection 集成测试全部通过。包含当时全部 40 个 Store 测试及此次其他代理并行修改的 core 测试。
- 最终追加首次初始化候选竞争及初始化后连续 daemon 提交测试后，重新执行 `cargo test -p fwm-core store:: --offline --locked`：**42 个 Store 测试全部通过**。
- `cargo test -p fwm --offline --locked --bin fwm daemon::batch::`：12/12；`--test intuitive_add --test ux_commands`：5/5 和 8/8。验证 daemon 初始化后的连续 mutation 会同步候选、IPC 返回与磁盘配置一致，以及首次 CLI 绝对路径输入仍按既有约定 canonicalize。
- `cargo clippy -p fwm-core --all-targets --offline --locked -- -D warnings` 通过。
- 对所改 Rust 文件执行 rustfmt。没有提交 Git，没有修改实际用户配置；故障均注入临时文件与 FileOps，SSH 测试使用仓库本地 fixture。

## 限制与兼容性

- DS01 的无覆盖发布处理普通编辑器的原子 rename 保存，也检测发布过程中已完成的旧句柄写入。无协作的外部程序若一直持有已移走文件的句柄、在镜像成功且清理暂存副本后才无限延迟写入，不能据此宣称完全线性化；没有为每次成功镜像永久保存副本。候选发布中存在很短的路径暂时缺失窗口，applied 始终保留权威配置。
- 冲突副本位于配置目录的 `.fwm-candidate-*/config.toml`，警告给出准确路径，成功无冲突时不保留这些目录。
- 对升级前就已损坏且无法读取任何旧 revision、又没有独立高水位的实例，恢复会明确拒绝猜测 revision；需要恢复旧 revision 字段或高水位元数据后再恢复，以免复用旧 CAS 身份。可读旧快照会在初始化时建立高水位。高水位预留后若快照提交失败，可以有版本空洞，但不使旧快照变化。
- 如果 applied、高水位、旧 recovery.json 等所有实例证据都被人为移除，无法与全新实例区分。正常单独丢失 applied 的报告触发路径已覆盖。
- 目录同步验证为故障模型，没有做真实断电实验；当前平台是 macOS，没有据本次执行结果声称原生 Windows/Linux 文件系统行为已验证。
