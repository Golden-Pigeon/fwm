# CLI completion / wait 全路径续审

日期：2026-09-20。本轮进入上一轮 96 项离线变更未覆盖的 completion、等待就绪、启动后返回和并发配置替换路径。确认 **1 个新独立根因 WC01 / P2**。完成 34 项检查：22 个内存 completion 场景、12 个实际 CLI 参数解析场景，其中 5 个 completion 场景复现 WC01 的不同表现，其他 29 项为正常/失败对照。没有修改生产代码或 REVIEW.md。

证据：[whole-cli-completion.json](whole-cli-completion.json)；可复现驱动：[whole-cli-completion.py](whole-cli-completion.py)、[whole-cli-completion.rs](whole-cli-completion.rs)。

## WC01 / P2：等待只绑定规则 ID，被替换的运行配置可替原请求报告 ready

**实际结果：** 原操作保存 revision 7，`rule-0` 请求 `127.0.0.1:42000 → localhost:8080`，第一次观察仍是 starting。另一个正常编辑保持该规则 ID，保存 revision 8，将映射改为 `127.0.0.1:43000 → localhost:9090`。随后只有新映射 established。原命令在第二次查询时仍成功返回：

```json
{
  "ok": true,
  "data": {
    "revision": 7,
    "ready": true,
    "state": "established",
    "config": {"forwards": [{"id": "rule-0", "listen": "127.0.0.1:42000", "target": "localhost:8080"}]},
    "runtime": {"config_revision": 8, "forwards": [{"id": "rule-0", "listen": "127.0.0.1:43000", "target": "localhost:9090", "state": "established"}]}
  }
}
```

上例为完整证据中的字段节选。42000 在此有效时序中从未就绪，却和 `ready:true` 一起出现在原保存结果中。脚本据此继续访问或依赖原监听会失败；用户也没有收到请求已被新运行配置取代的明确结果。

**同根因的关联实测：** 批次中的一个成员改变端口、规则移动到另一个服务器、local 变成 dynamic，都会让原命令对新语义报告 ready。第五个场景只对同一服务器 ID 做正常 `PutServer`，将 host 从 `alpha.invalid` 改为 `replacement.invalid`，服务器显示名、规则 ID 和监听映射字符串都保持相同，也报告原请求成功。因此仅比较 `ForwardStatus` 的可见映射字符串无法完整修复；服务器连接配置也是运行语义的一部分。

**根因与定位：**

