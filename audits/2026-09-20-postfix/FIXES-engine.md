# SSH、engine 与 cleanup 修复记录

2026-09-21。对应 REVIEW.md 的 ST08、SE01–SE10、RE01、DE-C01、DE-C02，以及 C4 的事件产生端。事件记录/API 修复由主任务覆盖。

| 编号 | 修复 | 回归证据 |
|---|---|---|
| ST08 | reconcile 只解析有规则引用的服务器。SSH 主配置/Include 拒绝非普通文件，单文件读取限制为 1 MiB；Unix 使用 O_NONBLOCK 打开并再次检查打开句柄类型，避免 FIFO 替换竞争阻塞读取。 | 无规则服务器指向 FIFO 时 reconcile 正常返回且不产生 fingerprint；直接配置与 Include 指向 FIFO 均立即返回明确错误。 |
| SE01 | 运行时端点门禁复用模型的 IPv4-mapped 归一化及地址族 overlap 规则。业务路由仅在 tcpip-forward 成功且 verified ownership 确认完成后安装；回调只允许精确地址或等价 IP 表示，不再仅按唯一端口猜测。 | 内存 SSH 协议 fixture：旧取消拒绝时 mapped 替代项不发起新注册；IPv4 wildcard 与 IPv6 loopback 可同时注册；请求尚未确认时 route 表为空；未知地址不能接入同端口路由。既有取消/拒绝/ownership 补偿测试保持通过。 |
| SE02 | watch 仅在规则集合或 generation 改变时通知；元数据静默刷新，不唤醒 needs_attention/backoff 的 supervisor。 | 规则/服务器重命名及新增无关服务器不改变 watch 版本或错误状态；停止规则仍通知。 |
| SE03 | 每个 Include 文件使用局部 Host 条件状态，返回后恢复外层条件。 | Include 内 Host other 不再使外层 dev 的 Port 失效。 |
| SE04 | 支持的 scalar 配置遵守首值语义；ForwardAgent、Compression、StrictHostKeyChecking 等记录生效首值，被覆盖的后续默认值不再触发拒绝。 | 支持的首值加后续不支持/无效值组合通过；首个值不支持的既有拒绝测试仍通过。 |
| SE05 | Host 别名匹配区分大小写，known_hosts 的共享 wildcard 入口仍保持原有不区分大小写语义。 | DEV 不再匹配 Host dev；known_hosts wildcard 大小写对照保持通过。 |
| SE06 | lexer 只对支持的引号/反斜杠转义消耗反斜杠，普通路径反斜杠与末尾反斜杠保留。 | Windows 风格路径、引号内普通反斜杠/转义双引号、末尾反斜杠测试。 |
| SE07 | 配置中的连接超时、keepalive 间隔、最大重试延迟、稳定重置时间上限统一为一年，避免极端整数进入时间计算。 | i64::MAX keepalive 被拒绝；一年边界可通过校验。 |
| SE08 | 等待连接许可和握手/认证 Future 同时监听 desired 更新，更新优先于许可/连接成功，重新检查最新运行集合。 | 停止排队规则后再释放许可，无 TCP accept；停在握手中的本地连接会关闭，状态为 stopped。 |
| SE09 | 未实现的 IdentityAgent $ENV / ${ENV} 形式明确拒绝，避免当作字面路径成功解析；原 SSH_AUTH_SOCK 特殊值仍支持。 | $SSH_AUTH_SOCK、$FWM_AGENT、${FWM_AGENT} 返回明确配置错误，且被首值覆盖的默认项不会误拒。 |
| SE10 | lsof 请求增加 t 类型字段；依据 IPv4/IPv6 将 *:port 解析为 0.0.0.0/::；缺少类型的 wildcard 明确报 unsupported。 | Python parser IPv4/IPv6 wildcard 对照及缺少 family 的拒绝测试；本机两个临时真实 socket → lsof → parser 的地址族均正确。 |
| RE01 | 显式 restart/retry 更新 generation 时重置 active_connections 与 retry_count，旧 guard 不能污染新代计数。 | 两个旧 guard 在换代后关闭，不影响一个新 guard 的计数；新 guard 关闭后恢复 0。 |
| DE-C01 | 保存组启动参数；reconcile/retry/restart 检查已完成 supervisor 并重建。retry 的 already_established/starting 跳过只适用于活任务。 | 注入完成任务后，三个入口均替换 task ID，替代任务保持存活；停止意图仍不被启动。 |
| DE-C02 | shutdown/server restart 共享一个 5 秒退出预算，超时 abort 后 await join；connected 内被强制停止的 worker 同样 join。 | Tokio 虚拟时钟下注入三个不响应取消的 future，总耗时小于 6 秒，返回时三个 Drop 已完成。 |
| C4 产生端 | 事件产生时携带时间与 EventContext。规则事件捕获 forward/server/group 标签；同代元数据读取当前标签，退休规则迟到事件保留原服务器和标签。 | 排队事件在后续改名后仍保留原上下文/时间；同代改名/组调整标签刷新；同 ID 换服务器后旧代错误仍指向原服务器。 |

验证：`cargo test -p fwm-core --lib --offline --locked`，183 passed；`python3 -m unittest discover -s crates/fwm-core/src/cleanup -p 'test_*.py'`，28 passed。另用本机临时 IPv4/IPv6-only wildcard listener，仅查询测试进程的 lsof `-F pftnT` 输出并经生产 parser 验证，两种地址族均匹配真实 socket。已格式化本域修改。数字是这次执行的仓库快照，其他代理继续添加测试后总数可能增长。

边界：协议测试使用内存 russh 流与本地临时 TCP listener，不连接真实 SSH 服务、agent 或 daemon，不执行远端清理、信号或服务操作。macOS lsof 同时经过 fixture 与本机临时 socket 验证，临时 listener 已关闭；没有测试远端 sshd 的完整 verified 转发。DE-C01/DE-C02 验证仍使用注入异常任务，未声称复现真实 socket 泄漏。尚未确认的“双次 SSH resolve 读取不一致”候选不在本次修复清单中。注册确认前到达的 remote channel 会被拒绝，避免将未确认监听的业务流量交给目标。
