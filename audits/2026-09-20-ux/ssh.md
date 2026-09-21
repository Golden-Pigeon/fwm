# SSH/认证/信任/配置 UX 复查

范围：SSH aliases、SSH config 与环境、私钥/agent、主机信任、ProxyJump、配置刷新与 DNS。只读源码；仅在 `/private/tmp` 创建脚本和临时 fixture。使用现有 `target/release/fwm`，没有编译、修改仓库、访问真实服务器、默认 daemon 或用户默认 agent。所有 sshd/ssh-agent/fixture daemon 已停止，私钥与 fixture 目录已删除。

共确认 8 个独立问题：上轮 3 个，本轮新发现 5 个。以下不是对“缺少某项功能”的泛泛意见，均有 CLI 实测或确定源码路径。

## S1 [P2] 相对 SSH 路径依赖调用/daemon 工作目录，等价路径被误判为不同配置（上轮已知）

复现：目录 A 中存在 ssh.conf，仅指向 127.0.0.1:1。用绝对 `--config-dir` 保存：`server add dev --ssh dev --ssh-config ssh.conf`。A 中 `server check dev` 正确解析配置、得到 loopback Connection refused；到 B 运行相同 check，改为 `SSH config does not exist: ssh.conf`。在 A 显式传 `--ssh-config ./ssh.conf` 或同文件的绝对路径，又被拒绝为 `already configured with a different SSH config`。

实际：相对路径被原样持久化，daemon 继承启动 cwd，路径比较也是字面比较。保存的 profile 可以因切目录/重启方式而失效。相对 `identity_files`、`known_hosts` 经相同 expand_path 原样保留，存在同类 cwd 依赖；其中 SSH config 路径已实测，后两者为明确源码证据。

建议：CLI 持久化路径时定义稳定基准并规范化；判断显式 --ssh-config 是否同一配置时比较规范化路径。不要把所有相对路径一律解释为“当前这次调用的 cwd”。

源：
- `crates/fwm/src/cli/servers.rs:30`（原样保存 ssh_config）
- `crates/fwm/src/cli/server_selection.rs:18`（字面比较）
- `crates/fwm-core/src/ssh/config.rs:47`、`:105`（仅展开 home/tokens，没有基准目录）
- `crates/fwm/src/platform/background.rs:19`（启动继承 cwd）

## S2 [P2] 同一已信任主机再次 trust 仍返回 trust_required（上轮已知）

临时回环 sshd 实测：`server trust test --fingerprint FP` exit 0、status trusted；随后 `server trust test --json` exit 3、error.code trust_required，但同一 JSON 的 data.status 是 trusted。交互分支同样无条件要求重新输入 yes。

建议：已由现有可信数据库验证的同一 key 应幂等成功；显式给出 fingerprint 时仍需验证匹配，密钥变更/撤销仍需现有严格处理。

源：`crates/fwm/src/cli/servers.rs:102`，没有按 inspection.status 分支。

## S3 [P2] 跳板 trust 成功却不能解决目标服务器的同一 trust 错误（上轮已知）

临时回环 sshd 实测：SSH config 的 target 使用 ProxyJump jump；jump 的 UserKnownHostsFile 为 alias-known。保存 target 时再传 `--known-hosts shared-known`。`server trust target` 报 jump unknown host key。执行 `server trust jump --ssh-config FILE --fingerprint FP` 显示 trusted，随后 `server trust target` 仍报完全相同的 unknown host key。

原因：target profile 的 known_hosts 会覆盖到所有跳板，单独操作 jump alias 则写它自身的数据库。错误只展示 host/fingerprint，不说明失败的是哪个 hop、端口和实际数据库；trust 命令也不能指定要沿用哪个 target 的跳板上下文。绕过方式是额外登记 jump profile 并手工指定同一 shared-known，但现有提示没有说明。

建议：按目标连接上下文检查/信任特定 hop，或至少输出可执行且使用正确数据库的补救命令；不能简单关闭验证或自动信任整条链。

源：
- `crates/fwm-core/src/ssh/connection.rs:187`
- `crates/fwm/src/cli/args.rs:95`

## S4 [P2] 修改 SSH config 后，“重启该服务器所有转发”仍使用旧 SSH 连接（本轮新增）

实测：用 alias dev 建立唯一共享 local 转发 web；将 SSH config 中 dev 的 Port 从临时 sshd 端口改为已关闭的 1。`server check dev` 立刻按新配置报 Connection refused；但 `restart --server dev --wait --timeout 5s` 返回 exit 0、ready true、established。`config reload` 随后也显示成功，status 仍 established。

源头：连接分组键只包含保存的 ServerProfile（包含 SSH config 路径，不包含解析结果），restart 只更新 listener generation，健康共享 SSH 会话保留。用户用常见的 SSH config 将别名改指另一台机器后，选中该服务器全部转发重启，仍可能连接旧机器，命令没有提示。

