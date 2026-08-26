//! MCP（Model Context Protocol）client：连接外部 MCP server（stdio 传输，
//! 行分隔 JSON-RPC 2.0），把 server 提供的 tools 动态注册进 ToolRegistry。
//!
//! 设计要点：
//! - 读写都是阻塞 std IO（MCP stdio server 请求-响应紧耦合），调用方在
//!   tokio::task::block_in_place 里使用，不引入 tokio::process 复杂度
//! - stdout 由独立 reader 线程逐行读入 mpsc 通道：request 用 recv_timeout
//!   实现超时保护（裸 BufReader 阻塞读无法超时）
//! - 同一 server 的多个工具共享一条 McpConnection（Arc<Mutex>）

use crate::config::{McpConfig, McpServerConfig};
use crate::tools::{Tool, ToolRegistry};
use crate::types::ToolSchema;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// MCP 协议版本（握手时声明）
const PROTOCOL_VERSION: &str = "2024-11-05";
/// 请求默认超时（秒）
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// 一个已连接的 MCP server 连接
pub struct McpConnection {
    name: String,
    child: Child,
    stdin: ChildStdin,
    /// reader 线程喂来的 stdout 行（Err = IO 错误）
    lines: mpsc::Receiver<Result<String, String>>,
    next_id: u64,
    timeout_secs: u64,
}

impl McpConnection {
    /// spawn 子进程 + initialize 握手 + initialized 通知
    pub fn connect(cfg: &McpServerConfig) -> Result<Self, String> {
        let mut child = Command::new(&cfg.command)
            .args(&cfg.args)
            .envs(&cfg.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // stderr 丢弃：不接盘会写满管道把 server 卡死；调试用 RUST_LOG 看 tracing
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("MCP server {} 启动失败（{}）：{e}", cfg.name, cfg.command))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("MCP server {} 无法打开 stdin", cfg.name))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("MCP server {} 无法打开 stdout", cfg.name))?;

        // reader 线程：阻塞读行进通道，request 侧用 recv_timeout 拿超时能力
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut buf = String::new();
                match reader.read_line(&mut buf) {
                    Ok(0) => break, // EOF：server 已退出
                    Ok(_) => {
                        if tx.send(Ok(buf.trim_end().to_string())).is_err() {
                            break; // 连接已 drop
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e.to_string()));
                        break;
                    }
                }
            }
        });

        let mut conn = Self {
            name: cfg.name.clone(),
            child,
            stdin,
            lines: rx,
            next_id: 1,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        };

        // 握手：initialize → notifications/initialized
        conn.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "clientInfo": {"name": "r2-agent", "version": env!("CARGO_PKG_VERSION")},
            }),
        )?;
        conn.notify("notifications/initialized")?;
        Ok(conn)
    }

    /// tools/list → Vec<ToolSchema>（server 侧原始工具名，无前缀）
    pub fn list_tools(&mut self) -> Result<Vec<ToolSchema>, String> {
        let result = self.request("tools/list", json!({}))?;
        let tools = result
            .get("tools")
            .and_then(|t| t.as_array())
            .ok_or_else(|| format!("MCP server {} 的 tools/list 响应缺少 tools 数组", self.name))?;
        let mut out = Vec::new();
        for t in tools {
            let Some(name) = t.get("name").and_then(|n| n.as_str()) else {
                continue; // 无名工具条目：跳过
            };
            let description = t
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string();
            let parameters = t
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            out.push(ToolSchema {
                name: name.to_string(),
                description,
                parameters,
            });
        }
        Ok(out)
    }

    /// tools/call，返回拼接的文本结果；isError / 无文本内容 → Err（含 "ERROR:" 前缀）
    pub fn call_tool(&mut self, name: &str, arguments: &Value) -> Result<String, String> {
        let result = self.request(
            "tools/call",
            json!({"name": name, "arguments": arguments}),
        )?;
        let text = concat_text_content(&result);
        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if is_error {
            return Err(format!("ERROR: MCP 工具 {name} 返回错误：{text}"));
        }
        if text.is_empty() {
            return Err(format!("ERROR: MCP 工具 {name} 未返回文本内容"));
        }
        Ok(text)
    }

    /// 写一行请求 → 阻塞读响应（跳过通知行与 id 不匹配的行），带超时保护
    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.write_line(&req)?;

        let deadline = Instant::now() + Duration::from_secs(self.timeout_secs);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!(
                    "MCP server {} 请求 {method} 超时（{}s）",
                    self.name, self.timeout_secs
                ));
            }
            let line = match self.lines.recv_timeout(remaining) {
                Ok(Ok(line)) => line,
                Ok(Err(e)) => {
                    return Err(format!("MCP server {} 读取 stdout 失败：{e}", self.name))
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(format!(
                        "MCP server {} 请求 {method} 超时（{}s）",
                        self.name, self.timeout_secs
                    ))
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(format!("MCP server {} 的 stdout 已关闭（server 退出？）", self.name))
                }
            };
            if line.trim().is_empty() {
                continue; // 空行跳过
            }
            let msg: Value = serde_json::from_str(&line).map_err(|e| {
                let preview: String = line.chars().take(120).collect();
                format!("MCP server {} 返回无法解析的行：{e}（内容：{preview}）", self.name)
            })?;
            // 通知行（无 id）跳过；id 不匹配的响应也跳过（防御乱序 server）
            let Some(resp_id) = msg.get("id") else { continue };
            if resp_id.as_u64() != Some(id) {
                continue;
            }
            if let Some(err) = msg.get("error") {
                return Err(format!("MCP server {} 返回 JSON-RPC 错误：{err}", self.name));
            }
            return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// 发送通知（无 id，不等响应）
    fn notify(&mut self, method: &str) -> Result<(), String> {
        self.write_line(&json!({"jsonrpc": "2.0", "method": method}))
    }

    /// 写一行 JSON + flush
    fn write_line(&mut self, msg: &Value) -> Result<(), String> {
        let line = serde_json::to_string(msg)
            .map_err(|e| format!("MCP 请求序列化失败：{e}"))?;
        writeln!(self.stdin, "{line}")
            .and_then(|_| self.stdin.flush())
            .map_err(|e| format!("MCP server {} 写入 stdin 失败：{e}", self.name))
    }
}

