# vida

**给 AI Agent 和开发者用的终端管理器：加密金库 + SSH 终端 + MCP 服务端。**

vida 解决两个问题：

1. **主机凭据安全**：所有 SSH 主机、口令、私钥存进一个 age 加密的金库（`vault.age`），
   标准 `age` CLI 即可解密，支持本地文件夹同步（Dropbox/Syncthing 友好）。
2. **让 AI Agent 直接操作你的主机**：内置 MCP 服务端，AI 编码助手
   （Claude Code、Cursor 等）可以通过 MCP 协议读写金库、连接主机，
   无需把凭据喂给任何第三方。

## 截图

![vida 创建金库](docs/screenshot_s1.png)

## 特性

- 🔐 **age 加密金库**：scrypt KDF（log_n=18），可用标准 `age` CLI 解密验证
- 🗝️ **系统钥匙串集成**：可选记住主口令（Keychain）
- 🔄 **本地路径同步**：Dropbox / Syncthing / iCloud 文件夹同步，带冲突检测
- 🖥️ **Tabby 风格界面**：标签页 + 快速连接面板 + 搜索
- 🌐 **多语言**：中文 / English，跟随系统语言
- 🤖 **MCP 服务端**：AI Agent 通过 stdio↔WebSocket 桥接访问金库与主机
- 🧹 **零泄漏**：敏感字段 zeroize、原子写入 + fsync、日志永不含凭据

## 安全说明（重要）

**项目处于早期开发阶段，未经第三方安全审计。不建议存放生产环境关键凭证。**

### 威胁模型

**保护什么：**
- 文件被拷贝走：无主口令无法解密金库内容
- 云盘服务商：同步文件夹中的密文不可读
- 口令缓存：仅显式勾选时写入系统钥匙串

**不保护什么：**
- 主机名、地址、端口为明文（为可搜索性和 MCP 工具可用性）
- 已解锁状态：金库内容驻留内存，具备本机权限的进程理论可读取
- 主口令丢失：数据永久丢失，无后门、无恢复通道
- 本机恶意软件：攻击者获得你的用户级权限后，可读取解锁后的内存

详细说明见 [SECURITY.md](SECURITY.md)。漏洞上报请走
[GitHub Security Advisory](https://github.com/robertshaqueen-cyber/vida/security/advisories/new)，
不要使用公开 Issue。

## 架构

```
vida (GUI)  ──WebSocket──▶  vida-daemon (PTY/SSH/MCP)
                              │
                              └── 金库 (vault.age, age 加密)

AI Agent ──MCP stdio──▶ vida-mcp-cli ──WebSocket──▶ vida-daemon
```

- **vida-core** — 金库、加密、同步、多语言（纯逻辑库）
- **vida-daemon** — 守护进程：WebSocket 服务端、协议路由、状态管理
- **vida-gui** — iced + wgpu 原生界面
- **vida-mcp-cli** — stdio→WebSocket 桥接（给只支持 stdio 的 MCP 客户端）

## 构建与运行

```bash
# 构建
cargo build --release

# 启动 daemon（第一个终端）
cargo run --bin vida-daemon

# 启动 GUI（第二个终端）
cargo run --bin vida

# 运行测试
cargo test --workspace
```

## 开发文档

- [内存指标与验收清单](docs/development.md)
- [GUI 屏幕清单](docs/screens.md)
- [技术决策](docs/decisions.md)

## License

[MIT](LICENSE)
