//! r2 web 子命令：R2 Console 服务层（axum + WebSocket）
//!
//! 架构：浏览器 ↔ WS/HTTP ↔ 本模块 ↔ r2-core 库（同进程直调，不走子进程）。
//! 多客户端共享单 Agent：AgentSession 全局一把锁，prompt 在途时整锁持有，
//! 在途时新输入自动排队（收尾完成续发）；事件经 broadcast 扇出给所有 WS 客户端。

use axum::{
    extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
    extract::{DefaultBodyLimit, Multipart, Query, State},
    http::StatusCode,
    response::{Html, Json},
    routing::{get, post},
    Router,
};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use r2_core::agents::{self, ChannelFeishu, MAIN};
use r2_core::channels::{ChannelStatus, FeishuClient, FeishuConfig, FeishuDm};
use r2_core::config::{self, Config};
use r2_core::groups::{self, GroupEvent};
use r2_core::session::{self, SessionSummary};
use r2_core::tools::ToolRegistry;
use r2_core::{rpc, AgentEvent, AgentSession};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use toml;
use tokio::sync::{broadcast, mpsc, Mutex};

/// 上传大小上限：10MB
const MAX_UPLOAD: usize = 10 * 1024 * 1024;
/// 事件广播容量（所有 WS 客户端共享一条通道）
const EVENT_CAPACITY: usize = 512;

/// Web 服务共享状态
struct WebState {
    /// 启动配置（std Mutex：只短持锁取副本。模型切换 = 改副本 + 用新配置重建会话）
    config: StdMutex<Config>,
    /// 当前会话（None = 未创建；prompt 运行期间锁被任务持有）
    agent: Mutex<Option<AgentSession>>,
    /// 当前会话的 steer 发送端缓存：prompt 持锁期间 steering 仍能直达（不经过 agent 锁）
    steer_tx: StdMutex<Option<mpsc::Sender<String>>>,
    /// 排队的下一条 prompt（在途时新输入自动排队，收尾完成后续发；最多 1 条，新的覆盖旧的）
    pending_prompt: StdMutex<Option<String>>,
    /// 事件广播：AgentEvent → JSON 扇出给全部 WS 客户端
    event_tx: broadcast::Sender<Value>,
    /// 会话 JSONL 目录（~ 已展开）
    session_dir: String,
    /// 工作目录（~ 已展开），uploads/ 与 AGENTS.md 都基于它
    work_dir: String,
    /// 工具清单快照（启动时与 Agent 同源构造一次，含 MCP；避免每次请求重连 MCP server）
    tools: Vec<Value>,
    /// 当前选中的 agent（"main" = 主 agent；其余为 ~/.r2/agents/<name> 分身）
    current_agent: StdMutex<String>,
    /// 群调度任务表：群 sid → (世代号, JoinHandle)；世代号防自然退出清理误删新任务
    group_running: StdMutex<HashMap<String, (u64, tokio::task::JoinHandle<()>)>>,
    /// 群任务世代号自增器
    group_seq: StdMutex<u64>,
    /// 运行中的飞书通道：agent 名 → 运行时（只短持锁做增删/查状态）
    channels: StdMutex<HashMap<String, FeishuRuntime>>,
    /// 飞书 DM 会话：key = "agent|open_id" → 会话槽。
    /// 与 state.agent 主槽位完全隔离：main console 会话不受任何影响。
    dm_sessions: StdMutex<HashMap<String, Arc<DmSession>>>,
}

/// 当前 agent 名
fn current_agent_name(state: &WebState) -> String {
    state.current_agent.lock().expect("agent 锁中毒").clone()
}

/// 当前 agent 的会话目录：main = 启动时的 session_dir；分身 = {profile_dir}/sessions
fn session_dir_for(state: &WebState) -> String {
    let name = current_agent_name(state);
    if name == MAIN {
        state.session_dir.clone()
    } else {
        agents::profile_dir(&name)
            .join("sessions")
            .to_string_lossy()
            .to_string()
    }
}

/// 分身配置叠加：persona_dir/work_dir/session 目录指向分身目录，模型按档案覆盖。
/// main 不改动（纯函数，便于测试；目录不存在则顺带创建）。
fn apply_persona(name: &str, cfg: &mut Config) {
    if name == MAIN {
        return;
    }
    let dir = agents::profile_dir(name);
    let sessions = dir.join("sessions");
    let work = dir.join("work");
    let _ = std::fs::create_dir_all(&sessions);
    let _ = std::fs::create_dir_all(&work);
    cfg.agent.persona_dir = Some(dir.to_string_lossy().to_string());
    cfg.agent.work_dir = work.to_string_lossy().to_string();
    cfg.session.dir = sessions.to_string_lossy().to_string();
    if let Some(p) = agents::load_profile(name) {
        if !p.model.is_empty() {
            match cfg.model.provider.as_str() {
                "anthropic" => cfg.model.anthropic.model = p.model.clone(),
                _ => cfg.model.openai_compat.model = p.model.clone(),
            }
        }
    }
    // 分身自己的 MCP.toml 叠加（一亩三分地读侧：agent 用 mcp 工具装的 server
    // 写在自己目录，这里 upsert 进会话配置；同名覆盖全局，幂等可重复调用）
    let mcp_toml = dir.join("MCP.toml");
    if let Ok(content) = std::fs::read_to_string(&mcp_toml) {
        if let Ok(v) = toml::from_str::<toml::Value>(&content) {
            let servers: Vec<config::McpServerConfig> = v
                .get("mcp")
                .and_then(|m| m.get("servers"))
                .and_then(|s| s.clone().try_into().ok())
                .unwrap_or_default();
            for s in servers {
                cfg.mcp.servers.retain(|x| x.name != s.name);
                cfg.mcp.servers.push(s);
            }
        }
    }
}

/// 当前生效配置副本（含当前 agent 的分身叠加）
fn config_snapshot(state: &WebState) -> Config {
    let mut cfg = state.config.lock().expect("config 锁中毒").clone();
    apply_persona(&current_agent_name(state), &mut cfg);
    cfg
}

/// 新会话专用配置快照：从源文件刷新 mcp 段（agent 可用 mcp 工具装 server，
/// 写盘后新建会话即连接），其余段保留运行时状态（模型切换不被文件覆盖）。
fn config_snapshot_fresh_mcp(state: &WebState) -> Config {
    let mut cfg = config_snapshot(state);
    if let Some(p) = cfg.source_path.clone() {
        if let Ok(fresh) = Config::load_from_file(&p) {
            cfg.mcp = fresh.mcp; // 全局原文（此时不含分身条目）
        }
    }
    // 全局刷新会抹掉 apply_persona 的分身合并，重放一次（幂等）
    apply_persona(&current_agent_name(state), &mut cfg);
    cfg
}

/// 向单个 WS 客户端发消息
async fn ws_send(sink: &WsSink, v: Value) {
    let mut s = sink.lock().await;
    let _ = s.send(WsMessage::Text(v.to_string())).await;
}

/// 向单个 WS 客户端发错误
async fn ws_error(sink: &WsSink, message: &str) {
    ws_send(sink, json!({"t": "error", "message": message})).await;
}

/// 广播错误给全部客户端
fn broadcast_error(state: &WebState, message: &str) {
    let _ = state
        .event_tx
        .send(json!({"t": "error", "message": message}));
}

/// 广播会话列表刷新
fn broadcast_sessions(state: &WebState) {
    let _ = state.event_tx.send(json!({
        "t": "sessions",
        "list": sessions_with_groups(&session_dir_for(state)),
    }));
}

/// 执行一条用户 prompt（Prompt 处理器主体；收尾后被排队输入复用）。
/// 在途（锁被持有=上一轮还在收尾）→ 排队最多 1 条（新的覆盖旧的），收尾完自动续发。
/// 8/23 修复：此前直接拒绝（"prompt in flight"），而 Done 后反思/技能钩子
/// 仍在锁内跑 LLM，用户回复完发消息吃闭门羹。
async fn run_prompt(st: Arc<WebState>, input: String) {
    let mut guard = match st.agent.try_lock() {
        Ok(g) => g,
        Err(_) => {
            *st.pending_prompt.lock().expect("排队锁中毒") = Some(input);
            let _ = st.event_tx.send(json!({"t": "event", "evt": {
                "type": "queued_prompt",
                "message": "⏳ 上一条回复还在收尾，本条已排队，完成后自动发送",
            }}));
            return;
        }
    };
    if guard.is_none() {
        // 懒建会话预热反馈：MCP 子进程冷启动（npx 实测 ~9s）期间前端原本完全沉默，
        // 用户以为“没反应”——先推一条 notice，灯亮者有据可依
        let _ = st.event_tx.send(json!({"t": "event", "evt": {
            "type": "notice",
            "data": {"text": "正在创建会话（工具预热，首次约 10 秒）…"},
        }}));
        let config = config_snapshot_fresh_mcp(&st);
        match AgentSession::new(config) {
            Ok(s) => install_session(&st, &mut guard, s),
            Err(e) => {
                broadcast_error(&st, &format!("创建会话失败：{e}"));
                return;
            }
        }
    }
    let session = guard.as_mut().expect("刚确保过会话存在");
    let result = session.prompt(&input).await;
    let sid = session.session_id().map(String::from);
    drop(guard);
    match result {
        Ok(text) => {
            let _ = st.event_tx.send(
                json!({"t": "prompt_done", "final_text": text, "session_id": sid}),
            );
        }
        Err(e) => broadcast_error(&st, &e),
    }
    broadcast_sessions(&st);
    // 收尾完成：取出排队输入自动发起下一轮。
    // 若此刻锁已被并发的新 prompt 抢占 → 该输入会重新排队，自愈无重发。
    // 注：不能直接 spawn(run_prompt(..)) —— 递归 async fn 的 Send 推不出来，
    // 经 spawn_prompt 包装间接递归即可。
    let queued = st.pending_prompt.lock().expect("排队锁中毒").take();
    if let Some(next) = queued {
        spawn_prompt(st.clone(), next);
    }
}

/// run_prompt 的 spawn 包装（打破递归 async fn 的 Send 推断环）
fn spawn_prompt(st: Arc<WebState>, input: String) {
    tokio::spawn(run_prompt(st, input));
}

/// 广播完整状态快照（agent 审批/拒绝后推给全部客户端）
fn broadcast_state(state: &WebState) {
    let mut v = state_json(state);
    v["t"] = json!("state");
    let _ = state.event_tx.send(v);
}

/// 安装新会话：接事件转发 + 缓存 steer 端 + 放入槽位
fn install_session(state: &WebState, slot: &mut Option<AgentSession>, session: AgentSession) {
    spawn_event_forward(&session, &state.event_tx);
    *state.steer_tx.lock().expect("steer 锁中毒") = Some(session.steer_handle());
    *slot = Some(session);
}

/// 事件转发任务：AgentSession 事件流 → event_tx（随会话生命周期存活，会话丢弃后自动退出）
fn spawn_event_forward(session: &AgentSession, event_tx: &broadcast::Sender<Value>) {
    let mut rx = session.subscribe();
    let tx = event_tx.clone();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(evt) => {
                    let _ = tx.send(json!({"t": "event", "evt": rpc::event_json(&evt)}));
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    let _ = tx.send(
                        json!({"t": "error", "message": format!("事件通道滞后，丢弃 {n} 条")}),
                    );
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// 会话摘要 → JSON
fn summary_json(s: &SessionSummary) -> Value {
    json!({
        "id": s.id,
        "message_count": s.message_count,
        "preview": s.first_user_preview,
        "last_ts": s.last_ts,
        "branch_from": s.branch_from,
        "branch_upto": s.branch_upto,
    })
}

/// 组装完整状态快照（GET /api/state 与 WS init 共用）

/// 会话历史 → 前端回放 JSON（Switch/Fork/恢复时随 sessions 一起发）
/// 工具调用还原成 toolblk 结构（thead=名称+参数摘要 / tbody=结果），与实时流一致
fn session_history_json(s: &AgentSession) -> Value {
    use r2_core::types::Role;
    let items: Vec<Value> = s
        .messages()
        .iter()
        .filter(|m| m.role != Role::System)
        .map(|m| match m.role {
            Role::User => json!({"kind": "user", "text": m.content}),
            Role::Assistant => json!({
                "kind": "assistant",
                "text": m.content,
                "tool_calls": m.tool_calls.clone().unwrap_or_default().iter().map(|c| json!({
                    "id": c.id, "name": c.name, "arguments": c.arguments,
                })).collect::<Vec<_>>(),
            }),
            Role::Tool => json!({
                "kind": "tool_result",
                "call_id": m.tool_call_id.clone().unwrap_or_default(),
                "text": m.content,
            }),
            Role::System => unreachable!(),
        })
        .collect();
    json!({"messages": items})
}

fn state_json(state: &WebState) -> Value {
    let cfg = config_snapshot(state);
    let sessions = session::list_sessions(&session_dir_for(state)).unwrap_or_default();
    // prompt 运行期间 agent 锁被持有：try_lock 失败即 running
    let (running, current) = match state.agent.try_lock() {
        Ok(g) => (
            false,
            g.as_ref().and_then(|s| s.session_id().map(String::from)),
        ),
        Err(_) => (true, None),
    };
    let (_full, sections) = r2_core::agent::build_system_prompt(&cfg);
    // 群会话条目与普通会话合并（kind:"group" 供前端区分渲染）+ 按最近活动混排（新→旧）
    let mut session_list: Vec<Value> = sessions.iter().map(summary_json).collect();
    session_list.extend(group_entries(&session_dir_for(state)));
    session_list.sort_by_key(|v| {
        std::cmp::Reverse(v.get("last_ts").and_then(|x| x.as_u64()).unwrap_or(0))
    });
    // 当前会话的历史（浏览器刷新/重连时回放画面；prompt 运行中则跳过）
    let history = match state.agent.try_lock() {
        Ok(g) => g.as_ref().map(session_history_json),
        Err(_) => None,
    };
    json!({
        "model": cfg.current_model(),
        // 可切换模型档案（下拉/命令用；只给名字与模型名，绝不带 key）
        "model_profiles": cfg.model.profiles.iter()
            .map(|p| json!({"name": p.name, "model": p.model}))
            .collect::<Vec<_>>(),
        "active_profile": cfg.model.active_profile,
        "agents": agents::list_profiles().iter().map(agents::profile_json).collect::<Vec<_>>(),
        "current_agent": current_agent_name(state),
        "channels": channels_json(state),
        "tasks": r2_core::tasks::load_store().tasks,
        "history": history,
        "current_session": current,
        "sessions": session_list,
        "tools": state.tools,
        "sandbox": {
            "level": cfg.sandbox.level,
            "bash_timeout_secs": cfg.sandbox.bash_timeout_secs,
            "max_processes": cfg.sandbox.max_processes,
            "max_memory_mb": cfg.sandbox.max_memory_mb,
            "cpu_time_secs": cfg.sandbox.cpu_time_secs,
            "max_file_size_mb": cfg.sandbox.max_file_size_mb,
            "cgroup_memory_mb": cfg.sandbox.cgroup_memory_mb,
        },
        "prompt_sections": {
            "core": sections.core,
            "soul": sections.soul,
            "agents": sections.agents,
            "skills": sections.skills,
            "custom": sections.custom,
        },
        "running": running,
    })
}

/// 启动时构造工具清单快照（与 Agent 同源的注册表，含 MCP 连接）
fn build_tool_list(config: &Config) -> Vec<Value> {
    let work_dir = config::expand_tilde(&config.agent.work_dir);
    let Ok(mut registry) = ToolRegistry::new_default(&work_dir, &config.sandbox, config.mcp_write_path().as_deref()) else {
        return Vec::new();
    };
    registry.connect_mcp(&config.mcp);
    registry
        .schemas()
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "desc": s.description,
                "mcp": s.name.starts_with("mcp_"),
            })
        })
        .collect()
}

// ---------- HTTP 路由 ----------

/// GET /：内嵌的单页 UI。
/// no-cache：HTML 编译期内嵌，每次发版后浏览器若拿旧缓存 JS 会静默丢新事件类型
/// （8/25 实锤：旧页面无 gActRender，3593 条过程事件全部丢弃）——必须每次回源验证
async fn index() -> impl axum::response::IntoResponse {
    (
        [("Cache-Control", "no-cache")],
        Html(include_str!("web_ui.html")),
    )
}

/// 上传扩展名白名单（只收文本类，v0.1 克制清单）
fn is_allowed_upload(filename: &str) -> bool {
    const ALLOWED: &[&str] = &[
        "txt", "md", "rs", "py", "js", "ts", "json", "csv", "toml", "yaml", "yml", "log",
    ];
    Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| ALLOWED.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// POST /upload：multipart 文件 → {work_dir}/uploads/{时间戳}_{原名}
async fn upload(State(state): State<Arc<WebState>>, mut multipart: Multipart) -> (StatusCode, Json<Value>) {
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return (StatusCode::PAYLOAD_TOO_LARGE, Json(json!({"error": e.to_string()}))),
        };
        let Some(raw_name) = field.file_name().map(String::from) else {
            continue; // 跳过非文件字段
        };
        // 只取文件名部分，防路径穿越
        let file_name = Path::new(&raw_name)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if file_name.is_empty() || !is_allowed_upload(&file_name) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("不允许的文件类型：{file_name}")})),
            );
        }
        let data = match field.bytes().await {
            Ok(d) => d,
            Err(e) => return (StatusCode::PAYLOAD_TOO_LARGE, Json(json!({"error": e.to_string()}))),
        };
        if data.len() > MAX_UPLOAD {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({"error": "文件超过 10MB 上限"})),
            );
        }
        let dir = Path::new(&state.work_dir).join("uploads");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("创建 uploads 目录失败：{e}")})),
            );
        }
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let stored = format!("{ts}_{file_name}");
        if let Err(e) = std::fs::write(dir.join(&stored), &data) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("写入失败：{e}")})),
            );
        }
        return (StatusCode::OK, Json(json!({"path": format!("uploads/{stored}")})));
    }
    (StatusCode::BAD_REQUEST, Json(json!({"error": "请求中没有文件字段"})))
}

/// prompt_file 路径解析：soul = ~/.r2/SOUL.md，agents = {work_dir}/AGENTS.md
fn prompt_file_path(state: &WebState, name: &str) -> Option<PathBuf> {
    match name {
        "soul" => Some(PathBuf::from(config::expand_tilde("~/.r2/SOUL.md"))),
        "agents" => Some(Path::new(&state.work_dir).join("AGENTS.md")),
        _ => None,
    }
}

/// GET /prompt_file?name=soul|agents：读文件内容（不存在返回空串 + exists:false）
async fn get_prompt_file(
    State(state): State<Arc<WebState>>,
    Query(q): Query<HashMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    let Some(path) = prompt_file_path(&state, q.get("name").map(String::as_str).unwrap_or(""))
    else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "name 必须是 soul 或 agents"})));
    };
    match std::fs::read_to_string(&path) {
        Ok(content) => (StatusCode::OK, Json(json!({"exists": true, "content": content}))),
        Err(_) => (StatusCode::OK, Json(json!({"exists": false, "content": ""}))),
    }
}

/// PUT /prompt_file?name=...：写文件（body 为纯文本；父目录不存在则创建）
async fn put_prompt_file(
    State(state): State<Arc<WebState>>,
    Query(q): Query<HashMap<String, String>>,
    body: String,
) -> (StatusCode, Json<Value>) {
    let Some(path) = prompt_file_path(&state, q.get("name").map(String::as_str).unwrap_or(""))
    else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "name 必须是 soul 或 agents"})));
    };
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("创建目录失败：{e}")})),
            );
        }
    }
    match std::fs::write(&path, body.as_bytes()) {
        Ok(()) => (StatusCode::OK, Json(json!({"exists": true}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("写入失败：{e}")})),
        ),
    }
}

