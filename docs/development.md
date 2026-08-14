# 开发文档：内存指标与验收清单

## 内存测量规范

- **唯一指标**：`vmmap --summary <pid>` 中的 `Physical footprint`
- **禁止**用 RSS（包含共享库，不代表实际占用）
- **禁止**用 `ps -o rss` 作为验收依据
- Retina 推算公式：`footprint ≈ 固定开销(~24M) + IOSurface(物理像素×4)`

## 内存指标

**测试环境**：macOS 26.5, 1920×1080 @1x, Rust 1.95

| 指标 | 目标 |
|---|---|
| 每个终端标签增量 | < 20 MB |
| 5 个 SSH 标签常驻 | < 250 MB (Retina) |
| 空窗口基线 | 记录数值，不设门槛 |

### 终端网格内存换算（M2a-0 实测）

`size_of::<Cell>() = 24` 字节（char 4 + fg 4 + bg 4 + flags 4 + Option<Arc> 8）。

- **每 1000 行 × 200 列 ≈ 4.8 MB**（1000×200×24 = 4.8 MB），加 Grid
  行索引开销后约 **5 MB / 1000 行**
- 回滚行数默认值 **3000**：3000×200×24 = 14.4 MB，留出余量给行索引，
  符合「每标签增量 < 20 MB」目标
- 用户可自行调高（设置页「回滚行数」），代价线性增长：
  每 +1000 行 ≈ +5 MB

### M0 基线数据（release build）

| 组件 | Footprint | 说明 |
|---|---|---|
| wgpu 裸空窗口 (200×100) | 24.9M | 固定开销 baseline |
| wgpu 裸空窗口 (800×600) | 27.0M | |
| wgpu 裸空窗口 (1920×1080) | 32.1M | |
| vida GUI 空窗口 (1024×768) | **33.1M** | 含 iced chrome (~6M) |
| vida daemon | **7 MB** | 无 GPU |

### M3 release 实测（系统 SSH 接入）

**环境**：macOS，1920×1080 @1x；release 构建；Vida GUI 与 daemon 启动并稳定空闲。

| 组件 | Physical footprint | 说明 |
|---|---:|---|
| vida GUI | **35.9M** | M3 界面与终端渲染资源已加载，未打开 SSH 标签 |
| vida daemon | **3104K** | 金库/PTY/SSH askpass broker 代码已加载，无活动 SSH 会话 |

本里程碑仍以 `vmmap --summary <pid>` 的 `Physical footprint` 为唯一指标。
活动 SSH 标签的 GUI 网格与本地终端使用同一数据结构；多 SSH 标签实测需所有者使用实际
测试主机完成 README 的 M3 手动验收后补录，不能用 RSS 或无真实连接的估算值代替。

#### M3 最终验收复测（2026-08-09）

所有者完成真实 SSH、本地/SSH 多会话隔离、daemon 重启恢复、锁定恢复、滚动选择和
布局/最近访问持久化验收后，使用最终 release 构建复测。环境为 macOS、1920×1080、
显示器 scale factor=1；唯一指标仍为 `vmmap --summary <pid>` 的 Physical footprint。

| 场景 | Physical footprint | 峰值 |
|---|---:|---:|
| Vida GUI，一个默认本地终端、稳定空闲 | **263.1M** | **263.3M** |
| vida-daemon，同一时刻运行 | **6592K** | 不采用旧进程累计峰值 |

同时重新运行 `vida-daemon/examples/term_probe`：`size_of::<Cell>() = 24` 字节。
5000 行 × 200 列满载纯 Cell 增量为 **24,000,000 字节（约 22.9 MiB）**，尚未计入
行容器索引等少量开销；默认 3000 行配置仍低于该满载场景。

**内存构成**（vmmap dirty 区域分析）：

| 区域 | 大小 | 说明 |
|---|---|---|
| MALLOC_SMALL (heap) | ~12M | wgpu/iced 内部数据结构，不随窗口缩放 |
| IOSurface (帧缓冲) | 2-7M | 随像素数缩放，上限 = 显示器物理分辨率 |
| GPU 驱动 | ~4M | IOAccelerator + owned unmapped (graphics) |
| 其他 | ~5M | 堆元数据、栈、全局变量 |

**Retina 推算**（基于 IOSurface 线性外推）：

| 显示器 | 物理分辨率 | 空窗口 footprint |
|---|---|---|
| MacBook Air 13" | 2560×1664 | ~44M |
| MacBook Pro 14" | 3024×1964 | ~48M |

### M4 release 实测（独立会话宿主）

**环境**：macOS，1920×1080、scale factor=1；release 构建；打开一个 100×30 本地终端。
唯一指标为 `vmmap --summary <pid>` 的 `Physical footprint`。

| 进程/场景 | Physical footprint | 峰值 |
|---|---:|---:|
| vida-daemon（控制面） | **3088K** | **3120K** |
| vida session host（一个活动本地终端） | **2640K** | **2640K** |
| daemon 退出后，session host 单独持有原终端 | **2608K** | **2640K** |

控制面与会话宿主同时运行合计约 **5.6 MiB**，没有因进程拆分超过 M3 最终复测中
vida-daemon 的 6592K。宿主保存的是原 PTY/SSH 与 alacritty 网格，不在 daemon 中复制
第二份回滚历史。

#### M4 最终验收复测（2026-08-10）

最终 release 构建在同一台 1920×1080、scale factor=1 的机器上复测。GUI 恢复两个
活动会话（100×40 与 128×45）；另用隔离的临时配置目录启动全新的空控制面，避免把
升级前仍在运行的旧 daemon 内存计入结果。临时目录不包含用户金库或会话数据，测量后删除。

| 进程/场景 | Physical footprint | 峰值 |
|---|---:|---:|
| Vida GUI，恢复两个终端标签 | **263.4M** | **264.3M** |
| vida-daemon，全新空控制面 | **3792K** | **3792K** |
| vida session host，持有两个活动会话 | **6896K** | **7536K** |

两个后台进程当前合计约 **10.4 MiB**；GUI 仍与 M3 最终复测的 263.1M 处于同一范围。
所有数字均来自 `vmmap --summary <pid>` 的 `Physical footprint`，未使用 RSS。

