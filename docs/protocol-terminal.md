# 终端二进制推送协议（M2a-2）

daemon → 客户端的二进制帧格式，用于实时推送终端屏幕变化。

## 帧格式

```
[0x01][session_id_len: u16 BE][session_id bytes][payload]
```

| 字段 | 类型 | 说明 |
|---|---|---|
| 魔法字节 | u8 = 0x01 | 帧头标识 |
| session_id_len | u16 BE | session_id 字节长度 |
| session_id | bytes | 会话标识符 |
| payload | bytes | 见下方 |

## Payload

```
[seq: u64 BE]
[cursor_row: u16][cursor_col: u16][cursor_visible: u8]
[line_count: u16]
  每行：
    [row: u16][start_col: u16][end_col: u16]
    [run_count: u16]
      每个 run：
        [run_len: u16]
        [flags: u8]
        [fg_tag: u8][fg payload]
        [bg_tag: u8][bg payload]
        [char_len: u8][char bytes]
```

### seq

单调递增序列号。客户端可用检测丢帧（seq 不连续 = 中间有帧丢失）。

### cursor

| 字段 | 类型 | 说明 |
|---|---|---|
| cursor_row | u16 | 光标所在行（0-based） |
| cursor_col | u16 | 光标所在列 |
| cursor_visible | u8 | 0/1，光标是否显示 |

### lines

脏行列表。每行只包含 damage 报告的区间 `[start_col, end_col]`，而非整行。

### runs

RLE 编码的 cell 序列。相邻 cell 的字符/fg/bg/flags 完全相同则合并。

| 字段 | 类型 | 说明 |
|---|---|---|
| run_len | u16 | 该 run 覆盖的列数（宽字符 = 2） |
| flags | u8 | 属性标志位 |
| fg_tag | u8 | 前景色编码（见下方） |
| bg_tag | u8 | 背景色编码 |
| char_len | u8 | 字符 UTF-8 字节长度 |
| char bytes | UTF-8 | 单个字符（重复 run_len 次渲染） |

### flags

| 位 | 值 | 含义 |
|---|---|---|
| 0 | 0x01 | Bold |
| 1 | 0x02 | Italic |
| 2 | 0x04 | Underline |
| 3 | 0x08 | Reverse |
| 4 | 0x10 | Hidden |
| 5 | 0x20 | Strikeout |
| 6 | 0x40 | Wide（占两列） |

### 颜色编码

| tag | 值 | 后续字节 | 说明 |
|---|---|---|---|
| Default | 0x00 | 0 字节 | 终端默认色 |
| Indexed | 0x01 | 1 字节 (u8) | 256 色索引 |
| RGB | 0x02 | 3 字节 (r, g, b) | 24 位真彩色 |

### 宽字符处理

CJK 等宽字符占两列。编码规则：

- `flags` 置 `FLAG_WIDE`（0x40）
- `run_len` = 2（覆盖两列）
- `char bytes` 为完整 UTF-8（如 "你" = 3 字节）
- `WIDE_CHAR_SPACER` 不单独编码 — 客户端根据 `FLAG_WIDE` 自行推进两列

客户端渲染逻辑：
```
if flags & FLAG_WIDE:
    draw(char, col, col + 2)  # 占两列
    col += 2
else:
    draw(char, col, col + 1)  # 占一列
    col += 1
```

## 控制命令

| Request | 说明 |
|---|---|
| `OpenLocalSession { cols, rows }` | 打开会话（固定 $SHELL，cwd=HOME） |
| `SessionInput { session_id, data }` | 原始字节输入 |
| `ResizeSession { session_id, cols, rows }` | 调整尺寸 |
| `CloseSession { session_id }` | 关闭会话 |
| `ListSessions` | 列出所有会话 |
| `ReadScreen { session_id }` | 全量文本快照 |
| `SubscribeSession { session_id }` | 订阅推送：立即回全量，此后增量 |
| `UnsubscribeSession { session_id }` | 取消订阅 |

## 推送策略

- **帧率限制**：最高 60fps（≥16ms 间隔）
- **合并**：窗口内的多次变化合并为一次推送
- **有界通道**：容量 4，满时丢弃最旧帧、保留最新帧
- **背压**：客户端只需最终状态，中间帧无意义

## 持锁约束

推送循环在 term 锁下只做三件事：
1. 读 damage → 确定脏行区间
2. 把脏区 cell 拷贝进临时结构
3. reset_damage

然后立即释放锁。RLE 编码与二进制序列化在锁外进行。
