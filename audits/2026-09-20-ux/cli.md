# CLI 只读审查证据

使用当前 `target/release/fwm`，不编译、不修改源码。全部操作均使用独立临时 `--config-dir`，服务器配置是 `127.0.0.1:1`，创建的规则均 `--disabled`。400 次 CLI 调用、19 个独立案例；每个案例结尾检查 `daemon_running=false`。两个地址重叠判断另有短生命周期本机 socket 绑定证明，没有连接真实 SSH 服务器。

详细逐命令 argv、退出码、JSON/error、配置前后状态见 `evidence/fwm-audit-cli.json`。

## 轮次和停止条件

- 第 1 轮：创建/编辑/端口/命名结构审查和 release 实测，新增 5 项（CLI-N1～N5）。
- 第 2 轮：沿参数语法、IPv6、身份和自动命名线索继续检查，新增 2 项监听重叠问题（CLI-N6～N7）；其余映射往返、非法输入、原子性检查通过。
- 第 3 轮：完成剩余创建拼写、30 种编辑组合、非法组合/选择器、512 上限、SOCKS 转换和已知根因延伸矩阵，新增 0 项。
- 停止条件已满足：本审查范围内一轮完整复查未发现新的独立问题。这不是所有可能输入、操作系统和运行环境绝对无缺陷的证明。

## 新问题

### CLI-N1 — 数字名称会随参数顺序被方向选项吃掉

创建名称 `1234` 后，`edit 1234 --remote` 成功；等价排序 `edit --local 1234` 却退出 2，报告缺少 NAME。名称 `3000-3001` 同样如此。失败操作保持配置原样。

根因：`crates/fwm/src/cli/input.rs:30` 将方向后的纯数字/范围状 token 无条件归一化为旧式 SPEC，即使它实际上是 edit 的必填名称。预期：支持相同的参数排序，或者明确且一致地消除 edit 的这个歧义，不能把用户已提供的有效规则名当成未提供。

### CLI-N2 — 完整映射编辑省略可选 bind 时，仍静默重置地址

原规则 `[::1]:3000:db.internal:8080`，执行 `edit web --local=3001:new.internal:8081` 后，绑定变为 `127.0.0.1:3001`。`--port` 和方向单端口拼写已能保留绑定，但完整映射中的可选 bind 没有同样处理。

根因：`crates/fwm/src/cli/parse.rs:57` 直接走完整替换，`parse.rs:159` 对未写 bind 的三段式调用默认 loopback 的 `listening`。`args.rs:295` 的 edit 帮助明确承诺保留未指定地址。预期：编辑时未指定 bind 就保留现有值，明确指定新 bind 才替换。

附带观察：`edit socks --dynamic 1081` 也会重置 `[::1]` 为 `127.0.0.1`，但 `--dynamic` 帮助明确写的是 Replace；本报告不把此行为单独算作新缺陷。

### CLI-N3 — 无效的方括号 IPv6 目标能保存并通过配置校验

`add --server dev --name bad --local=3200:[2001:::1]:8080 --disabled` 成功，`config validate` 也成功；`[not:ipv6]` 同样被接受。明显错误会拖到实际流量到达后才失败。

根因：`crates/fwm-core/src/model.rs:50` 的 Endpoint 解析只去掉两端方括号，没有校验所包含的 IPv6 字面量。预期：对明确使用 IPv6 字面量语法的目标，在保存前拒绝无效地址。

### CLI-N4 — 名称和稳定 ID 共用输入空间，但没有跨空间冲突校验

三种 release 实测：

1. 两条规则 first、second，把 first 重命名为 second 的真实 UUID。随后 `status UUID` 返回 first，`remove UUID` 删除 first，而拥有该 UUID 的 second 留下。
2. 两个服务器 dev、other，把 dev 重命名为 other 的真实 UUID。`add --server UUID ...` 将新规则绑定到 dev 的 ID，而不是 other。
3. 新建组，名称等于已有规则 outside 的真实 UUID；`status UUID` 选择 outside，`status --group UUID` 选择另一个 member。