/// GET /skills：列 ~/.r2/skills/*\/SKILL.md 的名称 + 首行（目录不存在返回空数组）
/// skills 搜索路径：~/.r2/skills —— R2 唯一的 skill 家。
/// 归属原则（用户拍板）：各 agent 管各自的目录，不扫别人的
/// （~/.agents/skills 属于 OpenClaw 生态，与 R2 无关）。
/// 安装约定：agent 装新 skill 一律写到 ~/.r2/skills/<name>/SKILL.md。
fn skills_dirs() -> Vec<PathBuf> {
    vec![PathBuf::from(config::expand_tilde("~/.r2/skills"))]
}

/// 从 SKILL.md 提取描述：优先 YAML frontmatter 的 description，
/// 回退正文第一条非空行（剥 markdown 标题符）。
fn skill_description(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    // frontmatter：首行 --- 到下一个 --- 之间
    if lines.first().map(|l| l.trim()) == Some(&"---") {
        if let Some(end) = lines[1..].iter().position(|l| l.trim() == "---") {
            for l in &lines[1..=end] {
                let t = l.trim();
                if let Some(v) = t.strip_prefix("description:") {
                    let v = v.trim().trim_matches('"').trim_matches('\'');
                    if !v.is_empty() {
                        return v.to_string();
                    }
                }
            }
        }
    }
    // 回退：frontmatter 之后的第一条非空行
    let start = if lines.first().map(|l| l.trim()) == Some(&"---") {
        lines.iter().position(|l| l.trim() == "---").map(|p| p + 1).unwrap_or(0)
    } else {
        0
    };
    lines[start..]
        .iter()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .trim_start_matches('#')
        .trim()
        .to_string()
}

async fn list_skills() -> Json<Value> {
    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for dir in skills_dirs() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !seen.insert(name.clone()) {
                    continue; // 专属目录优先，生态目录去重
                }
                let path = entry.path().join("SKILL.md");
                let Ok(content) = std::fs::read_to_string(&path) else {
                    continue;
                };
                out.push(json!({
                    "name": name,
                    "first_line": skill_description(&content),
                    // 真实绝对路径（引用按钮插给 agent，用 bash cat 读取）
                    "path": entry.path().to_string_lossy(),
                }));
            }
        }
    }
    out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Json(json!(out))
}

/// 校验 skill 名 / 会话 id：只允许安全字符，防路径穿越
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !name.contains("..")
}

/// GET /skill_preview?name=xxx：返回该 SKILL.md 全文
async fn skill_preview(Query(q): Query<HashMap<String, String>>) -> (StatusCode, Json<Value>) {
    let name = q.get("name").map(String::as_str).unwrap_or("");
    if !valid_name(name) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "非法 skill 名"})));
    }
    let mut path = None;
    for dir in skills_dirs() {
        let p = dir.join(name).join("SKILL.md");
        if p.exists() {
            path = Some(p);
            break;
        }
    }
    let Some(path) = path else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": format!("skill 不存在：{name}")})));
    };
    match std::fs::read_to_string(&path) {
        Ok(content) => (StatusCode::OK, Json(json!({"name": name, "content": content}))),
        Err(_) => (StatusCode::NOT_FOUND, Json(json!({"error": format!("skill 不存在：{name}")}))),
    }
}


/// GET /api/growth：成长可观测聚合（事件流 + 技能盘点 + 目标 + 校准统计）
async fn api_growth() -> Json<Value> {
    let events = r2_core::evolution::read_events(200);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let week_ago = now.saturating_sub(7 * 86400);
    let events_json: Vec<Value> = events
        .iter()
        .map(|e| {
            json!({
                "ts": e.ts, "kind": e.kind, "content": e.content,
                "evidence": e.evidence, "session_id": e.session_id,
            })
        })
        .collect();
    // 技能盘点（复用 skills 扫描逻辑的轻量版）
    let skills_dir = config::expand_tilde("~/.r2/skills");
    let mut skills: Vec<Value> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&skills_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if entry.path().join("SKILL.md").exists() {
                skills.push(json!({"name": name}));
            }
        }
    }
    let lessons_7d = events.iter().filter(|e| e.kind == "lesson" && e.ts >= week_ago).count();
    // 技能状态徽章 + 使用统计 + 衰退预警（阶段2/3）
    let usage = r2_core::evolution::read_usage();
    let skills_with_status: Vec<Value> = skills
        .into_iter()
        .map(|mut s| {
            let name = s["name"].as_str().unwrap_or("").to_string();
            let status = r2_core::evolution::read_skill_status(&name);
            s["status"] = match status.as_deref() {
                Some("trial") => json!("trial"),
                Some("promoted") => json!("promoted"),
                _ => json!("native"), // 手写技能
            };
            if let Some(u) = usage.get(&name) {
                s["used"] = json!(u.count);
                s["success_rate"] = if u.count > 0 {
                    json!(u.success * 100 / (u.success + u.fail))
                } else {
                    json!(null)
                };
            }
            s
        })
        .collect();
    let decayed: Vec<Value> = r2_core::evolution::decayed_skills()
        .into_iter()
        .map(|(name, last)| json!({"name": name, "last_used": last}))
        .collect();
    Json(json!({
        "goal": r2_core::evolution::read_goal(),
        "skills": skills_with_status,
        "skills_count": skills_with_status.len(),
        "events": events_json,
        "lessons_7d": lessons_7d,
        "total_events": events.len(),
        "decayed": decayed,
        "tasks": r2_core::tasks::load_store().tasks,
    }))
}

/// GET /api/state：完整状态快照
async fn api_state(State(state): State<Arc<WebState>>) -> Json<Value> {
    Json(state_json(&state))
}

/// main agent 的固定档案视图（无 AGENT.toml，~/.r2 根即档案）
fn main_profile_json() -> Value {
    json!({
        "name": MAIN,
        "display_name": "R2 主Agent",
        "model": "",
        "state": "active",
        "description": "主 Agent（~/.r2 根，无分身目录）",
        "created_ts": 0,
    })
}

/// GET /api/agent-files?name=xxx：读 agent 的 SOUL 全文 + 档案（配置页用）
async fn get_agent_files(
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let name = q.get("name").map(String::as_str).unwrap_or("");
    if name == MAIN {
        let soul = std::fs::read_to_string(config::expand_tilde("~/.r2/SOUL.md")).unwrap_or_default();
        return Ok(Json(json!({"soul": soul, "profile": main_profile_json()})));
    }
    let Some(p) = agents::load_profile(name) else {
        return Err((StatusCode::BAD_REQUEST, format!("agent 不存在：{name}")));
    };
    let soul = std::fs::read_to_string(agents::profile_dir(name).join("SOUL.md")).unwrap_or_default();
    Ok(Json(json!({"soul": soul, "profile": agents::profile_json(&p)})))
}

/// POST /api/agent-files（JSON: name/soul/display_name/description/model）：
/// 写回 {profile_dir}/SOUL.md 与 AGENT.toml 对应字段；main 只允许改 soul
async fn post_agent_files(Json(body): Json<Value>) -> Result<Json<Value>, (StatusCode, String)> {
    let name = body.get("name").and_then(|x| x.as_str()).unwrap_or("");
    if name.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "缺少字符串字段 name".to_string()));
    }
    let opt = |k: &str| body.get(k).and_then(|x| x.as_str());
    if name == MAIN {
        if let Some(soul) = opt("soul") {
            let path = PathBuf::from(config::expand_tilde("~/.r2/SOUL.md"));
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(&path, soul)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("写入失败：{e}")))?;
        }
        return Ok(Json(json!({"ok": true, "profile": main_profile_json()})));
    }
    if !agents::valid_name(name) {
        return Err((StatusCode::BAD_REQUEST, format!("非法档案名：{name}")));
    }
    let dir = agents::profile_dir(name);
    if !dir.exists() {
        return Err((StatusCode::BAD_REQUEST, format!("agent 目录不存在：{name}")));
    }
    if let Some(soul) = opt("soul") {
        std::fs::write(dir.join("SOUL.md"), soul)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("写入失败：{e}")))?;
    }
    let Some(mut p) = agents::load_profile(name) else {
        return Err((StatusCode::BAD_REQUEST, format!("agent 档案损坏：{name}")));
    };
    if let Some(v) = opt("display_name") {
        p.display_name = v.to_string();
    }
    if let Some(v) = opt("description") {
        p.description = v.to_string();
    }
    if let Some(v) = opt("model") {
        p.model = v.to_string();
    }
    agents::save_profile(&p).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(json!({"ok": true, "profile": agents::profile_json(&p)})))
}

// ---------- WebSocket ----------

type WsSink = Arc<Mutex<SplitSink<WebSocket, WsMessage>>>;

/// C→S 客户端消息（解析结果；垃圾 JSON 在 parse_client_msg 阶段拦下）
enum ClientMsg {
    Prompt(String),
    Steer(String),
    NewSession,
    Switch(String),
    Fork { parent: String, upto: Option<usize> },
    DeleteSession(String),
    SetModel(String),
    SetProfile(String),
    SandboxSet {
        bash_timeout_secs: u64,
        max_processes: usize,
        max_memory_mb: usize,
        cpu_time_secs: u32,
        max_file_size_mb: u32,
        cgroup_memory_mb: u32,
    },
    TaskApprove(String),
    TaskReject(String),
    TaskPause(String),
    TaskResume(String),
    TaskDelete(String),
    AgentApprove(String),
    AgentReject(String),
    AgentSwitch(String),
    GroupCreate { title: String, members: Vec<(String, String)> },
    GroupPrompt { id: String, text: String },
    GroupDiscuss { id: String, topic: String },
    GroupDelegate { id: String, topic: String, lead: String },
    GroupPause(String),
    GroupStop(String),
    GroupRevokeLead(String),
    GroupSummary(String),
    GroupOpen(String),
    GroupSubtaskApprove { id: String, to: String },
    ChannelSet { agent: String, config: Value },
    ChannelTest { agent: String },
}

/// 取字符串字段的辅助
fn get_str<'a>(v: &'a Value, key: &str) -> Result<&'a str, String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .ok_or_else(|| format!("缺少字符串字段 {key}"))
}

/// 解析客户端文本消息（纯函数，便于测试）
fn parse_client_msg(text: &str) -> Result<ClientMsg, String> {
    let v: Value = serde_json::from_str(text.trim()).map_err(|e| format!("JSON 解析失败：{e}"))?;
    let t = v
        .get("t")
        .and_then(|x| x.as_str())
        .ok_or_else(|| "缺少字符串字段 t".to_string())?;
    match t {
        "prompt" => Ok(ClientMsg::Prompt(get_str(&v, "input")?.to_string())),
        "steer" => Ok(ClientMsg::Steer(get_str(&v, "text")?.to_string())),
        "new_session" => Ok(ClientMsg::NewSession),
        "switch" => Ok(ClientMsg::Switch(get_str(&v, "id")?.to_string())),
        "fork" => {
            let parent = get_str(&v, "parent")?.to_string();
            let upto = match v.get("upto") {
                None | Some(Value::Null) => None,
                Some(x) => match x.as_u64() {
                    Some(n) => Some(n as usize),
                    None => return Err("upto 必须为非负整数".to_string()),
                },
            };
            Ok(ClientMsg::Fork { parent, upto })
        }
        "delete_session" => Ok(ClientMsg::DeleteSession(get_str(&v, "id")?.to_string())),
    "task_approve" => Ok(ClientMsg::TaskApprove(get_str(&v, "id")?.to_string())),
    "task_reject" => Ok(ClientMsg::TaskReject(get_str(&v, "id")?.to_string())),
    "task_pause" => Ok(ClientMsg::TaskPause(get_str(&v, "id")?.to_string())),
    "task_resume" => Ok(ClientMsg::TaskResume(get_str(&v, "id")?.to_string())),
    "task_delete" => Ok(ClientMsg::TaskDelete(get_str(&v, "id")?.to_string())),
    "agent_approve" => Ok(ClientMsg::AgentApprove(get_str(&v, "name")?.to_string())),
    "agent_reject" => Ok(ClientMsg::AgentReject(get_str(&v, "name")?.to_string())),
    "agent_switch" => Ok(ClientMsg::AgentSwitch(get_str(&v, "name")?.to_string())),
        "set_model" => Ok(ClientMsg::SetModel(get_str(&v, "model")?.to_string())),
        "set_profile" => Ok(ClientMsg::SetProfile(get_str(&v, "name")?.to_string())),
        "sandbox_set" => {
            let num = |k: &str| -> Result<u64, String> {
                v.get(k)
                    .and_then(|x| x.as_u64())
                    .ok_or_else(|| format!("缺少数字字段 {k}"))
            };
            Ok(ClientMsg::SandboxSet {
                bash_timeout_secs: num("bash_timeout_secs")?,
                max_processes: num("max_processes")? as usize,
                max_memory_mb: num("max_memory_mb")? as usize,
                cpu_time_secs: num("cpu_time_secs")? as u32,
                max_file_size_mb: num("max_file_size_mb")? as u32,
                cgroup_memory_mb: num("cgroup_memory_mb")? as u32,
            })
        }
        "group_create" => {
            let title = get_str(&v, "title")?.to_string();
            let arr = v
                .get("members")
                .and_then(|x| x.as_array())
                .ok_or_else(|| "缺少数组字段 members".to_string())?;
            let mut members = Vec::new();
            for m in arr {
                let name = m
                    .get("name")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| "members[] 缺少字符串字段 name".to_string())?;
                let display = m
                    .get("display_name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                members.push((name.to_string(), display.to_string()));
            }
            if members.is_empty() {
                return Err("members 至少包含 1 个 agent 分身".to_string());
            }
            Ok(ClientMsg::GroupCreate { title, members })
        }
        "group_prompt" => Ok(ClientMsg::GroupPrompt {
            id: get_str(&v, "id")?.to_string(),
            text: get_str(&v, "text")?.to_string(),
        }),
        "group_discuss" => Ok(ClientMsg::GroupDiscuss {
            id: get_str(&v, "id")?.to_string(),
            topic: get_str(&v, "topic")?.to_string(),
        }),
        "group_delegate" => Ok(ClientMsg::GroupDelegate {
            id: get_str(&v, "id")?.to_string(),
            topic: get_str(&v, "topic")?.to_string(),
            lead: get_str(&v, "lead")?.to_string(),
        }),
        "group_pause" => Ok(ClientMsg::GroupPause(get_str(&v, "id")?.to_string())),
        "group_stop" => Ok(ClientMsg::GroupStop(get_str(&v, "id")?.to_string())),
        "group_revoke_lead" => Ok(ClientMsg::GroupRevokeLead(get_str(&v, "id")?.to_string())),
        "group_summary" => Ok(ClientMsg::GroupSummary(get_str(&v, "id")?.to_string())),
        "group_open" => Ok(ClientMsg::GroupOpen(get_str(&v, "id")?.to_string())),
        "group_subtask_approve" => Ok(ClientMsg::GroupSubtaskApprove {
            id: get_str(&v, "id")?.to_string(),
            to: get_str(&v, "to")?.to_string(),
        }),
        "channel_set" => {
            let agent = get_str(&v, "agent")?.to_string();
            let config = v
                .get("config")
                .cloned()
                .ok_or_else(|| "缺少字段 config".to_string())?;
            Ok(ClientMsg::ChannelSet { agent, config })
        }
        "channel_test" => Ok(ClientMsg::ChannelTest {
            agent: get_str(&v, "agent")?.to_string(),
        }),
        other => Err(format!("未知消息类型：{other}")),
    }
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<WebState>>) -> impl axum::response::IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