impl Drop for McpConnection {
    /// 连接销毁时杀掉子进程，避免 MCP server 变孤儿
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 拼接 tools/call 响应里全部 text 内容块
fn concat_text_content(result: &Value) -> String {
    let Some(content) = result.get("content").and_then(|c| c.as_array()) else {
        return String::new();
    };
    content
        .iter()
        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 工具名合法化：非 [A-Za-z0-9_] 字符替换为 _
fn sanitize_name(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// MCP 工具包装：实现 Tool trait，转发调用到 McpConnection。
/// 同一 server 的多个 McpTool 共享一条连接（Arc<Mutex>）。
pub struct McpTool {
    /// 注册名：mcp_{server}_{tool}（避免与内置工具撞名）
    name: String,
    /// server 侧原始工具名（tools/call 时用）
    tool_name: String,
    description: String,
    parameters: Value,
    conn: Arc<tokio::sync::Mutex<McpConnection>>,
}

impl McpTool {
    pub fn new(
        server: &str,
        schema: ToolSchema,
        conn: Arc<tokio::sync::Mutex<McpConnection>>,
    ) -> Self {
        Self {
            name: format!("mcp_{}_{}", sanitize_name(server), sanitize_name(&schema.name)),
            tool_name: schema.name,
            description: schema.description,
            parameters: schema.parameters,
            conn,
        }
    }
}

#[async_trait::async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Value {
        self.parameters.clone()
    }

    async fn execute(&self, input: &Value) -> String {
        // McpConnection 是阻塞 std IO：block_in_place 让出 runtime worker，避免卡调度器
        //（注意：current_thread runtime 不支持 block_in_place，宿主需用 multi_thread）
        let result = tokio::task::block_in_place(|| {
            let mut conn = self.conn.blocking_lock();
            conn.call_tool(&self.tool_name, input)
        });
        match result {
            Ok(text) => text,
            // call_tool 的工具级错误已带 "ERROR:" 前缀；传输层错误在这里补上
            Err(e) if e.starts_with("ERROR:") => e,
            Err(e) => format!("ERROR: {e}"),
        }
    }
}

impl ToolRegistry {
    /// 连接所有配置的 MCP server，注册它们的工具。
    /// 单个 server 失败只 warn 跳过（MCP 是增强不是依赖）。
    /// 返回成功连接的 server 数。
    pub fn connect_mcp(&mut self, cfg: &McpConfig) -> usize {
        let mut connected = 0;
        for server in &cfg.servers {
            let result = McpConnection::connect(server)
                .and_then(|mut conn| conn.list_tools().map(|schemas| (conn, schemas)));
            match result {
                Ok((conn, mut schemas)) => {
                    // server 返回的工具顺序不稳定（实现相关），不排序会让 tools 列表
                    // 前缀抖动、击穿 KV-cache 命中——按名字排序保证跨调用稳定
                    schemas.sort_by(|a, b| a.name.cmp(&b.name));
                    let conn = Arc::new(tokio::sync::Mutex::new(conn));
                    let count = schemas.len();
                    for schema in schemas {
                        self.push_tool(Box::new(McpTool::new(
                            &server.name,
                            schema,
                            Arc::clone(&conn),
                        )));
                    }
                    // 走 stderr：serve 模式 stdout 是 JSON-RPC 协议通道，不能污染
                    eprintln!("[mcp] 已连接 MCP server {}（{count} 个工具）", server.name);
                    connected += 1;
                }
                Err(e) => {
                    tracing::warn!("MCP server {} 连接失败（跳过）：{e}", server.name);
                }
            }
        }
        connected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SandboxConfig;
    use crate::types::ToolCall;

    /// 检测 python3 是否可用（CI 无 python3 时测试跳过）
    fn python3_available() -> bool {
        Command::new("python3")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// mock server 配置；mode 传 "garbage" 启用坏响应模式
    fn mock_cfg(mode: &str) -> McpServerConfig {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/mock_mcp_server.py"
        );
        let mut args = vec![path.to_string()];
        if !mode.is_empty() {
            args.push(mode.to_string());
        }
        McpServerConfig {
            name: "mock".to_string(),
            command: "python3".to_string(),
            args,
        }
    }

    #[test]
    fn test_connect_and_list_tools() {
        if !python3_available() {
            return;
        }
        let mut conn = McpConnection::connect(&mock_cfg("")).expect("connect 应成功");
        let tools = conn.list_tools().expect("list_tools 应成功");
        assert_eq!(tools.len(), 2);
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"echo"));
        assert!(names.contains(&"fail"));
        assert_eq!(tools[0].parameters["type"], "object");
    }

