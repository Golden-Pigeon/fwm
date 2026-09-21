# 续审：异步事件归属与代次

2026-09-20，从原任务指定的状态机进度继续。仅审查，没有修复生产实现。

## 结论

既有 **C4 / P2** 已用当前生产 `EventJournal` 和 `Rule` 模块补充确认；共 7 个案例的行为断言通过，没有增加独立问题编号。

| 案例 | 实际结果 |
|---|---|
| 旧规则事件不带 server_id，消费前规则迁至另一服务器 | 事件和历史标签都被写成新服务器，原来源无法还原 |
| 旧事件显式带旧 server_id，消费前迁移规则 | 内层 event 保留旧服务器，外层历史标签却是新服务器 |
| 同一 ID 改组后再消费旧事件 | 旧事件被标为新组 |
| 删除规则后再消费旧事件 | 正常保留已删除规则的旧标签 |
| 删除后用新 ID 重建同名规则 | 正常保留旧 ID 的归属，没有串到重建规则 |
| 旧 generation 的任务完成 | `Rule::update` 抑制旧状态；`connection_error` 仍发布无代次、无服务器的事件 |
| 规则删除后的旧任务报告连接错误 | 仍发布旧 ID 事件；它可用于追溯，但必须携带原来源，不能依赖未来同 ID 的当前标签 |

前五种落盘案例中，传入的 `timestamp_ms=1234` 都被记录时的当前时间取代。页游标在这些小事件案例中正常递增，没有据此另报分页缺陷。

## 生产控制流

- [Rule::update](../../crates/fwm-core/src/engine/state.rs#L73) 校验 generation；[connection_error](../../crates/fwm-core/src/engine/state.rs#L115) 不带 generation 或服务器上下文。这不意味着所有迟到错误都应删除：旧任务的结束信息仍有追溯价值。
- [daemon 主循环](../../crates/fwm/src/daemon/mod.rs#L65) 取得事件后等待共享状态锁；配置提交持锁时可先执行 [finish_commit 的标签更新](../../crates/fwm/src/daemon/state.rs#L130)，所以延迟消费时序在真实调度中可达。
- [record_engine](../../crates/fwm/src/daemon/events.rs#L116) 丢弃源时间；[record_scoped](../../crates/fwm/src/daemon/events.rs#L135) 使用最新标签补 server_id，[HistoryLabels::apply](../../crates/fwm-core/src/history.rs#L55) 又可覆盖外层服务器归属。
- [设计约定](../../DESIGN.md#L348) 要求历史保留事件当时的规则名、稳定 ID、服务器和分组。事件结构缺少相应上下文，无法通过日志显示层修正。

建议修复时为事件保留产生时间、原服务器与规则代次/标签快照；消费端不能将与显式来源冲突的当前标签覆盖进去。删除、新 ID 同名重建的正常行为应继续保留。

## 验证与边界

```sh
cargo build -p fwm --offline --locked
python3 audits/2026-09-20-postfix/run_resume_events.py
```

探针：[resume_events.rs](resume_events.rs)；运行器：[run_resume_events.py](run_resume_events.py)；结果：[resume-events.json](resume-events.json)。直接导入当前生产模块，临时目录自动清理。没有网络 socket、SSH、agent、daemon 或真实系统服务操作，也没有重放旧任务末尾的 API 探针。

补查已覆盖移动、改名/改组、删除、同名新 ID、旧代次和分页对照，最后一轮没有新的独立根因。测试通过表示成功确认当前行为，其中的错误行为仍需修复；不是修复回归已通过。