建议：明确区分规则 listener 重启与服务器 SSH 重新连接；提供直接刷新该服务器连接的语义，或检测 SSH config 解析结果变更并给出明确提示。当前可靠强制刷新只能重启整个 daemon，会影响其他服务器。

源：
- `crates/fwm-core/src/engine/mod.rs:85`（分组键）
- `crates/fwm-core/src/engine/lifecycle.rs:16`、`:71`（保留 SSH 会话）
- `crates/fwm-core/src/engine/connection.rs:245`（真正新建连接才 resolve）

证据：`evidence/fwm-ssh-deep-audit-results.json` 中 initial ready、check sees new ssh config、restart all server rules old ssh retained、after reload still established。

## S5 [P2] 当前终端 agent 已正确加载密钥，后台却继续使用旧 socket；错误补救不说明环境来源（本轮新增）

临时独立 ssh-agent 已加载 fixture key，SSH config 设 IdentityFile none，依赖 SSH_AUTH_SOCK。同一 `server check dev`、同一调用环境：daemon 停止时 exit 0；先在指向失效 old-dead-agent.sock 的环境启动 daemon 后，再从正确 agent 环境执行 check，得到 `SSH agent unavailable: No such file or directory; load a valid key into ssh-agent...`。当前 agent 实际已有正确 key。用正确环境执行 `daemon restart` 后 check 又成功。

原因：在线诊断转给 daemon；daemon 的环境固定在启动时，retry/在线 check 不接收当前终端的 agent 环境。实际选用的 socket 不在错误或诊断中，指引“load key”无法解决此情况。服务启动也使用服务管理器环境而非 CLI 当前环境，这一部分是源码证据，未注册真实系统服务。

建议：显示认证发生在哪个进程及实际 agent socket；提供正确的后台环境刷新/固定 IdentityAgent 指引。不能让在线与离线的同一诊断命令在不说明环境差异的情况下给出矛盾结果。

源：
- `crates/fwm/src/offline.rs:16`
- `crates/fwm-core/src/ssh/auth.rs:218`
- `crates/fwm-core/src/ssh/config.rs:174`
- `crates/fwm/src/platform/background.rs:19`

证据：`evidence/fwm-ssh-deep-audit-results.json` 中 offline current agent works、same CLI env online stale agent、same check restored。

## S6 [P2] 有效 OpenSSH 布尔值被解释成相反行为，非法值又未拒绝（本轮新增）

真实回环认证与本机 OpenSSH 对照：
- `IdentitiesOnly yes` 且 IdentityFile none：fwm 不尝试 agent 中未配置的 key，认证失败，符合预期。
- 改为 `IdentitiesOnly true`：OpenSSH `ssh -G` 输出 `identitiesonly yes`，但 fwm 认证成功，使用了原本不应尝试的 agent key。
- `IdentitiesOnly potato`：OpenSSH 拒绝配置；fwm 按 false 继续连接，没有配置错误。
- 第三轮相关路径：有效 `StrictHostKeyChecking true` 被 fwm 错报为试图禁用显式信任；有效 `ForwardAgent false` 被错报 `forwardagent=yes is unsupported`。
- 排除：本机 OpenSSH 自己也不接受 `Compression false`，不将该样例当成额外兼容问题。

建议：对每个支持的布尔/枚举选项按 OpenSSH 的真实可接受值解析，未知值明确报错，不能用“等于 yes 否则 false”。

源：`crates/fwm-core/src/ssh/config.rs:192`、`:300`、`:307`。

证据：`evidence/fwm-ssh-deep-audit-round2-results.json`、`evidence/fwm-ssh-bool-audit-results.json`。注意 round2 JSON 的历史 label “OpenSSH rejects invalid IdentitiesOnly true” 是初始假设命名，实际记录 exit 0；随后专门 `ssh -G` 对照确认 true 是合法 yes 同义值，本报告已纠正这一假设。

## S7 [P2] IdentityFile 指向公钥文件时，无法使用 agent 中已有的对应私钥（本轮新增）

同一临时 SSH config 写 `IdentityFile /tmp/.../client.pub`、`IdentitiesOnly yes`，agent 已加载对应 client 私钥。原生 `ssh -F FILE -o BatchMode=yes -o StrictHostKeyChecking=yes dev true` exit 0。fwm `server check dev` exit 3，报 client.pub Could not read key，并指引 encrypted keys must be unlocked in ssh-agent；但文件并非加密私钥，agent 已解锁。

原因：只读 `IdentityFile + .pub` sidecar 加入 agent 白名单，没有先识别 IdentityFile 本身是公钥；随后把该 .pub 当私钥解析失败，agent 正确身份又因 IdentitiesOnly 被筛掉。公钥文件选中 agent 私钥是常见 OpenSSH 用法，不是已明示不支持的选项。

