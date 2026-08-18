//! JSON-RPC 2.0 serve 协议层（`r2 serve` 的核心逻辑）
//!
//! 传输：stdin/stdout，行分隔 JSON（JSONL 帧），每行一个请求/响应/通知。
//! 本模块只做协议解析与路由，不绑定真实 stdin/stdout——驱动层（r2-cli）
//! 负责读行、写行和异步执行 prompt。详见 docs/rpc.md。

use crate::config::Config;
use crate::events::AgentEvent;
use crate::session_api::AgentSession;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};

// ---- 错误码 ----
/// 解析错误（垃圾 JSON）
pub const PARSE_ERROR: i64 = -32700;
/// 无效请求（缺 method / id 非数字等）
pub const INVALID_REQUEST: i64 = -32600;
/// 未知方法
pub const METHOD_NOT_FOUND: i64 = -32601;
/// 参数无效
pub const INVALID_PARAMS: i64 = -32602;
/// 会话层错误（初始化失败 / 恢复失败 / 运行失败等）
pub const SESSION_ERROR: i64 = -32001;
/// 已有 prompt 在途
pub const PROMPT_IN_FLIGHT: i64 = -32002;

/// 配置解析器：config_path（可选）→ Config。测试可注入，避免读真实配置文件。
type ConfigLoader = Box<dyn Fn(Option<&str>) -> Result<Config, String> + Send + Sync>;

/// RPC 服务核心：会话持有 + 请求路由。绝不 panic（serve 是长驻进程）。
pub struct RpcServer {
    /// 当前会话（prompt 在途时被驱动层临时 take 走，见 begin_prompt/end_prompt）
    session: Option<AgentSession>,
    /// initialize 传入或 CLI --config 提供的默认配置路径
    config_path: Option<String>,
    /// 是否有 prompt 在途
    in_flight: bool,
    /// prompt 在途时的 steer 发送端（begin_prompt 时从会话克隆）
    steer_tx: Option<mpsc::Sender<String>>,
    /// 配置解析器（默认读文件；测试注入内存配置）
    config_loader: ConfigLoader,
}

/// handle_line 的处理结果：驱动层据此决定下一步动作
pub enum RpcOutcome {
    /// 立即回写一行响应
    Line(String),
    /// prompt 请求：驱动层应调用 begin_prompt 并异步执行
    PendingPrompt {
        /// 请求 id（仅支持非负整数 id）
        id: u64,
        /// 用户输入
        input: String,
    },
    /// 先回写这行响应，然后进程退出 0
    Shutdown(String),
    /// 不产生任何输出（如宿主发来的通知）
    None,
}

/// 构造成功响应行
pub fn result_line(id: u64, result: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
}

/// 构造错误响应行（id 为 None 时输出 null，对应解析错误场景）
pub fn error_line(id: Option<u64>, code: i64, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message},
    })
    .to_string()
}

/// AgentEvent → {"type": ..., "data": ...}（web 壳直接复用，包进 {"t":"event","evt":...}）
pub fn event_json(event: &AgentEvent) -> Value {
    let (event_type, data) = match event {
        AgentEvent::AgentStart => ("agent_start", json!({})),
        AgentEvent::MessageUpdate(text) => ("message_update", json!({"text": text})),
        AgentEvent::Thinking(text) => ("thinking", json!({"text": text})),
        AgentEvent::Evolved(text) => ("evolved", json!({"text": text})),
        AgentEvent::ToolCall { name, arguments } => {
            ("tool_call", json!({"name": name, "arguments": arguments}))
        }
        AgentEvent::ToolResult { name, output } => {
            ("tool_result", json!({"name": name, "output": output}))
        }
        AgentEvent::Steered(instruction) => ("steered", json!({"instruction": instruction})),
        AgentEvent::Done { final_text } => ("done", json!({"final_text": final_text})),
        AgentEvent::UsageUpdate(u) => ("usage_update", json!({
            "input_tokens": u.input_tokens,
            "output_tokens": u.output_tokens,
            "llm_calls": u.llm_calls,
        })),
        AgentEvent::Error(message) => ("error", json!({"message": message})),
    };
    json!({"type": event_type, "data": data})
}