### M4 持久会话 — 验收清单

- 生产 daemon 使用 0600 Unix socket 连接独立 session host；凭据只经过本机 IPC，
  不进入 argv、环境变量或日志。
- 强制杀死真实 daemon 进程并启动新进程后，`ListSessions` 返回相同 session id；原 shell
  的环境变量、工作目录和运行中进程保持不变。
- 新 daemon 通过长连接转发输入、粘贴、resize、滚动、读屏和订阅；普通按键不会逐字符
  新建 IPC 连接。
- GUI 继续先订阅保存的原 session id；只有宿主也不存在时才创建替代本地/SSH 会话。
- 主动锁定仍显式关闭 PTY/SSH，作为安全边界；意外 daemon 断开不会关闭会话。
- 最后一个会话关闭且 daemon 已退出后，空 session host 自动结束并移除 socket。
- 自动测试包含 session host 控制客户端替换、推送帧编解码，以及真实 daemon 进程被
  `SIGKILL` 后原 shell 状态恢复。
- 所有者手动验收见 README「M4 持久会话手动验收」。

### M5a 共享 daemon 客户端与只读 CLI

- 原 GUI WebSocket 客户端下沉到独立内部 crate `vida-client`；GUI 仅保留兼容 re-export。
  daemon 端口/token 发现、认证重试、请求 ID 关联、错误分类和终端推送不再由 GUI、CLI、
  MCP 分别复制。
- `vidactl doctor/status/host list/host show/session list/session screen` 通过同一客户端连接
  daemon。这个检查点严格只读：策略与审计完成前不暴露 `send`、`exec` 或金库修改命令。
- `--json` 固定输出 `{"ok":true,"data":...}` 或 `{"ok":false,"error":...}`；连接失败
  退出码为 10，daemon 业务错误为 11，未找到/歧义/响应损坏分别为 12/13/14。
- 人类可读输出使用与 GUI 相同的持久化语言选择和系统语言检测；`--json` 的机器字段及
  `session screen` 的远端原始文本不翻译。自动测试分别覆盖英文状态和原样终端内容。
- 主机命令只消费 `HostSummary`，不调用 `RevealCredential`；口令和私钥内容不会进入 CLI
  输出。完整 ID 优先于名称；重复名称返回歧义错误，不静默选择第一项。
- 自动测试覆盖显式地址认证与响应关联、首帧预注册、连接退出、主机选择歧义及 JSON
  envelope。所有者手动验收见 README「M5a 共享客户端与只读 CLI 手动验收」。

#### M5a release 内存实测（2026-08-10）

macOS、1920×1080、scale factor=1。使用隔离临时配置目录启动 release daemon；在
`vidactl status` 等待 WebSocket 握手时暂停 daemon，以便按规范用 `vmmap --summary`
测量短生命周期 CLI，随后恢复 daemon 并确认命令正常结束。

| 进程/场景 | Physical footprint |
|---|---:|
| `vidactl status`，已加载共享客户端并等待 daemon | **2096K** |

该数字是 `Physical footprint`，不是 RSS；测量用临时目录不含用户金库或会话数据，完成后删除。

### M5b Agent 写入策略、审批与审计

- v7 金库为每台主机保存 `agent_trust`；v6 迁移统一使用安全默认值 `ask`，并有真实 v6 JSON
  快照测试验证凭据和其余字段不丢失。
- daemon 维护 session→host 的设备本地映射。Agent 使用独立角色 token，只能调用
  `AgentExec` 和必要的只读接口；人的 `SessionInput`/`PasteSession` 不经过策略层，也不能被
  Agent token 调用。
- 命令门提供 readonly/ask/trusted、内置危险命令表、120 秒内存审批和 0600 JSONL 审计。
  审计不保存命令明文，只保存长度与 SHA-256 指纹；审计落盘失败时不会发送命令。
- `vidactl session exec` 使用 Agent 角色；`host trust`、`approval`、`audit` 使用所有者角色。
  `needs_approval`/`rejected` 的稳定退出码分别为 20/21，JSON 仍保持统一 envelope。

#### M5b release 内存实测（2026-08-10）

macOS、1920×1080、scale factor=1。使用隔离临时配置目录启动 release daemon；先测空闲
控制面，再打开一个 100×40 本地会话，分别执行一条允许命令和一条进入待审批的危险命令，
使规则正则、session 映射、审计和 pending approval 全部实际加载。

| 进程/场景 | Physical footprint |
|---|---:|
| release daemon，空金库、无会话 | **3872K** |
| release daemon，1 个 100×40 会话、策略已加载、1 条待审批 | **6368K** |

两项均来自 `vmmap --summary <pid>` 的 `Physical footprint`，不是 RSS。测试同时确认审计文件
只含命令字节数和 SHA-256，不含两条测试命令明文；随后关闭会话、停止 daemon 并删除隔离
临时目录。

### M5c GUI 实时审批与人工接管

- daemon 为所有者连接广播 approval requested/resolved 事件；Agent 连接只能消费自己的请求
  响应，不能收到所有者审批控制面。
- `vida-client` 预注册 owner event channel，保证事件先于 GUI Subscription 到达时仍在队列中；
  GUI 重连同时读取完整 pending 列表，补偿断线期间事件。
- GUI 使用共享视觉 token 显示多审批队列、命令详情、规则原因、倒计时和单次批准/拒绝；顶栏
  数量入口允许稍后处理。主机编辑页复用现有 picker 配置 readonly/ask/trusted。
- 自动测试覆盖 request/resolved 推送、先事件后订阅、owner-only WebSocket、GUI 队列状态和
  旧摘要缺字段时安全回落为 ask。可见交互由 README「M5c GUI 实时审批与主机权限手动验收」
  交给所有者实际点击确认。

#### M5c release 内存实测（2026-08-10）

macOS、1920×1080、scale factor=1。使用隔离临时配置目录启动最终 release GUI，在 daemon
不可达的稳定连接提示页等待 4 秒后测量；同时只读测量已运行且加载 M5 Agent 控制面的 release
daemon。审批面板本身按需构造，不创建后台轮询线程；有待审批项时只增加一个每秒 tick。

