# vida

原生 Rust 终端 + 加密金库 + MCP 服务端

## 架构

```
vida (GUI)  ──WebSocket──▶  vida-daemon (PTY/SSH/MCP)
                              │
                              └── 金库 (vault.age, age 加密)
```

## 构建

```bash
cargo build --release
```

## 运行

```bash
# 启动 daemon
cargo run --bin vida-daemon

# 启动 GUI（另一个终端）
cargo run --bin vida
```

## 内存指标（footprint，vmmap 测量）

| 指标 | 目标 |
|---|---|
| 每个终端标签增量 | < 20 MB |
| 5 个 SSH 标签常驻 | < 250 MB (Retina) |
| 空窗口基线 | 记录数值，不设门槛 |

**测量方法**：`vmmap --summary <pid>` 取 `Physical footprint`。
**注意**：RSS 包含共享库映射，不代表实际占用，不用于验收。

### M0 基线数据（release build）

**测试环境**：macOS 26.5, 1920×1080 @1x, Rust 1.95

| 组件 | Footprint | 说明 |
|---|---|---|
| wgpu 裸空窗口 (200×100) | 24.9M | 固定开销 baseline |
| wgpu 裸空窗口 (800×600) | 27.0M | |
| wgpu 裸空窗口 (1920×1080) | 32.1M | |
| vida GUI 空窗口 (1024×768) | **33.1M** | 含 iced chrome (~6M) |
| vida daemon | **7 MB** | 无 GPU |

### M1 内存数据（含 S0-S9 GUI）

**测试环境**：macOS, vmmap --summary → Physical footprint

| 场景 | Footprint | 说明 |
|---|---|---|
| S0 空窗口（daemon 未启动） | **32.9M** | 与 M0 基线一致，GUI 代码无额外开销 |
| 解锁期间峰值 | **229.7M** | scrypt KDF 瞬时增加，解锁后回落 |

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

### 关键发现

1. **字体加载不占内存**：`__FONT_DATA` 仅 2352 bytes，0K dirty。macOS 通过 Core Text 内存映射按需加载。
2. **帧缓冲受显示限制**：IOSurface 上限 = 显示器物理分辨率，不随窗口增大无限增长。
3. **RSS ≠ 实际占用**：RSS ~90MB 包含共享库页面，footprint ~33MB 才是活动监视器显示的数字。

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
- [x] 主口令设置与解锁流程（S1 Setup + S2 Unlock）
- [x] 主机条目增删改（S4 Host Editor: 新增/编辑/删除/凭据 Option 语义）
- [x] LocalPath 同步后端（SyncBackend trait + LocalPathBackend + SyncCoordinator）
- [x] GUI 屏幕 S0-S9 全部实现
- [ ] 场景 2：首次启动创建金库
  - 启动 vida GUI → 看到 S1「创建金库」页面
  - 输入口令 → 确认口令 → 勾选风险确认 → 点击「创建金库」
  - 自动跳转到 S3 主机列表（空列表）
  - 锁定后重新解锁 → 口令正确可解锁
- [ ] 场景 3：解锁已有金库
  - 启动 vida GUI → 看到 S2「解锁金库」页面
  - 输入正确口令 → 解锁成功 → 进入 S3 主机列表
  - 输入错误口令 → 显示错误提示，留在 S2
  - 勾选「记住口令」→ 解锁后口令缓存到系统钥匙串
- [ ] 场景 4：主机编辑器
  - S3 点击「新增主机」→ 进入 S4
  - 填写名称/地址/用户名/端口/口令 → 保存 → 返回 S3 列表
  - S3 选中主机 → 点击「编辑」→ 进入 S4（预填充现有数据）
  - 修改名称 → 保存 → 返回 S3 → 确认名称已更新
  - 口令框留空 → 提示「保持原有口令不变」→ 保存后原口令不变
  - S3 选中主机 → 点击「删除」→ 确认后主机从列表消失
- [ ] 场景 5：远端更新后的同步（Downloaded 路径）
  - 设备 A 添加一台主机并同步 → 设备 B 触发同步
  - 确认 B 的列表立即显示新主机
  - 关闭 B 重开并解锁 → 确认新主机仍在（文件已写入，不只是内存）
  - 在 B 再次触发同步 → 确认返回「无变化」而不是重复下载
- [ ] 场景 6：选择远端后的界面状态（resolve_conflict_remote 路径）
  - 制造冲突 → 选择「使用远端」
  - 确认列表立即变为远端的主机集合（不是本地的旧列表）
  - 不重启，直接编辑任意一台主机并保存
  - 再次同步 → 确认远端仍是远端版本 + 那一处编辑
  - **关键断言**：本地旧数据没有被写回（否则说明内存未替换）

### GUI 屏幕清单

| 编号 | 名称 | 说明 | 状态 |
|---|---|---|---|
| S0 | 连接失败 | 守护进程不可达，显示错误 + 重试按钮 | ✅ |
| S1 | 创建金库 | 口令输入 + 确认 + 风险确认勾选框 | ✅ |
| S2 | 解锁金库 | 口令输入 + 记住口令勾选框 | ✅ |
| S3 | 主机列表 | 左侧栏主机列表 + 右侧详情 + 新增/编辑/删除/同步/设置/锁定 | ✅ |
| S4 | 主机编辑器 | 新增/编辑模式，凭据留空 = 保持原有 | ✅ |
| S5 | 设置 | 同步文件夹路径 + 回滚行数 | ✅ |
| S6 | 同步冲突 | 本地/远端版本对比，选择保留哪个 | ✅ |
| S7 | 冲突文件 | Dropbox/Syncthing 冲突文件列表，显示来源类型 | ✅ |
| S8 | 远端缺失 | 重新上传 / 清除同步状态 | ✅ |
| S9 | 导出备份 | 口令选项 + 导出按钮 | ✅ |
