# CLI 行为与易用性复查

> 修复状态：本页保留修复前审查快照；U01–U36 的当前行为与回归入口见[修复对照](FIXES.md)。下文的“待处理”“未修改实现”等描述均指原审查时点。

日期：2026-09-20。审查对象为当前 Rust 源码及 `target/release/fwm`，源码和二进制 SHA-256 见 [证据快照](evidence/source-sha256.json)。本轮仅审查和记录，未修改实现、未开始修复。

## 结论与计数

共整理 **36 项待处理审查项**：上轮已经报告的 10 项，加上本轮新增的 26 项。这里包含行为缺陷、确定的失败路径风险和明确标注的易用性/诊断缺口，不把所有条目都说成已经发生的线上故障。

没有预设条数。按命令族、配置来源、正常/失败/重试/重命名/跨进程状态逐轮检查，沿新发现继续追查，最后重新遍历对应检查矩阵。**各范围最后一轮均未发现新的可确认问题**。这是本次覆盖范围的停止标准，不是“程序已经不可能有任何问题”的证明。

归并规则：同一原因的多个样例合并；例如 OpenSSH 布尔值的三个表现合为一项，IPv4/IPv6 监听校验的误拒与漏检合为一项，日志组名简写的两种不一致合为一项。组按名称 watch 后改名变空，经交叉审查认定符合目前文档描述的标签集合语义，列入排除项，不计问题。

证据：**A** 为 Release CLI、临时 IPC/agent/回环 SSH/socket 实测；**B** 为明确条件下由源码调用顺序确定，未做真实操作系统服务实验。P1 优先修复；P2 行为或可靠性问题；P3 易用性、恢复或诊断缺口。表内定位是当前源码，不是建议实现。

## 优先处理

- U30：拼错启停字段仍通过校验，并保存成 running。
- U14：名字与稳定 ID 冲突时可能操作错误对象。
- U18：SSH 别名已改指新端点，重启所选服务器规则却继续使用旧连接。
- U20：有效的 `IdentitiesOnly true` 被当成 false，非法值也没有被拒绝。
- U22/U23/U29：安装服务失败、写入中断和并发回滚可能停掉旧后台或破坏服务定义。
- U25：同一配置目录的等价拼写会产生不同服务/Windows IPC 身份。

## 上轮已知，仍未修复