/// 单条 WS 连接生命周期：先推 init 快照，然后循环处理客户端消息；
/// 同时起一个转发任务把 event_tx 广播扇出到本连接
async fn handle_ws(socket: WebSocket, state: Arc<WebState>) {
    let (sink, mut stream) = socket.split();
    let sink: WsSink = Arc::new(Mutex::new(sink));

    // 广播 → 本连接（心跳+事件扇出：hb 每 15s，前端看门狗 35s 无帧断线重连）
    let mut evt_rx = state.event_tx.subscribe();
    let sink_fwd = sink.clone();
    let forward = tokio::spawn(async move {
        let mut hb = tokio::time::interval(std::time::Duration::from_secs(15));
        loop {
            tokio::select! {
                r = evt_rx.recv() => match r {
                    Ok(v) => {
                        let mut s = sink_fwd.lock().await;
                        if s.send(WsMessage::Text(v.to_string())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                _ = hb.tick() => {
                    let mut s = sink_fwd.lock().await;
                    if s.send(WsMessage::Text(json!({"t": "hb"}).to_string())).await.is_err() {
                        break;
                    }
                },
            }
        }
    });

    // 握手后先推 init（同 /api/state）
    let mut init = state_json(&state);
    init["t"] = json!("init");
    ws_send(&sink, init).await;

    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            WsMessage::Text(text) => handle_client_msg(&text, &state, &sink).await,
            WsMessage::Close(_) => break,
            _ => {} // ping/pong/binary 忽略
        }
    }
    forward.abort();
}

/// 分发一条客户端消息（prompt 异步执行不阻塞 WS 循环，其余同步处理完）
async fn handle_client_msg(text: &str, state: &Arc<WebState>, sink: &WsSink) {
    let msg = match parse_client_msg(text) {
        Ok(m) => m,
        Err(e) => {
            ws_error(sink, &e).await;
            return;
        }
    };
    match msg {
        ClientMsg::Prompt(input) => {
            // prompt 不能阻塞 WS 循环（否则 steer 进不来）：spawn 独立任务；
            // 在途时新输入自动排队，收尾完成后续发（不再拒绝）
            spawn_prompt(state.clone(), input);
        }
        ClientMsg::Steer(text) => {
            // 走缓存的 steer 端：prompt 持锁期间也能注入
            let handle = state.steer_tx.lock().expect("steer 锁中毒").clone();
            match handle {
                Some(tx) => {
                    if let Err(e) = tx.try_send(text) {
                        ws_error(sink, &format!("steer 注入失败：{e}")).await;
                    }
                }
                None => ws_error(sink, "会话尚未创建").await,
            }
        }
        ClientMsg::NewSession => {
            let config = config_snapshot_fresh_mcp(state);
            let mut guard = state.agent.lock().await;
            match AgentSession::new(config) {
                Ok(s) => {
                    install_session(state, &mut guard, s);
                    drop(guard);
                    broadcast_sessions(state);
                }
                Err(e) => {
                    drop(guard);
                    ws_error(sink, &format!("新建会话失败：{e}")).await;
                }
            }
        }
        ClientMsg::Switch(id) => {
            if !valid_name(&id) {
                ws_error(sink, "非法会话 id").await;
                return;
            }
            let config = config_snapshot(state);
            let mut guard = state.agent.lock().await;
            match AgentSession::resume(config, &id) {
                Ok(s) => {
                    let hist = session_history_json(&s);
                    install_session(state, &mut guard, s);
                    drop(guard);
                    ws_send(sink, json!({"t": "session_history", "history": hist})).await;
                    broadcast_sessions(state);
                }
                Err(e) => {
                    drop(guard);
                    ws_error(sink, &format!("切换会话失败：{e}")).await;
                }
            }
        }
        ClientMsg::Fork { parent, upto } => {
            if !valid_name(&parent) {
                ws_error(sink, "非法父会话 id").await;
                return;
            }
            let config = config_snapshot(state);
            let mut guard = state.agent.lock().await;
            match AgentSession::branch_from(config, &parent, upto) {
                Ok(s) => {
                    let new_id = s.session_id().map(String::from);
                    let hist = session_history_json(&s);
                    install_session(state, &mut guard, s);
                    drop(guard);
                    ws_send(sink, json!({"t": "forked", "id": new_id})).await;
                    ws_send(sink, json!({"t": "session_history", "history": hist})).await;
                    broadcast_sessions(state);
                }
                Err(e) => {
                    drop(guard);
                    ws_error(sink, &format!("分叉失败：{e}")).await;
                }
            }
        }
        ClientMsg::DeleteSession(id) => {
            if !valid_name(&id) {
                ws_error(sink, "非法会话 id").await;
                return;
            }
            // 正在用的会话禁删；prompt 在途时锁不可用，直接拒绝
            let current = match state.agent.try_lock() {
                Ok(g) => g.as_ref().and_then(|s| s.session_id().map(String::from)),
                Err(_) => {
                    ws_error(sink, "prompt 运行中，稍后再删").await;
                    return;
                }
            };
            if current.as_deref() == Some(id.as_str()) {
                ws_error(sink, "当前会话使用中，禁止删除").await;
                return;
            }
            let path = Path::new(&session_dir_for(state)).join(format!("{id}.jsonl"));
            match std::fs::remove_file(&path) {
                Ok(()) => broadcast_sessions(state),
                Err(e) => ws_error(sink, &format!("删除失败：{e}")).await,
            }
        }
        ClientMsg::TaskApprove(id) => {
            // 签字权：唯一的 pending→active 通道（工具永远到不了这里）
            let mut store = r2_core::tasks::load_store();
            let tz = r2_core::tasks::local_tz_offset_secs();
            match r2_core::tasks::transition(&mut store, &id, "active") {
                Ok(()) => {
                    if let Some(t) = store.tasks.iter_mut().find(|t| t.id == id) {
                        t.next_due = r2_core::tasks::next_run(&t.schedule, r2_core::tasks::now_ts(), tz);
                    }
                    let _ = r2_core::tasks::save_store(&store);
                    let _ = state.event_tx.send(json!({"t": "task_state", "id": id, "state": "active"}));
                    let _ = state.event_tx.send(tasks_broadcast_payload());
                }
                Err(e) => { ws_error(sink, &format!("批准失败：{e}")).await; }
            }
        }
        ClientMsg::TaskReject(id) => {
            let mut store = r2_core::tasks::load_store();
            match r2_core::tasks::transition(&mut store, &id, "rejected") {
                Ok(()) => {
                    let _ = r2_core::tasks::save_store(&store);
                    let _ = state.event_tx.send(json!({"t": "task_state", "id": id, "state": "rejected"}));
                    let _ = state.event_tx.send(tasks_broadcast_payload());
                }
                Err(e) => { ws_error(sink, &format!("拒绝失败：{e}")).await; }
            }
        }
        ClientMsg::TaskPause(id) => {
            let mut store = r2_core::tasks::load_store();
            match r2_core::tasks::transition(&mut store, &id, "paused") {
                Ok(()) => {
                    let _ = r2_core::tasks::save_store(&store);
                    let _ = state.event_tx.send(tasks_broadcast_payload());
                }
                Err(e) => { ws_error(sink, &format!("暂停失败：{e}")).await; }
            }
        }
        ClientMsg::TaskResume(id) => {
            let mut store = r2_core::tasks::load_store();
            let tz = r2_core::tasks::local_tz_offset_secs();
            match r2_core::tasks::transition(&mut store, &id, "active") {
                Ok(()) => {
                    if let Some(t) = store.tasks.iter_mut().find(|t| t.id == id) {
                        t.next_due = r2_core::tasks::next_run(&t.schedule, r2_core::tasks::now_ts(), tz);
                    }
                    let _ = r2_core::tasks::save_store(&store);
                    let _ = state.event_tx.send(tasks_broadcast_payload());
                }
                Err(e) => { ws_error(sink, &format!("恢复失败：{e}")).await; }
            }
        }
        ClientMsg::TaskDelete(id) => {
            let mut store = r2_core::tasks::load_store();
            match r2_core::tasks::remove_task(&mut store, &id) {
                Ok(()) => {
                    let _ = r2_core::tasks::save_store(&store);
                    let _ = state.event_tx.send(tasks_broadcast_payload());
                }
                Err(e) => { ws_error(sink, &format!("删除失败：{e}")).await; }
            }
        }
        ClientMsg::AgentApprove(name) => {
            // 签字权：pending→active 唯一通道（与 task 审批同款信任模型）
            match agents::approve(&name) {
                Ok(_) => {
                    start_feishu_channels(state); // 档案变更后幂等重载通道
                    broadcast_state(state);
                }
                Err(e) => ws_error(sink, &format!("批准 agent 失败：{e}")).await,
            }
        }
        ClientMsg::AgentReject(name) => {
            match agents::reject(&name) {
                Ok(()) => broadcast_state(state),
                Err(e) => ws_error(sink, &format!("拒绝 agent 失败：{e}")).await,
            }
        }
        ClientMsg::AgentSwitch(name) => {
            // 校验：main 直接放行；分身必须存在且已批准（active）
            if name != MAIN {
                match agents::load_profile(&name) {
                    Some(p) if p.state == "active" => {}
                    Some(p) => {
                        ws_error(sink, &format!("agent {} 当前状态 {}，不可切换", p.name, p.state)).await;
                        return;
                    }
                    None => {
                        ws_error(sink, &format!("agent 不存在：{name}")).await;
                        return;
                    }
                }
            }
            // 旧 agent 的会话不带过去：清槽位（prompt 在途则拒切，防历史串台）
            let mut guard = match state.agent.try_lock() {
                Ok(g) => g,
                Err(_) => {
                    ws_error(sink, "prompt 运行中，稍后再切换 agent").await;
                    return;
                }
            };
            *state.current_agent.lock().expect("agent 锁中毒") = name.clone();
            *state.steer_tx.lock().expect("steer 锁中毒") = None;
            *guard = None;
            drop(guard);
            // 广播全新 init（state_json 已含新 agent 的会话列表 + agents + current_agent）
            let mut init = state_json(state);
            init["t"] = json!("init");
            let _ = state.event_tx.send(init);
        }
        ClientMsg::SandboxSet {
            bash_timeout_secs,
            max_processes,
            max_memory_mb,
            cpu_time_secs,
            max_file_size_mb,
            cgroup_memory_mb,
        } => {
            // 运行时配置即时更新（新会话生效——BashTool 持构建时快照）+ 持久化
            let persist_err = {
                let mut cfg = state.config.lock().expect("config 锁中毒");
                cfg.sandbox.bash_timeout_secs = bash_timeout_secs;
                cfg.sandbox.max_processes = max_processes;
                cfg.sandbox.max_memory_mb = max_memory_mb;
                cfg.sandbox.cpu_time_secs = cpu_time_secs;
                cfg.sandbox.max_file_size_mb = max_file_size_mb;
                cfg.sandbox.cgroup_memory_mb = cgroup_memory_mb;
                let path = cfg.source_path.clone();
                drop(cfg);
                path.and_then(|p| {
                    persist_sandbox(
                        &p,
                        &[
                            ("bash_timeout_secs", bash_timeout_secs.to_string()),
                            ("max_processes", max_processes.to_string()),
                            ("max_memory_mb", max_memory_mb.to_string()),
                            ("cpu_time_secs", cpu_time_secs.to_string()),
                            ("max_file_size_mb", max_file_size_mb.to_string()),
                            ("cgroup_memory_mb", cgroup_memory_mb.to_string()),
                        ],
                    )
                    .err()
                })
            };
            if let Some(e) = persist_err {
                eprintln!("[console] sandbox 持久化失败（运行时已生效）：{e}");
            }
            broadcast_state(state);
            ws_send(sink, json!({"t": "sandbox_set", "ok": true})).await;
        }
        ClientMsg::SetProfile(name) => {
            // 应用模型档案（跨 provider 整套切换）+ 持久化 + 会话重建（历史保留）
            let apply_result = {
                let mut cfg = state.config.lock().expect("config 锁中毒");
                cfg.apply_profile(&name).map(|_| {
                    if let Some(p) = cfg.source_path.clone() {
                        if let Err(e) = persist_active_profile(&p, &name) {
                            eprintln!("[console] active_profile 持久化失败：{e}");
                        }
                    }
                    cfg.current_model().to_string()
                })
            };
            match apply_result {
                Ok(model) => {
                    let mut guard = state.agent.lock().await;
                    let sid = guard.as_ref().and_then(|s| s.session_id().map(String::from));
                    *state.steer_tx.lock().expect("steer 锁中毒") = None;
                    *guard = None;
                    if let Some(sid) = sid {
                        let config = config_snapshot(state);
                        match AgentSession::resume(config, &sid) {
                            Ok(s) => install_session(state, &mut guard, s),
                            Err(e) => broadcast_error(state, &format!("模型已切换，但会话重建失败：{e}")),
                        }
                    }
                    drop(guard);
                    let _ = state
                        .event_tx
                        .send(json!({"t": "model_changed", "model": model}));
                    broadcast_state(state);
                }
                Err(e) => ws_error(sink, &e).await,
            }
        }
        ClientMsg::SetModel(model) => {
            {
                let mut cfg = state.config.lock().expect("config 锁中毒");
                match cfg.model.provider.as_str() {
                    "anthropic" => cfg.model.anthropic.model = model.clone(),
                    _ => cfg.model.openai_compat.model = model.clone(),
                }
            }
            // 运行中的 Agent 换不了模型：用新配置按原 session id 重建（历史保留，模型生效）
            let mut guard = state.agent.lock().await;
            let sid = guard.as_ref().and_then(|s| s.session_id().map(String::from));
            *state.steer_tx.lock().expect("steer 锁中毒") = None;
            *guard = None;
            if let Some(sid) = sid {
                let config = config_snapshot(state);
                match AgentSession::resume(config, &sid) {
                    Ok(s) => install_session(state, &mut guard, s),
                    Err(e) => broadcast_error(state, &format!("模型已切换，但会话重建失败：{e}")),
                }
            }
            drop(guard);
            let _ = state
                .event_tx
                .send(json!({"t": "model_changed", "model": model}));
        }
        ClientMsg::GroupCreate { title, members } => {
            match do_group_create(&state.session_dir, &title, &members) {
                Ok((sid, g)) => {
                    let _ = state
                        .event_tx
                        .send(json!({"t": "group_created", "id": sid, "group": g}));
                    broadcast_sessions(state);
                }
                Err(e) => ws_error(sink, &e).await,
            }
        }
        ClientMsg::GroupPrompt { id, text } => {
            let dir = match resolve_group_dir(state, &id) {
                Ok(d) => d,
                Err(e) => {
                    ws_error(sink, &e).await;
                    return;
                }
            };
            let g = groups::load_group(&dir).expect("resolve 已校验");
            if g.state == "stopped" {
                ws_error(sink, "群已终止（stopped），不能发言").await;
                return;
            }
            // 人发言/插话照常入流（discussing 中不打断当前发言者，下轮调度自然读到）
            let mentions = parse_mentions(&text, &g);
            let ev = if mentions.is_empty() {
                GroupEvent::message("user", &text)
            } else {
                GroupEvent::mention("user", &text, mentions.clone())
            };
            if let Err(e) = append_and_broadcast(state, &id, &dir, &ev) {
                ws_error(sink, &e).await;
                return;
            }
            // 人的 @点名 同样跳序（@唤醒权只属于人）
            if g.state == "discussing" {
                if let Some(target) = mentions.first() {
                    if let Some(mut g2) = groups::load_group(&dir) {
                        if let Some(sp) = speaking_to_force_next(&g2, target) {
                            g2.speaking = Some(sp);
                            let _ = groups::save_group(&dir, &g2);
                        }
                    }
                }
            }
            // idle/paused/summarized（重开继续聊）→ discussing 并启动调度
            if g.state == "idle" || g.state == "paused" || g.state == "summarized" {
                match groups::set_state(&dir, "discussing") {
                    Ok(g2) => {
                        broadcast_last_event(state, &id, &dir);
                        broadcast_group_state(state, &id, &g2);
                        if !start_group_scheduler(state, &id) {
                            ws_error(sink, "群调度已在运行").await;
                        }
                    }
                    Err(e) => ws_error(sink, &e).await,
                }
            }
        }
        ClientMsg::GroupDiscuss { id, topic } => {
            let dir = match resolve_group_dir(state, &id) {
                Ok(d) => d,
                Err(e) => {
                    ws_error(sink, &e).await;
                    return;
                }
            };
            let Some(mut g) = groups::load_group(&dir) else {
                ws_error(sink, "群档案损坏").await;
                return;
            };
            if g.state != "idle" && g.state != "paused" {
                ws_error(sink, &format!("群当前状态 {}，不能发起讨论", g.state)).await;
                return;
            }
            g.task = Some(groups::GroupTask {
                topic: topic.clone(),
                kind: "discussion".into(),
                lead: None,
                depth_left: r2_core::groups::DEFAULT_TASK_DEPTH,
                started_ts: now_ts(),
            });
            if let Err(e) = groups::save_group(&dir, &g) {
                ws_error(sink, &e).await;
                return;
            }
            if let Err(e) = append_and_broadcast(state, &id, &dir, &GroupEvent::message("user", &topic)) {
                ws_error(sink, &e).await;
                return;
            }
            match groups::set_state(&dir, "discussing") {
                Ok(g2) => {
                    broadcast_last_event(state, &id, &dir);
                    broadcast_group_state(state, &id, &g2);
                    if !start_group_scheduler(state, &id) {
                        ws_error(sink, "群调度已在运行").await;
                    }
                }
                Err(e) => ws_error(sink, &e).await,
            }
        }
        ClientMsg::GroupDelegate { id, topic, lead } => {
            let dir = match resolve_group_dir(state, &id) {
                Ok(d) => d,
                Err(e) => {
                    ws_error(sink, &e).await;
                    return;
                }
            };
            let Some(g) = groups::load_group(&dir) else {
                ws_error(sink, "群档案损坏").await;
                return;
            };
            if g.state != "idle" && g.state != "paused" {
                ws_error(sink, &format!("群当前状态 {}，不能委任", g.state)).await;
                return;
            }
            if let Err(e) = groups::promote_lead(&dir, &lead) {
                ws_error(sink, &e).await;
                return;
            }
            let mut g = groups::load_group(&dir).expect("promote 后必可读");
            g.task = Some(groups::GroupTask {
                topic: topic.clone(),
                kind: "delegation".into(),
                lead: Some(lead.clone()),
                depth_left: r2_core::groups::DEFAULT_TASK_DEPTH,
                started_ts: now_ts(),
            });
            if let Err(e) = groups::save_group(&dir, &g) {
                ws_error(sink, &e).await;
                return;
            }
            if let Err(e) = append_and_broadcast(
                state,
                &id,
                &dir,
                &GroupEvent::message("user", &format!("委任 @{lead}：{topic}")),
            ) {
                ws_error(sink, &e).await;
                return;
            }
            match groups::set_state(&dir, "discussing") {
                Ok(g2) => {
                    broadcast_last_event(state, &id, &dir);
                    broadcast_group_state(state, &id, &g2);
                    if !start_group_scheduler(state, &id) {
                        ws_error(sink, "群调度已在运行").await;
                    }
                }
                Err(e) => ws_error(sink, &e).await,
            }
        }
        ClientMsg::GroupPause(id) => {
            handle_group_pause_stop(state, sink, id, "paused").await;
        }
        ClientMsg::GroupStop(id) => {
            handle_group_pause_stop(state, sink, id, "stopped").await;
        }
        ClientMsg::GroupRevokeLead(id) => {
            let dir = match resolve_group_dir(state, &id) {
                Ok(d) => d,
                Err(e) => {
                    ws_error(sink, &e).await;
                    return;
                }
            };
            match groups::revoke_lead(&dir) {
                Ok(g) => broadcast_group_state(state, &id, &g),
                Err(e) => ws_error(sink, &e).await,
            }
        }
        ClientMsg::GroupSummary(id) => {
            let dir = match resolve_group_dir(state, &id) {
                Ok(d) => d,
                Err(e) => {
                    ws_error(sink, &e).await;
                    return;
                }
            };
            let g = groups::load_group(&dir).expect("resolve 已校验");
            if g.state != "discussing" {
                ws_error(sink, &format!("仅讨论中可小结（当前 {}）", g.state)).await;
                return;
            }
            abort_group_scheduler(state, &id);
            let st = state.clone();
            tokio::spawn(async move { run_group_summary(&st, &id).await });
        }
        ClientMsg::GroupOpen(id) => {
            let dir = match resolve_group_dir(state, &id) {
                Ok(d) => d,
                Err(e) => {
                    ws_error(sink, &e).await;
                    return;
                }
            };
            let g = groups::load_group(&dir).expect("resolve 已校验");
            let events = groups::read_stream(&dir);
            ws_send(sink, json!({"t": "group_open", "id": id, "group": g, "events": events})).await;
        }
        ClientMsg::GroupSubtaskApprove { id, to } => {
            let dir = match resolve_group_dir(state, &id) {
                Ok(d) => d,
                Err(e) => {
                    ws_error(sink, &e).await;
                    return;
                }
            };
            // 找该成员最近一条 pending 子任务（append-only：新事件覆盖语义）
            let pending = groups::read_stream(&dir).into_iter().rev().find(|e| {
                matches!(e, GroupEvent::Subtask { to: t, state, .. } if *t == to && state == "pending")
            });
            let Some(GroupEvent::Subtask { from, prompt, .. }) = pending else {
                ws_error(sink, &format!("成员 {to} 没有待批准子任务")).await;
                return;
            };
            let approved = GroupEvent::Subtask {
                from,
                to: to.clone(),
                prompt: prompt.clone(),
                ts: now_ts(),
                state: "approved".into(),
            };
            if let Err(e) = append_and_broadcast(state, &id, &dir, &approved) {
                ws_error(sink, &e).await;
                return;
            }
            let st = state.clone();
            tokio::spawn(async move { run_group_subtask(st, id, to, prompt).await });
        }
        ClientMsg::ChannelSet { agent, config } => {
            // channel_feishu 全量写入档案 + 幂等重载该通道
            let Some(mut p) = agents::load_profile(&agent) else {
                ws_error(sink, &format!("agent 不存在：{agent}")).await;
                return;
            };
            match serde_json::from_value::<ChannelFeishu>(config) {
                Err(e) => ws_error(sink, &format!("channel_feishu 配置解析失败：{e}")).await,
                Ok(cf) => {
                    p.channel_feishu = cf;
                    match agents::save_profile(&p) {
                        Ok(()) => {
                            start_feishu_channels(state);
                            // 回包带保存后的配置（前端就地更新 store，不用刷新页面）
                            let cf = &p.channel_feishu;
                            ws_send(
                                sink,
                                json!({
                                    "t": "channel_set", "agent": agent, "ok": true,
                                    "config": {
                                        "enabled": cf.enabled,
                                        "app_id": cf.app_id,
                                        "dm_policy": cf.effective_policy().0,
                                        "policy_list": cf.effective_policy().1,
                                        "show_process": cf.show_process,
                                    }
                                }),
                            )
                            .await;
                            // 全量广播：其他打开的标签页也同步新配置/新通道状态
                            broadcast_state(state);
                        }
                        Err(e) => ws_error(sink, &format!("保存档案失败：{e}")).await,
                    }
                }
            }
        }
        ClientMsg::ChannelTest { agent } => {
            // 自检有网络等待（≤10s）：spawn 独立任务，不阻塞 WS 循环
            let sink2 = sink.clone();
            tokio::spawn(async move {
                let (ok, detail) = test_feishu_channel(&agent).await;
                ws_send(
                    &sink2,
                    json!({"t": "channel_test", "agent": agent, "ok": ok, "detail": detail}),
                )
                .await;
            });
        }
    }
}

// ---------- 飞书 DM 通道（v0.10.0-B：飞书消息 ↔ agent 会话） ----------
//
// 每个 active 分身可在 AGENT.toml 配 [channel_feishu]：启用后该 agent 的飞书机器人
// 私聊消息路由到它自己的持久 AgentSession（每 (agent, open_id) 一个），回复发回飞书。
// dm_sessions 与 state.agent 主槽位完全隔离：main console 会话不受任何影响。

/// DM prompt 超时（秒）。8/25 用户实测：复杂任务（思考+工具链）120s 截断太短——
/// 提到 600s 与后台任务同档；Console 单聊本就无超时
const DM_PROMPT_TIMEOUT_SECS: u64 = 600;
/// none 档心跳间隔：静默处理超 2 分钟时告知用户还活着（compact/full 有过程流不需）
const DM_HEARTBEAT_SECS: u64 = 120;
/// 单个 DM 会话排队上限（满了回"忙"）
const DM_QUEUE_MAX: usize = 8;
/// full 档思考流累计多少字发一段
const DM_THINKING_FLUSH_CHARS: usize = 500;
/// 通道自检等待 Connected/Failed 的超时（秒）
const CHANNEL_TEST_TIMEOUT_SECS: u64 = 10;

/// 运行中的飞书通道
struct FeishuRuntime {
    client: Arc<FeishuClient>,
    /// 幂等对比用：凭证变了才重启
    app_id: String,
    app_secret: String,
    status: ChannelStatus,
}

/// 一个 (agent, open_id) 的持久 DM 会话
struct DmSession {
    /// prompt 串行化：持锁期间新消息进 pending
    session: Mutex<AgentSession>,
    /// 会话级排队：FIFO，元素 (text, message_id)——排队消息完成时贴各自的 👍
    pending: StdMutex<VecDeque<(String, String)>>,
    /// steer 发送端（与 session 内部通道同源克隆）：在途消息插话不打断排队机制，
    /// 也不用锁 session（运行中锁被 prompt 持有）。/new /model 重建会话后刷新
    steer_tx: StdMutex<tokio::sync::mpsc::Sender<String>>,
}

/// show_process 分档（带序：None < Compact < Full，便于 >= 判断）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ShowProcess {
    None,
    Compact,
    Full,
}

