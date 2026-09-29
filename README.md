# R2 Agent

> **Small droid, big jobs.**

一个 Rust 实现的轻量 Agent 运行时：引擎极简（零平台代码）、分身独居一亩三分地、飞书原生渠道、沙箱五层防护。release 二进制约 2MB，零外部服务依赖——Linux / macOS 丢上就能跑。

> 名字致敬 R2-D2：房间里最小的机器人，没有光剑，不多话，只有工具和执行力。拯救世界几十年。

## 特性

- **多 Provider × 多模型档案**：OpenAI 兼容 + Anthropic Messages API，SSE 流式（字节级 UTF-8 安全），429/5xx 指数退避；`[[model.profiles]]` 跨 provider 配多套模型（如 glm + kimi），飞书/Console 一键切换
- **飞书原生渠道**：每分身一个飞书自建应用，WebSocket 长连接免公网回调；CardKit 流式卡片；斜杠命令 `/model` `/new` `/status` `/help`；**在途消息 = steer 转向**（打断当前流注入新指令，不丢消息）；文件/图片/富文本（post）全接收；DM 会话跨重启续接
- **多分身**：每个 agent 独居 `~/.r2/agents/<name>/`（人格 / MCP / 记忆 / 会话 / 工作目录物理隔离），群聊多分身协作 MVP
- **三级上下文 + 三层记忆**：L1 工作记忆 → L2 压缩摘要 → MEMORY.md 策展记忆注入 system prompt + `history` 全文检索工具（零 embedding 依赖）
- **五层沙箱**：rlimits / cgroup v2（pids + memory.max RSS 护栏）/ 环境清洗 / namespace 假根隔离（userns 双 fork + mount + pid + 断网）/ seccomp 白名单；**Linux 全功能，macOS 自动降级**（rlimit + 超时组杀 + 环境清洗）
- **MCP 一亩三分地**：作为 MCP host 连外部 server，动态注册工具；每分身独立 `MCP.toml` + env 注入，能力扩展不进引擎核心
- **崩溃安全会话**：JSONL 追加写、逐行 flush，断电最多丢半行
- **R2 Console**：单文件内嵌 Web UI（黑白终端美学），流式对话 / steer / 会话分支树 / 三层 system prompt 编辑 / 模型切换 / 沙箱面板 / 文件上传 / 成本显示
- **265 个测试**：含 SSE 畸形输入 fuzz、组杀身份验证、namespace 边界、UTF-8 截断回归

## Quick Start

```bash
# 方式一：下载预编译（GitHub Releases，tag 触发自动构建）
#   r2-linux-x86_64 / r2-linux-aarch64 / r2-macos-x86_64 / r2-macos-aarch64
curl -LO https://github.com/joevise/r2-agent/releases/latest/download/r2-linux-x86_64.tar.gz

# 方式二：源码构建（~2MB 二进制）
cargo build --release

# 配置：复制示例并填入 API Key
mkdir -p ~/.r2 && cp docs/config.example.toml ~/.r2/config.toml

r2          # 交互模式
r2 --once "读一下 config.toml 并解释每个字段"   # 单发
r2 web      # Console（http://127.0.0.1:5290）
```

### 平台支持

| 能力 | Linux | macOS | Windows |
|---|---|---|---|
| 核心引擎 / 工具 / 会话 / Console | ✅ | ✅ | 规划中 |
| 飞书渠道 / MCP / 多分身 | ✅ | ✅ | 规划中 |
| namespace 假根隔离（strict） | ✅ | 自动降级 | — |
| seccomp 白名单 | ✅（feature） | 自动降级 | — |
| cgroup 资源护栏 | ✅ | 自动降级 | — |
| rlimit / 墙钟超时组杀 / 环境清洗 | ✅ | ✅ | — |

## CLI 参考

| 命令 | 说明 |
|---|---|
| `r2` | 交互模式（`/help` `/clear` `/quit`） |
| `r2 --once <问题>` | 单发模式 |
| `r2 --model <名称>` | 覆盖当前模型 |
| `r2 --session <id>` / `r2 sessions [show/export <id>]` | 会话恢复 / 列表 / 查看 / 导出 |
| `r2 web [--host 0.0.0.0] [--port 5290]` | 启动 Console Web UI |
| `r2 sandbox run <命令>` | 在沙箱会话中执行（每会话独立 cgroup + 假根） |

## 配置说明

完整字段见 [docs/config.example.toml](docs/config.example.toml)，均可缺省。核心段：

```toml
[model]
provider = "openai_compat"          # openai_compat | anthropic

# 多模型档案：跨 provider 配多套，active 持久化，Console/飞书可切换
[[model.profiles]]
name = "glm"
provider = "openai_compat"
model = "glm-5.2"

[[model.profiles]]
name = "kimi"
model = "kimi-k3"

[agent]
max_turns = 50
max_total_tokens = 500000

[sandbox]
level = "container"       # off | container | strict
cgroup = true             # cgroup v2 pids 硬限（不可用自动降级）
cgroup_memory_mb = 0      # RSS 物理内存护栏（0=不限；不误伤 JIT 的 VA 预留）
max_processes = 0         # RLIMIT_NPROC；0=不限（桌面共享 uid 勿设小值）
bash_timeout_secs = 30

[feishu]                  # 飞书渠道（每分身一个自建应用）
app_id = ""
app_secret = ""
```

