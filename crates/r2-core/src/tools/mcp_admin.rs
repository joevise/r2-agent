//! mcp 工具：管理 MCP server 配置（跨会话持久化）
//!
//! 背景：agent 靠 bash 手搓 MCP 客户端虽然可行（能力证明），但产物是一次性脚本，
//! 新会话即蒸发。本工具把"装 MCP"变成一等公民动作：写入 config.toml 的
//! [[mcp.servers]] 段，新会话启动时自动连接（connect_mcp）。
//!
//! 写入策略：**文本手术**（append / 精确删除块），不整体序列化——
//! 保住用户的注释、字段顺序和其他段，绝不重写整个文件。
//!
//! 语义：写入后**下次新建会话生效**（当前会话的工具清单已快照，不做热重连——
//! 先用法后造法，热重连等真实需求出现再议）。

use super::Tool;
use crate::config::McpServerConfig;

/// MCP server 配置的持久化管理工具
pub struct McpAdminTool {
    /// 配置文件路径（None = 运行时解析默认路径 ~/.r2/config.toml）
    config_path: Option<String>,
}

impl McpAdminTool {
    pub fn new(config_path: Option<&str>) -> Self {
        Self {
            config_path: config_path.map(String::from),
        }
    }

    /// 实际写入目标路径：显式路径优先，否则 ~/.r2/config.toml
    fn resolve_path(&self) -> String {
        match &self.config_path {
            Some(p) => p.clone(),
            None => crate::config::expand_tilde("~/.r2/config.toml"),
        }
    }