- [completion.rs:52](../../crates/fwm/src/cli/completion.rs#L52) 已解码保存结果，包含配置和 revision，但 [completion.rs:66](../../crates/fwm/src/cli/completion.rs#L66) 只把 `ids` 与时限传入 `wait_ready`。
- [completion.rs:138](../../crates/fwm/src/cli/completion.rs#L138) 按 ID 筛选；`:139–145` 只检查数量和 `Established`。不验证状态是否属于请求保存的运行配置。
- [completion.rs:68](../../crates/fwm/src/cli/completion.rs#L68) 随后在原 mutation response 中设置 `ready:true` 并加入新 snapshot，原 config/revision 保持不变。

**可达性已经追踪，未依赖非法模拟响应：**

1. `add --wait` 在 [add.rs:43](../../crates/fwm/src/cli/add.rs#L43) 调用该 completion；`up/restart --wait` 在 [forwards.rs:152](../../crates/fwm/src/cli/forwards.rs#L152) 解析选中 ID，在 `:166–175` 完成提交/启动后调用同一函数。
2. 初始 mutation 的 expected_revision 检查保护提交时的选择，[dispatch.rs:101](../../crates/fwm/src/daemon/dispatch.rs#L101) 不会把这个锁延伸到客户端稍后的循环 Status 请求。普通后续 edit 可以在两次轮询之间提交；每次 Status 由 `:117–121` 组合当前配置 revision 和当前引擎快照。
3. 所有替换配置都实际通过生产 `configuration::prepare` 和 `Config::validate`，规则保持相同 ID，正是已有编辑语义。服务器移动/目标改变会经 [engine/mod.rs:116](../../crates/fwm-core/src/engine/mod.rs#L116) 的 `runtime_equal` 判定换代，`:143–163` 将新映射写入状态；新代正常建立后可以产生脚本中的 Established 状态。服务器 host 改动也会改变 connection key。
4. 测试直接使用生产 completion/output 代码；客户端替换为内存响应队列，运行状态用合法配置生成并模拟正常状态推进。没有 IPC 帧、外部服务、真实 SSH 连接或解析实验。这里证明的是客户端对合法并发状态序列的处理错误，不宣称做过网络端到端实测。

**修复方向：** 将等待绑定到已接受操作的运行语义；选中规则的监听、方向、服务器/连接配置等被后续编辑取代时，明确返回 superseded/configuration_changed 一类结果。可以使用由服务端提供的操作/规则代次或运行配置标识，并定义同语义 retry、后台重启的继承规则。不能简单要求全局 revision 永远等于保存 revision：仅改标签或修改无关服务器时，原监听仍应正常完成等待；本轮已有通过的对照。

**去重说明：** C3 是只读 status 在多次读取间把不同 revision 的选择配置与快照拼接，导致选错行；修复 `status_watch::load` 不会改变 `completion::wait_ready`。WC01 是变更命令等待时没有把成功结果绑定到其接受的运行语义，可在每一份单独的 snapshot 都完全一致时发生，单列一个根因。RE01 是换代后旧连接计数不归零，与此处 ready 判定无关。五种替换场景只计 WC01 一项。

## 邻接边界和排除项

| 检查 | 结果 |
| --- | --- |
| 批次一部分 ready、一部分 starting | 不提前成功；全部建立才成功，否则到时返回 wait_timeout。 |
| 选中成员 needs_attention | 立即返回 needs_attention，保留 saved:true 和保存 revision。 |
| 未选中规则 needs_attention | 不阻止本次已选对象成功。 |
| 等待期间向原组增加一个 starting 成员 | 继续等待本次提交选中的原 ID，新增成员不被误加到等待集合。 |
| 删除、停止原规则，或删后同名新建不同 ID | 均不误报 ready；到时返回超时并保留对应快照。 |
| 仅改名、修改无关服务器 | 正常成功；revision 增长本身不构成错误。 |
| 同配置 daemon 更换实例 | 新实例同一规则真正 established 后成功，未把实例 ID 变化本身列为缺陷。 |
| Status 请求不返回或比 deadline 更慢 | 350ms 等待在虚拟时钟 351ms 内结束；额外 1ms 是 Tokio timer 分辨率，没有无限等待。 |
| 查询中断 | 返回 wait_failed，保留最后快照和 saved:true；watch 承诺继续监视不等同于一次性 wait 的错误恢复契约。 |
| 启动失败 | 保留 saved:true、原 revision、ready:false。底层错误被包成 daemon_unavailable，但消息保留原因，未仅据错误分类差异再计问题。 |
| 启动阶段与等待时限 | 模拟启动 2s 加 350ms readiness 时限；ready 阶段按时限执行。当前 help 描述等待监听时限，未把总命令耗时超过该值单列为确认缺陷。 |
| 未指定 wait | 不查询运行状态，ready:null，避免仅凭保存成功宣称就绪。 |
| CLI wait/timeout 参数 | --wait 默认20s；--timeout 单独生效；明确 timeout 覆盖默认；ms/s/m 和裸秒解析正确；零、负值、小数、空值、缺值、未知单位拒绝。 |

另从调用方排除：空 ID 集合的 vacuous success 不作为问题，add 保证非空批次，up/restart 在 `forwards.rs:153–155` 明确拒绝空选择。重复 ID 或 `desired_state=stopped` 却 `state=established` 的人为矛盾响应未被用于确认缺陷。restart 本身会在 `engine/lifecycle.rs:138` 将新代标记 Starting，旧代更新受 `engine/state.rs:85–86` 约束；没有将 RE01 的旧活动计数误解为旧监听可立即通过 readiness。

## 完成范围

第一组20个 completion 场景确认 WC01；后续关联扩展到22个 completion 加12个参数场景，补强同一根因，没有第二个独立新根因。原96项离线变更没有重跑。本轮没有验证真实网络、凭据、服务管理器或多进程调度；结论严格限于已追踪可达路径和内存状态序列，不保证其他功能已无问题。