| ID | 优先级/证据 | 问题、触发与建议 |
|---|---|---|
| U01 | P2 / A | `edit apps --rename services` 同时把成员的自定义名字改成 `services-端口`；跨服务器合法的同端口成员会重名而无法改组名。组改名应保留成员名，成员批量改名应单独表达。[forwards.rs:45](../../crates/fwm/src/cli/forwards.rs#L45) |
| U02 | P2 / A | 将组 alpha 重命名为已有 beta 会静默合并，之后 down/remove beta 的范围扩大。应拒绝冲突，合并需明确操作。[forwards.rs:47](../../crates/fwm/src/cli/forwards.rs#L47) |
| U03 | P2 / A+B | 相对 SSH 配置路径依赖调用/后台 cwd；同文件 `ssh.conf`、`./ssh.conf`、绝对路径还被误判不同。identity/known_hosts 路径有同类代码路径。保存时应明确基准、规范化并比较文件身份。[server_selection.rs:18](../../crates/fwm/src/cli/server_selection.rs#L18) |
| U04 | P2 / A | 同一主机已信任，再执行不带 fingerprint 的 trust 仍提示确认；JSON 同时返回 trusted 和 trust_required。已验证同一密钥应幂等成功。[servers.rs:102](../../crates/fwm/src/cli/servers.rs#L102) |
| U05 | P2 / A | 目标的独立 known_hosts 会传给跳板，单独 trust jump 却写另一文件；显示成功仍无法连接目标。需要按目标上下文信任 hop，或给出使用正确数据库的补救命令。[connection.rs:187](../../crates/fwm-core/src/ssh/connection.rs#L187) |
| U06 | P2 / B | 换安装路径升级后，新 binary 的 daemon restart 仍通过既有服务定义启动旧 binary。应核验/刷新托管路径，或明确要求重新安装服务。[client.rs:155](../../crates/fwm/src/client.rs#L155) |
| U07 | P2 / A+B | service uninstall 的帮助和结果只说取消自启，实际也停止后台及转发。应明确副作用，或支持保留当前运行。[daemon.rs:64](../../crates/fwm/src/cli/daemon.rs#L64) |
| U08 | P3 / A | 已有规则缺少加入/换组/退组的 CLI 入口，status 也不显示组归属。应补组整理和发现入口，并保留规则 ID/历史。[args.rs:283](../../crates/fwm/src/cli/args.rs#L283) |
| U09 | P3 / A | 当前只有用户服务，却强制填写 service install/uninstall 的 --user。可默认用户模式，保留参数兼容。[args.rs:388](../../crates/fwm/src/cli/args.rs#L388) |
| U10 | P3 / A | add/up/restart 接受显式 --timeout 而没有 --wait，此时 timeout 实际被忽略。应明确关联、自动等待或拒绝无效组合。[forwards.rs:150](../../crates/fwm/src/cli/forwards.rs#L150) |

## 本轮新增：参数、端口与对象身份

完整证据见 [CLI 分报告](cli.md) 和 [逐命令记录](evidence/fwm-audit-cli.json)。

| ID | 优先级/证据 | 问题、触发与建议 |
|---|---|---|
| U11 | P2 / A | 数字规则名随参数顺序被误吃：`edit 1234 --remote` 成功，`edit --local 1234` 却报缺 NAME。需要消除旧式 SPEC 与名称的歧义。[input.rs:30](../../crates/fwm/src/cli/input.rs#L30) |
| U12 | P2 / A | 原 bind 为 ::1，`edit web --local=3001:new.internal:8081` 省略可选 bind 后仍被重置为 127.0.0.1，违背编辑保留未指定地址的帮助说明。[parse.rs:57](../../crates/fwm/src/cli/parse.rs#L57) |
| U13 | P2 / A | `[2001:::1]`、`[not:ipv6]` 等明确无效 IPv6 目标能保存且 validate 成功，直到流量到达才失败。应在解析字面量时校验。[model.rs:50](../../crates/fwm-core/src/model.rs#L50) |
| U14 | P2 / A | 名称可等于另一规则/服务器的真实 UUID；按该 UUID status/remove/add --server 会先命中名字而操作错误对象，组名也可撞规则 ID。应消除跨命名空间歧义。[model.rs:388](../../crates/fwm-core/src/model.rs#L388) |
| U15 | P2 / A | 两个合法长服务器名的不同尾部被自动组名截掉，分别创建的批次静默并为同一组。自动组应按服务器身份区分，或拒绝冲突。[add.rs:169](../../crates/fwm/src/cli/add.rs#L169) |
| U16 | P2 / A | 双栈重叠校验同时有误拒和漏检：0.0.0.0:p 与 ::1:p 可实际同时 bind，却被拒绝；127.0.0.1:p 与 IPv4-mapped ::ffff:127.0.0.1:p 实际冲突，却能通过同组校验。应按地址族及映射关系判断。[model.rs:504](../../crates/fwm-core/src/model.rs#L504) |

## 本轮新增：SSH 配置与认证

完整证据见 [SSH 分报告](ssh.md)。S4–S8 对应以下五项。

| ID | 优先级/证据 | 问题、触发与建议 |
|---|---|---|
| U17 | P2 / A | 当前终端 agent 已加载密钥，在线 check 却使用后台启动时的失效 socket；离线 check 成功，在线 check 失败，错误只让用户 load key。应显示实际 agent 来源/socket，并提供后台环境刷新或固定 IdentityAgent 指引。[auth.rs:218](../../crates/fwm-core/src/ssh/auth.rs#L218) |
| U18 | P2 / A | SSH config Port 改变后，check 已读新值，但 `restart --server dev --wait` 和 config reload 仍保留健康旧 SSH 会话并显示 ready。应提供该服务器连接刷新，或检测配置变化并明确提示。[engine/mod.rs:85](../../crates/fwm-core/src/engine/mod.rs#L85) |
| U19 | P2 / A | 原生 OpenSSH 可用 `IdentityFile key.pub` 从 agent 选择对应私钥，fwm 却把公钥当损坏私钥、又因 IdentitiesOnly 筛掉正确 agent key。应识别公钥文件。[auth.rs:35](../../crates/fwm-core/src/ssh/auth.rs#L35) |
| U20 | P2 / A | 有效 `IdentitiesOnly true` 被当 false，非法 potato 也被当 false；StrictHostKeyChecking true、ForwardAgent false 有同类错误解析。应按支持的 OpenSSH 选项真实取值解析并拒绝非法值。[config.rs:192](../../crates/fwm-core/src/ssh/config.rs#L192) |
| U21 | P3 / A | 显式指定的私钥不存在时被静默跳过，最终错误只让用户配置 agent/identity，不指出已配置路径缺失。应区分自动探测默认文件与显式文件，并保留实际失败原因。[auth.rs:44](../../crates/fwm-core/src/ssh/auth.rs#L44) |

## 本轮新增：后台与服务管理

完整证据及 A/B 边界见 [服务分报告](services.md)。下列源码项是明确前提下的控制流确认，没有实际安装服务或制造磁盘满。

| ID | 优先级/证据 | 问题、触发与建议 |
|---|---|---|
| U22 | P2 / B | service install 先停止旧后台，后续定义/注册失败却不恢复原运行状态；局部 definition 回滚不等于恢复整个接管操作。应保存并恢复接管前状态，或明确告知后台已停止及补救。[service_lifecycle.rs:58](../../crates/fwm/src/cli/service_lifecycle.rs#L58) |
| U23 | P2 / B | 服务定义先被 fs::write 截断，写成功后才建立回滚 guard；中途失败可留下半份旧定义。stage 和 rollback 都应使用原子替换。[service.rs:112](../../crates/fwm/src/platform/service.rs#L112) |
| U24 | P2 / A | 持锁、监听 IPC 但不回应的实例被 status 当成 stopped；start 又 spawn 冲突实例；stop 只不断 ping，未发 shutdown，却说 still shutting down。应区分不存在与无响应，并准确报告实际阶段。[client.rs:143](../../crates/fwm/src/client.rs#L143) |
| U25 | P2 / A+B | 同一 config-dir 的 p、p/. 等价路径产生不同 service hash；Windows pipe 同样依赖原始字符串。会找不到既有后台/服务，甚至重复注册争锁。应建立规范化实例身份。[paths.rs:17](../../crates/fwm-core/src/paths.rs#L17) |
| U26 | P2 / B | 本地 marker 存在性被当成系统注册状态。marker 丢失但 OS task 尚在时 uninstall 假成功；marker 尚在、task 已删时清理又会卡在停止失败。应分别检查与幂等清理两侧状态。[service.rs:27](../../crates/fwm/src/platform/service.rs#L27) |
| U27 | P2 / B | launchctl/systemctl/schtasks 使用无时限的同步 output；5 秒启动检查和 15 秒停止检查都在它返回后才开始，manager 卡住时 CLI 可无限等待。应给该阶段单独设有界等待并保留结果不确定性。[service.rs:48](../../crates/fwm/src/platform/service.rs#L48) |
| U28 | P2 / A+B | 服务命令非零退出、后台停止超时被编码为 invalid_request/exit 2，混淆运行故障与参数错误。应返回明确阶段错误码及正确退出码。[cli/mod.rs:114](../../crates/fwm/src/cli/mod.rs#L114) |
| U29 | P2 / B | 服务安装/卸载没有跨进程串行化；A 失败回滚可覆盖 B 已成功提交的新定义。应在整个服务事务持操作锁，并核对回滚所有权。[service.rs:105](../../crates/fwm/src/platform/service.rs#L105) |

## 本轮新增：配置、日志与持续查询

完整步骤见 [状态与配置分报告](state.md)，JSON 在 evidence 目录。

| ID | 优先级/证据 | 问题、触发与建议 |
|---|---|---|
| U30 | P1 / A | server/forward 未知字段被忽略；`desired_sate="stopped"` 通过 validate，export/reload 后却是 running；`usr` 同样悄悄失效。应拒绝未知字段，尤其不能把错拼启停意图静默换成默认值。[model.rs:235](../../crates/fwm-core/src/model.rs#L235) |
| U31 | P2 / A | 手写/导入 schema 3 的 remote 规则未写 remote_cleanup 时得到 off/shared，与 CLI 默认 verified/dedicated 不同。默认恢复能力取决于创建途径。应统一方向相关缺省值，并保留显式 opt-out。[model.rs:133](../../crates/fwm-core/src/model.rs#L133) |
| U32 | P2 / A | 历史同名规则可遮蔽当前组：status task 选择新组，logs task 却只返回已改名的旧规则；logs GROUP 和 --group GROUP 对改名前历史也不等价。应明确名称空间，确保已承诺等价的组选择方式一致。[history.rs:161](../../crates/fwm-core/src/history.rs#L161) |
| U33 | P3 / A | logs --server dev --follow 在服务器改名后仍静默等待，后续事件只带新标签而不再匹配。历史记录没有丢失，当前按事件标签过滤有设计依据；需要说明此语义/作用域变化，或开始时解析成服务器 ID。[query_selection.rs:70](../../crates/fwm/src/cli/query_selection.rs#L70) |
| U34 | P3 / A | applied 损坏、candidate 有效时，validate 成功而 reload 只返回底层 TOML 错误，没有受支持的恢复入口/指引。拒绝盲目覆盖损坏快照本身合理，应补显式且保留备份/控制意图的恢复流程。[store.rs:42](../../crates/fwm-core/src/store.rs#L42) |
| U35 | P2 / A | status --watch 在已经输出一次正常快照后，下一次 IPC 断开即 exit 5，无法持续观察后台重启/临时失联。应对运行时通信故障保持监视并报告状态变化，参数错误仍立即拒绝。[queries.rs:28](../../crates/fwm/src/cli/queries.rs#L28) |
| U36 | P2 / A+B | 服务器配置、信任与连接类事件没有 server 标签；logs --server dev 无法查到这些事件，即使无筛选日志中存在 server saved。应为服务器级事件保留归属。[offline/history.rs:30](../../crates/fwm/src/offline/history.rs#L30) |

## 分轮与覆盖边界

| 范围 | 原始各轮新增 | 最后复查 |
|---|---|---|
| CLI 参数/创建/编辑/端口/对象选择 | 5 → 2 → 0 | 19 个隔离案例、400 次 Release CLI 调用；双栈两项在总表合并为 U16，所以新增归并为 6 项。 |
| SSH/信任/认证/路径/连接刷新 | 2 → 3 → 0，再完整回扫 0 | 对照原生 OpenSSH，使用临时 sshd/agent；新增归并为 5 项。 |
| daemon/service/失败与并发路径 | 3 → 5 → 0 | 正常、失败、重复、竞争、三平台代码矩阵；新增 8 项，区分 A/B。 |
| 配置/状态/日志 | 4 → 3 → 1 → 0 | 交叉审查排除组 watch 改名这一项；日志同根因表现合并，新增 7 项。运行中 IPC 断开另补实测，只加强 U35 证据，不另计问题。 |

全部实现代码保持不变。CLI 数据与网络实验使用临时目录、回环监听、临时 SSH/agent；未操作真实远程服务器、默认后台或真实登录服务。操作系统登录/休眠/Windows 原生 ACL 等未经原生实验的行为不冒称已验证。

明确排除的候选包括：SOCKS 显式 Replace 语义、保留未指定服务器覆盖字段、草稿期间控制意图保护、未知主机密钥必须明确确认、已声明不支持且错误清楚的 SSH 功能、组按名称动态监视后的改名变空，以及已经明确描述的 daemon stop 保留 running intent。详细排除矩阵见分报告。

后续修复应按这些复现建立回归，保留跨模块交叉复查；不能仅以“测试数增加”或“覆盖率达标”替代用户可观察行为的验证。