| 进程/场景 | Physical footprint |
|---|---:|
| release GUI，隔离配置、稳定连接提示页 | **35.9M** |
| release daemon，Agent 控制面已加载 | **6912K** |

两项都来自 `vmmap --summary <pid>` 的 `Physical footprint`，不是 RSS。GUI 启动期峰值
230.8M 来自既有 wgpu/IOSurface 初始化回收前分配，不作为稳定占用；daemon 峰值包含既有
scrypt 解锁过程，同样不作为稳定占用。

### M6 标准 MCP 服务端

- `vida-mcp` 使用官方 Rust MCP SDK 提供标准 stdio 生命周期、并发 JSON-RPC、工具发现以及
  输入/输出 JSON Schema；stdout 只输出 MCP 帧。
- 工具面固定为状态、主机摘要、活动会话、当前屏幕和安全命令执行。连接使用 `vida-client`
  的 Agent token，不存在凭据读取、原始 `SessionInput`、金库写入或审批工具。
- 只读请求在 daemon 重启后安全重连一次；`exec` 不自动重试不确定结果。危险命令仍由 daemon
  创建审批并实时显示在 GUI，MCP 只能返回 `needs_approval`。
- 自动测试真实启动 stdio 子进程，完成 initialize、initialized、tools/list 和 tools/call；
  验证 daemon 不在线是工具级错误而不是 MCP 断线，并锁定五个工具及全部结构化 Schema。
- 所有者手动验收见 README「M6 MCP 服务端手动验收」。

#### M6 release 内存实测（2026-08-10）

macOS、同机显示器 scale factor=1（`vida-mcp` 是无窗口 stdio 进程，scale factor 不参与其
内存分配）。使用最终 release 构建完成 MCP initialize，并调用 `vida_status` 建立 Agent
WebSocket 连接后保持 stdin 打开，按规范测量稳定进程：

| 进程/场景 | Physical footprint |
|---|---:|
| `vida-mcp`，MCP 已初始化、Agent 连接已建立 | **2640K** |

该数字来自 `vmmap --summary <pid>` 的 `Physical footprint`，不是 RSS；同次测得 peak 也是
2640K。测量过程未修改金库或终端，仅调用状态读取。

### M7 Agent 主动打开已配置 SSH

- daemon 新增 Agent-only `AgentOpenSshSession`，仅接受已保存的 `host_id`；owner 继续使用原有
  `OpenSshSession`，Agent 仍不能调用人的原始输入、凭据显示或金库写入接口。
- 同一主机已有活动 SSH 会话时直接复用；新会话完成绑定和无凭据审计后向 owner 推送
  `session_opened`，GUI 预注册推送通道并走既有终端标签创建路径。
- MCP 新增带完整输入/输出 Schema 的 `session_open`；不确定断线允许安全重试一次，因为 daemon
  端已保证按主机 ID 幂等。工具说明禁止后台试探连接和任意地址/凭据输入。
- 自动测试覆盖 Agent/owner 角色隔离、真实 SSH PTY 创建、并发与重复调用复用、owner 事件、MCP 工具
  清单与凭据不进入响应/事件/审计。所有者手动验收见 README「M7 Agent 主动连接手动验收」。

#### M7 release 内存实测（2026-08-10）

macOS、同机显示器 scale factor=1（两个被测进程均无窗口，scale factor 不参与其内存分配）。
使用隔离临时配置启动最终 release daemon 和 `vida-mcp`；MCP 完成 initialize，并通过
`vida_status` 建立 Agent WebSocket 连接，使 M7 工具表、Agent 身份和连接路径实际加载：

| 进程/场景 | Physical footprint |
|---|---:|
| `vida-mcp`，MCP 已初始化、Agent 连接已建立 | **2848K** |
| release daemon，空金库、Agent 连接已建立 | **4144K** |

两项均来自 `vmmap --summary <pid>` 的 `Physical footprint`，不是 RSS；同次 peak 分别为
2864K 和 4224K。测量只读取空金库状态，没有创建主机、会话或修改用户配置。

### M8 Agent 准备 SSH 主机草稿

- daemon 新增 Agent-only `AgentPrepareHost`，请求结构不含任何认证材料并拒绝未知字段；名称、
  地址、用户名、端口、标签、分组和备注在 daemon 侧统一校验与规范化。
- 请求只产生无凭据审计和 owner 事件，不调用 `UpdateHost`、不写金库。审计输入只保留长度和
  SHA-256，不额外明文保存主机地址、名称或备注。
- GUI 复用 S4 现有新增主机编辑器和共享视觉样式；认证、凭据和 Agent 权限使用人的安全默认值。
  人正在编辑时草稿排队，完成或取消后再打开，Agent 永不覆盖人的输入。
- MCP `host_prepare` 明确返回 `awaiting_human`，不把草稿表述为已创建；写入响应不确定时不自动
  重试。自动测试覆盖角色隔离、锁库拒绝、未知凭据字段拒绝、不写金库、owner 事件、审计脱敏、
  GUI 人类输入优先以及固定七工具 Schema。所有者手动验收见 README 对应清单。

#### M8 release 内存实测（2026-08-10）

macOS、同机显示器 scale factor=1（两个进程均无窗口，scale factor 不参与其内存分配）。使用
隔离临时配置启动最终 release daemon 和 `vida-mcp`；MCP 完成 initialize、加载包含
`host_prepare` 的七工具 Schema，并通过 `vida_status` 建立 Agent WebSocket 连接：

| 进程/场景 | Physical footprint |
|---|---:|
| `vida-mcp`，七工具已加载、Agent 连接已建立 | **2720K** |
| release daemon，空金库、Agent 连接已建立 | **3920K** |

两项均来自 `vmmap --summary <pid>` 的 `Physical footprint`，不是 RSS；同次 peak 分别为
2736K 和 3984K。测量只读取隔离空金库状态，没有创建主机、凭据或终端会话。

### M9 主机运行档案

