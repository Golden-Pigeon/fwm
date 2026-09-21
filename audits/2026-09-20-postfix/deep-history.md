# 深入续审：日志身份选择与轮转时序

2026-09-20。此前仅做了常规离线读取与一次轮转对照，本轮扩展到先后复用名称，以及读取中发生多次轮转。新确认两个独立根因，没有修改生产实现。

## DH01 / P2：已删除规则的稳定 ID 查询混入另一历史对象

全部操作均使用普通 CLI 和 disabled 规则：创建 `first`，记下 CLI 自动生成的 UUID A；删除它；再创建名称为 A、自动 ID 为 B 的另一条规则；删除 B；最后运行 `logs A`。当前配置中已经没有任何规则，因此不存在当前名称优先的歧义。

实际返回 A、B 两条规则的历史，warnings 为空。单独查询旧名称 `first` 或 B 的 ID 都能正确筛选各自记录。README 178 明确将稳定 ID 作为查询历史对象的方法，但这里无法靠 A 排除 B。

根因：[FollowFilter::select](../../crates/fwm/src/cli/query_selection.rs#L95) 将 `forward_name == selector` 与 `forward_id == selector` 的结果直接取并集并固定下来，没有优先精确历史 ID。[HistoryFilter::select](../../crates/fwm-core/src/history.rs#L192) 也有同样的后备选择逻辑。正常创建时的当前名称/ID 冲突校验只约束同时存在的对象，不能防止跨时间复用。

这与之前的 C4 不同：持久记录本身的服务器、名字和 ID 都正确，错误发生在查询选取阶段。建议先识别精确历史 ID，再做名称/组后备匹配，并在流式选择中固定该 ID。

证据：[CLI 脚本](deep-history-cli.py)、[完整结果](deep-history-cli.json)。新增/删除均离线，所有新增规则 stopped，没有启动 daemon 或网络连接。

## DH02 / P2：读取中的两次轮转让历史逆序，tail 返回旧记录

`read_history` 先打开 active 文件，再打开 archive 文件，但固定先读 archive、后读之前打开的 active。如果在两次 open 之间写入者恰好轮转两次，reader 持有的 active 句柄已经属于更老的文件，而后来打开的 archive 属于更新一代。固定拼接顺序因此逆转。

在临时复制的生产 `storage.rs` 中，只在 active 的 open 之后插入一次写入者调度点，调用未修改的生产 `append_history` 直到发生指定轮转次数。保持真实 2 MiB 轮转阈值，没有修改算法、阈值或手工伪造文件顺序。结果：

- 返回 979 条完整、可解析记录，拼接处序号从 **979 倒退到 1**，没有读取警告。
- 返回内容中最新序号为 979，`tail(..., 1)` 却选中 **490**。
- `HistoryCursor::take_new` 按递增高水位处理，只交付较新一段；已读入内存的旧一段被跳过，并产生 gap 提示。这里没有将带 gap 提示的 follow 描述为静默丢失。
- 零次、一次轮转的对照保持顺序；并发窗口结束后再稳定读取也正常。

主要位置：[storage.rs:22](../../crates/fwm-core/src/history/storage.rs#L22)、[storage.rs:25](../../crates/fwm-core/src/history/storage.rs#L25)，依赖顺序的调用为 [history.rs:248](../../crates/fwm-core/src/history.rs#L248) 和 [cursor.rs:34](../../crates/fwm-core/src/history/cursor.rs#L34)。应检测读取期间的文件代次变化并有界重试，或保证合并后的实例内序号顺序，不能假设文件路径仍代表打开时的那一代。

证据：[调度 fixture](deep-history-rotation.rs)、[临时注入 runner](deep-history-rotation.py)、[三个轮转次数的结果](deep-history-rotation.json)。这是确定性时序注入，不是声称已测得自然负载下的发生频率。日志文件共数 MiB，使用正常大小记录，临时目录自动清理；没有网络、API 或服务操作。

## 关联检查

复核了单次轮转的去重、历史名跨改名关联、当前规则优先、旧 ID 单独查询、部分写后补换行、损坏行后的有效事件、记录截断与 JSON 转义预算。没有将已有 C2 的超长 ID 问题或 C4 的标签覆盖再计数。

最后的零/一次/两次轮转对照没有产生第三个独立根因。已确认的问题仍未修复；这里不对其他模块或所有调度组合做零缺陷保证。