    /// 读当前已配置的 servers（文件不存在 = 空）
    fn current_servers(&self, path: &str) -> Result<Vec<McpServerConfig>, String> {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return Ok(Vec::new()),
        };
        let value: toml::Value =
            toml::from_str(&content).map_err(|e| format!("配置文件解析失败：{e}"))?;
        Ok(value
            .get("mcp")
            .and_then(|m| m.get("servers"))
            .and_then(|s| s.clone().try_into::<Vec<McpServerConfig>>().ok())
            .unwrap_or_default())
    }

    /// 校验 server 名：工具名会拼成 mcp_{name}_{tool}，只允许安全字符
    fn valid_name(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    }

    /// TOML 基本字符串转义（name/command/args 都是常规值，处理引号/反斜杠足够）
    fn toml_quote(s: &str) -> String {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }

    /// 生成一个 [[mcp.servers]] 块（以空行开头，保证与上文分隔）
    fn server_block(s: &McpServerConfig) -> String {
        let mut out = String::from("\n[[mcp.servers]]\n");
        out.push_str(&format!("name = {}\n", Self::toml_quote(&s.name)));
        out.push_str(&format!("command = {}\n", Self::toml_quote(&s.command)));
        if !s.args.is_empty() {
            let args: Vec<String> = s.args.iter().map(|a| Self::toml_quote(a)).collect();
            out.push_str(&format!("args = [{}]\n", args.join(", ")));
        }
        if !s.env.is_empty() {
            // 内联表：与 config.toml 现有 env = { KEY = "value" } 风格一致
            let pairs: Vec<String> = s
                .env
                .iter()
                .map(|(k, v)| format!("{} = {}", k, Self::toml_quote(v)))
                .collect();
            out.push_str(&format!("env = {{ {} }}\n", pairs.join(", ")));
        }
        out
    }

    /// add 动作：查重 → append 块到文件尾
    fn do_add(&self, input: &serde_json::Value) -> String {
        let name = input
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let command = input
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if !Self::valid_name(&name) {
            return "ERROR: name 必须是 1-64 位的字母/数字/-/_（工具名会拼成 mcp_{name}_{tool}）".into();
        }
        if command.trim().is_empty() {
            return "ERROR: command 不能为空（如 npx / uvx / node）".into();
        }
        let args: Vec<String> = input
            .get("args")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        // 可选环境变量注入（如 API key：{"TAVILY_API_KEY": "tvly-..."}）
        let env: std::collections::HashMap<String, String> = input
            .get("env")
            .and_then(|v| v.as_object())
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        let path = self.resolve_path();
        match self.current_servers(&path) {
            Ok(existing) => {
                if existing.iter().any(|s| s.name == name) {
                    return format!(
                        "ERROR: 已存在名为 {name} 的 server（用 mcp list 查看，remove 后可重装）"
                    );
                }
            }
            Err(e) => return format!("ERROR: {e}"),
        }

        let server = McpServerConfig {
            name: name.clone(),
            command,
            args,
            env,
        };
        // append：文件不存在则创建（新文件只含本块，仍是合法 TOML）
        let append = Self::server_block(&server);
        if let Err(e) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(append.as_bytes())
            })
        {
            return format!("ERROR: 写入 {path} 失败：{e}");
        }
        format!(
            "OK: MCP server「{name}」已写入 {path}（跨会话持久）。\n\
             ⚠ 下次新建会话时自动连接生效；如命令无效会在启动日志中 warn。当前会话不热加载。"
        )
    }

    /// remove 动作：文本手术删块（从 [[mcp.servers]] 头到下一个任意表头/EOF）
    fn do_remove(&self, input: &serde_json::Value) -> String {
        let name = input
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if name.is_empty() {
            return "ERROR: 需要要删除的 server name".into();
        }
        let path = self.resolve_path();
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return format!("ERROR: 配置文件不存在：{path}"),
        };

        let lines: Vec<&str> = content.lines().collect();
        let mut out: Vec<&str> = Vec::with_capacity(lines.len());
        let mut removed = false;
        let mut i = 0;
        while i < lines.len() {
            let t = lines[i].trim();
            if t == "[[mcp.servers]]" {
                // 块尾 = 下一个任意表头（'[' 开头）或 EOF
                let mut j = i + 1;
                while j < lines.len() && !lines[j].trim_start().starts_with('[') {
                    j += 1;
                }
                let is_target = lines[i..j]
                    .iter()
                    .any(|l| l.trim_start().starts_with("name") && l.contains(&name));
                if is_target {
                    removed = true;
                    while out.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
                        out.pop();
                    }
                    i = j; // 跳过整个目标块
                    continue;
                }
            }
            out.push(lines[i]);
            i += 1;
        }
        if !removed {
            return format!("ERROR: 没找到名为 {name} 的 mcp server（mcp list 查看）");
        }
        let mut new_content = out.join("\n");
        if !new_content.ends_with('\n') {
            new_content.push('\n');
        }
        if let Err(e) = std::fs::write(&path, &new_content) {
            return format!("ERROR: 写回失败：{e}");
        }
        format!("OK: MCP server「{name}」已从配置移除（新会话起不再连接）")
    }

    /// list 动作：从磁盘读最新状态（不依赖内存快照）
    fn do_list(&self) -> String {
        let path = self.resolve_path();
        match self.current_servers(&path) {
            Ok(servers) if servers.is_empty() => format!(
                "当前配置了 0 个 MCP server（配置文件：{path}）。\n\
                 安装示例：mcp add name=memory command=npx args=[\"-y\",\"@modelcontextprotocol/server-memory\"]"
            ),
            Ok(servers) => {
                let mut out = format!("配置文件 {} 共 {} 个 MCP server：\n", path, servers.len());
                for s in &servers {
                    let cmdline = std::iter::once(s.command.clone())
                        .chain(s.args.iter().cloned())
                        .collect::<Vec<_>>()
                        .join(" ");
                    out.push_str(&format!("\n- {} · `{cmdline}`", s.name));
                }
                out.push_str("\n（新会话启动时自动连接；工具名格式 mcp_{name}_{tool}）");
                out
            }
            Err(e) => format!("ERROR: {e}"),
        }
    }
}

#[async_trait::async_trait]
impl Tool for McpAdminTool {
    fn name(&self) -> &str {
        "mcp"
    }