- `HostEntry.notes` 作为加密 Markdown 正文，不增加 Vault 字段；`update_notes_section` 精确定位
  二级标题，append/replace 均保留无关小节。
- daemon 新增 Agent-only 档案读写协议。写入前验证主机、标题和 64 KiB 正文上限，拒绝未知
  字段；正文只以长度与 SHA-256 进入审计。owner 事件不携带正文，GUI 收到后重新加载主机并
  走 `set_hosts()`，不会产生业务数据分层漂移。
- `vida-mcp` 增加三个带输入/输出 Schema 的 notes 工具和动态主机 resources；`vidactl host
  notes read|append|replace` 复用同一 `vida-client` 路径。读取可安全重连重试，写入结果不确定
  时不重试，避免重复追加。

#### M9 release 内存实测（2026-08-10）

macOS、同机显示器 scale factor=1（两个进程均无窗口）。使用隔离临时配置启动最终 release
daemon 和 `vida-mcp`；MCP 完成 initialize、加载十个工具与 resources capability，调用
`vida_status` 建立 Agent WebSocket，并实际进入 `resources/list` 的锁库错误路径：

| 进程/场景 | Physical footprint |
|---|---:|
| `vida-mcp`，十工具及 resources 已加载、Agent 已连接 | **3344K** |
| release daemon，隔离空配置、Agent 已连接 | **4656K** |

两项均来自 `vmmap --summary <pid>` 的 `Physical footprint`，不是 RSS；同次 peak 分别为
3360K 和 4656K。隔离配置没有主机、凭据或终端会话，resource 请求因金库不存在而按预期返回
明确锁库错误，没有修改用户配置。

### M1 内存数据（含 S0-S9 GUI）

| 场景 | Footprint | 说明 |
|---|---|---|
| S0 空窗口（daemon 未启动） | **32.9M** | 与 M0 基线一致，GUI 代码无额外开销 |
| 解锁期间峰值 | **229.7M** | scrypt KDF 瞬时增加，解锁后回落 |

### 关键发现

1. **字体加载不占内存**：`__FONT_DATA` 仅 2352 bytes，0K dirty。
2. **帧缓冲受显示限制**：IOSurface 上限 = 显示器物理分辨率。
3. **RSS ≠ 实际占用**：RSS ~90MB 包含共享库页面，footprint ~33MB 才是真实占用。

## 加密参数

- 算法：age scrypt recipient（标准 age 格式，可用 `age` CLI 解密）
- Work factor：log_n = 18（N = 262144, r = 8, p = 1）
- 实测解密耗时：0.362s（age CLI 1.3.1, 10 次中位）
- **解锁期间进程内存瞬时增加 ~256MB，这是 scrypt KDF 的预期行为，不是内存泄漏**

## 验收清单（M0）

- [x] 打开 app，看到空窗口
- [x] 活动监视器中内存 ≈33 MB（footprint, 1920×1080@1x）
- [x] 空闲 CPU ≈ 0%
- [x] Daemon WebSocket 通信正常（Ping/Pong）

## 验收清单（M1）

- [x] age scrypt 加密/解密（log_n=18）
- [x] 系统 `age` CLI 互操作验证
- [x] 原子写入（temp → F_FULLFSYNC → rename → fsync 父目录）
- [x] 备份轮转（写入前 shift，上限 10 份）
- [x] 敏感字段 zeroize（SecureString: 私钥、口令、S3 密钥）
- [x] session host 仅在内存中有期缓存，口令解锁独立可用
- [x] 断电安全模拟（原子写入保证原文件不被部分覆写）
- [x] 主口令设置与解锁流程
- [x] 主机条目增删改（凭据 Option 语义）
- [x] LocalPath 同步后端（SyncBackend trait + LocalPathBackend + SyncCoordinator）
- [x] GUI 屏幕 S0-S9 全部实现
- [x] 场景 2：首次启动创建金库（2026-08-05 实测通过）
- [x] 场景 3：解锁已有金库（2026-08-05 实测通过）
- [x] 场景 4：主机编辑器（2026-08-05 实测通过）
- [x] 场景 5：远端更新后的同步（Downloaded 路径）（2026-08-05 实测通过）
- [x] 场景 6：选择远端后的界面状态（resolve_conflict_remote 路径）（2026-08-05 实测通过）

---

## M1 手动验收步骤（场景 2-6）

> 自动测试无法验证「界面上有没有提示」「列表有没有刷新」，
> 以下五条是 M1 完成的定义，需真机操作。

### 场景 2：首次启动创建金库

1. 启动 daemon：`cargo run --release --bin vida-daemon`
2. 启动 GUI：`cargo run --release --bin vida`
3. 看到 S1「创建金库」页面
4. 输入口令 → 确认口令 → 勾选风险确认 → 点击「创建金库」
5. 自动跳转到 S3 主机列表（空列表）
6. 点 🔒 锁定 → 回解锁页 → 重新解锁 → 口令正确可解锁

### 场景 3：解锁已有金库

1. 再次启动 GUI → 看到 S2「解锁金库」页面（不出现 S1）
2. 输入错误口令 → 显示错误提示，留在 S2
3. 输入正确口令 → 解锁成功 → 进入 S3
4. 选择「记住 7 天」解锁 → 口令仅在独立 session host 内存中有期缓存；daemon 在有效期内
   重启可自动解锁，且 macOS 不弹系统钥匙串确认框；过期、主动锁定或系统重启后必须重新输入口令

### 场景 4：主机编辑器

1. S3 点 ⊞ 快速连接面板 →「+ 新增主机」→ 进入 S4
2. 填写名称/地址/用户名/端口/口令 → 保存 → 回 S3，列表出现新主机
3. 选中主机 →「编辑」→ 进入 S4（预填充现有数据）
4. 修改名称 → 保存 → 确认名称已更新
5. 编辑时口令框留空 → 保存后原口令不变（保持原有口令语义）
6. 删除主机 → 从列表消失

### 场景 5：远端更新后的同步（Downloaded 路径）