/// AgentEvent → 通知行（无 id）
pub fn event_notification(event: &AgentEvent) -> String {
    let evt = event_json(event);
    json!({
        "jsonrpc": "2.0",
        "method": "event",
        "params": evt,
    })
    .to_string()
}

/// 默认配置解析：显式路径 > ~/.r2/config.toml（存在时）> 内置默认
fn default_config_loader(config_path: Option<&str>) -> Result<Config, String> {
    match config_path {
        Some(path) => {
            if !std::path::Path::new(path).exists() {
                return Err(format!("指定的配置文件不存在：{path}"));
            }
            Config::load_from_file(path).map_err(|e| format!("加载配置失败：{e}"))
        }
        None => {
            let default_path = crate::config::expand_tilde("~/.r2/config.toml");
            if std::path::Path::new(&default_path).exists() {
                Config::load_from_file(&default_path).map_err(|e| format!("加载配置失败：{e}"))
            } else {
                Ok(Config::default_config())
            }
        }
    }
}

impl Default for RpcServer {
    fn default() -> Self {
        Self::new()
    }
}

impl RpcServer {
    pub fn new() -> Self {
        Self {
            session: None,
            config_path: None,
            in_flight: false,
            steer_tx: None,
            config_loader: Box::new(default_config_loader),
        }
    }

    /// 设置默认配置路径（CLI --config）
    pub fn set_config_path(&mut self, path: String) {
        self.config_path = Some(path);
    }

    /// 测试注入配置解析器
    pub fn with_config_loader(
        mut self,
        loader: impl Fn(Option<&str>) -> Result<Config, String> + Send + Sync + 'static,
    ) -> Self {
        self.config_loader = Box::new(loader);
        self
    }

    /// 是否有 prompt 在途
    pub fn in_flight(&self) -> bool {
        self.in_flight
    }

    /// 处理一行请求 JSON，返回驱动层指令。同步方法：非 prompt 请求直接处理完。
    pub fn handle_line(&mut self, line: &str) -> RpcOutcome {
        let value: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(e) => {
                return RpcOutcome::Line(error_line(
                    None,
                    PARSE_ERROR,
                    &format!("JSON 解析失败：{e}"),
                ));
            }
        };
        // 宿主发来的通知（无 id）：本协议不定义，直接忽略
        let Some(id_value) = value.get("id") else {
            return RpcOutcome::None;
        };
        let Some(id) = id_value.as_u64() else {
            return RpcOutcome::Line(error_line(
                None,
                INVALID_REQUEST,
                "id 必须为非负整数",
            ));
        };
        let Some(method) = value.get("method").and_then(|m| m.as_str()) else {
            return RpcOutcome::Line(error_line(Some(id), INVALID_REQUEST, "缺少 method 字段"));
        };
        let params = value.get("params").cloned().unwrap_or(Value::Null);

