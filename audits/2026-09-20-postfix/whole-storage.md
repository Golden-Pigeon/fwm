# 身份、迁移、初始化与服务路径续查

2026-09-20。本轮扩大到此前没有深入验证的配置身份和迁移组合、只读加载、目录身份、服务定义路径及跨平台参数转义。新增 **WS01，P2**；没有重复计算 ST、DS 或 S 系列。生产代码和总报告未修改。

最终新增审查测试 **7/7 通过**：4 个 Store/路径测试、3 个服务定义/发现测试。部分断言描述当前缺陷，不表示问题已修复。全部使用普通临时文件和已有 FakeExecutor；没有运行 SSH、认证 agent、后台、API 请求或真实系统服务。所有临时目录已清理。

## WS01 — P2：Linux 服务发现重复解码反斜杠，遗漏合法旧服务定义

**具体触发条件：** 配置目录名含连续两个或更多反斜杠，紧接一个双引号；已有定义使用支持发现的旧服务文件名，且当前不在 manager 的 loaded-unit 列表中。这是 Unix 允许的文件名组合，较少见。当前规范文件名仍可被文件名回退识别，不能扩大为“此类路径的所有 Linux 服务都失败”。

测试先由当前 Linux adapter 的 `render()` 生成定义，再保存成临时 `fwm-1000000000000001.service`。该文件仍指向本次配置，内容没有手工构造或损坏；文件名代表当前产品已支持的旧服务标识。假管理器返回空 loaded-unit 列表，符合已有定义尚未加载或已卸载的状态。随后执行生产 `Service::discover` 和 `Operation::installed`：

| 双引号前相邻反斜杠数量 | 发现器解码后的数量 | 找到旧定义 | `installed()` |
|---:|---:|---|---|
| 1 | 1 | 是 | true |
| 2 | 1 | 否 | false |
| 3 | 2 | 否 | false |

更完整的生成/解析矩阵确认 4 个反斜杠也被缩为 2 个。已有定义文件始终存在，实际 profile 目录也存在，但解析出的另一个目录没有匹配本 profile，于是发现器退回一个不存在的当前文件名。用户因而无法正常识别已有的旧登录启动定义。

**根因：** [service_linux.rs:303](../../crates/fwm/src/platform/service_linux.rs#L303) 生成 systemd 参数时先加倍反斜杠，再转义双引号。[service_discovery.rs:173](../../crates/fwm/src/platform/service_discovery.rs#L173) 却先调用面向 Windows 参数的 `quoted`；该函数在 [同文件:146](../../crates/fwm/src/platform/service_discovery.rs#L146) 已折叠双引号前的反斜杠。随后 [同文件:176](../../crates/fwm/src/platform/service_discovery.rs#L176) 再执行全局双反斜杠折叠，导致同一段内容解码两次。[同文件:81](../../crates/fwm/src/platform/service_discovery.rs#L81) 使用错误的路径做归属匹配，漏掉该文件。

**期望：** Linux unit 参数必须按照自己的编码规则恰好解码一次，生成器和发现器需能往返保留所有接受的合法路径。不能通过更宽松的归属匹配来掩盖错误路径。

**证据范围：** 这是当前生产 adapter、发现器和 Operation 加普通文件/假管理器的确定性结果；没有把它描述为原生 systemd 的运行验收。未声称发现了服务启动命令执行方面的问题。它与 S02 的旧文件名归属回退不同：S02 错认其他 profile，本条是解析改变路径后遗漏本 profile。

脚本：[whole-storage-service-run.py](whole-storage-service-run.py)；测试：[whole-storage-service.rs](whole-storage-service.rs)；完整结果：[whole-storage-service-results.json](whole-storage-service-results.json)。

## 存储和路径组合的正常结果

通过公开 Store/Paths 方法执行了以下组合，未新增独立问题：

- 手写配置缺少规则 ID 时，多次 `load` 得到稳定身份，不改原始候选，也不创建 state 目录。`initialize` 补齐显式 ID 后，保留该 ID 的改名草稿不会改变规则身份；只读加载继续返回已应用配置。
- schema 1/2 分别与 applied 存在/缺失组合：只读迁移稳定生成 schema 3、revision 5；初始化后可重复读取相同配置。既有 applied 被完整备份，显式和推导 ID 均保留。随后显式恢复得到 revision 6，所有规则 stopped。
- 旧 schema 的 applied 与不同候选并存时，初始化保留候选编辑。随后模拟一个普通损坏 TOML 的显式恢复，验证备份保留 applied 与候选的原始字节，恢复后规则全部停止且可读取。
- 通过目录祖先别名与规范路径访问同一配置，得到相同 profile 目录和 pipe 标识。保存的最终文件符号链接在切换到另一个普通文件后，配置身份、revision、保存路径和候选字节保持不变；没有读取密钥或进行认证。

脚本：[whole-storage-core-run.py](whole-storage-core-run.py)；测试：[whole-storage-core.rs](whole-storage-core.rs)；完整结果与源码哈希：[whole-storage-core-results.json](whole-storage-core-results.json)。

## 服务路径关联检查与边界

WS01 发现后进一步检查了相邻输入和发现路径，没有新增第二个独立问题：

- Linux 普通目录、空格、Unicode、美元符号、百分号、单独反斜杠、单独双引号、双引号前单个反斜杠均往返正确。
- 对 WS01 的相同目录，当前规范文件名回退仍能找到定义；旧文件名才暴露该解析缺口。
- Windows Task XML 的参数字符串在包含空格、Unicode、UNC 和末尾目录分隔符时往返正确。这里将 Windows 字符串作为 PathBuf 载体，只执行生成/解析算法，未验证 Windows 文件系统、ACL 或原生 Task Scheduler。
- macOS plist 的 XML 特殊字符、空格、Unicode 和上述反斜杠/双引号组合均往返正确。

本轮未执行原生 Windows 路径规范化或三平台服务管理器验收，未将仅靠构造旧 schema 内部控制头才能出现的状态列为确认问题。上轮 DS-C01 的真实崩溃行为仍未验证。以上是新增范围的审查结果，不保证其他范围没有缺陷。