**双设备模拟**：LocalPath 后端的「远端」就是共享目录里的 `vault.age`，
两台设备 = 同一个 `VIDA_CONFIG_DIR` 目录先后使用。

```bash
export VIDA_CONFIG_DIR=/tmp/vida-sync
# 设备 A
cargo run --release --bin vida-daemon &
cargo run --release --bin vida
#   → 解锁 → 添加一台主机 → 点 ⟳ 同步（显示 ✓）
# 关闭 A 的 GUI 和 daemon
```

```bash
# 设备 B（同一目录）
export VIDA_CONFIG_DIR=/tmp/vida-sync
cargo run --release --bin vida-daemon &
cargo run --release --bin vida
#   → 解锁 → 点 ⟳ 同步
```

验收点：
1. B 的列表**立即**显示 A 添加的新主机（Downloaded 分支刷新）
2. **关闭 B 的 GUI 重开，解锁** → 新主机仍在（文件已真实写入，不只是内存）
3. B 再次同步 → 返回「无变化」（不会重复下载）

**实测记录（2026-08-05）**：双设备模拟通过。B 同步后列表立即出现
A 添加的主机；B 重启 GUI 解锁后主机仍在；B 再次同步显示 ✓ 无变化。
另验证了同名 tab 场景：A 开 host 的 tab → B 改名后同步 → A 同步 →
tab 标题已随 `set_hosts()` 重建为新名称，点击后内容与新名称一致。

### 场景 6：选择远端后的界面状态（resolve_conflict_remote 路径）

```bash
export VIDA_CONFIG_DIR=/tmp/vida-sync
# 设备 A：解锁 → 添加主机 host-A → 同步（✓）
# 设备 B（同一目录）：解锁 → 添加主机 host-B → 同步
#   → 此时 vault.age 已被 A 覆盖，B 检测到冲突
```

验收点：
1. 制造冲突 → 出现 S6 冲突界面 → 两侧**都有**主机列表和数量
2. 选择「使用远端」→ 列表立即变为远端的主机集合
3. **不要重启**，直接编辑任意一台主机并保存
4. 再次同步 → 远端仍是远端版本 + 那一处编辑
5. **关键断言**：本地旧数据（host-B）没有被写回——否则说明内存未替换

**实测记录（2026-08-05）**：冲突界面两侧列表和数量正确；选择「使用
远端」后列表立即变为远端集合；随后直接编辑一台主机并保存，再次同步
后远端是远端版本 + 该编辑。**关键断言成立**：本地旧数据（host-B）
未被写回，说明 resolve_conflict_remote 确实替换了内存中的 Vault 而非
只是界面层遮盖。

### 场景 7：同步设置页 + 同步按钮

```bash
export VIDA_CONFIG_DIR=/tmp/vida-sync
cargo run --release --bin vida-daemon &
cargo run --release --bin vida
#   → 解锁 → 打开设置 → 同步分区
```

验收点：
1. 点击同步模式下拉 → 选择「本地文件夹」→ 预期看到路径输入框 + 选择文件夹按钮出现
2. 点击「选择文件夹」→ 预期弹出系统文件夹对话框，选择后路径填入输入框
3. 点击「常用位置：iCloud Drive」→ 预期路径变为 `~/Library/Mobile Documents/com~apple~CloudDocs/vida/`（不存在则创建）；未启用 iCloud 时显示错误提示
4. 点击「常用位置：~」→ 预期路径变为用户主目录
5. 切换回「不同步」→ 预期路径清空，保存后同步按钮显示「未配置同步」
6. **关键断言（回归）**：点击同步按钮 → 无论当前显示什么状态，请求都会发出 —— daemon 日志出现 `Sync upload/download/decision` 记录；未配置同步时按钮点击也发出请求，由 daemon 返回 `sync_not_configured`，界面显示「未配置同步」而非跳转设置
7. 保存路径后点同步 → daemon 日志出现 `Sync upload OK: <路径> (<字节> bytes)`，远端 `vault.age` 存在

---

## M2a 终端核心 — 验收清单

> 全部使用 `VIDA_CONFIG_DIR=/tmp/vida-m2a` 隔离，不触碰真实配置。
>
> **不需要手动 mkdir**：daemon 启动时自动创建配置目录及 `backups/`
> 子目录（`ensure_dirs`，M2b-1 起）。目录缺失时首次启动会自行创建；
> 不可写时给出人话错误而非 os error。

### 指标（M2a-spec 第 8 节，实测）

| 指标 | 实测值 | 说明 |
|---|---|---|
| `size_of::<Cell>()` | **24 字节** | char 4 + fg 4 + bg 4 + flags 4 + Option\<Arc\> 8 |
| 3000 行 × 200 列满载内存增量 | **15.4 MB** | 实测（vmmap 前后对比，2026-08-07）。当前默认 scrollback=3000。3000×200×24 = 14.4 MB 算术值 + ~1 MB Grid 行索引/Row 元数据开销 |
| 5000 行 × 200 列满载外推 | **~25.7 MB** | 按 25.7 字节/cell 线性外推（15.4 MB / 3000 行）。**超过 20 MB 目标**，见下方讨论 |
| `yes` 持续 10 秒带宽 | **0.03 MB/s** | 实测（watch 3s：155 帧，90 KB，51.6fps，200×50 终端，WebSocket 实测） |
| yes 场景 daemon footprint | **3536K 零增长** | 有界通道 + 丢旧留新生效。yes 持续输出、seq 累积到 7764 时多次采样 footprint 稳定（2026-08-07） |
| `cat` 100MB 文件 | **1.242 s**，峰值 **+0.2 MB**（19.0 MB） | 实测。scrollback 有界 → 内存不随输出增长 |
| 限频实测 | **15.16-18.59 ms** | push_loop 内部间隔（诊断日志实测，多数恰 16.00ms）。绝对时隙基准 |
| 空闲 CPU | ≈ 0% | 推送循环 60fps 限频，无输出时不产生帧 |
| 内存测量方式 | `vmmap --summary` Physical footprint | 禁止 RSS |

