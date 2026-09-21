# fwm — SSH 端口转发管理器

使用 Rust、Tokio 和 russh 管理多台服务器的 SSH 转发。CLI 提交配置，用户级后台持续维护连接；远端使用现有 OpenSSH 服务，无需安装 fwm。

当前实现支持 macOS、Linux、Windows 的条件编译与平台适配。macOS 已进行真实 OpenSSH 端到端测试；Linux 和 Windows 已通过交叉编译检查，原生运行测试由 CI 和对应平台验收补充。

## 从源码构建与安装

需要 Rust 1.90 或更新版本，以及平台 C 编译工具链（加密依赖 ring 使用原生编译）。基础运行不依赖本地 `ssh` 命令。

```sh
cargo build --release --locked
./target/release/fwm --help

# 从当前源码构建 Release 并安装/更新到 ~/.local/bin
./install-from-source.sh

# 自定义安装到 ~/apps/fwm/bin
./install-from-source.sh --root "$HOME/apps/fwm"

# 依赖已缓存时离线安装
./install-from-source.sh --offline

# 不修改 Shell 启动配置，仍安装补全并启动/重启后台
./install-from-source.sh --shell none
```

`install-from-source.sh` 用于完整的本地源码仓库，支持 macOS/Linux 的 Bash（3.2 或更新版本）。可以从任意目录通过脚本路径调用；`--root` 的相对路径以调用时的工作目录为基准。内部使用 `cargo install --path … --locked --force --root …`，默认安装为 `~/.local/bin/fwm`，不受 Cargo 默认安装目录设置影响；`--root` 可显式覆盖，重复运行会更新已有安装。脚本同时安装补全文件，并默认按 `$SHELL` 配置 Bash 或 Zsh 的启动文件；加载补全时会将本次安装的 `bin` 目录置于 PATH 首位。

程序和补全安装成功后，脚本会使用刚安装的二进制执行 `daemon restart`：后台未运行时启动，已运行时重启以加载新版本；已注册的登录服务也会刷新其程序路径。保存的转发启停状态保留。这一步作用于默认配置实例 `~/.fwm`，`--root` 仅改变安装位置，`--shell none` 也会启动或重启后台。若后台启动失败，脚本返回非零退出码，说明程序已经安装，并打印手动重试命令。

此脚本专门用于源码构建。以后若提供从 GitHub Releases 下载预编译程序的一键安装脚本，会使用单独的名称和入口；当前尚未提供该下载脚本。`--offline` 只限制 Cargo 依赖下载，省略时 Cargo 可以获取缺少的依赖。

Windows 可直接执行 `cargo install --path crates/fwm --locked --force`；仅构建时的程序路径为 `target\release\fwm.exe`。

## Shell 补全