    fn description(&self) -> &str {
        "管理 MCP server 配置（写入配置文件，跨会话持久生效）。action=add 安装（name+command+args），\
         action=list 查看，action=remove 卸载。需要给会话装新工具时优先用本工具，\
         不要用 bash 手搓一次性 MCP 客户端（新会话会丢失）。安装后下次新建会话生效。"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["add", "list", "remove"],
                    "description": "add=安装 / list=列出已配置 / remove=卸载"},
                "name": {"type": "string", "description": "server 名（add/remove 必填，字母数字-_）"},
                "command": {"type": "string", "description": "启动命令，如 npx / uvx / node（add 必填）"},
                "args": {"type": "array", "items": {"type": "string"},
                    "description": "命令参数，如 [\"-y\",\"@modelcontextprotocol/server-memory\"]（add 可选）"}
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: &serde_json::Value) -> String {
        match input.get("action").and_then(|v| v.as_str()) {
            Some("add") => self.do_add(input),
            Some("list") => self.do_list(),
            Some("remove") => self.do_remove(input),
            other => format!("ERROR: action 必须是 add/list/remove，收到 {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_with(path: &std::path::Path) -> McpAdminTool {
        McpAdminTool::new(Some(path.to_str().unwrap()))
    }

    #[tokio::test]
    async fn test_add_list_remove_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config.toml");
        // 预置：含其他段（验证不破坏）
        std::fs::write(&cfg, "# 用户注释\n[agent]\nwork_dir = \"/x\"\n").unwrap();
        let tool = tool_with(tmp.path().join("config.toml").as_path());

        // add
        let out = tool
            .execute(&serde_json::json!({
                "action": "add", "name": "memory",
                "command": "npx",
                "args": ["-y", "@modelcontextprotocol/server-memory"]
            }))
            .await;
        assert!(out.starts_with("OK"), "{out}");

        // 注释和其他段保留
        let content = std::fs::read_to_string(&cfg).unwrap();
        assert!(content.contains("# 用户注释"));
        assert!(content.contains("[agent]"));
        assert!(content.contains("[[mcp.servers]]"));
        assert!(content.contains("name = \"memory\""));
        // 用完整 Config 解析验证合法且 mcp 段正确
        let parsed = toml::from_str::<toml::Value>(&content).unwrap();
        let servers = parsed["mcp"]["servers"]
            .clone()
            .try_into::<Vec<McpServerConfig>>()
            .unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "memory");
        assert_eq!(servers[0].args, vec!["-y", "@modelcontextprotocol/server-memory"]);

        // list（读磁盘）
        let out = tool.execute(&serde_json::json!({"action": "list"})).await;
        assert!(out.contains("1 个 MCP server") && out.contains("memory"), "{out}");

        // 重复 add 报错
        let out = tool
            .execute(&serde_json::json!({"action": "add", "name": "memory", "command": "npx"}))
            .await;
        assert!(out.starts_with("ERROR") && out.contains("已存在"), "{out}");

        // remove
        let out = tool
            .execute(&serde_json::json!({"action": "remove", "name": "memory"}))
            .await;
        assert!(out.starts_with("OK"), "{out}");
        let content = std::fs::read_to_string(&cfg).unwrap();
        assert!(!content.contains("[[mcp.servers]]"));
        assert!(!content.contains("memory"));
        // 其他段和注释仍在
        assert!(content.contains("# 用户注释") && content.contains("[agent]"));
    }

    #[tokio::test]
    async fn test_add_two_remove_first_keeps_second() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg_path = tmp.path().join("c.toml");
        let tool = tool_with(&cfg_path);
        for name in ["alpha", "beta"] {
            tool.execute(&serde_json::json!({
                "action": "add", "name": name, "command": "node", "args": ["s.js"]
            }))
            .await;
        }
        // 删第一个，第二个必须完好
        tool.execute(&serde_json::json!({"action": "remove", "name": "alpha"}))
            .await;
        let out = tool.execute(&serde_json::json!({"action": "list"})).await;
        assert!(out.contains("beta") && !out.contains("alpha"), "{out}");
        let content = std::fs::read_to_string(&cfg_path).unwrap();
        let parsed = toml::from_str::<toml::Value>(&content).unwrap();
        let servers = parsed["mcp"]["servers"]
            .clone()
            .try_into::<Vec<McpServerConfig>>()
            .unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "beta");
    }

    #[tokio::test]
    async fn test_invalid_name_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tool_with(&tmp.path().join("c.toml"));
        let out = tool
            .execute(&serde_json::json!({"action": "add", "name": "bad name!", "command": "x"}))
            .await;
        assert!(out.starts_with("ERROR"), "{out}");
        let out = tool
            .execute(&serde_json::json!({"action": "add", "name": "ok", "command": " "}))
            .await;
        assert!(out.starts_with("ERROR"), "{out}");
    }
}