        match method {
            "initialize" => self.handle_initialize(id, &params),
            "prompt" => self.handle_prompt(id, &params),
            "steer" => self.handle_steer(id, &params),
            "reset" => self.handle_reset(id),
            "branch" => self.handle_branch(id, &params),
            "resume" => self.handle_resume(id, &params),
            "shutdown" => self.handle_shutdown(id),
            _ => RpcOutcome::Line(error_line(
                Some(id),
                METHOD_NOT_FOUND,
                &format!("未知方法：{method}"),
            )),
        }
    }

    /// prompt 在途检查：除 steer 外的方法统一拦 -32002
    fn guard_in_flight(&self, id: u64) -> Option<RpcOutcome> {
        if self.in_flight {
            Some(RpcOutcome::Line(error_line(
                Some(id),
                PROMPT_IN_FLIGHT,
                "prompt in flight",
            )))
        } else {
            None
        }
    }

    fn handle_initialize(&mut self, id: u64, params: &Value) -> RpcOutcome {
        if let Some(outcome) = self.guard_in_flight(id) {
            return outcome;
        }
        // 可选的 config_path 参数：更新默认配置路径
        if let Some(path) = params.get("config_path") {
            match path.as_str() {
                Some(p) => self.config_path = Some(p.to_string()),
                None => {
                    return RpcOutcome::Line(error_line(
                        Some(id),
                        INVALID_PARAMS,
                        "config_path 必须是字符串",
                    ));
                }
            }
        }
        match self.create_session() {
            Ok(session) => {
                let session_id = session.session_id().map(|s| s.to_string());
                self.session = Some(session);
                RpcOutcome::Line(result_line(
                    id,
                    json!({
                        "session_id": session_id,
                        "version": env!("CARGO_PKG_VERSION"),
                    }),
                ))
            }
            Err(e) => RpcOutcome::Line(error_line(Some(id), SESSION_ERROR, &e)),
        }
    }

    fn handle_prompt(&mut self, id: u64, params: &Value) -> RpcOutcome {
        if let Some(outcome) = self.guard_in_flight(id) {
            return outcome;
        }
        let Some(input) = params.get("input").and_then(|v| v.as_str()) else {
            return RpcOutcome::Line(error_line(
                Some(id),
                INVALID_PARAMS,
                "缺少字符串参数 input",
            ));
        };
        // 未 initialize 时自动用默认配置初始化
        if self.session.is_none() {
            match self.create_session() {
                Ok(session) => self.session = Some(session),
                Err(e) => return RpcOutcome::Line(error_line(Some(id), SESSION_ERROR, &e)),
            }
        }
        self.in_flight = true;
        RpcOutcome::PendingPrompt {
            id,
            input: input.to_string(),
        }
    }

    fn handle_steer(&mut self, id: u64, params: &Value) -> RpcOutcome {
        let Some(instruction) = params.get("instruction").and_then(|v| v.as_str()) else {
            return RpcOutcome::Line(error_line(
                Some(id),
                INVALID_PARAMS,
                "缺少字符串参数 instruction",
            ));
        };
        // 在途：走 begin_prompt 克隆出的发送端；空闲：直接走会话的发送端
        // （非运行时注入的指令会被下次 run 开头丢弃，语义与 CLI 一致）
        let result = match (&self.steer_tx, &self.session) {
            (Some(tx), _) => tx.try_send(instruction.to_string()),
            (None, Some(session)) => session.steer_handle().try_send(instruction.to_string()),
            (None, None) => {
                return RpcOutcome::Line(error_line(
                    Some(id),
                    SESSION_ERROR,
                    "会话尚未初始化",
                ));
            }
        };
        match result {
            Ok(()) => RpcOutcome::Line(result_line(id, json!({}))),
            Err(e) => RpcOutcome::Line(error_line(
                Some(id),
                SESSION_ERROR,
                &format!("steer 注入失败：{e}"),
            )),
        }
    }

    fn handle_reset(&mut self, id: u64) -> RpcOutcome {
        if let Some(outcome) = self.guard_in_flight(id) {
            return outcome;
        }
        if self.session.is_none() {
            match self.create_session() {
                Ok(session) => self.session = Some(session),
                Err(e) => return RpcOutcome::Line(error_line(Some(id), SESSION_ERROR, &e)),
            }
        }
        let session = self.session.as_mut().expect("刚确保过会话存在");
        session.reset_context();
        let session_id = session.session_id().map(|s| s.to_string());
        RpcOutcome::Line(result_line(id, json!({"session_id": session_id})))
    }

    fn handle_branch(&mut self, id: u64, params: &Value) -> RpcOutcome {
        if let Some(outcome) = self.guard_in_flight(id) {
            return outcome;
        }
        let Some(parent_id) = params.get("parent_id").and_then(|v| v.as_str()) else {
            return RpcOutcome::Line(error_line(
                Some(id),
                INVALID_PARAMS,
                "缺少字符串参数 parent_id",
            ));
        };
        let upto = match params.get("upto") {
            None | Some(Value::Null) => None,
            Some(v) => match v.as_u64() {
                Some(n) => Some(n as usize),
                None => {
                    return RpcOutcome::Line(error_line(
                        Some(id),
                        INVALID_PARAMS,
                        "upto 必须为非负整数",
                    ));
                }
            },
        };
        let config = match (self.config_loader)(self.config_path.as_deref()) {
            Ok(c) => c,
            Err(e) => return RpcOutcome::Line(error_line(Some(id), SESSION_ERROR, &e)),
        };
        match AgentSession::branch_from(config, parent_id, upto) {
            Ok(session) => {
                let session_id = session.session_id().map(|s| s.to_string());
                let inherited_count = session.history_len();
                self.session = Some(session);
                RpcOutcome::Line(result_line(
                    id,
                    json!({"session_id": session_id, "inherited_count": inherited_count}),
                ))
            }
            Err(e) => RpcOutcome::Line(error_line(Some(id), SESSION_ERROR, &e)),
        }
    }

    fn handle_resume(&mut self, id: u64, params: &Value) -> RpcOutcome {
        if let Some(outcome) = self.guard_in_flight(id) {
            return outcome;
        }
        let Some(session_id) = params.get("session_id").and_then(|v| v.as_str()) else {
            return RpcOutcome::Line(error_line(
                Some(id),
                INVALID_PARAMS,
                "缺少字符串参数 session_id",
            ));
        };
        let config = match (self.config_loader)(self.config_path.as_deref()) {
            Ok(c) => c,
            Err(e) => return RpcOutcome::Line(error_line(Some(id), SESSION_ERROR, &e)),
        };
        match AgentSession::resume(config, session_id) {
            Ok(session) => {
                let new_id = session.session_id().map(|s| s.to_string());
                let message_count = session.history_len();
                self.session = Some(session);
                RpcOutcome::Line(result_line(
                    id,
                    json!({"session_id": new_id, "message_count": message_count}),
                ))
            }
            Err(e) => RpcOutcome::Line(error_line(Some(id), SESSION_ERROR, &e)),
        }
    }

    fn handle_shutdown(&mut self, id: u64) -> RpcOutcome {
        if let Some(outcome) = self.guard_in_flight(id) {
            return outcome;
        }
        RpcOutcome::Shutdown(result_line(id, json!({})))
    }

    /// 创建新会话（用当前 config_path 解析配置）
    fn create_session(&self) -> Result<AgentSession, String> {
        let config = (self.config_loader)(self.config_path.as_deref())?;
        AgentSession::new(config)
    }

    /// 驱动层调用：取出会话开始异步执行 prompt。
    /// 返回 (会话, 事件订阅)；同时缓存 steer 发送端供 handle_steer 使用。
    pub fn begin_prompt(&mut self) -> Option<(AgentSession, broadcast::Receiver<AgentEvent>)> {
        let session = self.session.take()?;
        let events = session.subscribe();
        self.steer_tx = Some(session.steer_handle());
        Some((session, events))
    }

    /// 驱动层调用：prompt 完成后把会话放回
    pub fn end_prompt(&mut self, session: AgentSession) {
        self.session = Some(session);
        self.in_flight = false;
        self.steer_tx = None;
    }

    /// 驱动层调用：begin_prompt 失败时回滚 in_flight 标记
    pub fn cancel_prompt(&mut self) {
        self.in_flight = false;
    }

    /// 测试注入会话（跳过配置加载，便于 MockProvider 场景）
    #[cfg(test)]
    pub(crate) fn inject_session(&mut self, session: AgentSession) {
        self.session = Some(session);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;
    use crate::model::{ChunkStream, ModelProvider, ModelResult};
    use crate::types::{Message, StreamChunk, ToolCall, ToolSchema};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Mock Provider：第 0 次调用吐"第一段"后挂起等 gate（steer 测试在此打断）；
    /// 后续调用直接吐"最终回复"结束。
    struct MockProvider {
        gate: tokio::sync::watch::Receiver<bool>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ModelProvider for MockProvider {
        fn id(&self) -> &str {
            "mock"
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolSchema],
        ) -> ModelResult<ChunkStream> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let gate = self.gate.clone();
            let stream = futures_util::stream::unfold((n, 0usize), move |(n, step)| {
                let mut gate = gate.clone();
                async move {
                    match (n, step) {
                        (0, 0) => Some((Ok(StreamChunk::Delta("第一段".to_string())), (n, 1))),
                        (0, 1) => {
                            while !*gate.borrow() {
                                if gate.changed().await.is_err() {
                                    break;
                                }
                            }
                            Some((Ok(StreamChunk::Delta("第二段".to_string())), (n, 2)))
                        }
                        (0, 2) => Some((Ok(StreamChunk::Done), (n, 3))),
                        (_, 0) => Some((Ok(StreamChunk::Delta("最终回复".to_string())), (n, 1))),
                        (_, 1) => Some((Ok(StreamChunk::Done), (n, 2))),
                        _ => None,
                    }
                }
            });
            Ok(Box::pin(stream))
        }

        fn parse_response(&self, chunks: &[StreamChunk]) -> ModelResult<(String, Vec<ToolCall>)> {
            let text: String = chunks
                .iter()
                .filter_map(|c| match c {
                    StreamChunk::Delta(s) => Some(s.as_str()),
                    _ => None,
                })
                .collect();
            Ok((text, vec![]))
        }
    }

    /// 构造注入 MockProvider 会话的 RpcServer（会话目录指向临时目录）
    fn mock_server(
        tmp: &tempfile::TempDir,
        gate: tokio::sync::watch::Receiver<bool>,
    ) -> RpcServer {
        let mut config = Config::default_config();
        config.session.dir = tmp.path().to_string_lossy().to_string();
        let mut agent = Agent::new(config).unwrap();
        agent.set_provider(Box::new(MockProvider {
            gate,
            calls: Arc::new(AtomicUsize::new(0)),
        }));
        let mut server = RpcServer::new();
        server.inject_session(AgentSession::wrap_test(agent));
        server
    }

    /// 解析响应行，返回 (id, result 或 error)
    fn parse_response(line: &str) -> Value {
        serde_json::from_str(line).expect("响应必须是合法 JSON")
    }

    #[test]
    fn test_rpc_initialize_and_prompt_serialize() {
        // 用临时会话目录的配置，避免写真实 ~/.r2
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_string_lossy().to_string();
        let server = RpcServer::new().with_config_loader(move |_| {
            let mut config = Config::default_config();
            config.session.dir = dir.clone();
            Ok(config)
        });
        let mut server = server;
        let outcome = server.handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#);
        let RpcOutcome::Line(line) = outcome else {
            panic!("initialize 应立即回行");
        };
        let resp = parse_response(&line);
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 1);
        // 结果只含 session_id 和 version，不泄漏内部状态
        let result = &resp["result"];
        assert!(result["session_id"].is_string());
        assert!(result["version"].is_string());
        assert_eq!(result.as_object().unwrap().len(), 2);
    }

    #[test]
    fn test_rpc_unknown_method() {
        let mut server = RpcServer::new();
        let outcome = server.handle_line(r#"{"jsonrpc":"2.0","id":7,"method":"fly"}"#);
        let RpcOutcome::Line(line) = outcome else {
            panic!("未知方法应立即回错误行");
        };
        let resp = parse_response(&line);
        assert_eq!(resp["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(resp["id"], 7);
    }

    #[test]
    fn test_rpc_prompt_in_flight() {
        let tmp = tempfile::tempdir().unwrap();
        let (_gate_tx, gate_rx) = tokio::sync::watch::channel(false);
        let mut server = mock_server(&tmp, gate_rx);
        // 第一个 prompt → PendingPrompt
        let outcome = server.handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"prompt","params":{"input":"任务"}}"#);
        assert!(matches!(outcome, RpcOutcome::PendingPrompt { id: 1, .. }));
        // 未完成时第二个 prompt → -32002
        let outcome = server.handle_line(r#"{"jsonrpc":"2.0","id":2,"method":"prompt","params":{"input":"插队"}}"#);
        let RpcOutcome::Line(line) = outcome else {
            panic!("在途期间的 prompt 应回错误行");
        };
        let resp = parse_response(&line);
        assert_eq!(resp["error"]["code"], PROMPT_IN_FLIGHT);
        assert_eq!(resp["id"], 2);
        // reset / shutdown 等同理被拦
        let outcome = server.handle_line(r#"{"jsonrpc":"2.0","id":3,"method":"reset"}"#);
        let RpcOutcome::Line(line) = outcome else {
            panic!("在途期间的 reset 应回错误行");
        };
        assert_eq!(parse_response(&line)["error"]["code"], PROMPT_IN_FLIGHT);
    }

    #[tokio::test]
    async fn test_rpc_steer_routing() {
        let tmp = tempfile::tempdir().unwrap();
        let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
        let mut server = mock_server(&tmp, gate_rx);

        let outcome = server.handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"prompt","params":{"input":"任务"}}"#);
        let RpcOutcome::PendingPrompt { id, input } = outcome else {
            panic!("应为 PendingPrompt");
        };
        assert_eq!(id, 1);
        let (session, mut events) = server.begin_prompt().expect("会话应存在");

        // 驱动 prompt（与本模块 serve 主循环同构：边跑边收事件）
        let driver = tokio::spawn(async move {
            let mut session = session;
            let mut collected = Vec::new();
            let result = {
                let fut = session.prompt(&input);
                tokio::pin!(fut);
                loop {
                    tokio::select! {
                        r = &mut fut => break r,
                        evt = events.recv() => {
                            if let Ok(e) = evt {
                                collected.push(event_notification(&e));
                            }
                        }
                    }
                }
            };
            while let Ok(e) = events.try_recv() {
                collected.push(event_notification(&e));
            }
            (session, result, collected)
        });

        // 等 prompt 进入流式等待，再注入 steer
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let outcome = server.handle_line(
            r#"{"jsonrpc":"2.0","id":2,"method":"steer","params":{"instruction":"改口令"}}"#,
        );
        let RpcOutcome::Line(line) = outcome else {
            panic!("steer 应立即回空结果行");
        };
        assert_eq!(parse_response(&line)["result"], json!({}));
        // 兜底放行
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let _ = gate_tx.send(true);

        let (session, result, notifications) = driver.await.expect("驱动任务应完成");
        server.end_prompt(session);
        assert!(!server.in_flight());
        let final_text = result.expect("prompt 应成功");
        assert_eq!(final_text, "最终回复");
        // steered 事件应作为通知出现
        assert!(
            notifications.iter().any(|n| n.contains("\"steered\"")),
            "应收到 steered 通知，实际：{notifications:?}"
        );
    }

    #[test]
    fn test_rpc_shutdown() {
        let mut server = RpcServer::new();
        let outcome = server.handle_line(r#"{"jsonrpc":"2.0","id":9,"method":"shutdown"}"#);
        let RpcOutcome::Shutdown(line) = outcome else {
            panic!("shutdown 应返回 Shutdown 指令");
        };
        let resp = parse_response(&line);
        assert_eq!(resp["id"], 9);
        assert_eq!(resp["result"], json!({}));
    }

    #[test]
    fn test_event_notification_serialize() {
        let line = event_notification(&AgentEvent::MessageUpdate("你好".into()));
        let v = parse_response(&line);
        assert_eq!(v["method"], "event");
        assert_eq!(v["params"]["type"], "message_update");
        assert_eq!(v["params"]["data"]["text"], "你好");
        assert!(v.get("id").is_none(), "通知不应带 id");

        let line = event_notification(&AgentEvent::ToolCall {
            name: "read".into(),
            arguments: "{}".into(),
        });
        let v = parse_response(&line);
        assert_eq!(v["params"]["type"], "tool_call");
        assert_eq!(v["params"]["data"]["name"], "read");

        let line = event_notification(&AgentEvent::Done {
            final_text: "完".into(),
        });
        assert_eq!(parse_response(&line)["params"]["type"], "done");

        let line = event_notification(&AgentEvent::Error("炸了".into()));
        let v = parse_response(&line);
        assert_eq!(v["params"]["type"], "error");
        assert_eq!(v["params"]["data"]["message"], "炸了");
    }

    #[test]
    fn test_rpc_malformed_json() {
        let mut server = RpcServer::new();
        let outcome = server.handle_line("这不是 JSON {{{");
        let RpcOutcome::Line(line) = outcome else {
            panic!("垃圾行应回解析错误");
        };
        let resp = parse_response(&line);
        assert_eq!(resp["error"]["code"], PARSE_ERROR);
        assert_eq!(resp["id"], Value::Null);
        // 服务仍然存活，后续请求正常处理
        let outcome = server.handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"shutdown"}"#);
        assert!(matches!(outcome, RpcOutcome::Shutdown(_)));
    }

    #[test]
    fn test_rpc_steer_without_session() {
        let mut server = RpcServer::new();
        let outcome = server.handle_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"steer","params":{"instruction":"x"}}"#,
        );
        let RpcOutcome::Line(line) = outcome else {
            panic!("无会话 steer 应回错误行");
        };
        assert_eq!(parse_response(&line)["error"]["code"], SESSION_ERROR);
    }
}