/// show_process 配置 → 分档（空串/未识别值按默认 compact 处理）
fn show_process_mode(s: &str) -> ShowProcess {
    match s {
        "none" => ShowProcess::None,
        "full" => ShowProcess::Full,
        _ => ShowProcess::Compact,
    }
}

/// DM 策略判定（v0.10.1）：deny_all 拒绝所有人 / allow_all 允许所有人 /
/// allow_list 仅允许名单内 / deny_list 拒绝名单内（其余放行）。
/// 老档案 allow_from 语义经 effective_policy 归一，此处不再关心兼容细节
fn feishu_allowed(cf: &ChannelFeishu, open_id: &str) -> bool {
    match cf.effective_policy() {
        ("deny_all", _) => false,
        ("allow_all", _) => true,
        ("allow_list", list) => list.iter().any(|x| x == open_id),
        ("deny_list", list) => !list.iter().any(|x| x == open_id),
        _ => false,
    }
}

/// DM 会话槽位 key：agent|open_id
fn dm_key(agent: &str, open_id: &str) -> String {
    format!("{agent}|{open_id}")
}

/// DM 会话用的配置快照：全局运行时配置 + mcp 源文件刷新 + persona 叠加。
/// handle_dm 创建分支与 /model 原地重建共用（保证两条路拿到同款 config）
fn dm_config(state: &WebState, agent: &str) -> Config {
    let mut cfg = state.config.lock().expect("config 锁中毒").clone();
    if let Some(p) = cfg.source_path.clone() {
        if let Ok(fresh) = Config::load_from_file(&p) {
            cfg.mcp = fresh.mcp; // 全局原文（不含分身条目）
        }
    }
    apply_persona(agent, &mut cfg); // 含分身 MCP.toml upsert
    cfg
}

/// active_profile 持久化到 config.toml（文本手术：只动 active_profile 一行，
/// 绝不整体序列化——用户的注释与字段顺序全部保留）。无该行则在 [model] 段头插入
fn persist_active_profile(path: &str, name: &str) -> Result<(), String> {
    let content = std::fs::read_to_string(path).map_err(|e| format!("读配置失败：{e}"))?;
    let line = format!("active_profile = \"{name}\"");
    let mut out: Vec<String> = Vec::new();
    let mut replaced = false;
    let mut in_model = false;
    for l in content.lines() {
        if l.trim_start().starts_with("[model]") && !l.trim_start().starts_with("[model.") {
            in_model = true;
            out.push(l.to_string());
            if !replaced {
                out.push(line.clone());
                replaced = true;
            }
            continue;
        }
        if l.trim_start().starts_with('[') && !l.trim_start().starts_with("[model") {
            in_model = false;
        }
        if !replaced && in_model && l.trim_start().starts_with("active_profile") {
            out.push(line.clone());
            replaced = true;
            continue;
        }
        out.push(l.to_string());
    }
    if !replaced {
        return Err("config.toml 缺 [model] 段，无法持久化 active_profile".into());
    }
    let mut s = out.join("\n");
    if !s.ends_with('\n') {
        s.push('\n');
    }
    std::fs::write(path, s).map_err(|e| format!("写配置失败：{e}"))
}

/// [sandbox] 段多键持久化（文本手术：有则替换、无则段头后插入，
/// 绝不整体序列化——level 与用户注释原样保留）
fn persist_sandbox(path: &str, vals: &[(&str, String)]) -> Result<(), String> {
    let content = std::fs::read_to_string(path).map_err(|e| format!("读配置失败：{e}"))?;
    let mut lines: Vec<String> = Vec::new();
    let mut in_section = false;
    let mut done: Vec<&str> = Vec::new();
    let mut header_idx: Option<usize> = None;
    for l in content.lines() {
        let t = l.trim_start();
        if t.starts_with('[') {
            in_section = t == "[sandbox]";
            if in_section && header_idx.is_none() {
                header_idx = Some(lines.len());
            }
        } else if in_section {
            if let Some(eq) = t.find('=') {
                let key = t[..eq].trim();
                if let Some((_, val)) = vals.iter().find(|(k, _)| *k == key) {
                    lines.push(format!("{key} = {val}"));
                    done.push(key);
                    continue;
                }
            }
        }
        lines.push(l.to_string());
    }
    let h = match header_idx {
        Some(h) => h,
        None => {
            lines.push(String::new());
            lines.push("[sandbox]".to_string());
            lines.len() - 1
        }
    };
    let mut insert_at = h + 1;
    for (k, v) in vals {
        if !done.contains(&k) {
            lines.insert(insert_at, format!("{k} = {v}"));
            insert_at += 1;
        }
    }
    let mut s = lines.join("\n");
    if !s.ends_with('\n') {
        s.push('\n');
    }
    std::fs::write(path, s).map_err(|e| format!("写配置失败：{e}"))
}

/// 飞书 DM 斜杠命令（纯文本入口无 UI 控件，命令是刚需；Console/CLI 不需要）。
/// 返回 Some(回复文本) = 命令已处理，不再进 prompt
async fn dm_slash_command(
    state: &Arc<WebState>,
    agent: &str,
    dm_sess: Option<&Arc<DmSession>>,
    client: &Arc<FeishuClient>,
    open_id: &str,
    text: &str,
) -> Option<String> {
    let t = text.trim();
    if !t.starts_with('/') {
        return None;
    }
    let (cmd, arg) = match t.find(' ') {
        Some(i) => (&t[..i], t[i + 1..].trim()),
        None => (t, ""),
    };
    match cmd {
        "/help" => Some(
            "📋 可用命令：\n             /model [名字] — 查看/切换模型（不带参数列出可用档案）\n             /new — 开启新话题（历史自动归档）\n             /status — 当前模型/会话/通道状态\n             /help — 本帮助\n\n💡 回复进行中直接发消息 = ⚡ 插话转向（不用等它跑完）"
                .to_string(),
        ),
        "/status" => {
            let cfg = state.config.lock().expect("config 锁中毒").clone();
            let prof = if cfg.model.active_profile.is_empty() {
                "（单模型配置）".to_string()
            } else {
                cfg.model.active_profile.clone()
            };
            let ch_status = {
                let table = state.channels.lock().expect("通道锁中毒");
                table
                    .get(agent)
                    .map(|rt| channel_status_str(&rt.status))
                    .unwrap_or_else(|| "stopped".into())
            };
            let (busy, queued) = match dm_sess {
                Some(d) => (
                    d.session.try_lock().is_err(),
                    d.pending.lock().expect("排队锁中毒").len(),
                ),
                None => (false, 0),
            };
            Some(format!(
                "🤖 模型：{}（档案 {prof}）\n📭 会话：{}（排队 {queued} 条）\n🔌 通道：{ch_status}\n📦 沙箱：{}",
                cfg.current_model(),
                if busy { "处理中" } else { "空闲" },
                cfg.sandbox.level,
            ))
        }
        "/new" => {
            let Some(d) = dm_sess else {
                return Some("当前没有进行中的会话，直接发消息即可".into());
            };
            let mut guard = match d.session.try_lock() {
                Ok(g) => g,
                Err(_) => return Some("（正在处理消息，完成后在发 /new）".into()),
            };
            let cfg = dm_config(state, agent);
            match AgentSession::new(cfg) {
                Ok(s) => {
                    let sid = s.session_id().map(|x| x[..8.min(x.len())].to_string()).unwrap_or_default();
                    let steer = s.steer_handle();
                    *guard = s;
                    *d.steer_tx.lock().expect("steer 锁中毒") = steer;
                    Some(format!("🆕 新话题已开启（{sid}…）——历史已归档，我们从头开始"))
                }
                Err(e) => Some(format!("（新建会话失败：{e}）")),
            }
        }
        "/model" => {
            let apply = |name: &str| -> Result<String, String> {
                let mut cfg = state.config.lock().expect("config 锁中毒");
                cfg.apply_profile(name)?;
                if let Some(p) = cfg.source_path.clone() {
                    if let Err(e) = persist_active_profile(&p, name) {
                        eprintln!("[feishu] active_profile 持久化失败（本次会话仍生效）：{e}");
                    }
                }
                Ok(format!("模型已切换为 {}（{name}）", cfg.current_model()))
            };
            let msg = if arg.is_empty() {
                let cfg = state.config.lock().expect("config 锁中毒").clone();
                if cfg.model.profiles.is_empty() {
                    "（config.toml 未配置 [[model.profiles]]，只有单模型：{}）".replace("{}", cfg.current_model())
                } else {
                    let list: Vec<String> = cfg
                        .model
                        .profiles
                        .iter()
                        .map(|p| {
                            let cur = if p.name == cfg.model.active_profile { " ✓当前" } else { "" };
                            format!("· {} → {}{cur}", p.name, p.model)
                        })
                        .collect();
                    format!("可用模型档案：\n{}\n用法：/model 档案名", list.join("\n"))
                }
            } else {
                match apply(arg) {
                    Ok(m) => m,
                    Err(e) => return Some(format!("（{e}）")),
                }
            };
            if arg.is_empty() {
                return Some(msg);
            }
            // 已切换：空闲则原地重建会话（历史保留 + steer 通道刷新）
            let Some(d) = dm_sess else {
                return Some(format!("{msg}\n（当前无会话，下一条消息将以新模型开始）"));
            };
            let mut guard = match d.session.try_lock() {
                Ok(g) => g,
                Err(_) => return Some(format!("{msg}\n（正在处理消息，当前会话不动；下一条新消息用新模型）")),
            };
            let sid = guard.session_id().map(String::from);
            let cfg = dm_config(state, agent);
            let rebuilt = match sid {
                Some(id) => AgentSession::resume(cfg, &id),
                None => AgentSession::new(cfg),
            };
            match rebuilt {
                Ok(s) => {
                    let steer = s.steer_handle();
                    *guard = s;
                    *d.steer_tx.lock().expect("steer 锁中毒") = steer;
                    Some(format!("{msg}\n会话已按新模型重建（历史保留）"))
                }
                Err(e) => Some(format!("{msg}\n（会话重建失败：{e}；新消息将新建会话）")),
            }
        }
        _ => Some(format!("未知命令 {cmd}——/help 查看可用命令")),
    }
}

/// compact 档工具调用摘要：🔧 工具名 + 参数前 60 字（只发调用不发结果）
fn tool_call_summary(name: &str, arguments: &str) -> String {
    let mut args: String = arguments.chars().take(60).collect();
    if arguments.chars().count() > 60 {
        args.push('…');
    }
    format!("🔧 {name} {args}")
}