## 飞书渠道

每个分身绑定一个飞书自建应用（WebSocket 长连接，无需公网回调 / HTTPS / Nginx）：

- **私聊**：直接和分身对话，CardKit 流式卡片实时渲染
- **斜杠命令**：`/model`（切模型档案，会话原地重建保历史）/ `/new`（新会话）/ `/status` / `/help`
- **steer 插话**：生成过程中发消息 = 中途转向，注入当前流；收尾窗口残留自动补投，绝不静默丢弃
- **文件 / 图片 / post 富文本**：全部接收——文件落 `work/uploads/`，富文本自动拍平为纯文本进对话
- **会话续接**：DM 会话 ID 落盘（`dm/<open_id>.sid`），R2 重启后继续原会话

## 多分身与 MCP

```
~/.r2/agents/<name>/
├── AGENT.toml        # 分身配置（人格参数、模型档案）
├── MCP.toml          # 该分身专属的 MCP server 列表（含 env 注入）
├── SOUL.md           # 人格
├── work/             # 工作目录（MEMORY.md 记忆 / uploads/ 上传文件）
├── dm/               # 飞书 DM 会话指针
└── sessions/         # 会话 JSONL
```

MCP 工具以 `mcp_{server}_{tool}` 注册，单 server 失败只告警不阻塞；agent 退出自动回收子进程。**引擎核心零平台代码**——能力扩展一律外挂（MCP / skill 装在分身自己目录）。

## 架构

```
r2-agent/
├── crates/
│   ├── r2-core/               # 引擎（零平台代码）
│   │   └── src/
│   │       ├── agent.rs       # 循环引擎：流式 → 解析 → 工具 → 再提示
│   │       ├── channels.rs    # 飞书渠道：WS 长连接 / CardKit / steer / 文件接收
│   │       ├── namespaces.rs  # strict 沙箱：userns 双 fork + mount 假根 + 断网
│   │       ├── sandbox.rs     # rlimits / cgroup / 环境清洗 / seccomp
│   │       ├── mcp.rs         # MCP host：stdio + 动态工具注册
│   │       ├── groups.rs      # 群聊多分身协作
│   │       ├── evolution.rs / rpc.rs / config.rs / agents.rs
│   │       └── tools/         # read/write/edit/bash/task/history/mcp_admin
│   └── r2-cli/                # CLI + Console Web（web_ui.html 单文件内嵌）
└── .github/workflows/release.yml   # tag → 四平台产物自动构建
```

**三级上下文**：L1 工作记忆（token 硬上限）→ L2 压缩摘要（阈值触发，对齐工具组切分）→ MEMORY.md 策展注入 + `history` 工具全文检索。

## 沙箱

| 层 | 机制 | 级别 |
|---|---|---|
| rlimits | NPROC / AS / CPU / FSIZE（默认全 0 = 不设，避免误伤 JIT/桌面） | container 起可用 |
| cgroup v2 | `pids.max` 防 fork 炸弹 + `memory.max` RSS 护栏 | container 起可用 |
| 环境清洗 | API Key 不进子进程 + PATH 重置 | container 起 |
| namespace | mount 假根（chroot）/ pid ns / 断网，userns 双 fork 免 root | strict |
| seccomp | ~65 syscall 白名单（feature `sandbox-strict`） | strict |

降级链全程不 panic：strict 无 seccomp 编译 → container；cgroup 只读 → rlimits；**macOS → rlimit + 超时组杀 + 环境清洗**（namespace/cgroup/seccomp 为 Linux 内核机制，mac 自动降级并告警）。

超时组杀带 **PID 身份验证**（pgrp + starttime 双重核对，防 PID 复用误杀）。

## 测试

```bash
cargo test --workspace        # core 213 + cli 32 等共 265 个测试
```

覆盖：SSE 畸形输入 fuzz、会话恢复极端输入（5MB 大行 / 全坏行 / BOM / CRLF）、UTF-8 多字节截断回归、组杀身份验证、工具参数类型防御、沙箱降级路径。

## 版本纪要

| 版本 | 交付 |
|---|---|
| v0.5 | namespace 沙箱 strict 档（双 fork 假根 + 断网） |
| v0.8 | 桌面安全加固（组杀身份验证）；多分身；群聊 MVP；KV-cache |
| v0.9 | AppArmor userns 适配；沙箱自孵化会话 |
| v0.10 | 飞书渠道（斜杠命令 / steer / 沙箱资源面板 / DM 跨重启续接） |
| v0.11 | 三层记忆（MEMORY.md + history 工具）；文件/图片/post 接收；UTF-8 字节级修复 |
| v0.12 | **macOS 原生支持**（沙箱平台分叉）+ GitHub Actions 四产物发版流水线 |

**路线**：Windows 适配（bash→PowerShell、信号模型）、vision 多模态档案、群聊打磨。

## License

MIT
