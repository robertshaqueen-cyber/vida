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
- [ ] 场景 2：首次启动创建金库
- [ ] 场景 3：解锁已有金库
- [ ] 场景 4：主机编辑器
- [ ] 场景 5：远端更新后的同步（Downloaded 路径）
- [ ] 场景 6：选择远端后的界面状态（resolve_conflict_remote 路径）

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