补全使用 Clap 官方的 [`clap_complete`](https://docs.rs/clap_complete/4.6.11/clap_complete/env/index.html) 动态引擎；命令解析、文件候选和 Shell 适配由依赖提供，fwm 只提供保存的服务器、规则和分组候选。源码安装脚本会实际安装补全文件并配置自动加载：

```sh
./install-from-source.sh
```

默认写入 Zsh 的 `${ZDOTDIR:-$HOME}/.zshrc`，或 Bash 的 `~/.bashrc` 和当前登录配置（首个已存在的 `.bash_profile`、`.bash_login`、`.profile`，均不存在则新建 `.bash_profile`）。已有文件在变更前备份为 `原路径.fwm-backup.*`，重复运行只更新 fwm 自己的标记块，不重复追加。可用 `--shell zsh` / `--shell bash` 显式选择，`--rc-file PATH` 指定单个启动文件，`--shell none` 只安装文件而不编辑启动配置。其他 Shell 只安装文件并提示手动加载。

**新开终端会自动生效；安装进程无法修改已打开终端的状态。** 当前终端立即启用（默认安装路径）：

```zsh
source ~/.local/share/fwm/shell-init.zsh
```

Bash：

```bash
source ~/.local/share/fwm/shell-init.bash
```

使用自定义 `--root` 时按安装脚本输出的 `source` 路径加载。Zsh 加载器会在必要时初始化 `compinit`；已有 Oh My Zsh 等框架时直接复用。生成的补全文件分别安装在 `ROOT/share/zsh/site-functions/_fwm` 和 `ROOT/share/bash-completion/completions/fwm`。加载器会在每次 Shell 启动时从当前二进制重新生成注册代码，避免升级后的协议不匹配。自行管理配置时，可用 `source <(fwm completions zsh)` 或 Bash 的 `eval "$(fwm completions bash)"`；这里只执行 fwm 生成的注册代码，候选值不经过 eval。Zsh 应显式加载注册代码，不依赖单独把文件加入 fpath。

按 Tab 会根据当前配置动态补全服务器、规则名称/ID 和分组，例如：

```sh
fwm add --server <Tab>
fwm server edit <Tab>
fwm up <Tab>
fwm edit <Tab>
fwm logs --group <Tab>
fwm --config-dir ~/work-fwm status <Tab>
```

同时支持子命令、选项、枚举值和文件路径。补全读取所选实例的已保存配置，后台停止时也可使用；改名和删除在下一次补全立即反映。无有效配置时静默返回空的动态候选，静态命令/选项仍可补全。新建名称（`add --name`、`server add`）和 `--rename` 保持自由输入，不建议已有名称。补全不访问 SSH、启动后台或写入配置，也不依赖 jq、Python 或额外的 Bash 补全框架。

当前固定使用 `clap_complete 4.6.11` 的 `unstable-dynamic` 接口，并保留上游边界回归：Bash 3.2 在等号参数、含冒号 ID、词中光标位置存在兼容限制，建议使用 `--server NAME` 并在词尾补全；Bash/Zsh 已闭合引号的当前路径词可能无法补全，含空格路径可从未引用的前缀开始，让 Shell 自动引用候选。Bash 的 Readline 引用使用标准 `fullquote` / `filenames` 选项，较旧 Bash 的 `filenames` 回退会把与当前目录下文件夹同名的候选按目录处理。项目不再维护独立的 Shell 解析或引用实现。

## 使用

直接使用 SSH 配置里的别名添加转发，无需预先注册服务器或给规则起名：

```sh
# 远端 12222 → 本地 localhost:22
fwm add --server example-cluster --remote --src 12222 --tgt 22

# 本地 3000 → 远端 localhost:3000
fwm add --server example-cluster --local --port 3000

# 自定义名称是可选的
fwm add --server example-cluster --remote --src 12223 --tgt 22 --name cluster-ssh-mac
```

`--server` 始终必填，先匹配已保存的服务器；不存在时把它作为 SSH 别名，与规则一起自动保存。未指定名称时随机选取一个简短英文单词，例如 `maple`、`river`，并在输出中展示实际名称及转发路径；已用名称会自动避开。

使用单独的 SSH 配置可在首次添加时传 `--ssh-config PATH`。新服务器和整批规则在一次配置事务中保存；无效规则不会留下空服务器条目。`server add` 仍可用于预先组织名称，或配置地址、用户名、密钥等细节：

```sh
fwm server add dev --ssh my-dev-server
fwm server add production --host example.com --user alice --port 22 --identity ~/.ssh/id_ed25519
fwm server edit production --port 2222 --user bob
fwm server edit production --rename prod

# 删除 fwm 的字段覆盖，重新继承 SSH config 或默认值
fwm server edit prod --unset user,port,identity
fwm server edit prod --proxy-jump none       # 明确禁用跳板
fwm server edit prod --unset proxy-jump      # 恢复继承 SSH config 的跳板
```

首次连接未知主机时，前台检查指纹并明确添加信任；后台不会自动接受新密钥：

```sh
fwm server trust dev

# 自动化场景提供已经通过可信渠道确认的完整指纹
fwm server trust dev --fingerprint SHA256:...

# 在目标 dev 的跳板配置上下文中，先信任第一个跳板
fwm server trust dev --hop 1
# 也可指定这条跳板链中唯一的别名；一次只信任一个 hop
fwm server trust dev --hop bastion --fingerprint SHA256:...
```

`server trust`、`server check`、`doctor --server` 也接受未注册的 SSH 别名；需要时加 `--ssh-config PATH`。检查和信任不会创建空服务器记录，也不会启动已停止的后台。第一次信任需要确认指纹；非交互或 JSON 模式提供 `--fingerprint`。同一密钥已经受信任时，重复 trust 幂等成功，无需再次确认；显式提供的指纹仍必须匹配。

使用 ProxyJump 时，`server trust TARGET --hop 1` 中的序号从 1 开始，也可填写唯一跳板别名。它采用目标 TARGET 对该跳板实际生效的配置和 known_hosts 文件，避免“单独信任过跳板，连接目标时却读另一份信任库”。多跳链按顺序逐个信任，最后执行 `server trust TARGET` 信任目标。

信任成功后显示 `trusted`；后台在线时会自动重试因该主机信任问题阻塞的运行规则，已停止和正常工作的规则不受影响。密钥变更或撤销仍会明确阻止连接，不会被重复 trust 覆盖。

支持普通私钥、已解锁的 agent 和用户证书。`IdentityFile key.pub` 可用于选择 agent 中对应的私钥，包括 `IdentitiesOnly yes`；显式配置的身份文件不存在或损坏时，错误会列出该路径。RSA 使用服务器协商的 SHA-256/512；不支持的 RSA 身份会跳过并尝试后续身份。agent 拒绝签名时立即报告原因，解锁或授权后可执行 `retry`。

在线 `server check`、`doctor` 和 `trust` 在后台执行，使用后台启动时的环境；后台停止时使用当前 CLI 的环境。诊断结果显示实际执行进程/PID、agent socket 和当前终端的 socket。若终端中的 agent 已更新而后台仍使用旧 socket，可在 SSH 配置中明确指定 `IdentityAgent`，再执行 `restart --server dev`。独立后台也可从新环境执行 `daemon restart`；托管服务的环境由系统服务管理器提供，不保证继承当前终端。

`server edit` 只更新给出的字段并保留服务器 ID。`--unset` 支持 `user`、`port`、`identity`、`ssh-config`、`known-hosts`、`proxy-jump`，可用逗号或重复参数；清除覆盖后恢复继承，清除 identity 不等于关闭认证。同一字段不能同时设置与清除，空用户名、空路径等无效值在保存前拒绝。

CLI 给出的相对 SSH 配置、identity、known_hosts 路径以本次调用的工作目录为基准保存；手写 `config.toml` 中的相对路径以 fwm 配置目录为基准。普通路径会规范化父目录，末级文件符号链接保留，方便更换密钥或配置的链接目标；判断两个配置路径是否指向同一文件时会解析链接目标。因此 `ssh.conf`、`./ssh.conf` 和指向同一文件的绝对路径可以复用已有服务器。

支持 `CanonicalizeHostname no/yes/always`（含 `false/true`）：启用后，标准 IP 地址会规范化，普通主机名转为小写，再按最终目标重新匹配 `Host`，保留已取得选项的优先级。身份文件按顺序去重，路径 token 在最终匹配后展开。当前支持不需要 DNS 名称改写的默认场景；`CanonicalDomains`、`CanonicalizePermittedCNAMEs` 等高级规则、需要 DNS 处理的尾点域名及非标准数字地址仍明确报错，不会静默忽略。语义依据见 [OpenSSH 配置说明](https://man.openbsd.org/ssh_config#CanonicalizeHostname)。

建立转发：

```sh
# 本地 3000 → 从远端访问 127.0.0.1:3000
fwm add dev-web --server dev --local 3000:127.0.0.1:3000 --wait --timeout 20s

# 远端 17890 → 从本地访问 127.0.0.1:7890
fwm add local-proxy --server dev --remote 17890:127.0.0.1:7890 --wait

# 本地 SOCKS5 1080，通过服务器访问请求的目标
fwm add dev-socks --server dev --dynamic 1080

# 远端 SOCKS5 7897，通过本机网络访问请求的目标
fwm add school-socks --server example-cluster --remote-dynamic 127.0.0.1:7897
```

`--remote-dynamic [bind:]PORT` 在远端创建 SOCKS5 CONNECT 代理，请求的目标由运行 fwm 的本机连接、解析域名，无需本机另开 SOCKS 服务。它对应 `ssh -R 127.0.0.1:7897 example-cluster` 的反向动态转发用途；远端程序可使用 `socks5h://127.0.0.1:7897`。省略 bind 时使用 `127.0.0.1`，IPv6 写为 `[::1]:7897`。它与 `--local`、`--remote`、`--dynamic` 和端口简写参数互斥；`--remote 7897` 仍表示固定转发到本机 `localhost:7897`。

Local 和 Remote 都支持单端口、闭区间和逗号列表简写：

```sh
# 等价于 --local 3000:localhost:3000
fwm add web --server dev --local --port 3000

# 3000、3001、3002、3003 和 8080，各自转发到目标侧的同名端口
fwm add services --server dev --remote --port 3000-3003,8080

# 多个源端口统一转发到目标侧 localhost:8080
fwm add web-pool --server dev --local --src 3000-3003,8081 --tgt 8080

# 向组中追加单个或多个成员；--name 可独立指定成员名或批次前缀
fwm add --server dev --remote --port 8081 --group services
fwm add --server dev --remote --port 9000-9002 --group services --name extra
```

简写默认监听 `127.0.0.1`，目标主机是 `localhost`，其所在侧仍由 Local/Remote 方向决定。范围包含两端，重叠和重复端口自动去重并排序；端口必须在 1–65535，一次最多展开 512 条规则，且总规则数仍受现有限制。

未指定名称时，每条规则随机使用一个 3–8 字母的英文单词。未指定 `--name` 和 `--group` 的多端口批次另选一个随机单词作为组名，成员各自使用不同单词；每个新批次创建独立组。指定名称时，展开一条保留名称，多条按源端口加后缀，并保存组名。例如 `--name services --port 3000-3003` 创建组 `services`，成员为 `services-3000` 等。可以直接 `down services`、`restart services`、`edit services --tgt 8080`、`remove services`。启停、重启、重试、删除与查询命令也可以显式使用 `--group services`；编辑使用位置参数中的组名。单独操作成员名称只影响该成员。

`--group` 显式指定所属组，对一个或多个端口都有效；`--name` 仍控制单条名称或批次前缀。不写 `--name` 时成员随机选词，并避开既有规则名、规则 ID、组名以及同批次已选名称。词表内置，离线可用；实际名称和组可用 `status`、`group list` 查看。保存后的随机名称不会在重启或 reload 时重新生成。

```sh
fwm group list                         # 组名、成员和所属服务器
fwm edit dev-web --group services       # 将已有规则加入或移到组中
fwm edit dev-web --ungroup              # 退出组，保留规则本身
fwm edit services --rename backend      # 只改组名，保留成员名称和 ID
fwm edit backend --group another-group  # 明确把整组成员移入另一组
```

组名不能重命名为已存在的组；合并需显式使用 `edit GROUP --group TARGET`。`--group` 和 `--ungroup` 互斥；对整组执行 `--ungroup` 会保留所有成员并清除组归属。状态表和 JSON 同时显示每条规则的 group。

整个批次先统一校验，一次保存；名称或配置内的监听冲突会使整个批次失败，已有规则不会被覆盖。同一组的成员即使已停止，也不能保存重复监听地址；不同组的停用备选规则可以复用端口，启用时再检查运行冲突。双栈校验区分 IPv4 与 IPv6 地址，并识别 IPv4-mapped IPv6 的同一监听；IPv6 通配地址可能同时覆盖 IPv4，按重叠处理。实际网络连接独立建立，外部进程占用端口等运行错误由对应规则报告。`--wait` 在同一超时时间内等待本批全部规则建立。

也可直接写 `--local 3000` 或 `--remote 3000-3003,8080`。`--port` 与 `--src/--tgt` 二选一，添加时 `--src` 和 `--tgt` 必须同时提供，`--tgt` 只接受单个端口；不能与完整映射或 SOCKS 的 `--dynamic` / `--remote-dynamic` 混用。

推荐用 `--name NAME` 自定义名称，旧的位置名称仍可用。使用 `--port` 或 `--src/--tgt` 时，`--local`/`--remote` 仅表示方向，不会吞掉后面的位置名称。完整映射可写 `--remote=12222:localhost:22`，也兼容旧的空格写法。数字规则名可用于 `edit --remote 1234`；编辑时若方向选项本身要携带一个端口，使用 `edit 1234 --remote=3000` 或 `--remote --port 3000` 可以直接表达意图。

编辑只修改给出的字段：`edit web --tgt 8080` 保留监听及目标主机；`edit web --src 3001` 保留绑定 IP 和目标；`edit web --remote` 保留端口与地址并切换方向。`--port 3001` 同时修改两个端口，保留现有地址，`--local=3001` / `--remote=3001` 的端口修改行为相同。完整映射 `edit web --local=3001:new.internal:8081` 没写 bind 时也保留原绑定 IP；显式填写 bind 才会替换它。`--dynamic [bind:]PORT` 和 `--remote-dynamic [bind:]PORT` 分别替换为本地、远端 SOCKS 监听规格；从 SOCKS 切换到固定转发时需提供目标端口，例如 `edit school-socks --remote --tgt 8080`。

组编辑对每个成员应用相同变更并一次提交，不会把编辑变成新增监听；统一源端口导致组内冲突时整次拒绝。需要扩展端口数量时使用 `add --group`。`edit web --server another-alias` 可直接移到新的 SSH 别名，与新服务器记录原子保存，规则 ID、组归属和启停意图保留。

创建时省略监听地址默认绑定 `127.0.0.1`；Local/Remote 编辑则保留未指定的绑定地址。可显式指定地址，例如 `127.0.0.1:3000:10.0.0.2:3000`；IPv6 使用括号，例如 `[::1]:3000:[::1]:3000`。目标可带 scope，如 `[fe80::1%lo0]:8080` 或 `[fe80::1%3]:8080`，scope 由目标所在侧解释。无效的 IPv6 字面量会在保存前被拒绝。

目标地址由转发方向决定：Local 的 target 从远端访问，Remote 的 target 从本地访问。SOCKS5 支持 CONNECT；`--dynamic` 的目标连接和域名解析在远端，`--remote-dynamic` 则在本机。UDP ASSOCIATE 和 BIND 不受支持。

```sh
fwm status
fwm status --watch
fwm status dev-web
fwm status --server dev
fwm status --group services
fwm --json status
fwm logs dev-web --follow
fwm logs --server dev --tail 50
fwm logs --group services
fwm doctor --server dev

fwm edit dev-web --local 3000:127.0.0.1:3001
fwm edit dev-web --rename another-name
fwm down another-name
fwm up another-name --wait --timeout 20s
fwm restart another-name --wait
fwm restart --server dev --wait
fwm retry --server dev
fwm up --server dev
fwm down --all
fwm remove another-name
```

`add` 默认保存并启动，`--disabled` 只保存。Local 和本地 SOCKS 默认共享同一服务器 profile 的 SSH 连接；Remote 和远端 SOCKS 默认启用 verified 回收并使用独占连接，保证清理旧会话时不影响其他规则。受限 SSH 服务器无法运行回收助手时，可显式使用 `--remote-cleanup off`。规则改名不重连；修改转发端点会关闭该规则的旧数据连接，其他规则继续工作。

`restart NAME` / `restart --group GROUP` 重建选中规则，已停止的规则会启动；共享连接中未选中的规则保持运行。`restart --server dev` 会重新解析 SSH 配置并强制刷新该服务器的 SSH 连接，适合切换 SSH 端点、密钥或 agent；它也会启动该服务器上选中的停止规则。在线 `config reload` 会重新读取 SSH 连接配置，只刷新解析结果变化的服务器，并保留规则启停意图。更换密钥文件内容或需要强制重建连接时使用服务器级 restart。

`retry` 只重试异常且已启用的规则，明确报告重试数量和跳过原因；已停止或已连接的规则不会被假报为“已重试”。

`down/remove` 在后台停止时直接保存停用或删除意图，**不会启动后台或其他转发**。后台在线时同步应用；远端取消尚未确认时不会假报端口已释放。未应用甚至尚未写完的配置草稿不会阻止停删，也不会被覆盖；之后 reload 草稿仍会保留这些控制操作，防止规则意外复活。

`add --disabled`、规则编辑、服务器增删改和 `config reload` 也支持离线保存，保持后台停止；空编辑或相同值编辑报告 No changes，不增加配置版本。普通配置修改仍会拒绝覆盖未应用草稿。`add`（未禁用）、`up`、`restart` 先校验并保存有效操作，再按需启动后台；名称错误或冲突不会启动其他转发。`doctor` 同时报告候选配置是否有效、是否尚未应用，以及当前诊断是否使用已保存快照。

`add/up/restart --wait` 默认等待 20 秒；显式提供 `--timeout 500ms`、`--timeout 20s` 或 `--timeout 2m` 会自动隐含 `--wait`，不再忽略等待。`add --disabled` 不能与 `--wait` 或 `--timeout` 同用。等待的是实际监听建立，超时后后台继续恢复。

一次性 `--json` 命令只输出一个最终 JSON：等待成功才报告 ready；超时或失败输出 `ok:false`，同时通过 `data.saved:true` 说明配置已保存。流式 watch/follow 使用 JSON Lines。

日志默认显示最近 100 条，`--tail N` 调整条数，`--tail 0 --follow` 只看新事件。后台停止、重启或规则删除后仍可查看保留的历史；规则通过稳定 ID 关联重命名前后的记录。按名称查询时，当前规则/组优先于历史同名别名；需要明确指定历史对象时可使用规则 ID 或 `--server SERVER_ID`。服务器配置、信任及连接事件也带服务器归属，可由 `logs --server` 查询。

`logs GROUP` 与 `logs --group GROUP` 对当前组等价，按事件发生时的组标签筛选；组改名不会回写历史标签，旧组记录仍用旧组名查询。缺少原始上下文的旧日志保留已有标签或显示稳定 ID，不再用当前配置猜测旧归属；名称无法还原时给出提示，仍可按 ID 或不筛选查看。日志时间显示为 UTC 日期，状态表显示完整转发路径、组归属、保存意图和运行状态。

按规则或服务器名称开始 `status --watch` 后，重命名仍跟踪原 ID；删除后显示空结果并继续监视。按组监视使用动态组标签，会包含后续加入的成员，组暂时为空也不会退出；组改名后需改用新组名查询。按规则名称启动的 `logs NAME --follow`，以及 `logs --server NAME --follow`，会固定已识别的对象 ID，重命名、删除以及旧标签从轮转日志中消失后仍跟随同一身份；组筛选继续按事件标签匹配。

后台重启或 IPC 暂时失联时，`status --watch` 继续监视。无法确认运行状态时，JSON 显示 `daemon_state: "unresponsive"`、`runtime_available: false` 和 `daemon_running: null`；此时保存意图不能当作实际监听状态。连接恢复后继续输出正常快照。非法参数或最初不存在的选择对象仍会立即报错。watch/follow 可用 Ctrl-C 退出。

状态查询使用同一配置版本的配置与运行快照，单次查询在后台完成规则/服务器/组筛选。大量规则同时报错时，全量状态会带明确标记地缩短错误摘要以保持响应可读取；使用 `status RULE_ID` 查看该规则的完整诊断。首次开启 watch 就失联时，仍会显示可读取的保存规则，状态标为 `unverified`。升级 CLI 后若旧后台不支持联合快照，会提示重启后台完成升级。

资源 ID 限制为 1–128 字节，以保证已接受配置的事件元数据可写入日志。历史对象已被删除时，精确历史 ID 优先于历史同名别名；异步事件保留产生时的名称、服务器、组和时间。日志读取遇到轮转会有界重试，持续轮转无法取得一致视图时会明确报错。

## 后台与登录自启

启用转发的命令自动启动当前用户后台；关闭终端不影响它。查询、SSH 检查及离线配置编辑不会启动后台。

```sh
fwm daemon start
fwm daemon status
fwm daemon stop
fwm daemon restart             # 等待旧后台退出并启动新后台，保留规则启停状态
fwm daemon run                 # 前台调试或交给外部服务管理器
fwm service install            # 安装当前用户登录自启，并启动后台
fwm service status             # 查看系统注册、运行、启用状态和定义文件
fwm service uninstall          # 停止本实例后台及转发，再移除登录自启
```

服务命令默认操作当前用户，`--user` 保留兼容。平台实现分别为 macOS LaunchAgent、Linux systemd 用户服务、Windows 当前用户登录计划任务。自启服务依赖当前用户可用的凭据/agent 环境；需要口令或交互解锁时会显示 `needs_attention`。当前不配置无人登录的系统级服务。

`service status` 区分定义文件是否存在与操作系统中的 `registered`、`running`、`enabled`；本地标记缺失时仍会查找系统注册。`service uninstall` 会停止此配置实例的后台和活动转发，并删除匹配的登录服务；保存的规则及 desired state 保留，之后启动后台仍按这些意图恢复。若检测到同一配置的多个旧注册，安装会给出冲突提示，卸载可清除所有匹配注册。

服务安装、卸载与重启按配置实例串行执行，服务定义采用原子替换。接管或刷新失败时，会尝试恢复旧定义、注册、原运行状态和登录启用策略；恢复本身失败会明确报告 `service_recovery_failed`，不会假报已恢复。每个外部服务管理命令都有超时；错误会说明请求结果可能仍需查询确认。服务故障保留具体阶段的 `error.code`，与输入错误区分。无法取得可恢复旧定义时，会在停止旧后台前报 `service_recovery_unavailable`，并提示卸载后重新安装。

更新可执行文件或更换安装路径后，执行 `fwm daemon restart`。托管后台会刷新服务定义中的可执行文件和规范化配置路径，保留原登录 enabled 策略；单个旧注册沿用原服务 ID。已保存规则及启停状态保留；后台原本未运行时，restart 会启动它。

重启会等到旧后台的 IPC 关闭、清理完成并释放实例锁后再启动。实例仍持锁但不回应时，`daemon status` 报 `unresponsive`，`daemon start` 返回 `daemon_unresponsive`；`daemon stop` 仍尝试 Shutdown 并等待释放锁，无法确认则报明实际失败阶段。restart 不会在停止结果未确认时启动替代实例。系统服务管理器不可用且没有服务定义时，普通独立后台仍可启动；已有托管定义则会明确报告服务问题。

## 状态与恢复

- `established`：SSH 已认证，并且本地监听建立或远端请求得到确认；目标应用健康仍未检测。
- `backoff` / `starting`：连接或监听正在恢复，状态中给出错误及下次重试时间。
- `needs_attention`：需要修复主机信任、认证或不支持的配置；信任成功会自动重试相关运行规则，其他问题修复后可执行 `retry`。
- `stopping` / `unverified`：远端取消或其他协议结果仍待确认。

心跳默认 `keepalive_interval_secs = 5`、`keepalive_max = 3`。锁定的 russh 版本在下一次计时检查判定超时，本机纯黑洞测试约 20 秒发现失联；这与收到 TCP reset 等明确断开后立即安排重连不同。参数可在 TOML 中调整。故障后首次快速重试，持续失败时使用带抖动的指数退避，最大间隔 30 秒。服务器 SSH 连接故障会影响其共享规则，恢复后逐条重新建立监听。目标服务连接失败只影响对应业务连接。

断线重连恢复新 TCP 连接，不能续接已断开的应用连接。Remote 默认会主动回收本管理器登记的旧 SSH 会话，确认释放监听后在同一端口重建。当前断线检测依靠心跳与定时重试。

## 反向转发的主动恢复

直接使用普通 remote 命令即可，默认启用：

```sh
fwm add --server example-cluster --remote --src 12222 --tgt 22 --wait
```

建立前通过这条独占 SSH 连接执行内置 Python 辅助脚本，登记管理器身份、规则、代次、会话进程出生身份及 SSH 连接信息；SSH 确认监听成功后再核对并记录监听证据。脚本按需运行，无需另装常驻服务。远端支持 Linux（Python 3.9+、支持 pidfd 的内核）和 macOS（Python 3、系统 libproc/lsof），需要允许该 SSH 用户执行命令及终止自己的会话。

断线后，新连接先核对旧登记和实际进程身份，只终止同一管理器、同一规则的旧独占会话；先 TERM，仍未退出才 KILL，再确认监听可重新绑定。Linux 使用 pidfd 固定目标进程；正常 OpenSSH 降权导致普通用户无法读取 `/proc/PID/fd` 时，使用原 SSH exec 的祖先证明、进程出生身份、传输 inode 和监听确认记录进行核验，仍无需 sudo。macOS 在每次发送信号前重新核验 libproc 出生身份。

新旧操作通过单调递增的持久代次隔离；旧任务不能删除新登记。正常 down/remove 会取消监听并释放自己的登记；连接异常中断保留登记供恢复。helper 意外退出也会触发该独占连接的恢复，避免留下无人监督的监听。

普通进程占用端口时，无论属于当前 SSH 用户还是其他用户，都不会被终止。规则报告 `unmanaged_conflict` 并退避重试，端口释放后自动恢复。主动回收仅限当前 SSH 用户拥有、已登记且再次核验为同一管理器、同一规则旧连接的 `sshd` / `sshd-session` 会话；不能仅凭进程名或端口号终止进程。另一管理器或未登记的 SSH 会话也按外部占用处理。无法核验登记归属、权限不足或不支持 helper 时进入 `needs_attention`，不静默降级。

配置版本 1 的反向规则会自动迁移为 verified/dedicated，保留规则 ID、启停意图和原配置备份；未应用的手工修改不会被覆盖。确需连接禁止远端命令执行的服务器时，可以明确选择只等待服务端释放的模式：

```sh
fwm add --server restricted --remote --port 8080 --remote-cleanup off
fwm edit existing-rule --remote-cleanup verified
```

## 配置和兼容范围

Linux、macOS、Windows 默认统一使用用户主目录下的 `.fwm`：

| 平台 | 默认配置文件 |
|---|---|
| Linux / macOS | `~/.fwm/config.toml` |
| Windows | `%USERPROFILE%\.fwm\config.toml`（通常为 `C:\Users\用户名\.fwm\config.toml`） |

使用 `--config-dir PATH` 可覆盖默认目录并建立独立实例。配置目录与程序安装目录相互独立，不受 `install-from-source.sh --root` 影响。指向同一目录的绝对/相对路径、`.`/`..` 和祖先目录的符号链接会规范化为同一后台、IPC 和登录服务身份。`config-dir` 自身不能是末级符号链接，`state` 目录也不接受符号链接。该目录包含：

```text
config.toml             可编辑的候选配置
state/applied.toml      后台已提交的配置快照
state/applied.vN.toml   旧配置格式升级备份（N 为迁移前版本）
state/recovery.json     持久管理器身份及规则恢复代次
state/recovery-backups/ 显式配置恢复的独立备份目录
state/daemon.lock       单实例锁
state/daemon.sock       Unix IPC（配置路径过长时使用当前用户私有短路径）；Windows 使用用户专属 named pipe
state/events.jsonl     有界事件日志及一个轮转文件
state/daemon.log        启动错误和运行诊断，单文件最多 2 MiB，保留一个轮转文件；RUST_LOG 可调整级别
```

升级前保存在平台配置目录中的实例不会自动搬迁。可用 `--config-dir` 指向旧目录继续使用，例如 macOS 上执行 `fwm --config-dir "$HOME/Library/Application Support/fwm" status`。目录决定后台和登录服务的身份；迁移已有实例前，应先针对旧目录停止后台并卸载已安装的登录服务，再迁移完整配置目录（包括 `state`），随后在新目录重新安装登录服务或启动后台。

手写配置省略 ID 时，按对象类别和名称生成稳定初始 ID；读取不会改写文件。显式 ID 始终保留，成功修改后 ID 随配置保存，之后重命名不改变 ID。名称不能占用另一同类对象的 ID，组名不能与规则名称或 ID 冲突；旧配置出现歧义时，错误会指出双方对象，修复名称即可，勿改动 ID。

服务器和转发配置拒绝未知字段；例如 `usr` 或 `desired_sate` 的拼写错误会使校验失败。手写或导入的 `remote` / `remote_dynamic` 规则省略 `remote_cleanup` 时也默认使用 verified/dedicated，与 CLI 一致；显式 `remote_cleanup = "off"` 会保留。反向 SOCKS 规则使用 `kind = "remote_dynamic"` 和 `listen = "127.0.0.1:7897"`，不填写 `target`。

```sh
fwm config export
fwm config validate
fwm config reload
```

编辑 `config.toml` 后需显式 validate/reload。存在未应用编辑时，新增或编辑规则会拒绝覆盖草稿；启停、删除仍可执行，其控制意图与已应用快照一起原子持久化，草稿随后 reload 也不会撤销这些操作。配置错误或文件丢失不会清空已提交规则；后台重启继续使用最后有效快照。配置提交采用原子替换，配置 revision 防止多个客户端互相覆盖。

如果 `state/applied.toml` 损坏，而已经修复的 `config.toml` 有效，可执行显式恢复：

```sh
fwm daemon stop
fwm config validate
fwm config recover --from-candidate
fwm status
# 恢复后按需重新启用
fwm up --server dev --wait
```

恢复要求后台停止，并持有独占实例锁；先校验候选配置，再把原候选和快照保存到 `state/recovery-backups/<uuid>/`。可读取的删除/停用控制意图会继续应用，ID 保留；剩余规则统一设为 stopped，需要显式 up 才运行。重复恢复会创建独立备份。只有控制意图记录不可读时，才需要额外显式传 `--discard-unreadable-intent`；它允许舍弃这部分不可读取的覆盖，恢复结果仍全部停用。

当前配置版本为 3，升级会保存旧版本快照备份。旧的同前缀 `名称-源端口` 多成员规则会恢复为同名前缀的组，规则 ID、地址和启停意图保留；与已有规则名称或规则 ID 冲突的前缀不会被推断为组。

当前支持 SSH `Host`、`Include`、HostName/User/Port、IdentityFile、IdentityAgent、IdentitiesOnly、known_hosts 路径和原生 ProxyJump 等常用选项。支持的布尔选项按合法的 OpenSSH 值解析，非法值明确拒绝；主机密钥验证始终开启。严格验证哈希 known_hosts、非默认端口、密钥变更和撤销，支持普通私钥、用户证书以及 Unix/Windows agent。

SSH 文件里的相对 identity、agent、known_hosts 等路径以实际声明它们的文件目录为基准，包含被 Include 的文件；`Include` 指令自身的相对路径仍以 `~/.ssh` 为基准。`~`、`%d` 保留运行用户 home 语义；其他路径模板使用固定的文件基准后再展开，后台 cwd 改变不会重定向这些路径。

复杂 `Match`、任意 `ProxyCommand`、主机证书信任、`UseKeychain yes` 等未实现选项会明确报错；可以通过 `--ssh-config` 指定一个精简配置。加密私钥可先用系统 `ssh-add` 加入已解锁的 agent。`IdentityFile`、配置路径、目标服务和服务端转发权限均需符合实际环境。

远端登记默认位于 `$XDG_STATE_HOME/fwm/leases` 或 `~/.local/state/fwm/leases` 的用户私有目录。不要在仍有转发运行时删除本地 `state/recovery.json` 或远端登记，否则会失去旧会话的归属证据。

资源边界：最多 128 个服务器 profile、512 条规则，已序列化配置不超过 256 KiB；IPC 单帧不超过 1 MiB，事件按字节分页。正常个人开发场景无需调整这些限制。

## 模块结构

```text
crates/fwm-core/
  model.rs              领域模型与校验
  store.rs / store/     配置事务、草稿保护与控制意图恢复
  selection.rs          规则、服务器和分组选择
  history.rs / history/  持久日志、筛选、轮转与跟随游标
  paths.rs              用户目录
  ssh/                  配置、信任、认证、连接、跳板
  cleanup/              持久恢复身份、SSH helper 协议和远端会话核验/回收
  engine/               连接监督、转发、SOCKS、退避、运行状态
crates/fwm-api/
  protocol.rs           版本化请求、响应、事件和错误
  codec.rs              有界消息帧
  client.rs             与传输无关的 Rust 客户端
crates/fwm/
  main.rs               程序入口
  cli/                  按命令领域拆分的 CLI
  daemon/               请求分发、状态、配置应用和事件日志
  offline.rs / offline/ 持有实例锁执行离线配置操作，避免唤醒后台
  configuration/        在线和离线共用的配置变更校验及批量操作
  ssh_actions.rs        在线和离线共用的主机信任与诊断
  platform/             IPC、后台启动和各平台用户服务
```

未来 TUI、Desktop 和 Web 适配层可以复用 `fwm-api` 与同一个后台，不需要解析终端表格。IPC 为四字节大端长度加 JSON，协议版本为 1；每次连接一个请求。客户端使用 request ID 和 expected revision。近期配置操作可按相同 request ID 重试，缓存仅覆盖当前后台实例最近 32 次成功提交；后台重启后应查询配置重新对账。事件使用 daemon instance ID 和分页游标检测重启或丢失。

## 验证

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
python3 -m unittest discover -s crates/fwm-core/src/cleanup -p 'test_*.py' -v
cargo build --locked
python3 tests/smoke.py
```

最后一项需要 Unix、`sshd` 与 `ssh-keygen`，只创建临时密钥、配置和回环监听。Linux 上运行 sshd 的权限/运行目录需由测试环境提供；macOS 的嵌套执行沙箱可能阻止 sshd 初始化，须在允许本地 SSH 测试的环境运行。故障代理会刻意保留旧 sshd 的 TCP 连接，让端口真实残留，验证主动回收、另一管理器隔离、其他长连接保护、helper 崩溃恢复，以及原有转发、黑洞和配置恢复行为。`tests/linux_recovery_smoke.py` 可对显式指定的回环 Linux VM SSH 端点复现同类测试。

本轮验证：**438 项 Rust 测试、27 项 helper 测试全部通过**。Release 和覆盖率插桩版本均通过完整 macOS 回环 OpenSSH/PTY 测试，包含本地/反向/SOCKS 转发、真实旧会话回收、其他管理器与长连接保护、helper 崩溃恢复、黑洞重连、服务器配置刷新、跳板信任及日志归属。行覆盖率 **92.85%**，函数覆盖率 **90.99%**；格式、macOS 原生及 Windows 交叉目标严格 Clippy 检查通过。36 项审查问题的行为与对应测试见 [修复对照表](audits/2026-09-20-ux/FIXES.md)。

此前真实 Linux VM（Alpine / OpenSSH 10.3，UID 1000 无 sudo）已验证旧会话回收与 helper 崩溃恢复；本轮未重复 VM 实验。三平台服务流程使用状态化模拟管理器测试；实际登录恢复、系统休眠及 Windows 服务/ACL 的运行验收仍需对应系统环境。

测试矩阵、覆盖率报告及 CI 门槛见 [TESTING.md](TESTING.md)。详细架构与后续目标见 [DESIGN.md](DESIGN.md)。
