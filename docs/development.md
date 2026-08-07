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
- [x] Keyring 仅缓存，口令解锁独立可用
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
4. 勾选「记住口令」解锁 → 口令缓存到系统钥匙串（Keychain）

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
> - daemon 重启后所有旧 session_id 全部失效（会话属于 daemon，
>   不属于连接）。每次重启后需重新 open 获取新 session_id。
> - open / send / screen 等均为**一次性命令**，执行完即退出。
>   会话由 daemon 持有，用 open 返回的 session_id 在后续命令中引用。
> 每个 cargo run --release -p vida-term-test -- 子命令是独立进程，连接后即断，但会话保留在 daemon。

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

- 默认字体与密度改为本机 Ghostty 默认值：Menlo 13pt。设置值按点数保存，传入
  cosmic-text 前以 `96/72` 换算为 px；scale=1 实测 cell **10×18**、ascent **15**，
  `A` 位图 10×12、中文位图 17×17。前景 `#ffffff`、终端背景 `#282c34`。
- 增加 500ms 闪烁的实心方块光标；timer 仅在终端屏且用户启用闪烁时存在。
- WIDE 字符仍推进两个 cell，但按原始字形宽高比绘制，不再横向拉伸中文；
  低 DPI 灰度字形覆盖补偿保留。
- 「设置 → 终端」从系统已安装的等宽字体下拉选择字体，并从固定列表选择字号；
  可持久化字体、字号与光标闪烁，保存后新开的终端生效。
- 普通宽字符使用 Medium 字重，消除 zsh bold 输入正常、Regular 输出偏瘦的差异；
  字形仍按原始宽高比绘制，不做单轴拉伸。
- M2b-2 调试终端仍是临时 Screen；点击真实标签或设置时会先关闭调试会话并恢复
  Main，再执行标签切换，避免 active tab 已变化但终端仍覆盖内容。
- 自动 GUI 首轮已看到提示符处方块光标；最终字体视觉观感仍以所有者截图验收为准。