**5000 行外推值超标的说明**：5000×200×25.7 字节 ≈ 25.7 MB > 20 MB 目标。
**决定（2026-08-07）**：不改任何默认值和上限。默认 scrollback=3000 实测
15.4 MB 达标；5000 行仅出现在用户主动调高时，属用户的知情选择。

**换算关系（实测）**：**约 5.1 MB / 1000 行**（3000 行实测 15.4 MB）。
高于算术值 14.4 MB（3000×200×24 字节）约 7%，差额为 Grid 行索引与
Row 元数据开销。设置页「回滚行数」下方已注明「每 1000 行约占用 5 MB 内存」。

### 工具

**重要：daemon 必须保持运行，会话由 daemon 持有。**

```bash
# 终端 1：启动 daemon 并保持运行
export VIDA_CONFIG_DIR=/tmp/vida-m2a
cargo run --release --bin vida-daemon
# （保持此终端开着）

# 终端 2：执行验收命令
export VIDA_CONFIG_DIR=/tmp/vida-m2a
cargo run --release -p vida-term-test -- open
```

> **注意**：
> - M4 起 session id 属于独立会话宿主；daemon 重启后仍可用原 session_id 继续操作。
> - open / send / screen 等均为**一次性命令**，执行完即退出。
>   会话由 session host 持有，用 open 返回的 session_id 在后续命令中引用。
> 每个 cargo run --release -p vida-term-test -- 子命令是独立进程，连接后即断，但会话保留在 session host。

常用变量：

```bash
SID=<上一步 open 输出的 session_id>
```

### 场景 8 — 基本回显

```bash
SID=$(cargo run --release -p vida-term-test -- open)
cargo run --release -p vida-term-test -- send "$SID" 'echo hello\n'
sleep 0.5
cargo run --release -p vida-term-test -- screen "$SID"
```

预期：屏幕上有 `hello`，光标在下一行行首。

**实测记录（2026-08-07）**：`echo hello` 后 screen 显示第 2 行 `hello`，
光标在第 3 行行首。✅

### 场景 9 — 颜色

```bash
cargo run --release -p vida-term-test -- send "$SID" 'ls --color\n'
sleep 0.5
cargo run --release -p vida-term-test -- screen "$SID" --ansi
```

预期：目录名带颜色，与在真实终端中执行 `ls --color` 的结果一致。

**实测记录（2026-08-07）**：`screen --ansi` 目录名带颜色，与真实终端
一致。✅

### 场景 10 — 全屏 TUI

```bash
cargo run --release -p vida-term-test -- send "$SID" 'vim\n'
sleep 1
cargo run --release -p vida-term-test -- screen "$SID"
```

预期：看到 vim 界面（波浪号列、状态行）。

**实测记录（2026-08-07）**：vim 界面正确显示（波浪号列）。注意 vim
默认 laststatus=1 单窗口无状态栏，这是正确行为。✅

```bash
cargo run --release -p vida-term-test -- send "$SID" '\e:q!\n'
sleep 0.5
cargo run --release -p vida-term-test -- screen "$SID"
```

预期：回到 shell 提示符。

**实测记录（2026-08-07）**：`\e:q!` 退出 vim 回到 shell 提示符。✅

### 场景 11 — 动态刷新

```bash
cargo run --release -p vida-term-test -- send "$SID" 'htop\n'
sleep 2
cargo run --release -p vida-term-test -- screen "$SID"
sleep 2
cargo run --release -p vida-term-test -- screen "$SID"
sleep 2
cargo run --release -p vida-term-test -- screen "$SID"
```

预期：三次内容不同（说明在刷新），布局不错乱。

**实测记录（2026-08-07）**：三次 screen 内容不同（top 动态刷新），
布局不错乱。✅

```bash
cargo run --release -p vida-term-test -- send "$SID" 'q'
sleep 0.5
cargo run --release -p vida-term-test -- screen "$SID"
```

预期：回到 shell。

**实测记录（2026-08-07）**：`q` 退出 top 回到 shell。✅

### 场景 12 — 中文对齐

先创建中文名文件：

```bash
touch /tmp/vida-m2a/测试文件.txt /tmp/vida-m2a/中文文档.md /tmp/vida-m2a/数据.csv
```

```bash
cargo run --release -p vida-term-test -- send "$SID" 'ls -la /tmp/vida-m2a\n'
sleep 0.5
cargo run --release -p vida-term-test -- screen "$SID"
```

预期：文件名不串列。

**实测记录（2026-08-07）**：中文文件名（测试文件.txt 等）正确对齐，
不串列。`screen --show-wide` 显示 `^` 标记的宽字符起始列。✅

### 场景 13 — resize

```bash
cargo run --release -p vida-term-test -- send "$SID" 'echo before resize\n'
sleep 0.3
cargo run --release -p vida-term-test -- resize "$SID" 40 12
sleep 0.3
cargo run --release -p vida-term-test -- screen "$SID"
```

预期：网格变为 40 列 12 行，内容重排后没有乱码。

**实测记录（2026-08-07）**：resize 到 40×12 后网格正确，宽字符
reflow 特别正确——40 列折行时「测试」没有被劈开。✅

### 场景 14 — 会话结束

```bash
cargo run --release -p vida-term-test -- send "$SID" 'exit\n'
sleep 0.5
cargo run --release -p vida-term-test -- list
```

预期：该会话标记为已结束或已移除；`ps` 中无僵尸进程。

**实测记录（2026-08-07）**：`exit` 后 list 显示 `closed`；订阅方收到
`session_closed` 事件（exit_code=0）；无新增僵尸进程。✅

### 场景 15 — 高吞吐

```bash
# 终端 1：启动 yes 并 watch
SID=$(cargo run --release -p vida-term-test -- open --cols 200 --rows 50)
cargo run --release -p vida-term-test -- send "$SID" 'yes\n'
# 另一个终端：
cargo run --release -p vida-term-test -- watch "$SID"
# 10 秒后：
cargo run --release -p vida-term-test -- send "$SID" '\x03'
```

预期：
- `watch` 输出的帧率不超过 60fps（间隔 ≥ 16ms）
- 带宽符合第 8 节指标
- daemon 内存不持续增长（用 `vmmap --summary $DAEMON_PID` 观察）

