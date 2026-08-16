# R2 Web Harness 开发文档 v1.0

> 项目代号：**R2 Console** —— r2 内核的极简 Web 壳
> 日期：2026-08-16 | 负责人：William | 开发模式：William 出任务书+验收，OpenCode 编码

---

## 一、项目定位

**一句话**：给 2MB 的 r2 内核配一个同哲学的 Web 外壳——黑底终端美学、零框架、单二进制。浏览器打开，直接测试/使用/观察 R2 的全部能力。

**对标**：DeepSeek Harness（`npx dsh web`，3天前发布一夜5万星）、Pi 的 pi-web-ui、Codex Web。

**差异化**：
1. Rust 单二进制（`r2 web` 子命令，+~2MB），无 Node 依赖链
2. **Steering 可视化**——运行中输入即转向（别人没有）
3. **会话分支树**——SessionTree 可视化 + 任意节点 fork（别人没有）
4. 38 模型注册表内置 + 成本实时显示

**设计哲学铁律**：内核产生能力，壳只做观察与交互。壳对内核的全部依赖 = r2-core 库形态 API + JSONL 文件 + config.toml。

---

## 二、架构

```
浏览器（单 HTML + 原生 JS，零框架零构建，include_str! 内嵌进二进制）
   │ WebSocket（JSON 消息双向流）
   ▼
r2 web 子命令（axum 极简服务，同进程）
   │ 直接调用（不走子进程）
   ▼
r2-core 库（AgentSession / ToolRegistry / Session / MemoryStore / Config）
```

**关键复用**：
- 事件流：`AgentSession.subscribe()` broadcast → 桥接 WebSocket
- Steering：浏览器输入框（运行态）→ WS 消息 → `steer_handle()` 
- 会话/分支：直接读 JSONL 文件（`list_sessions` / `recover`）
- 工具/MCP/沙箱状态：读 `ToolRegistry.schemas()` + Config 渲染

---

## 三、内核改动（唯一一块：三层 system prompt，~60行）

```
最终 system prompt = 内核核心（不可覆盖）
                   + ~/.r2/SOUL.md（全局人格，可无）
                   + {work_dir}/AGENTS.md（项目上下文，行业标准文件名，可无）
                   + config [agent] system_prompt（显式覆盖，可无）
```

- Agent::new / resume / branch_from 构造时组装（带 `[SOUL.md]`/`[AGENTS.md]` 来源标注段）
- 文件不存在静默跳过；config 新字段 `system_prompt: String = ""`（非空时取代 SOUL/AGENTS 两层）
- `Agent::effective_system_prompt()` 访问器（壳展示用）

## 四、壳功能清单（v0.1 一刀切）

### 做
| # | 功能 | 数据源 | 内核改动 |
|---|------|--------|---------|
| F1 | 对话流式（逐字）+ 完成态 | MessageUpdate 事件 | 0 |
| F2 | 工具调用折叠块（⚙/🔌 标注来源，含 MCP） | ToolCall/ToolResult 事件 | 0 |
| F3 | **Steering**：运行中输入框变 steer 模式（变色+提示） | steer_handle | 0 |
| F4 | 会话列表（侧栏）：新建/切换/删除（删=删文件） | list_sessions | 0 |
| F5 | **分支树**：列表按分支缩进渲染 + 任意会话「fork」按钮 | SessionSummary.branch_from | 0 |
| F6 | 模型切换下拉（38 注册表 + 当前高亮） | models::registry() | 0 |
| F7 | 成本/用量实时显示（顶栏） | UsageUpdate 事件 | 0 |
| F8 | TOOLS 面板：内置4工具 + MCP 已连接工具状态 | ToolRegistry | 0 |
| F9 | SANDBOX 面板：level/limits/env 白名单只读展示 | Config | 0 |
| F10 | 文件上传：拖拽 → work_dir/uploads/ + 提示词注入 | 壳层 HTTP POST | 0 |
| F11 | SKILL 观察：列 ~/.r2/skills/ 的 SKILL.md + 预览 + 「引用」按钮（插入提示词） | 壳层文件读 | 0 |
| F12 | **PROMPT 面板**：核心（只读）/ SOUL.md（编辑）/ AGENTS.md（编辑）| effective_system_prompt + 文件读写 | 0（编辑=写文件） |

### 不做（v0.1 克制清单）
多用户/登录、语音、移动端优化、主题切换（就黑底）、内置网络工具（走 MCP）、skill 索引自动注入 system prompt（下一步）、会话内消息级分支（v0.2）。

## 五、界面设计规范（黑白终端美学）

