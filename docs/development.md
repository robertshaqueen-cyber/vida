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