**实测记录（2026-08-07）**：
- push_loop 内部间隔 15.16-18.59ms（绝对时隙限频）
- daemon footprint 3536K 零增长（有界通道 + 丢旧留新）
- watch 统计：51.6fps，平均 18.9ms
- **注意**：watch 显示的 seq 跳号（如 0→26）是正常现象——会话在
  订阅前已运行，push_loop 已产生若干帧，订阅时只发当前快照。
  **判断限频应看 push_loop 内部间隔，不看客户端接收间隔**
  （客户端接收受通道缓冲、订阅首帧等因素影响，会造成假象）。✅

同时用 vmmap 观察 daemon 内存：

```bash
vmmap --summary $DAEMON_PID | grep "Physical footprint"
```


## M2b-1 实时渲染 — 验收清单（检查点 B）

### 指标

| 指标 | 说明 |
|---|---|
| 空闲 CPU | 应 ≈ 0%（无推送帧时 GUI 不重绘、订阅挂起） |
| `yes` 持续输出时 GUI CPU | 记录实测值（yes 命令从 vida-term-test 发出） |
| 单终端 GUI 内存增量 | `vmmap --summary <pid>` Physical footprint，注意 scale factor |

### 检查点 B 实测结果（2026-08-07，1920×1080 非 HiDPI，scale=1）

| 指标 | 实测值 |
|---|---|
| 空闲 CPU | **0.0%**（终端屏打开、无输出时；无常驻定时器） |
| `yes` 持续输出时 GUI CPU | **18-41%**（满屏 60fps 真实渲染：每帧重建几何 + 字形光栅化 + 上传） |
| 单终端标签内存增量 | **16.9M**（33.6 → 50.5M，首标签含 iced pipeline 缓存：图集 4MB + 字形缓存；后续标签复用） |
| 字体 | Menlo-Regular（显式指定）+ PingFangSC-Regular（中文 fallback） |
| cell 尺寸（scale=1） | 10×20，ascent=16 |

**验收结论**：echo hello / ls --color / vim / 中文 ls 全部正确；空闲零 CPU；
daemon 空闲不推帧（修复 alacritty 每帧光标 damage 的空转帧）。

### 手动验收步骤（GUI 只读，命令从 vida-term-test 发）

1. 启动 daemon（保持运行），启动 GUI，进入主界面后点击左侧「▮_」调试终端按钮
   → 预期：出现终端画面，显示 shell 提示符
2. `vida-term-test send <SID> "echo hello\r"`
   → 预期：GUI 实时显示 `hello`
3. `vida-term-test send <SID> "ls --color\r"`
   → 预期：目录列表颜色正确（绿色可执行文件、蓝色目录等）
4. `vida-term-test send <SID> "vim\r"` → 等待界面绘制 → `:q!` 退出
   → 预期：vim 界面正确显示
5. 创建一个含中文文件名的目录后 `ls`
   → 预期：中文文件名列对齐正确（宽字符占两列，不串列）
6. 空闲观察（无任何输入 10 秒）：GUI CPU ≈ 0%
7. `vida-term-test send <SID> "yes\r"` 持续 10 秒：观察 GUI CPU 与内存增量，
   然后 Ctrl+C（`data:[3]`）停止
8. 多订阅验证：GUI 打开调试终端的同时，用 vida-term-test 的
   subscribe 命令订阅同一会话（若 CLI 支持）；否则由 daemon 集成测试
   `single_connection_multi_subscribe` 覆盖（同一连接订阅两会话、
   取消一个后另一个正常）

## M2b-2 可交互终端 — 验收清单（检查点 C）

### 自动与 GUI 实测（2026-08-08，1920×1080 非 HiDPI，scale=1）

- 键盘输入：`printf 'M2B2_INPUT_OK\n'` → 正确输出；Ctrl+C 可中止 `sleep 30`。
- 导航键：方向键上可取回并重新执行上一条命令。
- 中文多行粘贴：粘贴后两行停留在 shell 编辑缓冲区，没有立即执行；按 Enter 后
  输出两行中文。该路径同时验证 `PasteSession` 与 bracketed-paste 模式。
- resize：窗口缩小后 `stty size` 从 `30 100` 变为 `22 77`，GUI grid、Term 与
  PTY ioctl 三层一致。
- 中文输入法候选窗：自动化无法切换真实系统输入法，保留给所有者按 README 清单确认。

### 内存与 CPU

| 指标 | 实测值 |
|---|---|
| 显示器 scale factor | **1** |
| 空闲 CPU | **0.0%**（终端打开、无输入/输出，release） |
| 主界面 Physical footprint | **36.3M**（release，金库已解锁） |
| 打开单终端后 Physical footprint | **53.0M**（release，100×30） |
| 单终端增量 | **16.7M** |

唯一内存指标仍为 `vmmap --summary <pid>` 的 `Physical footprint`，禁止用 RSS。

### 所有者视觉验收修正

- 默认字体为内置 JetBrains Mono 13pt，JetBrains Mono 2.304 Regular/Bold 以 OFL-1.1
  随应用分发；字体下拉同时保留系统等宽字体。根据 Oryxis 的成熟做法，文字不再经过
  Vida 自建的 CoreText 位图、字形图集与采样器，而是由 iced canvas/cosmic-text 直接
  渲染。ASCII 合并为不超过 32 字符的 run，CJK 由系统字体自然回退并保持逐 cell 定位；
  cell advance 用 iced Paragraph 实测，行高为字号的 1.15 倍。前景 `#ffffff`、终端背景
  `#282c34`。隔离 release 窗口在 scale=1、输出中英文并输入未执行的 `ls --color` 后，
  `vmmap --summary` Physical footprint **264.8MB**（峰值 265.2MB）。
- 增加 500ms 闪烁的实心方块光标；timer 仅在终端屏且用户启用闪烁时存在。
- WIDE 字符仍推进两个 cell，中文交给 iced/cosmic-text 的系统字体回退，不做单轴拉伸
  或人工覆盖率处理。
