//! r2 web 子命令：R2 Console 服务层（axum + WebSocket）
//!
//! 架构：浏览器 ↔ WS/HTTP ↔ 本模块 ↔ r2-core 库（同进程直调，不走子进程）。
//! 多客户端共享单 Agent：AgentSession 全局一把锁，prompt 在途时整锁持有，
//! 其余操作排队（try_lock 失败即 "prompt in flight"）；事件经 broadcast 扇出给所有 WS 客户端。

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
use r2_core::agents::{self, MAIN};
use r2_core::config::{self, Config};
use r2_core::session::{self, SessionSummary};
use r2_core::tools::ToolRegistry;
use r2_core::{rpc, AgentSession};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
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
            cfg.mcp = fresh.mcp;
        }
    }
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
    let list = session::list_sessions(&session_dir_for(state)).unwrap_or_default();
    let _ = state.event_tx.send(json!({
        "t": "sessions",
        "list": list.iter().map(summary_json).collect::<Vec<_>>(),
    }));
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
    // 当前会话的历史（浏览器刷新/重连时回放画面；prompt 运行中则跳过）
    let history = match state.agent.try_lock() {
        Ok(g) => g.as_ref().map(session_history_json),
        Err(_) => None,
    };
    json!({
        "model": cfg.current_model(),
        "agents": agents::list_profiles().iter().map(agents::profile_json).collect::<Vec<_>>(),
        "current_agent": current_agent_name(state),
        "tasks": r2_core::tasks::load_store().tasks,
        "history": history,
        "current_session": current,
        "sessions": sessions.iter().map(summary_json).collect::<Vec<_>>(),
        "tools": state.tools,
        "sandbox": {
            "level": cfg.sandbox.level,
            "bash_timeout_secs": cfg.sandbox.bash_timeout_secs,
            "max_processes": cfg.sandbox.max_processes,
            "max_memory_mb": cfg.sandbox.max_memory_mb,
            "cpu_time_secs": cfg.sandbox.cpu_time_secs,
            "max_file_size_mb": cfg.sandbox.max_file_size_mb,
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
    let Ok(mut registry) = ToolRegistry::new_default(&work_dir, &config.sandbox, config.source_path.as_deref()) else {
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

/// GET /：内嵌的单页 UI（W3 填充，当前为占位壳）
async fn index() -> Html<&'static str> {
    Html(include_str!("web_ui.html"))
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
    TaskApprove(String),
    TaskReject(String),
    TaskPause(String),
    TaskResume(String),
    TaskDelete(String),
    AgentApprove(String),
    AgentReject(String),
    AgentSwitch(String),
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

    // 广播 → 本连接
    let mut evt_rx = state.event_tx.subscribe();
    let sink_fwd = sink.clone();
    let forward = tokio::spawn(async move {
        loop {
            match evt_rx.recv().await {
                Ok(v) => {
                    let mut s = sink_fwd.lock().await;
                    if s.send(WsMessage::Text(v.to_string())).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
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
            // prompt 不能阻塞 WS 循环（否则 steer 进不来）：spawn 独立任务
            let st = state.clone();
            tokio::spawn(async move {
                // 锁被持有 = 已有 prompt 在途
                let Ok(mut guard) = st.agent.try_lock() else {
                    broadcast_error(&st, "prompt in flight");
                    return;
                };
                if guard.is_none() {
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
            });
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
                Ok(_) => broadcast_state(state),
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
        event_tx,
        session_dir,
        work_dir,
        tools,
        current_agent: StdMutex::new(MAIN.to_string()),
    });

    // 调度器先克隆再建路由（with_state 会 move state）
    spawn_scheduler(state.clone());
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
            event_tx,
            session_dir: "/main/sessions".to_string(),
            work_dir: "/main/work".to_string(),
            current_agent: StdMutex::new(MAIN.to_string()),
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
            event_tx,
            session_dir,
            work_dir: tmp.path().to_string_lossy().to_string(),
            current_agent: StdMutex::new(MAIN.to_string()),
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
}
