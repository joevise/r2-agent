# R2 Agent

> **Small droid, big jobs.**

一个 ~5000 行 Rust 实现的极简 Agent 运行时，release 二进制仅 ~2MB，零外部服务依赖——丢到任何 Linux 上就能跑。

> 名字致敬 R2-D2：房间里最小的机器人，没有光剑，不多话，只有工具和执行力。拯救世界几十年。

## 特性

- **双 Provider**：OpenAI 兼容协议（OpenAI / 智谱 / DeepSeek 等）+ Anthropic Messages API，SSE 流式解析，429/5xx 指数退避重试
- **三级上下文缓存**：L1 工作记忆（token 预算）→ L2 压缩摘要（阈值触发）→ L3 跨会话记忆（可选编译）
- **4 个核心工具**：`read` / `write` / `edit` / `bash`，全部路径越界防护
- **三级沙箱**：off / container（rlimits + 环境变量清洗）/ strict（+ seccomp 系统调用白名单）
- **崩溃安全会话**：JSONL 追加写、逐行 flush，断电最多丢半行，恢复时自动丢弃残行
- **118 个测试**（`--features l3-memory`），含恶意输入 fuzz 与边界防御

## Quick Start

```bash
# 构建（release 二进制约 2MB）
cargo build --release

# 配置：复制示例并填入 API Key
mkdir -p ~/.r2 && cp docs/config.example.toml ~/.r2/config.toml

# 交互模式
r2

# 单发模式（问完即退）
r2 --once "读一下 config.toml 并解释每个字段"

# 恢复会话
r2 --session abc-123
```

交互模式下可用斜杠命令：`/help` `/clear`（清空上下文、开新会话）`/quit`。

## CLI 参考

| 命令 / 参数 | 说明 |
|---|---|
| `r2` | 交互模式 |
| `r2 --once <问题>` | 单发模式，回答完退出 |
| `r2 --config <路径>` | 指定配置文件（默认 `~/.r2/config.toml`，缺省用内置默认值） |
| `r2 --session <id>` | 恢复指定会话 |
| `r2 --model <名称>` | 覆盖当前 provider 的模型 |
| `r2 --work-dir <目录>` | 覆盖工作目录（工具与 bash 的根目录） |
| `r2 --list-sessions` | 列出所有会话（等同 `r2 sessions`） |
| `r2 sessions` | 列出所有会话（按最后活跃倒序） |
| `r2 sessions show <id>` | 人类可读地打印会话内容（工具参数截断 120 字符） |
| `r2 sessions export <id> [--out <文件>]` | 导出会话为 JSON |

## 配置说明

完整 `config.toml` 字段（均可缺省，有内置默认值；示例见 [docs/config.example.toml](docs/config.example.toml)）：

```toml
[model]
provider = "openai_compat"        # openai_compat | anthropic

[model.openai_compat]
base_url = "https://api.openai.com/v1"  # 兼容端点（智谱/DeepSeek 等均可）
api_key = ""                      # API 密钥
model = "gpt-4o"                  # 模型名

[model.anthropic]
base_url = "https://api.anthropic.com"
api_key = ""
model = "claude-sonnet-4-20250514"

[agent]
max_turns = 50                    # 单轮对话最大工具循环轮数
max_total_tokens = 500000         # 上下文硬上限（也是 L1 窗口大小）
work_dir = "."                    # 工作目录（支持 ~ 展开）

[context]
l1_threshold = 0.7                # L2 压缩触发阈值（占 max_total_tokens 比例）
l2_summary_model = "gpt-4o-mini"  # 摘要模型（当前版本预留，实际复用主模型）
l3_enabled = false                # 跨会话记忆（需 --features l3-memory 编译）

[sandbox]
level = "container"               # off | container | strict
bash_timeout_secs = 30            # bash 默认超时（上限 120s）
max_processes = 10                # RLIMIT_NPROC（生产建议 64+）
max_memory_mb = 512               # RLIMIT_AS
cpu_time_secs = 60                # RLIMIT_CPU
max_file_size_mb = 100            # RLIMIT_FSIZE

[session]
dir = "~/.r2/sessions"            # 会话 JSONL 存储目录（支持 ~ 展开）
```

## 架构

```
r2-agent/
├── src/
│   ├── main.rs             # CLI 入口：clap 参数、REPL、会话管理命令
│   ├── agent.rs            # 循环引擎：流式 → 解析 → 工具调用 → 再提示
│   ├── context.rs          # L1 工作记忆 + L2 压缩摘要
│   ├── memory.rs           # L3 跨会话记忆（feature: l3-memory，rusqlite）
│   ├── session.rs          # JSONL 持久化 + 崩溃恢复
│   ├── sandbox.rs          # 三级沙箱：rlimits + 环境清洗 + seccomp
│   ├── config.rs           # TOML 配置 + 校验 + ~ 展开
│   ├── types.rs            # Message / ToolCall / StreamChunk 等核心类型
│   ├── model/              # ModelProvider trait + 双 provider
│   │   ├── openai_compat.rs
│   │   └── anthropic.rs
│   └── tools/              # read / write / edit / bash + 路径防护
└── tests/                  # 集成测试 + E2E 套件
```

**三级上下文**：

- **L1 工作记忆**：token 预算内的活跃消息（`max_total_tokens` 硬上限，超限报错）
- **L2 压缩摘要**：token 超过 `l1_threshold` 比例时，旧消息（对齐工具调用组切分）被压缩成摘要，保留最近 12 条
- **L3 跨会话记忆**（可选）：256 维字符三元组哈希 embedding + 余弦检索，rusqlite 存储，跨会话召回历史结论

详细设计文档见飞书（内部）。

## 沙箱

| 级别 | 能力 |
|---|---|
| `off` | 不做任何隔离 |
| `container`（默认） | rlimits（NPROC/AS/CPU/FSIZE）+ 环境变量白名单清洗（API Key 不进子进程）+ PATH 重置 |
| `strict` | container + seccomp 系统调用白名单（~65 个，需 `--features sandbox-strict` 编译且安装 libseccomp-dev；未编译时降级为 container 并告警） |

`max_processes` 默认 10 对交互使用足够；**生产/CI 环境建议 64+**（编译器、管道等会 fork 较多进程）。

## 测试

```bash
# 默认套件（106 个测试）
cargo test

# 含 L3 记忆（118 个测试）
cargo test --features l3-memory

# E2E 套件（Python，需先构建 release）
python3 tests/e2e/l3_memory_suite.py
```

覆盖：SSE 畸形输入 fuzz（不 panic）、会话恢复极端输入（5MB 大行 / 全坏行 / BOM / CRLF）、工具参数类型错误、上下文阈值越界等。

## 路线图

| 阶段 | 交付物 | 状态 |
|---|---|---|
| P0 | 核心循环 + OpenAI 兼容 provider | ✅ |
| P0.5 | Anthropic provider | ✅ |
| P1 | 4 核心工具 + ToolRegistry | ✅ |
| P2 | 会话 JSONL + 崩溃恢复 | ✅ |
| P3 | L2 上下文压缩 | ✅ |
| P4 | 沙箱三级 | ✅ |
| P5 | L3 跨会话记忆 | ✅ |
| P6 | CLI + 配置打磨 | ✅ |
| P7 | 测试加固（fuzz + 边界） | ✅ |

**v0.2 展望**：neural embedding（替换哈希 embedding）、namespace 沙箱（真正的文件系统隔离）、MCP 工具协议接入。

## License

MIT