- **色板**：背景 `#0a0a0a` / 主文字 `#e8e8e8` / 次级 `#8a8a8a` / 边线 `#2a2a2a` / 强调（运行态/光标）`#ffffff` / 成功 `#9ece9e` / 错误 `#e07070`
- **字体**：`ui-monospace, "JetBrains Mono", "Cascadia Code", monospace`（全站等宽）
- **布局**：左侧栏 260px（SESSIONS / TOOLS / PROMPT / SKILL 四 tab）+ 右主区（顶栏 48px + 消息流 + 输入区）
- **细节**：1px 细边框无圆角或 2px 微圆角、无阴影无渐变、流式光标 `▌` 闪烁动画、工具块缩进+左边框线、steer 激活时输入框边框变白+显示「⌁ steer」
- 工具块格式：`⚙ read {path: "x.toml"} ▸` 点击展开结果

## 六、技术栈与新增依赖

- 服务端：axum（含 ws feature）、tokio（已有）——workspace dependencies 统一版本
- 前端：单 HTML `<style>` + `<script>`，原生 JS，**零 npm/零构建**，`include_str!` 嵌入
- 新 crate：`crates/r2-web`（依赖 r2-core，被 r2-cli 调用；或直接并入 r2-cli 新文件 web.rs——选并入，少一个 crate）

## 七、WebSocket 协议（壳↔服务）

C→S：`{"t":"prompt","input":"..."} {"t":"steer","text":"..."} {"t":"new_session"} {"t":"switch","id":"..."} {"t":"fork","parent":"...","upto":N?} {"t":"delete_session","id":"..."}` `{"t":"set_model","model":"glm-5.2"}`
S→C：`{"t":"event","evt":{AgentEvent序列化}}`（全事件转发）`{"t":"sessions","list":[...]}` `{"t":"tools","list":[...]}` `{"t":"prompt_state","sections":{core,soul,agents}}` `{"t":"model_changed","model":"..."}` `{"t":"upload_ok","path":"..."}` `{"t":"error","message":"..."}`

HTTP：`GET /`（HTML）、`POST /upload`（multipart → work_dir/uploads/）、`GET/PUT /prompt_file?name=soul|agents`（编辑）、`GET /skills`、`GET /skill_preview?name=...`

## 八、开发计划（任务拆分）

| 任务 | 内容 | 预估 | 执行 |
|------|------|------|------|
| **W1** | 内核：三层 system prompt + effective_system_prompt + config.system_prompt + 测试 | 60行 | OpenCode |
| **W2** | web.rs：axum 服务 + WS 协议 + 会话/工具/沙箱/模型 API + AgentSession 桥接（多客户端=单 Agent 锁） | ~450行 | OpenCode |
| **W3** | index.html：完整 UI（四面板+消息流+steer+上传+分支树渲染） | ~700行 | OpenCode（我出设计细节） |
| **W4** | 联调验收：冒烟清单逐项过 + 真实 GLM 跑通 | — | William |

**执行顺序**：W1 → W2 → W3 → W4 串行（W3 依赖 W2 协议）。每步 cargo test 全绿 + commit。

## 九、验收清单（W4 逐项打勾）

- [ ] `r2 web` 启动，浏览器 localhost:5290 打开黑底界面
- [ ] 对话流式逐字显示，工具调用块可折叠展开（含 MCP 工具标注 🔌）
- [ ] 运行中打字 → steer 模式 → 转向成功（散文→改诗级测试）
- [ ] 会话列表/新建/切换；fork 出分支且父不可变；树状缩进正确
- [ ] 模型下拉切换（mock 模型验证切换生效）
- [ ] 顶栏成本随 UsageUpdate 跳动
- [ ] TOOLS 面板显示 4 内置 + MCP server（若配）；SANDBOX 面板参数正确
- [ ] 拖文件上传 → uploads/ 落盘 → 提示词注入 → 模型 read 成功
- [ ] SKILL 面板列目录/预览/引用插入
- [ ] PROMPT 面板：核心只读、SOUL/AGENTS 可编辑保存、新会话生效
- [ ] 断线重连（刷新页面会话状态恢复）
- [ ] 全程无 panic；cargo test 全绿；release 二进制增量 ≤ 2.5MB

## 十、风险与对策

| 风险 | 对策 |
|------|------|
| 多浏览器标签并发操作同一 Agent | Mutex<AgentSession>，忙时其他请求排队（WS 广播事件共享） |
| 上传大文件撑爆内存 | 限 10MB + 只存文本类（v0.1） |
| axum 拉大依赖 | 只开 ws + multipart feature；若超预算换 tiny_http 手写 WS（保底方案） |
| 前端复杂度失控 | 严守 700 行；所有状态（当前会话/模型/运行态）单 store 对象 |

---

*文档完毕。审批通过后按 W1 开工。*