这些复现没有手写伪造 ID，全部使用应用生成的 UUID。

根因：`crates/fwm-core/src/model.rs:388` 和 `:393` 按数组顺序查找 `id == selector || name == selector`；`:426` 之后只分别校验 ID 集合、名称集合及 group/name 冲突。预期：稳定 ID 必须可靠定位自己的实体；新增/重命名应拒绝跨命名空间歧义，或提供明确的 ID 优先/显式选择规则。

### CLI-N5 — 自动组名截断会把不同服务器的批次静默合并

保存两个合法的 100 字节服务器名：`s` 重复 93 次后分别接 `AAAAAAA` 和 `BBBBBBB`。分别添加 remote `5000-5001` 与 `5002-5003`，未指定组名。最终四条规则进入同一个截断后的自动组，`status --group 该组` 选择两台服务器的所有四条。

根因：`crates/fwm/src/cli/add.rs:169` / `:173` 自动组名只按长度截断，`:131` 复用该字符串时不区分来源服务器。自动规则名碰撞会安全报错，但自动组碰撞直接合并。预期：自动组名必须按服务器身份稳定区分，或检测到跨服务器截断碰撞时明确报错。

### CLI-N6 — IPv4 wildcard 被错误地认为覆盖 IPv6 specific

同组 `0.0.0.0:3200` 与 `[::1]:3200` 无法保存；分开保存为 stopped 后，把候选配置中两条都设为 running，`config validate` 也报监听冲突。

实际 socket 证明：本机在相同临时端口同时 bind+listen `0.0.0.0` 和 `::1` 成功，未接受任何连接。

根因：`crates/fwm-core/src/model.rs:504` 只看任一 IP 是否 unspecified，不区分 IPv4 wildcard 与 IPv6 specific。预期：IPv4 wildcard 不能被当成 IPv6 wildcard；仅对确有双栈可能的 IPv6 wildcard 保守处理。

### CLI-N7 — IPv4-mapped IPv6 与 IPv4 的真实冲突却漏检

同组 `127.0.0.1:3200` 和 `[::ffff:127.0.0.1]:3200` 都成功保存，`config validate` 返回成功。

实际 loopback socket 证明：先 bind `127.0.0.1` 的随机端口，再用 AF_INET6 bind 同端口的 `::ffff:127.0.0.1`，第二次返回 EADDRINUSE（macOS errno 48，IPV6_V6ONLY 默认 0）。这个组不能同时启动。

根因同处 `crates/fwm-core/src/model.rs:504`，原始 IP 枚举直接比较，没有规范化 IPv4-mapped IPv6。预期：规范化映射地址后再判断监听重叠。

## 已覆盖并排除的类别

- local/remote 的 long、short、附着、等号、旧式空格拼写；12 个单端口等价组合。
- 单端口、含端点范围、逗号列表、前导零、空格和重复项；端口 1/65535、512/513 个不同端口边界。
- src/tgt 多对一；create 参数配对、edit 的独立 src/tgt、互斥校验；三种方向创建均强制 server。
- IPv4/IPv6 完整映射；有效目标往返；未平衡括号、空/空格目标、零/越界端口均原子失败。
- 30 个原方向/指定方向/局部端口编辑组合；未指定目标主机和绑定保留，ID/group/stopped intent 不变。
- 显式完整替换和 SOCKS 转换；SOCKS 转 local/remote 必须明确方向与目标；清理策略随方向正确转换。
- 普通 rename 与 server move 保留身份和运行意图。
- forward 自动名截断发生碰撞时安全报错；Unicode 截断不切断码点且满足 100 字节约束。
- 无效批次、生成名长度溢出、group/rule 同名、无效编辑不会顺带保存新服务器别名。
- 选择器 name/server/group/all 互斥；失败操作配置保持原样；所有测试后台均保持停止。

此前已知的组 rename 覆盖成员/同端口重名、rename 到已有组静默合并、已有规则不能换组/退组、status 不显示 group、timeout 无 wait 被忽略，未重复计数。