/// token 数紧凑格式：999 → 999，1234 → 1.2k，1234567 → 1.2m（状态行用）
fn fmt_tok(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// 通道状态 → 前端字符串（failed 附带原因）
fn channel_status_str(st: &ChannelStatus) -> String {
    match st {
        ChannelStatus::Connecting => "connecting".into(),
        ChannelStatus::Connected => "connected".into(),
        ChannelStatus::Reconnecting => "reconnecting".into(),
        ChannelStatus::Failed(e) => format!("failed: {e}"),
    }
}

/// 通道状态列表（init/state_json 的 "channels" 字段）：
/// 凡配过 [channel_feishu]（enabled 或填了 app_id）的分身都列出
fn channels_json(state: &WebState) -> Vec<Value> {
    let table = state.channels.lock().expect("通道锁中毒");
    agents::list_profiles()
        .iter()
        .filter(|p| p.channel_feishu.enabled || !p.channel_feishu.app_id.is_empty())
        .map(|p| {
            let status = table
                .get(&p.name)
                .map(|rt| channel_status_str(&rt.status))
                .unwrap_or_else(|| "stopped".to_string());
            json!({
                "agent": p.name,
                "enabled": p.channel_feishu.enabled,
                "app_id": p.channel_feishu.app_id,
                "status": status,
                // v0.10.1：策略化 DM 准入（前端下拉选择器用；老档案 allow_from 已归一）
                "dm_policy": p.channel_feishu.effective_policy().0,
                "policy_list": p.channel_feishu.effective_policy().1,
                // 过程可见性回显（缺了它前端下拉永远显示第一项，重存时静默覆盖成 none——8/25 实锤）
                "show_process": p.channel_feishu.show_process,
            })
        })
        .collect()
}

/// 启动/重载全部飞书通道（幂等对账，以 active 档案的 [channel_feishu] 为准）：
/// - active 且 enabled 且凭证齐备 → 未运行则起；(app_id, secret) 变了才重启
/// - 档案禁用/删除/不再是 active → 停掉并移除运行表项
/// main 不绑通道（v1 只遍历分身）
fn start_feishu_channels(state: &Arc<WebState>) {
    // 目标集合
    let mut desired: HashMap<String, ChannelFeishu> = HashMap::new();
    for p in agents::list_profiles() {
        let cf = &p.channel_feishu;
        if p.state == "active" && cf.enabled && !cf.app_id.is_empty() && !cf.app_secret.is_empty()
        {
            desired.insert(p.name.clone(), cf.clone());
        }
    }
    let mut table = state.channels.lock().expect("通道锁中毒");
    // ① 下线：不再需要的运行项
    let stale: Vec<String> = table
        .keys()
        .filter(|k| !desired.contains_key(*k))
        .cloned()
        .collect();
    for k in stale {
        if let Some(rt) = table.remove(&k) {
            rt.client.stop();
            // 停用后没有后续 start（永远不会有状态回调）→ 主动广播 stopped，
            // 前端状态点实时熄灭，不用刷新页面
            let _ = state.event_tx.send(json!({
                "t": "channel_status",
                "agent": k,
                "status": "stopped",
            }));
        }
    }
    // ② 上新/重启
    for (name, cf) in desired {
        let unchanged = table
            .get(&name)
            .map(|rt| rt.app_id == cf.app_id && rt.app_secret == cf.app_secret)
            .unwrap_or(false);
        if unchanged {
            continue; // 凭证没变 → 不动（幂等）
        }
        if let Some(old) = table.remove(&name) {
            old.client.stop();
        }
        let client = Arc::new(FeishuClient::new(FeishuConfig {
            app_id: cf.app_id.clone(),
            app_secret: cf.app_secret.clone(),
            domain: String::new(), // 空 = 默认 https://open.feishu.cn
        }));
        // on_message 是同步 Fn：里面 tokio::spawn 包异步工作
        let st = state.clone();
        let agent = name.clone();
        let cfg_msg = cf.clone();
        let client_msg = client.clone();
        let st_status = state.clone();
        let agent_status = name.clone();
        client.start(
            Box::new(move |dm: FeishuDm| {
                tokio::spawn(handle_dm(
                    st.clone(),
                    agent.clone(),
                    cfg_msg.clone(),
                    client_msg.clone(),
                    dm,
                ));
            }),
            Box::new(move |status: ChannelStatus| {
                // 注：初始状态即 Connecting，start() 内 set_status(Connecting) 相等不回调，
                // 这里的锁不会与 start_feishu_channels 持有的表锁形成重入
                {
                    let mut t = st_status.channels.lock().expect("通道锁中毒");
                    if let Some(rt) = t.get_mut(&agent_status) {
                        rt.status = status.clone();
                    }
                }
                let _ = st_status.event_tx.send(json!({
                    "t": "channel_status",
                    "agent": agent_status,
                    "status": channel_status_str(&status),
                }));
            }),
        );
        table.insert(
            name,
            FeishuRuntime {
                client,
                app_id: cf.app_id,
                app_secret: cf.app_secret,
                status: ChannelStatus::Connecting,
            },
        );
    }
}

/// 处理一条飞书 DM：白名单 → 文本校验 → 会话获取 → 排队/prompt → 回复
async fn handle_dm(
    state: Arc<WebState>,
    agent: String,
    cf: ChannelFeishu,
    client: Arc<FeishuClient>,
    dm: FeishuDm,
) {
    // a. DM 策略校验（deny_all/allow_all/allow_list/deny_list）
    if !feishu_allowed(&cf, &dm.open_id) {
        let _ = client
            .send_text(&dm.open_id, "（未授权：该机器人设置了访问限制）")
            .await;
        return;
    }
    // b. 非文本消息（图片等 text 为空）
    if dm.text.trim().is_empty() {
        let _ = client.send_text(&dm.open_id, "v1 只支持文本消息").await;
        return;
    }
    // b2. 斜杠命令（/model /new /status /help）：纯文本入口的控件，
    //     处理完直接回复，不进 prompt、不占会话；无会话时（首条 /help）也可用
    {
        let dm_sess0 = state
            .dm_sessions
            .lock()
            .expect("dm 锁中毒")
            .get(&dm_key(&agent, &dm.open_id))
            .cloned();
        if let Some(reply) = dm_slash_command(
            &state,
            &agent,
            dm_sess0.as_ref(),
            &client,
            &dm.open_id,
            &dm.text,
        )
        .await
        {
            let _ = client.send_text(&dm.open_id, &reply).await;
            return;
        }
    }
    // 收到确认：贴 Typing 表情（成功后回复完换 👍；失败记日志方便诊断权限缺失）
    if let Err(e) = client.add_reaction(&dm.message_id, "Typing").await {
        eprintln!("[feishu] Typing 表情贴失败（检查 im:message.reaction:write 权限是否开通并重新发布）：{e}");
    }
    // c. 会话获取：每 (agent, open_id) 一个持久 AgentSession
    let key = dm_key(&agent, &dm.open_id);
    let existing = state
        .dm_sessions
        .lock()
        .expect("dm 锁中毒")
        .get(&key)
        .cloned();
    let dm_sess = match existing {
        Some(d) => d,
        None => {
            // 锁外创建：AgentSession::new 可能做 MCP 冷启动（秒级），不能卡 dm_sessions 锁。
            // 飞书会话 = 该 agent 的一个普通会话：config 快照 + apply_persona
            // （session.dir 指向 ~/.r2/agents/<agent>/sessions，历史可审计、Console 可见）
            let cfg = dm_config(&state, &agent);
            let session = match AgentSession::new(cfg) {
                Ok(s) => s,
                Err(e) => {
                    let _ = client
                        .send_text(&dm.open_id, &format!("（创建会话失败：{e}）"))
                        .await;
                    return;
                }
            };
            let mut map = state.dm_sessions.lock().expect("dm 锁中毒");
            // 并发下同 key 只留一个（后到者丢弃自己建的，复用先入者）
            map.entry(key)
                .or_insert_with(|| {
                    let steer = session.steer_handle();
                    Arc::new(DmSession {
                        session: Mutex::new(session),
                        pending: StdMutex::new(VecDeque::new()),
                        steer_tx: StdMutex::new(steer),
                    })
                })
                .clone()
        }
    };
    // g. prompt 在途 → steer 插话（对齐 Console 语义：在途消息=转向指令，
    //    打断当前流注入 [用户中途指令]）；通道满/关闭才回落排队 FIFO
    let mut guard = match dm_sess.session.try_lock() {
        Ok(g) => g,
        Err(_) => {
            if dm_sess
                .steer_tx
                .lock()
                .expect("steer 锁中毒")
                .try_send(dm.text.clone())
                .is_ok()
            {
                let _ = client
                    .send_text(&dm.open_id, "⚡ 已插入当前回复（转向中）")
                    .await;
                return;
            }
            // 队列入队/判满在锁内做完即放锁（std guard 不可跨 await）
            let full = {
                let mut q = dm_sess.pending.lock().expect("排队锁中毒");
                if q.len() >= DM_QUEUE_MAX {
                    true
                } else {
                    q.push_back((dm.text, dm.message_id.clone()));
                    false
                }
            };
            if full {
                let _ = client.send_text(&dm.open_id, "（忙，请稍后再发）").await;
            }
            return;
        }
    };
    // d-f. 串行跑本 DM 的消息（含排队续发，同 spawn_prompt 的收尾续发语义）
    let mut item = (dm.text, dm.message_id.clone());
    loop {
        let (text, mid) = &item;
        run_dm_prompt(&state, &agent, &cf, &client, &dm.open_id, mid, &mut guard, text).await;
        // 收尾窗口落进来的 steer（本轮 run 没来得及消费）转为排队补投——用户消息绝不静默丢
        for x in guard.take_stale_steers() {
            dm_sess
                .pending
                .lock()
                .expect("排队锁中毒")
                .push_back((x, String::new()));
        }
        let next = dm_sess.pending.lock().expect("排队锁中毒").pop_front();
        match next {
            Some(x) => item = x,
            None => break,
        }
    }
}

/// 执行一条 DM prompt：事件转发到 Console（带 from_channel 标记）+ 按 show_process
/// 分档回传过程消息，完成后最终回复发回飞书（FeishuClient 自带 4000 字分片）
async fn run_dm_prompt(
    state: &Arc<WebState>,
    agent: &str,
    cf: &ChannelFeishu,
    client: &Arc<FeishuClient>,
    open_id: &str,
    message_id: &str,
    session: &mut AgentSession,
    text: &str,
) {
    let mode = show_process_mode(&cf.show_process);
    // ① 流式卡片（Card Kit）：主区=最终回复 markdown 流式渲染，note=灰色小字思考流。
    //    建/发卡任一步失败 → 降级纯文本路径（主链路不受影响），日志留座
    //    v0.10.3：note 区两段生命——生成中=过程流，完成后=状态行（模型/token/缓存/费用）
    //    （none 档也建 note：生成中留空，收尾时变身状态行）
    let card_holder: Arc<tokio::sync::Mutex<Option<r2_core::channels::StreamingCard>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    match client.start_streaming_card(open_id, true).await {
        Ok(c) => *card_holder.lock().await = Some(c),
        Err(e) => eprintln!("[feishu] 流式卡片创建失败，降级纯文本：{e}"),
    }
    let has_card = card_holder.try_lock().map(|g| g.is_some()).unwrap_or(false);
    // 用量快照：UsageUpdate 在 Done 前发出，消费任务写入，收尾时读出拼状态行
    let usage_snap: Arc<StdMutex<Option<r2_core::types::UsageStats>>> =
        Arc::new(StdMutex::new(None));
    // ② 事件消费任务：转发 Console + 卡片流式更新（降级时走纯文本过程消息）
    let mut rx = session.subscribe();
    let etx = state.event_tx.clone();
    let client2 = client.clone();
    let oid = open_id.to_string();
    let agent2 = agent.to_string();
    let cards = card_holder.clone();
    let usnap = usage_snap.clone();
    let consumer = tokio::spawn(async move {
        let mut thinking_buf = String::new();
        let mut main_buf = String::new();
        loop {
            match rx.recv().await {
                Ok(evt) => {
                    // Console 实时可见飞书来的对话（含过程）
                    let _ = etx.send(json!({
                        "t": "event",
                        "evt": rpc::event_json(&evt),
                        "from_channel": "feishu",
                        "agent": agent2,
                    }));
                    let mut card_guard = cards.lock().await;
                    match &evt {
                        // 主区流式：助手文本增量实时上屏（打字机效果）
                        AgentEvent::MessageUpdate(s) => {
                            main_buf.push_str(s);
                            if let Some(c) = card_guard.as_mut() {
                                let _ = c.update_content(&main_buf).await;
                            }
                        }
                        // note 小字：full=思考流追加；compact=工具状态单行替换
                        AgentEvent::Thinking(t) if mode == ShowProcess::Full => {
                            thinking_buf.push_str(t);
                            if let Some(c) = card_guard.as_mut() {
                                // 显示层截尾（思考流可能很长，PUT 全量代价高）
                                let tail: String =
                                    thinking_buf.chars().rev().take(1200).collect::<Vec<_>>()
                                        .into_iter().rev().collect();
                                let _ = c.update_note(&tail).await;
                            } else if thinking_buf.chars().count() >= DM_THINKING_FLUSH_CHARS {
                                let chunk = std::mem::take(&mut thinking_buf);
                                let _ = client2.send_text(&oid, &format!("💭 {chunk}")).await;
                            }
                        }
                        AgentEvent::ToolCall { name, arguments }
                            if mode >= ShowProcess::Compact =>
                        {
                            if let Some(c) = card_guard.as_mut() {
                                if mode == ShowProcess::Compact {
                                    // 单行状态：当前在干嘛（实时替换，不刷屏）
                                    let _ = c.update_note(&tool_call_summary(name, arguments)).await;
                                } else {
                                    thinking_buf.push('\n');
                                    thinking_buf.push_str(&tool_call_summary(name, arguments));
                                    let tail: String =
                                        thinking_buf.chars().rev().take(1200).collect::<Vec<_>>()
                                            .into_iter().rev().collect();
                                    let _ = c.update_note(&tail).await;
                                }
                            } else {
                                let _ = client2
                                    .send_text(&oid, &tool_call_summary(name, arguments))
                                    .await;
                            }
                        }
                        AgentEvent::UsageUpdate(u) => {
                            *usnap.lock().unwrap() = Some(u.clone());
                        }
                        // ⚡ 插话反馈：note 区瞬时提示（finalize 时被状态行替换）
                        AgentEvent::Steered(_) => {
                            if let Some(c) = card_guard.as_mut() {
                                let _ = c.update_note("⚡ 收到用户中途指令，转向中…").await;
                            }
                        }
                        AgentEvent::Done { .. } => {
                            // 收尾：flush 残余思考流（降级路径）后退出
                            if mode == ShowProcess::Full && !thinking_buf.is_empty() {
                                if card_guard.is_none() {
                                    let chunk = std::mem::take(&mut thinking_buf);
                                    let _ = client2.send_text(&oid, &format!("💭 {chunk}")).await;
                                }
                            }
                            break;
                        }
                        AgentEvent::Error(_) => break,
                        _ => {}
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    // ③ prompt 主体：deadline 兜底 + none 档心跳（select 循环，心跳不打断 prompt）
    let deadline = tokio::time::Instant::now() + Duration::from_secs(DM_PROMPT_TIMEOUT_SECS);
    let mut prompt_fut = Box::pin(session.prompt(text));
    let timed_out;
    let result = loop {
        tokio::select! {
            r = &mut prompt_fut => {
                timed_out = false;
                break r;
            }
            _ = tokio::time::sleep_until(deadline) => {
                timed_out = true;
                break Ok(String::new());
            }
            _ = tokio::time::sleep(Duration::from_secs(DM_HEARTBEAT_SECS)) => {
                // none 档且无卡片时防"假死"；有卡片时主区/note 自带活性
                if mode == ShowProcess::None && !has_card {
                    let _ = client.send_text(open_id, "⏳ 仍在处理中…").await;
                }
            }
        }
    };
    if !timed_out {
        // 等消费任务收尾（flush 残余思考流）；Done/Error 后它自行退出
        let _ = tokio::time::timeout(Duration::from_secs(2), consumer).await;
    } else {
        consumer.abort();
    }
    // ④ 收尾：卡片路径 finalize（主区定格最终回复 + note 变身状态行 + 关流式）；降级路径纯文本
    let succeeded = !timed_out && result.is_ok();
    // 状态行：模型/token/缓存命中/费用（estimate_cost 与 Console 顶栏同源）
    let model_name = state.config.lock().expect("config 锁中毒").current_model().to_string();
    let usage_now = usage_snap.lock().unwrap().clone();
    let status_line = match &usage_now {
        Some(u) => {
            let cache_pct = if u.input_tokens > 0 {
                u.cached_tokens * 100 / u.input_tokens
            } else {
                0
            };
            let cost = r2_core::models::estimate_cost(&model_name, u)
                .map(|c| format!(" · ≈¥{c:.2}"))
                .unwrap_or_default();
            format!(
                "🤖 {model_name} · ↑{} ↓{} tok · 💾 cache {cache_pct}%{cost}",
                fmt_tok(u.input_tokens),
                fmt_tok(u.output_tokens)
            )
        }
        None => format!("🤖 {model_name}"),
    };
    let card_opt = card_holder.lock().await.take();
    if let Some(mut c) = card_opt {
        if timed_out {
            let _ = c
                .finalize(
                    &format!("（超时 {DM_PROMPT_TIMEOUT_SECS} 秒，未能完成回复）"),
                    Some(status_line.as_str()),
                )
                .await;
        } else {
            match &result {
                Ok(reply) => {
                    if let Err(e) = c.finalize(reply, Some(status_line.as_str())).await {
                        eprintln!("[feishu] 卡片收尾失败，改发文本：{e}");
                        let _ = client.send_text(open_id, reply).await;
                    }
                }
                Err(e) => {
                    let brief: String = e.chars().take(200).collect();
                    let _ = c
                        .finalize(&format!("（出错：{brief}）"), Some(status_line.as_str()))
                        .await;
                }
            }
        }
    } else if timed_out {
        let _ = client
            .send_text(
                open_id,
                &format!("（超时 {DM_PROMPT_TIMEOUT_SECS} 秒，未能完成回复）"),
            )
            .await;
    } else {
        match result {
            Ok(reply) => {
                let _ = client.send_text(open_id, &reply).await;
            }
            Err(e) => {
                let brief: String = e.chars().take(200).collect();
                let _ = client.send_text(open_id, &format!("（出错：{brief}）")).await;
            }
        }
    }
    // 成功完成贴 👍（换掉收到时的 Typing；失败记日志方便诊断权限缺失）
    // （result 在上方 match 已部分 move，成功标志提前存）
    if succeeded && !message_id.is_empty() {
        if let Err(e) = client.add_reaction(message_id, "THUMBSUP").await {
            eprintln!("[feishu] 👍 表情贴失败：{e}");
        }
    }
}

/// 通道自检：用档案配置起临时 client 连一次，等首个 Connected/Failed（≤10s）后立即停掉
async fn test_feishu_channel(agent: &str) -> (bool, String) {
    let Some(p) = agents::load_profile(agent) else {
        return (false, format!("agent 不存在：{agent}"));
    };
    let cf = &p.channel_feishu;
    if cf.app_id.is_empty() || cf.app_secret.is_empty() {
        return (false, "app_id/app_secret 未配置".into());
    }
    let client = FeishuClient::new(FeishuConfig {
        app_id: cf.app_id.clone(),
        app_secret: cf.app_secret.clone(),
        domain: String::new(),
    });
    let (tx, mut rx) = mpsc::channel::<ChannelStatus>(8);
    client.start(
        Box::new(|_| {}),
        Box::new(move |st| {
            let _ = tx.try_send(st);
        }),
    );
    let verdict = tokio::time::timeout(Duration::from_secs(CHANNEL_TEST_TIMEOUT_SECS), async {
        while let Some(st) = rx.recv().await {
            match st {
                ChannelStatus::Connected => return (true, "连接成功".to_string()),
                ChannelStatus::Failed(e) => return (false, e),
                _ => {} // Connecting/Reconnecting 继续等
            }
        }
        (false, "连接提前结束".to_string())
    })
    .await
    .unwrap_or_else(|_| (false, "10 秒内未连上（超时）".to_string()));
    client.stop();
    verdict
}

// ---------- 群聊调度引擎（v0.9.1 会议室） ----------
//
// 一个群 = {session_dir}/group-<uuid>/（group.json + stream.jsonl，r2_core::groups 地基）。
// 调度器 run_group_turn 是 tokio 后台任务：轮流为每个成员开独立临时 AgentSession 发言，
// 事件逐条 append 到 stream 并实时广播；pause/stop 经 group.json 状态快照对比 + JoinHandle::abort 生效。

/// 群上下文带入最近事件条数
const GROUP_CTX_EVENTS: usize = 40;
/// 群上下文字符上限（防 prompt 爆炸）
const GROUP_CTX_CHARS: usize = 12_000;
/// 每位成员发言间隔（给前端渲染喘息）
const GROUP_TURN_GAP_MS: u64 = 300;
/// 连续失败上限：达到后自动 paused
const GROUP_MAX_FAIL_STREAK: u32 = 3;
/// 单轮发言超时（秒）
const GROUP_TURN_TIMEOUT_SECS: u64 = 300;

fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 群 sid 合法性：group-<uuid> 形态，防路径穿越
fn valid_group_sid(sid: &str) -> bool {
    sid.strip_prefix("group-")
        .map(|rest| !rest.is_empty() && valid_name(rest))
        .unwrap_or(false)
}

/// 群目录：{session_dir}/group-<uuid>
fn group_dir_path(session_dir: &str, sid: &str) -> PathBuf {
    Path::new(session_dir).join(sid)
}

/// 建群：自动含 owner（main=人），目录落为 group-<uuid>，stream 首条 StateChange idle。
/// 返回 (sid, config)。成员必须是已存在的 agent 分身。
fn do_group_create(
    session_dir: &str,
    title: &str,
    members: &[(String, String)],
) -> Result<(String, groups::GroupConfig), String> {
    if title.trim().is_empty() {
        return Err("ERROR: 群标题不能为空".into());
    }
    if members.is_empty() {
        return Err("ERROR: 至少 1 个 agent 分身".into());
    }
    let mut list: Vec<(&str, &str)> = vec![(MAIN, "小JOE（主Agent）")];
    for (n, d) in members {
        if n == MAIN {
            return Err("ERROR: main（主Agent）由系统自动加入，无需指定".into());
        }
        if agents::load_profile(n).is_none() {
            return Err(format!("ERROR: agent 不存在：{n}"));
        }
        list.push((n, d));
    }
    let root = Path::new(session_dir);
    std::fs::create_dir_all(root).map_err(|e| format!("ERROR: 创建会话目录失败：{e}"))?;
    let g = groups::create_group(root, title, &list)?;
    let sid = format!("group-{}", g.id);
    std::fs::rename(groups::group_dir(root, &g.id), root.join(&sid))
        .map_err(|e| format!("ERROR: 群目录改名失败：{e}"))?;
    let dir = root.join(&sid);
    groups::append_event(
        &dir,
        &GroupEvent::StateChange {
            from_state: "idle".into(),
            to_state: "idle".into(),
            ts: now_ts(),
        },
    )?;
    Ok((sid, g))
}

/// 群条目注入会话列表：group-<id> 目录 → kind:"group"
fn group_entries(session_dir: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let dir = Path::new(session_dir);
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("group-") {
                continue;
            }
            let Some(g) = groups::load_group(&path) else {
                continue;
            };
            out.push(json!({
                "id": name,
                "title": g.title,
                "kind": "group",
                "members_count": g.members.len(),
                "state": g.state,
                "round": g.round,
                "used_tokens": g.used_tokens,
                "preview": g.title,
                "last_ts": g.created_ts,
            }));
        }
    }
    out
}

/// 普通会话 + 群条目合并列表
fn sessions_with_groups(session_dir: &str) -> Vec<Value> {
    let mut list: Vec<Value> = session::list_sessions(session_dir)
        .unwrap_or_default()
        .iter()
        .map(summary_json)
        .collect();
    list.extend(group_entries(session_dir));
    // 群与普通会话按最近活动混排（新→旧）：刚聊过的群排最前，
    // 不再被埋在 256 个普通会话后面（8/23 大Joe 实测病灶：群在列表里消失）
    list.sort_by_key(|v| {
        std::cmp::Reverse(v.get("last_ts").and_then(|x| x.as_u64()).unwrap_or(0))
    });
    list
}

/// 广播一条群事件（前端实时渲染）
fn broadcast_group_event(state: &WebState, sid: &str, event: &GroupEvent) {
    let _ = state
        .event_tx
        .send(json!({"t": "group_event", "id": sid, "event": event}));
}

/// 广播群档案快照（speaking/round/token 账等变化）
fn broadcast_group_state(state: &WebState, sid: &str, g: &groups::GroupConfig) {
    let _ = state
        .event_tx
        .send(json!({"t": "group_state", "id": sid, "group": g}));
}

/// append 事件并广播
fn append_and_broadcast(state: &WebState, sid: &str, dir: &Path, event: &GroupEvent) -> Result<(), String> {
    groups::append_event(dir, event)?;
    broadcast_group_event(state, sid, event);
    Ok(())
}

/// set_state 后广播其内部追加的 StateChange（stream 末条）
fn broadcast_last_event(state: &WebState, sid: &str, dir: &Path) {
    if let Some(ev) = groups::read_stream(dir).last().cloned() {
        broadcast_group_event(state, sid, &ev);
    }
}

/// 文本里的 @成员名（只认群成员里的分身，人 main 不参与被点名）
fn parse_mentions(text: &str, g: &groups::GroupConfig) -> Vec<String> {
    // main（主Agent）也是同事，可被 @ 点名；人=user 不在成员表，天然不可被点
    let mut names: Vec<&str> = g.members.iter().map(|m| m.name.as_str()).collect();
    // 最长匹配优先，防前缀误伤（@cfo2 不被 @cfo 吃掉）
    names.sort_by(|a, b| b.len().cmp(&a.len()));
    let mut out: Vec<String> = Vec::new();
    for n in names {
        if text.contains(&format!("@{n}")) && !out.iter().any(|x| x == n) {
            out.push(n.to_string());
        }
    }
    out
}

/// 收敛信号
fn has_done(text: &str) -> bool {
    text.contains("[DONE]")
}

/// 解析 lead 派卡指令：[DELEGATE @成员名 任务描述] → (成员名, 任务)
fn parse_delegate(text: &str) -> Option<(String, String)> {
    let start = text.find("[DELEGATE")?;
    let inner = &text[start + "[DELEGATE".len()..];
    let end = inner.find(']')?;
    let inner = inner[..end].trim().strip_prefix('@')?.trim();
    let mut it = inner.splitn(2, char::is_whitespace);
    let to = it.next()?.to_string();
    let task = it.next().unwrap_or("").trim().to_string();
    if to.is_empty() || task.is_empty() {
        return None;
    }
    Some((to, task))
}

/// 与 next_speaker 同构的有效轮序（lead 挪队尾）
fn effective_turn_order(g: &groups::GroupConfig) -> Vec<String> {
    let mut order = g.settings.turn_order.clone();
    if let Some(lead) = g
        .members
        .iter()
        .find(|m| m.role == "lead")
        .map(|m| m.name.clone())
    {
        if let Some(pos) = order.iter().position(|n| *n == lead) {
            let l = order.remove(pos);
            order.push(l);
        }
    }
    order
}

/// 点名跳序：返回应写入 speaking 的值，使下一次 next_speaker(g, Some(main)) 命中 target
fn speaking_to_force_next(g: &groups::GroupConfig, target: &str) -> Option<String> {
    let order = effective_turn_order(g);
    let pos = order.iter().position(|n| n == target)?;
    if order.len() < 2 {
        return None;
    }
    Some(order[(pos + order.len() - 1) % order.len()].clone())
}

/// 单事件文本化（群上下文用）
fn event_text(e: &GroupEvent) -> String {
    match e {
        // 过程事件是给人看的可观测数据，不进 LLM 群上下文（防膨胀）；
        // 空串在 stream_context 拼接时破过滤掉
        GroupEvent::MemberActivity { .. } => String::new(),
        GroupEvent::Message { from, text, .. } => format!("{from}: {text}"),
        GroupEvent::Mention {
            from,
            text,
            mentions,
            ..
        } => format!("{from}: {text}（@{}）", mentions.join(" ")),
        GroupEvent::Subtask {
            from,
            to,
            prompt,
            state,
            ..
        } => format!("[{from} → {to}] 子任务（{state}）：{prompt}"),
        GroupEvent::Summary { text, .. } => format!("[小结] {text}"),
        GroupEvent::StateChange {
            from_state,
            to_state,
            ..
        } => format!("[状态] {from_state} → {to_state}"),
        GroupEvent::Error { text, .. } => format!("[错误] {text}"),
    }
}

/// 群上下文：最近 max_events 条、max_chars 字符上限（从头部截断保最近）
fn stream_context(dir: &Path, max_events: usize, max_chars: usize) -> String {
    let events = groups::read_stream(dir);
    // 过程事件（思考/工具）不进上下文：结论已沉淀在 Message/Summary 里
    let events: Vec<&GroupEvent> = events
        .iter()
        .filter(|e| !matches!(e, GroupEvent::MemberActivity { .. }))
        .collect();
    let start = events.len().saturating_sub(max_events);
    let s = events[start..].iter().map(|e| event_text(e)).filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n");
    if s.chars().count() <= max_chars {
        return s;
    }
    let kept: String = {
        let mut v: Vec<char> = s.chars().rev().take(max_chars).collect();
        v.reverse();
        v.into_iter().collect()
    };
    format!("……（前文省略）\n{kept}")
}

/// 组装成员发言 prompt（纯函数，便于测试）
fn build_member_prompt(g: &groups::GroupConfig, dir: &Path, name: &str) -> String {
    let roster = g
        .members
        .iter()
        .map(|m| {
            let disp = if m.display_name.is_empty() {
                m.name.clone()
            } else {
                m.display_name.clone()
            };
            format!("- {}（{}，{}）", m.name, disp, m.role)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let lead = g
        .members
        .iter()
        .find(|m| m.role == "lead")
        .map(|m| m.name.clone())
        .unwrap_or_else(|| "无".to_string());
    let is_lead = g.members.iter().any(|m| m.name == name && m.role == "lead");
    let topic = g
        .task
        .as_ref()
        .map(|t| t.topic.clone())
        .unwrap_or_else(|| "自由讨论".to_string());
    let budget_left = g.settings.budget_tokens.saturating_sub(g.used_tokens);
    let ctx = stream_context(dir, GROUP_CTX_EVENTS, GROUP_CTX_CHARS);
    format!(
        "你正在参加群聊「{title}」，你的身份是成员 {name}{lead_note}。\n\n\
         【群成员】\n{roster}\n\n\
         【当前 lead】{lead}\n\
         【主题】{topic}\n\
         【进度】第 {round}/{max_rounds} 轮，剩余 token 预算约 {budget_left}\n\n\
         【最近讨论记录】\n{ctx}\n\n\
         【规则】\n\
         1. 你是群聊中的 {name}，接续上下文讨论，不要重复别人说过的话。\n\
         2. 可以 @成员名 点名追问（被点名者将优先发言）。\n\
         3. 认为讨论已收敛时，在回复末尾单独输出 [DONE]。\n\
         4. 如果你是 lead：先做规划，可用 [DELEGATE @成员名 任务描述] 派卡（需人批准后执行）。\n\
         5. 回复控制在 300 字以内，直接说内容，不要客套。",
        title = g.title,
        name = name,
        lead_note = if is_lead { "（lead）" } else { "" },
        round = g.round + 1,
        max_rounds = g.settings.max_rounds,
    )
}

/// 小结 prompt（main persona 读全流收敛）
fn build_summary_prompt(g: &groups::GroupConfig, dir: &Path) -> String {
    let ctx = stream_context(dir, GROUP_CTX_EVENTS, GROUP_CTX_CHARS);
    format!(
        "以下是群聊「{title}」（主题：{topic}）的讨论记录：\n\n{ctx}\n\n\
         请输出不超过 300 字的小结：达成的结论、未决分歧、后续行动。直接给小结正文。",
        title = g.title,
        topic = g
            .task
            .as_ref()
            .map(|t| t.topic.clone())
            .unwrap_or_else(|| "自由讨论".to_string()),
    )
}

/// token 记账（落盘）；返回 false = 超预算
fn group_add_tokens(dir: &Path, n: u64) -> bool {
    let Some(mut g) = groups::load_group(dir) else {
        return true;
    };
    let ok = groups::add_tokens(&mut g, n);
    let _ = groups::save_group(dir, &g);
    ok
}

/// 预算耗尽：paused + Error 事件
fn group_pause_budget(state: &WebState, sid: &str, dir: &Path) {
    if let Ok(g) = groups::set_state(dir, "paused") {
        broadcast_last_event(state, sid, dir);
        broadcast_group_state(state, sid, &g);
    }
    let _ = append_and_broadcast(state, sid, dir, &GroupEvent::error("预算耗尽，等待续期"));
}

/// 失败记账：append Error；连续失败达上限 → paused，返回 true = 调度器应退出
fn group_note_failure(state: &WebState, sid: &str, dir: &Path, streak: u32, err: &str) -> bool {
    let _ = append_and_broadcast(state, sid, dir, &GroupEvent::error(err));
    if streak < GROUP_MAX_FAIL_STREAK {
        return false;
    }
    if let Ok(g) = groups::set_state(dir, "paused") {
        broadcast_last_event(state, sid, dir);
        broadcast_group_state(state, sid, &g);
    }
    let _ = append_and_broadcast(
        state,
        sid,
        dir,
        &GroupEvent::error("连续 3 次模型调用失败，群已自动暂停"),
    );
    true
}

/// 成员过程事件转发：AgentSession 事件流 → MemberActivity（全量存档 + 实时广播）。
/// thinking 增量逐条转发（前端合并渲染，回放同样合并）；工具调用/结果带完整参数与输出。
/// 挂在会话生命周期上，会话 drop 后自动退出；返回 JoinHandle 供调用方在轮次结束时 abort。
fn spawn_activity_forward(
    state: &Arc<WebState>,
    sid: &str,
    dir: &Path,
    from: &str,
    session: &AgentSession,
) -> tokio::task::JoinHandle<()> {
    let mut rx = session.subscribe();
    let state = state.clone();
    let sid = sid.to_string();
    let dir = dir.to_path_buf();
    let from = from.to_string();
    tokio::spawn(async move {
        while let Ok(e) = rx.recv().await {
            let ev = match e {
                r2_core::AgentEvent::Thinking(t) => {
                    GroupEvent::member_activity(&from, "thinking", &t)
                }
                r2_core::AgentEvent::ToolCall { name, arguments } => {
                    let payload =
                        serde_json::json!({"name": name, "arguments": arguments}).to_string();
                    GroupEvent::member_activity(&from, "tool_call", &payload)
                }
                r2_core::AgentEvent::ToolResult { name, output } => {
                    let payload =
                        serde_json::json!({"name": name, "output": output}).to_string();
                    GroupEvent::member_activity(&from, "tool_result", &payload)
                }
                _ => continue,
            };
            let _ = append_and_broadcast(&state, &sid, &dir, &ev);
        }
    })
}

/// 收敛：main persona 读全流生成 Summary → state summarized
async fn run_group_summary(state: &Arc<WebState>, sid: &str) {
    let dir = group_dir_path(&state.session_dir, sid);
    let Some(g) = groups::load_group(&dir) else {
        return;
    };
    let mut cfg = state.config.lock().expect("config 锁中毒").clone();
    apply_persona(MAIN, &mut cfg);
    // 小结轮同样写入群 turns/（不污染主会话列表）
    cfg.session.dir = dir.join("turns").to_string_lossy().to_string();
    let _ = std::fs::create_dir_all(&cfg.session.dir);
    let prompt = build_summary_prompt(&g, &dir);
    let outcome = match AgentSession::new(cfg) {
        Ok(mut s) => {
            // 小结轮过程同样全量可观测（from=main）
            let fwd = spawn_activity_forward(state, sid, &dir, MAIN, &s);
            let r = tokio::time::timeout(
                std::time::Duration::from_secs(GROUP_TURN_TIMEOUT_SECS),
                s.prompt(&prompt),
            )
            .await;
            fwd.abort();
            match r {
                Ok(Ok(text)) => Some(text),
                Ok(Err(e)) => {
                    let _ = append_and_broadcast(state, sid, &dir, &GroupEvent::error(&format!("小结生成失败：{e}")));
                    None
                }
                Err(_) => {
                    let _ = append_and_broadcast(state, sid, &dir, &GroupEvent::error("小结生成超时"));
                    None
                }
            }
        }
        Err(e) => {
            let _ = append_and_broadcast(state, sid, &dir, &GroupEvent::error(&format!("小结会话创建失败：{e}")));
            None
        }
    };
    if let Some(text) = outcome {
        let _ = append_and_broadcast(state, sid, &dir, &GroupEvent::summary(&text));
    }
    if let Ok(g2) = groups::set_state(&dir, "summarized") {
        broadcast_last_event(state, sid, &dir);
        broadcast_group_state(state, sid, &g2);
    }
    broadcast_sessions(state);
}

/// 群调度器主循环（tokio 后台任务；seq 用于退出清理防误删新一代任务）
async fn run_group_turn(state: Arc<WebState>, sid: String, seq: u64) {
    let dir = group_dir_path(&state.session_dir, &sid);
    let mut fail_streak: u32 = 0;
    // 本轮已发言的分身集合：全员覆盖 = 一轮结束
    let mut round_speakers: std::collections::HashSet<String> = std::collections::HashSet::new();
    loop {
        // 每步对比 group.json 实时状态：pause/stop 立即生效
        let Some(mut g) = groups::load_group(&dir) else {
            break;
        };
        if g.state != "discussing" {
            break;
        }
        // main（主Agent）也是群里的同事：与分身一起轮流发言（人=user 只经输入框插话）
        let name = match groups::next_speaker(&g, None) {
            Some(n) => n,
            None => {
                // speaking 占住唯一候选 → 清 speaking 重取一次
                g.speaking = None;
                let _ = groups::save_group(&dir, &g);
                match groups::next_speaker(&g, None) {
                    Some(n) => n,
                    None => break,
                }
            }
        };
        g.speaking = Some(name.clone());
        let _ = groups::save_group(&dir, &g);
        broadcast_group_state(&state, &sid, &g);

        // 独立临时 AgentSession（不碰前台 state.agent 槽位），成员自己的 persona/work_dir
        let mut cfg = state.config.lock().expect("config 锁中毒").clone();
        apply_persona(&name, &mut cfg);
        // 群轮次会话写入群目录 turns/：不污染主会话列表，也留每轮原始记录可审计
        cfg.session.dir = dir.join("turns").to_string_lossy().to_string();
        let _ = std::fs::create_dir_all(&cfg.session.dir);
        let prompt = build_member_prompt(&g, &dir, &name);
        let mut session = match AgentSession::new(cfg) {
            Ok(s) => s,
            Err(e) => {
                fail_streak += 1;
                if group_note_failure(&state, &sid, &dir, fail_streak, &format!("成员 {name} 会话创建失败：{e}")) {
                    break;
                }
                continue;
            }
        };
        // usage 捕获（UsageUpdate 在 Done 前发出，会话级累计；本会话只跑一轮，直接取值）
        // 过程事件捕获（全量存档）：思考/工具调用/结果 → MemberActivity 进 stream.jsonl + 实时广播
        let usage = Arc::new(StdMutex::new(r2_core::types::UsageStats::default()));
        let usage2 = usage.clone();
        let mut rx = session.subscribe();
        let fwd = spawn_activity_forward(&state, &sid, &dir, &name, &session);
        let cap = tokio::spawn(async move {
            while let Ok(e) = rx.recv().await {
                if let r2_core::AgentEvent::UsageUpdate(u) = e {
                    *usage2.lock().expect("usage 锁中毒") = u;
                }
            }
        });
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(GROUP_TURN_TIMEOUT_SECS),
            session.prompt(&prompt),
        )
        .await;
        cap.abort();
        fwd.abort();

        let text = match result {
            Ok(Ok(text)) => text,
            Ok(Err(e)) => {
                fail_streak += 1;
                if group_note_failure(&state, &sid, &dir, fail_streak, &format!("成员 {name} 发言失败：{e}")) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(GROUP_TURN_GAP_MS)).await;
                continue;
            }
            Err(_) => {
                fail_streak += 1;
                if group_note_failure(&state, &sid, &dir, fail_streak, &format!("成员 {name} 发言超时")) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(GROUP_TURN_GAP_MS)).await;
                continue;
            }
        };
        fail_streak = 0;

        // 发言入流（@点名 → Mention 事件）
        let mentions = parse_mentions(&text, &g);
        let ev = if mentions.is_empty() {
            GroupEvent::message(&name, &text)
        } else {
            GroupEvent::mention(&name, &text, mentions.clone())
        };
        let _ = append_and_broadcast(&state, &sid, &dir, &ev);

        // lead 派卡指令 → Subtask pending（前端审批卡；批准走 group_subtask_approve）
        if let Some((to, task)) = parse_delegate(&text) {
            if g.members.iter().any(|m| m.name == to) {
                let _ = append_and_broadcast(
                    &state,
                    &sid,
                    &dir,
                    &GroupEvent::Subtask {
                        from: name.clone(),
                        to,
                        prompt: task,
                        ts: now_ts(),
                        state: "pending".into(),
                    },
                );
            }
        }

        // 点名跳序：被 @ 者优先下一位
        if let Some(target) = mentions.first() {
            if *target != name {
                if let Some(mut g3) = groups::load_group(&dir) {
                    if let Some(sp) = speaking_to_force_next(&g3, target) {
                        g3.speaking = Some(sp);
                        let _ = groups::save_group(&dir, &g3);
                    }
                }
            }
        }

        // token 记账：usage 优先，无则按字符数/4 估算（prompt + 回复）
        let u = usage.lock().expect("usage 锁中毒").clone();
        let tokens = if u.input_tokens + u.output_tokens > 0 {
            u.input_tokens + u.output_tokens
        } else {
            (prompt.chars().count() + text.chars().count()) as u64 / 4
        };
        if !group_add_tokens(&dir, tokens) {
            group_pause_budget(&state, &sid, &dir);
            break;
        }

        // 轮次推进：全员（含 main 主Agent）都发过言 = 一轮结束
        let member_count = g.members.len();
        round_speakers.insert(name.clone());
        if round_speakers.len() >= member_count {
            round_speakers.clear();
            if let Some(mut g4) = groups::load_group(&dir) {
                g4.round += 1;
                let _ = groups::save_group(&dir, &g4);
                broadcast_group_state(&state, &sid, &g4);
                if g4.round + 1 > g4.settings.max_rounds {
                    run_group_summary(&state, &sid).await;
                    break;
                }
            }
        }

        // [DONE] 收敛
        if has_done(&text) {
            run_group_summary(&state, &sid).await;
            break;
        }

        tokio::time::sleep(std::time::Duration::from_millis(GROUP_TURN_GAP_MS)).await;
    }
    // 退出清理：只删自己这一代（pause/stop 已 abort 并移除时跳过；新一代已占位时不误删）
    let mut map = state.group_running.lock().expect("群任务锁中毒");
    if map.get(&sid).map(|(s, _)| *s == seq).unwrap_or(false) {
        map.remove(&sid);
    }
}

/// 启动群调度器：同群重复启动拒绝（任务还活着时）
fn start_group_scheduler(state: &Arc<WebState>, sid: &str) -> bool {
    let mut map = state.group_running.lock().expect("群任务锁中毒");
    if let Some((_, h)) = map.get(sid) {
        if !h.is_finished() {
            return false;
        }
    }
    let seq = {
        let mut s = state.group_seq.lock().expect("群世代锁中毒");
        *s += 1;
        *s
    };
    let st = state.clone();
    let id = sid.to_string();
    let handle = tokio::spawn(async move { run_group_turn(st, id, seq).await });
    map.insert(sid.to_string(), (seq, handle));
    true
}

/// 中止群调度器（pause/stop 路径；只用 JoinHandle::abort，不碰 OS 信号）
fn abort_group_scheduler(state: &WebState, sid: &str) {
    if let Some((_, h)) = state.group_running.lock().expect("群任务锁中毒").remove(sid) {
        h.abort();
    }
}

/// 群 sid → 目录（校验 + 存在性）
fn resolve_group_dir(state: &WebState, sid: &str) -> Result<PathBuf, String> {
    if !valid_group_sid(sid) {
        return Err("ERROR: 非法群 id".into());
    }
    let dir = group_dir_path(&state.session_dir, sid);
    if groups::load_group(&dir).is_none() {
        return Err(format!("ERROR: 群不存在：{sid}"));
    }
    Ok(dir)
}

/// 批准 lead 的子任务：置 approved → 被执行成员独立会话跑 → 结果回群
async fn run_group_subtask(state: Arc<WebState>, sid: String, to: String, prompt: String) {
    let dir = group_dir_path(&state.session_dir, &sid);
    let Some(g) = groups::load_group(&dir) else {
        return;
    };
    let mut cfg = state.config.lock().expect("config 锁中毒").clone();
    apply_persona(&to, &mut cfg);
    // 子任务执行轮写入群 turns/
    cfg.session.dir = dir.join("turns").to_string_lossy().to_string();
    let _ = std::fs::create_dir_all(&cfg.session.dir);
    let full = format!(
        "你在群聊「{title}」中被委任子任务：\n{prompt}\n\n最近群上下文：\n{ctx}\n\n\
         请执行该任务并给出结果汇报（300 字以内）。",
        title = g.title,
        ctx = stream_context(&dir, GROUP_CTX_EVENTS, GROUP_CTX_CHARS),
    );
    let outcome = match AgentSession::new(cfg) {
        Ok(mut s) => {
            // 子任务执行过程同样全量可观测（from=受任成员）
            let fwd = spawn_activity_forward(&state, &sid, &dir, &to, &s);
            let r = tokio::time::timeout(
                std::time::Duration::from_secs(GROUP_TURN_TIMEOUT_SECS),
                s.prompt(&full),
            )
            .await;
            fwd.abort();
            match r {
                Ok(Ok(text)) => Some(text),
                Ok(Err(e)) => {
                    let _ = append_and_broadcast(&state, &sid, &dir, &GroupEvent::error(&format!("子任务执行失败（{to}）：{e}")));
                    None
                }
                Err(_) => {
                    let _ = append_and_broadcast(&state, &sid, &dir, &GroupEvent::error(&format!("子任务执行超时（{to}）")));
                    None
                }
            }
        }
        Err(e) => {
            let _ = append_and_broadcast(&state, &sid, &dir, &GroupEvent::error(&format!("子任务会话创建失败（{to}）：{e}")));
            None
        }
    };
    if let Some(text) = outcome {
        let _ = append_and_broadcast(&state, &sid, &dir, &GroupEvent::message(&to, &text));
        let _ = append_and_broadcast(
            &state,
            &sid,
            &dir,
            &GroupEvent::Subtask {
                from: to.clone(),
                to,
                prompt,
                ts: now_ts(),
                state: "done".into(),
            },
        );
    }
}

/// pause/stop 公共路径：abort 调度句柄 + set_state + 清 speaking + 广播
async fn handle_group_pause_stop(state: &Arc<WebState>, sink: &WsSink, id: String, to: &str) {
    let dir = match resolve_group_dir(state, &id) {
        Ok(d) => d,
        Err(e) => {
            ws_error(sink, &e).await;
            return;
        }
    };
    abort_group_scheduler(state, &id);
    match groups::set_state(&dir, to) {
        Ok(mut g) => {
            g.speaking = None;
            let _ = groups::save_group(&dir, &g);
            broadcast_last_event(state, &id, &dir);
            broadcast_group_state(state, &id, &g);
            broadcast_sessions(state);
        }
        Err(e) => ws_error(sink, &e).await,
    }
}

// ---------- 启动 ----------

/// r2 web 入口：绑定 127.0.0.1（仅本机，安全默认），起 axum 服务

// ═══ 定时任务调度器（v0.8 后台成长）═══

/// 每日后台运行上限（预算护栏：无人值守的烧钱上限）
const BG_MAX_PER_DAY: u64 = 12;
/// 后台任务超时（秒）
const BG_TIMEOUT_SECS: u64 = 600;

fn tasks_broadcast_payload() -> Value {
    let store = r2_core::tasks::load_store();
    json!({"t": "tasks", "tasks": store.tasks})
}

/// 执行一个到期的后台任务：独立 AgentSession（不碰前台会话锁），
/// 事件桥接到全局广播，结果摘要回写任务表
async fn run_background_task(state: &Arc<WebState>, task: &mut r2_core::tasks::Task) {
    let cfg = config_snapshot_fresh_mcp(state);
    let Ok(mut session) = AgentSession::new(cfg) else {
        task.last_result = Some("ERROR: 后台会话创建失败".into());
        return;
    };
    // 事件桥：后台会话 → 全局 WS 广播（前端实时看到它在学什么）
    let mut rx = session.subscribe();
    let tx = state.event_tx.clone();
    let bg_name = task.name.clone();
    let fwd = tokio::spawn(async move {
        while let Ok(e) = rx.recv().await {
            let _ = tx.send(json!({"t": "event", "evt": rpc::event_json(&e), "bg": bg_name}));
        }
    });
    let _ = state.event_tx.send(json!({
        "t": "bg_started", "name": task.name, "prompt_preview":
        task.prompt.chars().take(120).collect::<String>(),
    }));
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(BG_TIMEOUT_SECS),
        session.prompt(&task.prompt),
    )
    .await;
    fwd.abort();
    let summary = match result {
        Ok(Ok(text)) => {
            let s: String = text.chars().take(300).collect();
            format!("✅ {s}")
        }
        Ok(Err(e)) => format!("❌ {e}"),
        Err(_) => format!("⏱ 超时（{}s）", BG_TIMEOUT_SECS),
    };
    task.last_result = Some(summary.clone());
    let _ = state.event_tx.send(json!({
        "t": "bg_done", "name": task.name,
        "result_preview": summary.chars().take(200).collect::<String>(),
    }));
}

/// 调度循环：每 20 秒一拍。①新起草任务 → 推审批卡 ②到期 active 任务 → 执行。
/// 挂在 Console 进程内（不加新常驻进程——方案C 核心）。
fn spawn_scheduler(state: Arc<WebState>) {
    tokio::spawn(async move {
        let tz = r2_core::tasks::local_tz_offset_secs();
        let mut known_pending: std::collections::HashSet<String> = r2_core::tasks::load_store()
            .tasks
            .iter()
            .filter(|t| t.state == "pending")
            .map(|t| t.id.clone())
            .collect();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(20)).await;
            let mut store = r2_core::tasks::load_store();
            // ① 新 pending → 广播审批卡（含启动时已有的：首次也会推一遍）
            for t in store.tasks.iter().filter(|t| t.state == "pending") {
                if known_pending.insert(t.id.clone()) {
                    let _ = state.event_tx.send(json!({"t": "task_pending", "task": t}));
                }
            }
            known_pending.retain(|id| {
                store.tasks.iter().any(|t| &t.id == id && t.state == "pending")
            });
            // ② 到期执行
            let now = r2_core::tasks::now_ts();
            let due: Vec<String> = store
                .tasks
                .iter()
                .filter(|t| t.state == "active" && t.next_due.map(|d| d <= now).unwrap_or(false))
                .map(|t| t.id.clone())
                .collect();
            for id in due {
                // 预算闸先行（独占可变借用），再取任务借用——两段借用不重叠
                if r2_core::tasks::budget_gate(&mut store, BG_MAX_PER_DAY).is_err() {
                    if let Some(task) = store.tasks.iter_mut().find(|t| t.id == id) {
                        task.last_result = Some("⏭ 今日预算已满，跳过（明日恢复）".into());
                        task.next_due = r2_core::tasks::next_run(&task.schedule, now, tz);
                    }
                    continue;
                }
                let Some(task) = store.tasks.iter_mut().find(|t| t.id == id) else {
                    continue;
                };
                run_background_task(&state, task).await;
                task.last_run = Some(now);
                task.next_due = r2_core::tasks::next_run(&task.schedule, now, tz);
            }
            let _ = r2_core::tasks::save_store(&store);
            let _ = state.event_tx.send(tasks_broadcast_payload());
        }
    });
}

pub async fn run(config: Config, port: u16, host: String) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let session_dir = config::expand_tilde(&config.session.dir);
    let work_dir = config::expand_tilde(&config.agent.work_dir);
    // 没有会话目录也能起服务（首次 new_session 时 Agent 会自动建）
    let _ = std::fs::create_dir_all(&session_dir);

    let tools = build_tool_list(&config);
    let (event_tx, _) = broadcast::channel(EVENT_CAPACITY);
    let state = Arc::new(WebState {
        config: StdMutex::new(config),
        agent: Mutex::new(None),
        steer_tx: StdMutex::new(None),
        pending_prompt: StdMutex::new(None),
        event_tx,
        session_dir,
        work_dir,
        tools,
        current_agent: StdMutex::new(MAIN.to_string()),
        group_running: StdMutex::new(HashMap::new()),
        group_seq: StdMutex::new(0),
        channels: StdMutex::new(HashMap::new()),
        dm_sessions: StdMutex::new(HashMap::new()),
    });

    // 调度器/通道先克隆再建路由（with_state 会 move state）
    spawn_scheduler(state.clone());
    let state_channels = state.clone();
    let app = Router::new()
        .route("/", get(index))
        .route("/upload", post(upload))
        .route("/prompt_file", get(get_prompt_file).put(put_prompt_file))
        .route("/skills", get(list_skills))
        .route("/api/growth", get(api_growth))
        .route("/skill_preview", get(skill_preview))
        .route("/api/state", get(api_state))
        .route("/api/agent-files", get(get_agent_files).post(post_agent_files))
        .route("/ws", get(ws_handler))
        // 请求体上限：upload 10MB + 1MB 表单余量（其余路由 body 都很小，一并覆盖）
        .layer(DefaultBodyLimit::max(MAX_UPLOAD + 1024 * 1024))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind((host.as_str(), port)).await?;
    // web server 已就绪：按档案起一轮飞书通道（之后档案变更/审批会幂等重载）
    start_feishu_channels(&state_channels);
    println!("R2 Console → http://{}:{port}", if host == "0.0.0.0" { "0.0.0.0（本机局域网IP）".to_string() } else { host.clone() });
    if host == "0.0.0.0" {
        println!("⚠ 已绑定 0.0.0.0：局域网内设备可访问，注意环境安全");
    }
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_channel_feishu_toml_roundtrip() {
        // 老档案（无 channel_feishu 段）→ 默认全关，show_process 默认 compact
        let old = "name = \"cfo\"\ndisplay_name = \"CFO\"\n";
        let p: agents::AgentProfile = toml::from_str(old).unwrap();
        assert!(!p.channel_feishu.enabled);
        assert!(p.channel_feishu.app_id.is_empty());
        assert!(p.channel_feishu.allow_from.is_empty());
        assert_eq!(p.channel_feishu.show_process, "compact");
        // 完整段 → roundtrip 不丢字段
        let full = "name = \"cfo\"\n\n[channel_feishu]\nenabled = true\napp_id = \"cli_xxx\"\napp_secret = \"sec\"\nallow_from = [\"ou_1\", \"*\"]\nshow_process = \"full\"\n";
        let p: agents::AgentProfile = toml::from_str(full).unwrap();
        assert!(p.channel_feishu.enabled);
        let s = toml::to_string(&p).unwrap();
        let p2: agents::AgentProfile = toml::from_str(&s).unwrap();
        assert_eq!(p2.channel_feishu.app_id, "cli_xxx");
        assert_eq!(p2.channel_feishu.app_secret, "sec");
        assert_eq!(
            p2.channel_feishu.allow_from,
            vec!["ou_1".to_string(), "*".to_string()]
        );
        assert_eq!(p2.channel_feishu.show_process, "full");
    }

    #[test]
    fn test_feishu_whitelist() {
        // 策略四档：deny_all / allow_all / allow_list / deny_list
        let mut cf = ChannelFeishu::default();
        // deny_all（默认）= 拒绝所有人
        assert!(!feishu_allowed(&cf, "ou_a"));
        // allow_all = 允许所有人
        cf.dm_policy = "allow_all".into();
        assert!(feishu_allowed(&cf, "ou_a"));
        // allow_list = 仅名单内
        cf.dm_policy = "allow_list".into();
        cf.policy_list = vec!["ou_a".into()];
        assert!(feishu_allowed(&cf, "ou_a"));
        assert!(!feishu_allowed(&cf, "ou_b"));
        // deny_list = 拒绝名单内，其余放行
        cf.dm_policy = "deny_list".into();
        assert!(!feishu_allowed(&cf, "ou_a"));
        assert!(feishu_allowed(&cf, "ou_b"));
        // 老档案 allow_from 归一：["*"] → allow_all；非空 → allow_list
        let mut old = ChannelFeishu::default();
        old.allow_from = vec!["*".into()];
        assert!(feishu_allowed(&old, "ou_any"));
        let mut old2 = ChannelFeishu::default();
        old2.allow_from = vec!["ou_x".into()];
        assert!(feishu_allowed(&old2, "ou_x"));
        assert!(!feishu_allowed(&old2, "ou_y"));
    }

    #[test]
    fn test_dm_key() {
        assert_eq!(dm_key("cfo", "ou_1"), "cfo|ou_1");
        assert_eq!(dm_key("main", "ou_1"), "main|ou_1");
        // 同 open_id 不同 agent 不串台
        assert_ne!(dm_key("a", "ou_1"), dm_key("b", "ou_1"));
    }

    #[test]
    fn test_show_process_mode() {
        assert_eq!(show_process_mode("none"), ShowProcess::None);
        assert_eq!(show_process_mode("compact"), ShowProcess::Compact);
        assert_eq!(show_process_mode("full"), ShowProcess::Full);
        // 空串/未识别值按默认 compact
        assert_eq!(show_process_mode(""), ShowProcess::Compact);
        assert_eq!(show_process_mode("verbose"), ShowProcess::Compact);
        // 档位序：None < Compact < Full
        assert!(ShowProcess::None < ShowProcess::Compact);
        assert!(ShowProcess::Compact < ShowProcess::Full);
    }

    #[test]
    fn test_tool_call_summary() {
        assert_eq!(tool_call_summary("bash", "{}"), "🔧 bash {}");
        // 参数超 60 字截断加省略号
        let long = "x".repeat(100);
        let s = tool_call_summary("read", &long);
        assert!(s.starts_with("🔧 read "));
        assert!(s.ends_with('…'));
        assert_eq!(s.chars().count(), "🔧 read ".chars().count() + 61);
    }

    #[test]
    fn test_parse_channel_msgs() {
        match parse_client_msg(
            r#"{"t":"channel_set","agent":"cfo","config":{"enabled":true,"app_id":"cli_1"}}"#,
        )
        .unwrap()
        {
            ClientMsg::ChannelSet { agent, config } => {
                assert_eq!(agent, "cfo");
                assert_eq!(config["app_id"], "cli_1");
            }
            _ => panic!("应为 ChannelSet"),
        }
        match parse_client_msg(r#"{"t":"channel_test","agent":"cfo"}"#).unwrap() {
            ClientMsg::ChannelTest { agent } => assert_eq!(agent, "cfo"),
            _ => panic!("应为 ChannelTest"),
        }
        // 缺字段 → 报错
        assert!(parse_client_msg(r#"{"t":"channel_set","agent":"cfo"}"#).is_err());
        assert!(parse_client_msg(r#"{"t":"channel_test"}"#).is_err());
    }

    #[test]
    fn test_upload_extension_whitelist() {
        // 白名单内（大小写不敏感）
        for name in ["a.txt", "b.md", "c.rs", "d.py", "e.js", "f.ts", "g.json", "h.csv", "i.toml", "j.yaml", "k.yml", "l.log", "M.MD"] {
            assert!(is_allowed_upload(name), "{name} 应被允许");
        }
        // 白名单外 / 无扩展名
        for name in ["x.exe", "y.png", "z.bin", "noext", "w.html", ".hidden"] {
            assert!(!is_allowed_upload(name), "{name} 应被拒绝");
        }
    }

    #[test]
    fn test_parse_prompt_and_steer() {
        match parse_client_msg(r#"{"t":"prompt","input":"你好"}"#).unwrap() {
            ClientMsg::Prompt(input) => assert_eq!(input, "你好"),
            _ => panic!("应为 Prompt"),
        }
        match parse_client_msg(r#"{"t":"steer","text":"改成诗"}"#).unwrap() {
            ClientMsg::Steer(text) => assert_eq!(text, "改成诗"),
            _ => panic!("应为 Steer"),
        }
        // 缺 input 字段 → 报错
        assert!(parse_client_msg(r#"{"t":"prompt"}"#).is_err());
    }

    #[test]
    fn test_parse_garbage_and_unknown() {
        assert!(parse_client_msg("这不是 JSON {{{").is_err());
        assert!(parse_client_msg(r#"{"t":"fly"}"#).is_err());
        assert!(parse_client_msg(r#"{"foo":1}"#).is_err());
        assert!(parse_client_msg("").is_err());
    }

    #[test]
    fn test_parse_fork_upto_validation() {
        // 不带 upto
        match parse_client_msg(r#"{"t":"fork","parent":"abc-123"}"#).unwrap() {
            ClientMsg::Fork { parent, upto } => {
                assert_eq!(parent, "abc-123");
                assert_eq!(upto, None);
            }
            _ => panic!("应为 Fork"),
        }
        // upto 为 null → None
        match parse_client_msg(r#"{"t":"fork","parent":"p","upto":null}"#).unwrap() {
            ClientMsg::Fork { upto, .. } => assert_eq!(upto, None),
            _ => panic!("应为 Fork"),
        }
        // upto 为非负整数
        match parse_client_msg(r#"{"t":"fork","parent":"p","upto":5}"#).unwrap() {
            ClientMsg::Fork { upto, .. } => assert_eq!(upto, Some(5)),
            _ => panic!("应为 Fork"),
        }
        // upto 为负数 / 字符串 / 浮点 → 报错
        assert!(parse_client_msg(r#"{"t":"fork","parent":"p","upto":-1}"#).is_err());
        assert!(parse_client_msg(r#"{"t":"fork","parent":"p","upto":"5"}"#).is_err());
        assert!(parse_client_msg(r#"{"t":"fork","parent":"p","upto":1.5}"#).is_err());
        // 缺 parent → 报错
        assert!(parse_client_msg(r#"{"t":"fork","upto":3}"#).is_err());
    }

    #[test]
    fn test_parse_other_msgs() {
        assert!(matches!(
            parse_client_msg(r#"{"t":"new_session"}"#).unwrap(),
            ClientMsg::NewSession
        ));
        match parse_client_msg(r#"{"t":"switch","id":"s1"}"#).unwrap() {
            ClientMsg::Switch(id) => assert_eq!(id, "s1"),
            _ => panic!("应为 Switch"),
        }
        match parse_client_msg(r#"{"t":"delete_session","id":"s2"}"#).unwrap() {
            ClientMsg::DeleteSession(id) => assert_eq!(id, "s2"),
            _ => panic!("应为 DeleteSession"),
        }
        match parse_client_msg(r#"{"t":"set_model","model":"glm-5.2"}"#).unwrap() {
            ClientMsg::SetModel(m) => assert_eq!(m, "glm-5.2"),
            _ => panic!("应为 SetModel"),
        }
    }

    #[test]
    fn test_parse_agent_msgs() {
        match parse_client_msg(r#"{"t":"agent_approve","name":"cfo"}"#).unwrap() {
            ClientMsg::AgentApprove(n) => assert_eq!(n, "cfo"),
            _ => panic!("应为 AgentApprove"),
        }
        match parse_client_msg(r#"{"t":"agent_reject","name":"bob"}"#).unwrap() {
            ClientMsg::AgentReject(n) => assert_eq!(n, "bob"),
            _ => panic!("应为 AgentReject"),
        }
        match parse_client_msg(r#"{"t":"agent_switch","name":"main"}"#).unwrap() {
            ClientMsg::AgentSwitch(n) => assert_eq!(n, "main"),
            _ => panic!("应为 AgentSwitch"),
        }
        // 缺 name 字段 → 报错
        assert!(parse_client_msg(r#"{"t":"agent_approve"}"#).is_err());
        assert!(parse_client_msg(r#"{"t":"agent_reject"}"#).is_err());
        assert!(parse_client_msg(r#"{"t":"agent_switch"}"#).is_err());
    }

    /// HOME 是进程级全局：改 HOME 的测试必须串行（r2-core 的 testutil 锁不导出，本 crate 自持一把）
    static HOME_LOCK: StdMutex<()> = StdMutex::new(());

    #[test]
    fn test_persona_config_overlay() {
        let _g = HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let old_home = std::env::var("HOME").unwrap_or_default();
        std::env::set_var("HOME", tmp.path());

        agents::draft_profile("cfo", "CFO 参谋", "glm-x", "管钱", "soul").unwrap();
        agents::approve("cfo").unwrap();

        // 分身叠加：persona_dir / work_dir / session.dir / 模型覆盖（openai_compat 分支）
        let mut cfg = Config::default_config();
        cfg.model.provider = "openai_compat".into();
        cfg.model.openai_compat.model = "base-model".into();
        apply_persona("cfo", &mut cfg);
        let dir = agents::profile_dir("cfo");
        assert_eq!(cfg.agent.persona_dir.as_deref(), Some(dir.to_string_lossy().as_ref()));
        assert_eq!(cfg.agent.work_dir, dir.join("work").to_string_lossy());
        assert_eq!(cfg.session.dir, dir.join("sessions").to_string_lossy());
        assert_eq!(cfg.model.openai_compat.model, "glm-x");
        // sessions/work 目录被顺带创建
        assert!(dir.join("sessions").is_dir());
        assert!(dir.join("work").is_dir());

        // anthropic 分支模型覆盖
        let mut cfg = Config::default_config();
        cfg.model.provider = "anthropic".into();
        cfg.model.anthropic.model = "claude-base".into();
        apply_persona("cfo", &mut cfg);
        assert_eq!(cfg.model.anthropic.model, "glm-x");

        // 档案 model 为空 → 不覆盖
        agents::draft_profile("plain", "Plain", "", "", "").unwrap();
        agents::approve("plain").unwrap();
        let mut cfg = Config::default_config();
        cfg.model.openai_compat.model = "keep-me".into();
        apply_persona("plain", &mut cfg);
        assert_eq!(cfg.model.openai_compat.model, "keep-me");

        // main：完全不改动
        let mut cfg = Config::default_config();
        cfg.model.openai_compat.model = "keep-me".into();
        apply_persona(MAIN, &mut cfg);
        assert!(cfg.agent.persona_dir.is_none());
        assert_eq!(cfg.model.openai_compat.model, "keep-me");

        std::env::set_var("HOME", old_home);
    }

    #[test]
    fn test_session_dir_for_switching() {
        let _g = HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let old_home = std::env::var("HOME").unwrap_or_default();
        std::env::set_var("HOME", tmp.path());

        let (event_tx, _) = broadcast::channel(EVENT_CAPACITY);
        let state = WebState {
            tools: vec![],
            config: StdMutex::new(Config::default_config()),
            agent: Mutex::new(None),
            steer_tx: StdMutex::new(None),
            pending_prompt: StdMutex::new(None),
            event_tx,
            session_dir: "/main/sessions".to_string(),
            work_dir: "/main/work".to_string(),
            current_agent: StdMutex::new(MAIN.to_string()),
            group_running: StdMutex::new(HashMap::new()),
            group_seq: StdMutex::new(0),
            channels: StdMutex::new(HashMap::new()),
            dm_sessions: StdMutex::new(HashMap::new()),
        };
        assert_eq!(session_dir_for(&state), "/main/sessions");
        *state.current_agent.lock().unwrap() = "cfo".to_string();
        assert_eq!(
            session_dir_for(&state),
            agents::profile_dir("cfo").join("sessions").to_string_lossy()
        );

        std::env::set_var("HOME", old_home);
    }

    #[test]
    fn test_valid_name() {
        assert!(valid_name("abc-123_def.skill"));
        assert!(!valid_name(""));
        assert!(!valid_name("../etc"));
        assert!(!valid_name("a/b"));
        assert!(!valid_name("a\\b"));
        assert!(!valid_name(".."));
    }

    #[test]
    fn test_state_json_shape() {
        // 用临时目录构造最小状态，验证快照结构（不起真实服务）
        let tmp = tempfile::tempdir().unwrap();
        let mut config = Config::default_config();
        config.session.dir = tmp.path().join("sessions").to_string_lossy().to_string();
        let session_dir = config.session.dir.clone();
        let (event_tx, _) = broadcast::channel(EVENT_CAPACITY);
        let state = WebState {
            tools: vec![json!({"name": "read", "desc": "读文件", "mcp": false})],
            config: StdMutex::new(config),
            agent: Mutex::new(None),
            steer_tx: StdMutex::new(None),
            pending_prompt: StdMutex::new(None),
            event_tx,
            session_dir,
            work_dir: tmp.path().to_string_lossy().to_string(),
            current_agent: StdMutex::new(MAIN.to_string()),
            group_running: StdMutex::new(HashMap::new()),
            group_seq: StdMutex::new(0),
            channels: StdMutex::new(HashMap::new()),
            dm_sessions: StdMutex::new(HashMap::new()),
        };
        let v = state_json(&state);
        assert!(v["model"].is_string());
        assert!(v["agents"].is_array());
        assert_eq!(v["current_agent"], MAIN);
        assert!(v["sessions"].is_array());
        assert_eq!(v["tools"][0]["name"], "read");
        assert_eq!(v["tools"][0]["mcp"], false);
        assert!(v["sandbox"]["level"].is_string());
        assert!(v["sandbox"]["max_memory_mb"].is_number());
        assert!(v["prompt_sections"]["core"].is_string());
        assert_eq!(v["running"], false);
        assert!(v["current_session"].is_null());
    }

    // ═══ 群聊调度引擎测试 ═══

    /// 最小 WebState（临时目录，不起服务）
    fn make_test_state(session_dir: &str) -> WebState {
        let (event_tx, _) = broadcast::channel(EVENT_CAPACITY);
        WebState {
            tools: vec![],
            config: StdMutex::new(Config::default_config()),
            agent: Mutex::new(None),
            steer_tx: StdMutex::new(None),
            pending_prompt: StdMutex::new(None),
            event_tx,
            session_dir: session_dir.to_string(),
            work_dir: "/tmp".to_string(),
            current_agent: StdMutex::new(MAIN.to_string()),
            group_running: StdMutex::new(HashMap::new()),
            group_seq: StdMutex::new(0),
            channels: StdMutex::new(HashMap::new()),
            dm_sessions: StdMutex::new(HashMap::new()),
        }
    }

    /// 临时群根 + 三人群（main + cfo + cto），走 r2-core 直建（不校验档案存在）
    fn make_test_group() -> (tempfile::TempDir, String, groups::GroupConfig) {
        let root = tempfile::tempdir().unwrap();
        let g = groups::create_group(
            root.path(),
            "评审会",
            &[(MAIN, "主人"), ("cfo", "CFO"), ("cto", "CTO")],
        )
        .unwrap();
        // 与生产一致：目录改名 group-<id>
        let sid = format!("group-{}", g.id);
        std::fs::rename(groups::group_dir(root.path(), &g.id), root.path().join(&sid)).unwrap();
        (root, sid, g)
    }

    #[test]
    fn test_parse_group_create() {
        match parse_client_msg(
            r#"{"t":"group_create","title":"评审会","members":[{"name":"cfo","display_name":"CFO"},{"name":"cto"}]}"#,
        )
        .unwrap()
        {
            ClientMsg::GroupCreate { title, members } => {
                assert_eq!(title, "评审会");
                assert_eq!(
                    members,
                    vec![("cfo".to_string(), "CFO".to_string()), ("cto".to_string(), String::new())]
                );
            }
            _ => panic!("应为 GroupCreate"),
        }
        // 空 members / 缺 title / members 非数组 → 报错
        assert!(parse_client_msg(r#"{"t":"group_create","title":"t","members":[]}"#).is_err());
        assert!(parse_client_msg(r#"{"t":"group_create","members":[{"name":"cfo"}]}"#).is_err());
        assert!(parse_client_msg(r#"{"t":"group_create","title":"t","members":"cfo"}"#).is_err());
        assert!(parse_client_msg(r#"{"t":"group_create","title":"t","members":[{"display_name":"x"}]}"#).is_err());
    }

    #[test]
    fn test_parse_group_msgs() {
        match parse_client_msg(r#"{"t":"group_prompt","id":"group-a","text":"大家好"}"#).unwrap() {
            ClientMsg::GroupPrompt { id, text } => {
                assert_eq!(id, "group-a");
                assert_eq!(text, "大家好");
            }
            _ => panic!("应为 GroupPrompt"),
        }
        match parse_client_msg(r#"{"t":"group_discuss","id":"group-a","topic":"预算"}"#).unwrap() {
            ClientMsg::GroupDiscuss { id, topic } => {
                assert_eq!(id, "group-a");
                assert_eq!(topic, "预算");
            }
            _ => panic!("应为 GroupDiscuss"),
        }
        match parse_client_msg(r#"{"t":"group_delegate","id":"group-a","topic":"年报","lead":"cfo"}"#).unwrap() {
            ClientMsg::GroupDelegate { id, topic, lead } => {
                assert_eq!(id, "group-a");
                assert_eq!(topic, "年报");
                assert_eq!(lead, "cfo");
            }
            _ => panic!("应为 GroupDelegate"),
        }
        assert!(matches!(
            parse_client_msg(r#"{"t":"group_pause","id":"group-a"}"#).unwrap(),
            ClientMsg::GroupPause(_)
        ));
        assert!(matches!(
            parse_client_msg(r#"{"t":"group_stop","id":"group-a"}"#).unwrap(),
            ClientMsg::GroupStop(_)
        ));
        assert!(matches!(
            parse_client_msg(r#"{"t":"group_revoke_lead","id":"group-a"}"#).unwrap(),
            ClientMsg::GroupRevokeLead(_)
        ));
        assert!(matches!(
            parse_client_msg(r#"{"t":"group_summary","id":"group-a"}"#).unwrap(),
            ClientMsg::GroupSummary(_)
        ));
        assert!(matches!(
            parse_client_msg(r#"{"t":"group_open","id":"group-a"}"#).unwrap(),
            ClientMsg::GroupOpen(_)
        ));
        match parse_client_msg(r#"{"t":"group_subtask_approve","id":"group-a","to":"cto"}"#).unwrap() {
            ClientMsg::GroupSubtaskApprove { id, to } => {
                assert_eq!(id, "group-a");
                assert_eq!(to, "cto");
            }
            _ => panic!("应为 GroupSubtaskApprove"),
        }
        // 缺字段 → 报错
        for bad in [
            r#"{"t":"group_prompt","id":"group-a"}"#,
            r#"{"t":"group_discuss","id":"group-a"}"#,
            r#"{"t":"group_delegate","id":"group-a","topic":"x"}"#,
            r#"{"t":"group_pause"}"#,
            r#"{"t":"group_open"}"#,
            r#"{"t":"group_subtask_approve","id":"group-a"}"#,
        ] {
            assert!(parse_client_msg(bad).is_err(), "{bad} 应报错");
        }
    }

    #[test]
    fn test_valid_group_sid() {
        assert!(valid_group_sid("group-550e8400-e29b-41d4-a716-446655440000"));
        assert!(!valid_group_sid("group-"));
        assert!(!valid_group_sid("abc"));
        assert!(!valid_group_sid("group-../etc"));
        assert!(!valid_group_sid("group-a/b"));
    }

    #[test]
    fn test_group_create_dir_structure() {
        let _g = HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let old_home = std::env::var("HOME").unwrap_or_default();
        std::env::set_var("HOME", tmp.path());
        agents::draft_profile("cfo", "CFO", "", "", "").unwrap();
        agents::approve("cfo").unwrap();
        agents::draft_profile("cto", "CTO", "", "", "").unwrap();
        agents::approve("cto").unwrap();

        let session_dir = tmp.path().join("sessions");
        let (sid, g) = do_group_create(
            session_dir.to_str().unwrap(),
            "评审会",
            &[("cfo".to_string(), "CFO".to_string()), ("cto".to_string(), "CTO".to_string())],
        )
        .unwrap();
        assert!(sid.starts_with("group-"));
        let dir = session_dir.join(&sid);
        // group.json + stream.jsonl 落盘；owner 自动加入且 role=owner
        assert!(dir.join("group.json").is_file());
        assert!(dir.join("stream.jsonl").is_file());
        assert_eq!(g.members[0].name, MAIN);
        assert_eq!(g.members[0].role, "owner");
        // stream 首条 = StateChange idle
        let events = groups::read_stream(&dir);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            GroupEvent::StateChange { ref from_state, ref to_state, .. }
                if from_state == "idle" && to_state == "idle"
        ));
        // 不存在的 agent / 指定 main / 空标题 → 报错
        assert!(do_group_create(session_dir.to_str().unwrap(), "t", &[("ghost".into(), "".into())]).is_err());
        assert!(do_group_create(session_dir.to_str().unwrap(), "t", &[(MAIN.into(), "".into())]).is_err());
        assert!(do_group_create(session_dir.to_str().unwrap(), "  ", &[("cfo".into(), "".into())]).is_err());

        std::env::set_var("HOME", old_home);
    }

    #[test]
    fn test_group_entries_inject() {
        let (root, sid, _g) = make_test_group();
        let session_dir = root.path().to_string_lossy().to_string();
        let entries = group_entries(&session_dir);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e["id"], json!(sid));
        assert_eq!(e["kind"], "group");
        assert_eq!(e["title"], "评审会");
        assert_eq!(e["members_count"], 3);
        assert_eq!(e["state"], "idle");
        // 普通 *.jsonl 列表不受群目录影响（list_sessions 只挑 .jsonl 文件）
        let plain = session::list_sessions(&session_dir).unwrap();
        assert!(plain.is_empty());
    }

    #[test]
    fn test_parse_mentions_done_delegate() {
        let (_root, _sid, g) = make_test_group();
        // @点名认群成员（含 main 主Agent——他也是同事）；user 不在成员表天然不可被点
        assert_eq!(parse_mentions("请 @cfo 报个数", &g), vec!["cfo".to_string()]);
        assert_eq!(parse_mentions("@main 你怎么看", &g), vec!["main".to_string()]);
        assert_eq!(parse_mentions("@cfo 和 @cto 都对", &g), vec!["cfo".to_string(), "cto".to_string()]);
        assert_eq!(parse_mentions("没有点名", &g), Vec::<String>::new());
        assert_eq!(parse_mentions("@ghost 不存在", &g), Vec::<String>::new());
        // 收敛信号
        assert!(has_done("综上 [DONE]"));
        assert!(!has_done("[DONE 还没收敛"));
        // 派卡指令
        assert_eq!(
            parse_delegate("规划如下 [DELEGATE @cto 拉取 Q3 数据] 以上"),
            Some(("cto".to_string(), "拉取 Q3 数据".to_string()))
        );
        assert_eq!(parse_delegate("[DELEGATE cfo 缺@号]"), None);
        assert_eq!(parse_delegate("[DELEGATE @cto]"), None); // 无任务描述
        assert_eq!(parse_delegate("没有指令"), None);
    }

    #[test]
    fn test_speaking_to_force_next_jump() {
        let (_root, _sid, mut g) = make_test_group();
        // 顺序 main → cfo → cto：点名 cto → speaking 置 cfo，下一位即 cto
        let sp = speaking_to_force_next(&g, "cto").unwrap();
        g.speaking = Some(sp);
        assert_eq!(groups::next_speaker(&g, Some(MAIN)).as_deref(), Some("cto"));
        // 点名 cfo → speaking 置 main
        let sp = speaking_to_force_next(&g, "cfo").unwrap();
        g.speaking = Some(sp);
        assert_eq!(groups::next_speaker(&g, Some(MAIN)).as_deref(), Some("cfo"));
        // 目标不存在 → None
        assert_eq!(speaking_to_force_next(&g, "ghost"), None);
    }

    #[test]
    fn test_budget_pause_path() {
        let (root, sid, _g) = make_test_group();
        let dir = root.path().join(&sid);
        groups::set_state(&dir, "discussing").unwrap();
        // 记账到顶仍放行
        assert!(group_add_tokens(&dir, 300_000));
        // 超 1 个 token → false（调度器据此走 group_pause_budget）
        assert!(!group_add_tokens(&dir, 1));
        let state = make_test_state(root.path().to_str().unwrap());
        group_pause_budget(&state, "group-x", &dir);
        let g2 = groups::load_group(&dir).unwrap();
        assert_eq!(g2.state, "paused");
        let events = groups::read_stream(&dir);
        assert!(matches!(events.last(), Some(GroupEvent::Error { text, .. }) if text.contains("预算耗尽")));
        // paused 后 budget 已超：used_tokens 落盘保留
        assert!(g2.used_tokens > 300_000);
    }

    #[test]
    fn test_group_state_smoke() {
        let (root, sid, _g) = make_test_group();
        let dir = root.path().join(&sid);
        // idle → discussing → summarized 状态落盘正确（无需真模型）
        let g = groups::set_state(&dir, "discussing").unwrap();
        assert_eq!(g.state, "discussing");
        let g = groups::set_state(&dir, "summarized").unwrap();
        assert_eq!(g.state, "summarized");
        // summarized 可重开继续聊（v0.9.1）；stopped 才是真终态
        assert!(groups::set_state(&dir, "discussing").is_ok());
        groups::set_state(&dir, "stopped").unwrap();
        assert!(groups::set_state(&dir, "discussing").is_err());
        // StateChange 事件入流
        let events = groups::read_stream(&dir);
        assert_eq!(events.len(), 4);
        assert!(matches!(events[1], GroupEvent::StateChange { ref from_state, ref to_state, .. } if from_state == "discussing" && to_state == "summarized"));
    }

    #[test]
    fn test_interject_keeps_speaking() {
        let (root, sid, _g) = make_test_group();
        let dir = root.path().join(&sid);
        groups::set_state(&dir, "discussing").unwrap();
        let mut g = groups::load_group(&dir).unwrap();
        g.speaking = Some("cfo".into());
        groups::save_group(&dir, &g).unwrap();
        // 人插话：只入流，不动 speaking（模拟 group_prompt discussing 分支）
        groups::append_event(&dir, &GroupEvent::message("user", "插一句")).unwrap();
        let g2 = groups::load_group(&dir).unwrap();
        assert_eq!(g2.speaking.as_deref(), Some("cfo"));
        assert_eq!(g2.state, "discussing");
    }

    #[test]
    fn test_group_note_failure_pause() {
        let (root, sid, _g) = make_test_group();
        let dir = root.path().join(&sid);
        groups::set_state(&dir, "discussing").unwrap();
        let state = make_test_state(root.path().to_str().unwrap());
        // 前两次失败：只记 Error，不暂停
        assert!(!group_note_failure(&state, "group-x", &dir, 1, "失败1"));
        assert!(!group_note_failure(&state, "group-x", &dir, 2, "失败2"));
        assert_eq!(groups::load_group(&dir).unwrap().state, "discussing");
        // 第三次：自动 paused
        assert!(group_note_failure(&state, "group-x", &dir, 3, "失败3"));
        assert_eq!(groups::load_group(&dir).unwrap().state, "paused");
        let events = groups::read_stream(&dir);
        assert!(events.iter().filter(|e| matches!(e, GroupEvent::Error { .. })).count() >= 3);
    }

    #[test]
    fn test_stream_context_caps() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        // 60 条事件 → 只取最近 40 条
        for i in 0..60 {
            groups::append_event(dir, &GroupEvent::message("cfo", &format!("第{i}条"))).unwrap();
        }
        let ctx = stream_context(dir, 40, 12_000);
        assert!(!ctx.contains("第19条"));
        assert!(ctx.contains("第20条"));
        assert!(ctx.contains("第59条"));
        // 字符上限：超长事件流从头部截断
        let long = "字".repeat(500);
        for _ in 0..30 {
            groups::append_event(dir, &GroupEvent::message("cto", &long)).unwrap();
        }
        let ctx = stream_context(dir, 40, 12_000);
        assert!(ctx.starts_with("……（前文省略）"));
        assert!(ctx.chars().count() <= 12_000 + 20); // 截断标记少量余量
    }

    #[test]
    fn test_build_member_prompt_contents() {
        let (root, sid, g) = make_test_group();
        let dir = root.path().join(&sid);
        groups::append_event(&dir, &GroupEvent::message("user", "聊聊 Q3 预算")).unwrap();
        let prompt = build_member_prompt(&g, &dir, "cfo");
        assert!(prompt.contains("评审会"));
        assert!(prompt.contains("成员 cfo"));
        assert!(prompt.contains("- cfo（CFO，member）"));
        assert!(prompt.contains("聊聊 Q3 预算"));
        assert!(prompt.contains("[DONE]"));
        assert!(prompt.contains("[DELEGATE"));
    }
}