    #[test]
    fn test_call_tool_echo() {
        if !python3_available() {
            return;
        }
        let mut conn = McpConnection::connect(&mock_cfg("")).unwrap();
        let result = conn
            .call_tool("echo", &json!({"text": "你好"}))
            .expect("echo 应成功");
        assert!(result.starts_with("echo: "), "实际：{result}");
        assert!(result.contains("你好"), "实际：{result}");
    }

    #[test]
    fn test_call_tool_fail_returns_error() {
        if !python3_available() {
            return;
        }
        let mut conn = McpConnection::connect(&mock_cfg("")).unwrap();
        let err = conn.call_tool("fail", &json!({})).unwrap_err();
        assert!(err.contains("ERROR"), "实际：{err}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_registry_connect_mcp_and_execute() {
        if !python3_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let sandbox = SandboxConfig {
            level: "off".to_string(),
            ..Default::default()
        };
        let mut reg = ToolRegistry::new_default(tmp.path().to_str().unwrap(), &sandbox, None).unwrap();
        let cfg = McpConfig {
            servers: vec![mock_cfg("")],
        };
        assert_eq!(reg.connect_mcp(&cfg), 1);

        // schema 导出含 mcp_mock_echo / mcp_mock_fail
        let names: Vec<String> = reg.schemas().iter().map(|s| s.name.clone()).collect();
        assert!(names.contains(&"mcp_mock_echo".to_string()), "实际：{names:?}");
        assert!(names.contains(&"mcp_mock_fail".to_string()), "实际：{names:?}");

        // 经 ToolRegistry.execute 走通完整调用链
        let call = ToolCall {
            id: "c1".to_string(),
            name: "mcp_mock_echo".to_string(),
            arguments: json!({"text": "hello"}).to_string(),
        };
        let result = reg.execute(&call).await;
        assert!(result.starts_with("echo: "), "实际：{result}");

        let fail_call = ToolCall {
            id: "c2".to_string(),
            name: "mcp_mock_fail".to_string(),
            arguments: "{}".to_string(),
        };
        let result = reg.execute(&fail_call).await;
        assert!(result.starts_with("ERROR:"), "实际：{result}");
    }

    #[test]
    fn test_garbage_server_returns_err_no_panic() {
        if !python3_available() {
            return;
        }
        // 坏响应 server：握手时收到垃圾行 → connect 报 Err，不 panic
        let result = McpConnection::connect(&mock_cfg("garbage"));
        assert!(result.is_err());
    }

    #[test]
    fn test_nonexistent_command_returns_err_no_panic() {
        let cfg = McpServerConfig {
            name: "ghost".to_string(),
            command: "/nonexistent/mcp-server".to_string(),
            args: vec![],
            env: Default::default(),
        };
        let result = McpConnection::connect(&cfg);
        assert!(result.is_err());
        assert!(result.err().unwrap().contains("启动失败"));
    }

    #[test]
    fn test_sanitize_name() {
        assert_eq!(sanitize_name("my-server.v2"), "my_server_v2");
        assert_eq!(sanitize_name("echo"), "echo");
    }
}