建议：IdentityFile 本身为有效公钥时将其加入允许的 agent key 集合；诊断区分公钥、加密私钥、损坏文件。

源：`crates/fwm-core/src/ssh/auth.rs:35`、`:47`、`:113`。

证据：`evidence/fwm-ssh-deep-audit-round2-results.json` 中 OpenSSH public IdentityFile authenticates from agent（exit 0）及 public IdentityFile cannot select loaded agent key（exit 3）。

## S8 [P3] 显式指定的私钥文件不存在，诊断却只引导配置 agent（本轮新增）

SSH config 显式设置 IdentityFile /tmp/.../missing-private、IdentityAgent none。回环 server check 报 `no SSH agent is configured; load a valid key into ssh-agent or configure an unencrypted identity file`，未出现 missing-private 路径或“文件不存在”。用户明明配置了 identity，却被告知重新配置 identity/agent。

源头是 auth.rs:44 对所有不存在路径静默 continue；这对自动探测的默认 id_* 文件合理，对用户显式配置的 IdentityFile 缺失应在最终错误中保留原因。

建议：认证失败时列出实际尝试的显式路径及缺失/不可读原因，避免无关补救。可以与 S1 路径诊断一起修，但独立指定一个错误绝对路径也触发，因此这里单列。

源：`crates/fwm-core/src/ssh/auth.rs:44`、`:171`。

证据：`evidence/fwm-ssh-deep-audit-round2-results.json` 中 explicit missing private path hidden。

## 分轮记录与排除

- 上轮已知：S1–S3，已经用 CLI/回环 SSH 确认。
- 第一轮：从配置路径生命周期扩展到连接与环境生命周期，新增 S4/S5；真实回环 sshd + 独立 agent 复现。
- 第二轮：从 agent 的选择规则与补救建议扩展，新增 S6/S7/S8；与本机 OpenSSH 对照实际认证及配置解析。
- 第三轮：沿 S6 检查相关布尔选项，确认 StrictHostKeyChecking true/ForwardAgent false 为同一解析问题的另外表现，没有新增独立类别；沿 S7 核对 sidecar、加密私钥及 plain/cert agent 匹配路径，没有再发现独立问题。
- 最后一轮完整回扫本子范围：无新增独立问题。回扫包括 SSH Host 大小写、绝对 Include、未匹配 Host 下的非支持指令隔离、Match 与环境变量的明确拒绝、路径、信任、agent 环境、身份选择、配置刷新和 DNS 重连生命周期。

明确排除/已解释项：
- Host 大小写匹配有效；绝对 Include 有效；不匹配 Host 的一般非支持选项被忽略。已用关闭的回环端口验证配置进入连接阶段。
- Match 和 ${ENV} 扩展被明确报 unsupported，并给出 Host/dedicated config 或 explicit path 的替代方法；不把支持范围限制单独记成 bug。
- UserKnownHostsFile 多路径等明确不支持且有清楚错误的功能未列为问题。
- SSH 断线后的新连接和 needs_attention 后 retry 会重新 resolve；S4 特指健康共享连接的 restart/reload，不是所有重试都缓存旧配置。
- DNS 名称在新连接时交给 TcpStream::connect 重新解析；local/SOCKS 目标经 SSH channel 在远端解析，remote 目标在本地解析，符合方向语义。未发现独立 DNS 缓存/方向问题。
- doctor/server check 已明确输出“authentication succeeded; forwarding permissions are checked by individual listener/channel requests”，没有将“仅验证认证”作为新 bug。
- Changed/revoked host keys 禁止直接覆盖是有意安全行为，不作为“不方便”问题报告。
- 本轮没有实际注册 macOS/Linux/Windows 服务、没有原生 Windows 运行；对 service 环境和 Windows 泛化仅标作源码推断，未伪称实测。

停止标准：本子范围的每个已确认问题都沿相邻路径追查；第三轮只扩充同一根因样例，最后一轮重新扫完整范围没有新增独立可复现问题。不是声称程序绝无问题，也不是仅因为达到条数停止。

## 复现材料

- `evidence/fwm-trust-ux-audit.py`（S2/S3，回环 sshd；需要允许 sshd 在自身沙箱中运行）
- `evidence/fwm-ssh-deep-audit.py`（S4/S5）
- `evidence/fwm-ssh-deep-audit-results.json`
- `evidence/fwm-ssh-deep-audit-round2.py`（S4–S8，原生 OpenSSH 对照）
- `evidence/fwm-ssh-deep-audit-round2-results.json`
- `evidence/fwm-ssh-bool-audit-results.json`
- `evidence/fwm-ssh-final-pass-results.json`（最后一轮排除项）