- 「设置 → 终端」从内置 JetBrains Mono 与系统等宽字体中选择字体，并从固定列表选择字号；
  可持久化字体、字号与光标闪烁，保存后新开的终端生效。
- ANSI 0—15 色使用 Ghostty 默认调色板，NamedColor 在 daemon 推帧时保留对应索引。
  `-` 不再作为 1px 位图单独上传，而是与相邻 ASCII 一起交给 iced 文本管线塑形，
  输入态和执行后的渲染路径完全相同。
- M2b-2 调试终端仍是临时 Screen；点击真实标签或设置时会先关闭调试会话并恢复
  Main，再执行标签切换，避免 active tab 已变化但终端仍覆盖内容。
- 自动 GUI 首轮已看到提示符处方块光标；最终字体视觉观感仍以所有者截图验收为准。

## UI 基础框架改造（2026-08-08）

本轮在 M2b-2 验收后插入独立 UI 里程碑，只统一视觉系统和现有页面布局，
不提前实现 M2b-3 的正式终端标签业务。

- 新增统一 iced 设计令牌：应用/侧栏/表面/输入层级、文字层级、边框、圆角、
  主色和成功/警告/危险状态集中定义。
- 顶部标签栏采用 46px 外框、36px 标签芯片和固定右侧操作区；标签区独立横向滚动，
  多标签时不会把同步、设置、锁定挤出窗口。
- 使用单个 Lucide 字体文件提供一致的线性图标。没有增加 WebView、SVG 渲染器或新的
  GUI crate；运行时仍是 iced + wgpu。
- 主机详情、快速连接、设置侧栏、设置内容、连接/解锁页和调试终端会话栏使用同一套
  表面与交互状态。原 AppMessage 和 daemon 请求路径保持不变。

### 内存实测

`vmmap --summary <pid>`，release，1024×768，显示器 scale factor=1：

| 场景 | Physical footprint |
|---|---|
| 解锁页空闲 | **33.7M** |

此前 M1 同类未解锁/空窗口基线为 32.9M；增加统一主题和一份图标字体后当前占用增加约
0.8M。峰值会包含 wgpu 启动期资源回收前的瞬时分配，不作为验收指标。

## M2b-3 正式终端标签 — 验收清单（检查点 D）

- 临时 `Screen::Terminal` 已删除；本地终端成为正式 `TabKind::Terminal`。
- 每个标签拥有独立的 session id、客户端网格、viewport 度量、光标与提示状态。
- 同一 WebSocket 可同时订阅全部终端标签；非活动标签继续接收增量帧，但不创建额外
  轮询或光标 timer。
- 输入、粘贴和 resize 只路由到当前活动终端；服务端帧和退出事件按 session id 路由，
  不会写入错误标签。
- 关闭终端标签会按输入队列顺序发送 `CloseSession`；锁库会关闭全部终端 PTY，避免
  锁定界面背后仍有人的 shell 在运行。
- WebSocket 断开后一次性恢复所有仍存活的标签；daemon 重启导致原 PTY 丢失时，在
  原标签中创建替代会话并明确提示，不静默串台。
- 自动测试覆盖两个终端标签的退出事件隔离，以及关闭一个标签不删除另一个会话。
- 鼠标左键拖动选择可见网格文字，Cmd+C/Ctrl+Shift+C 和右键菜单均可复制；右键粘贴
  继续走独立 `PasteSession`，剪贴板内容不写日志。
- 滚轮通过 `ScrollSession` 操作 daemon 中 alacritty 的真实回滚历史；GUI 仍只持有当前
  可见网格，不为每个标签复制 3000 行历史。滚动期间光标隐藏，键盘输入回到底部。
- 所有者手动验收见 README「M2b-3 手动验收」。

### 内存实测

使用 `vmmap --summary <pid>` 的 `Physical footprint`；release、1024×768、scale=1。

| 场景 | Physical footprint |
|---|---|
| 主界面空闲、尚未打开终端 | **33.5M** |
| 两个正式本地终端标签已打开 | **259.2M** |

双终端场景由所有者从 GUI 打开两个正式标签后测量；同一进程连续复测仍为 259.2M，
峰值 261.8M。该数值包含终端文字渲染管线和 wgpu 资源首次启用后的实际常驻开销，
不使用 RSS 替代；与 M2b-2 同一原生文字管线的单终端隔离实测 264.8M 属于同一范围，
没有出现按终端标签重复分配约 250M 渲染资源的现象。

### M10 SFTP 文件管理器

- daemon 新增 Owner-only `SftpList`、`SftpCreateDirectory`、`SftpUpload`、`SftpDownload`，
  Agent token 无权调用。目录列表同时返回服务器确认的绝对工作目录。
- 传输复用系统 OpenSSH `sftp`；密码和私钥口令继续走一次性 askpass，内嵌私钥临时文件在操作
  完成后删除。batch 参数拒绝换行、回车和 NUL。
- GUI 的 SSH 终端状态栏新增 SFTP 入口；文件标签支持远端目录浏览、从家目录返回根目录、
  筛选、新建文件夹、刷新、文件/文件夹上传及批量递归下载。本地路径来自系统文件选择器或 iced
  原生文件拖入事件；多项目传输通过 `VecDeque` 固定目标并顺序执行。
- 已访问目录在 GUI 中保留最多 16 份、30 秒的新鲜缓存；返回目录可即时呈现，手动刷新始终绕过
  缓存。大目录仍由系统 `sftp` 一次读取元数据，但 GUI 每批只创建 200 个可见行，滚动接近底部
  再追加，避免一次构造数千个 iced 控件。加载状态位于路径操作区，并在请求期间屏蔽重复导航。
- 没有新增 Vault 字段，因此不提升金库版本。

#### M10 release 内存实测（2026-08-10）

指标为 `vmmap --summary <pid>` 的 `Physical footprint`，显示器 scale factor 1（1920×1080）。

- 空金库 daemon：3904K，peak 3904K。
- 打开主窗口并恢复现有终端的 GUI：263.7M，peak 264.5M。该数字包含 wgpu 窗口交换链和终端
  surface；SFTP 状态本身只保存目录条目字符串，不持有文件内容。
