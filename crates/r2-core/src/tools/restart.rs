//! restart 工具（v0.17.0）：官方重启通道的 agent 入口。
//!
//! 替代「bash kill 自己」裸自杀模式：发进程级重启信号（host::request），
//! 宿主 web 壳订阅后做 graceful 收尾（等在途回合、停通道、停 MCP 子进程）
//! 再 exec 自替换（PID 不变，不依赖外部管理器）。
//!
//! 本工具不进审批门（它就是官方替代品，拦了 agent 会回去裸杀）；
//! 每次调用落 ~/.r2/approvals.log 留痕（decision = "restart-tool"）。

use super::Tool;

pub struct RestartTool;

impl RestartTool {
    pub fn new() -> Self {
        Self
    }
}

/// 留痕：复用 approvals.log 的 JSONL 格式（与 approval.rs 同 schema），
/// decision 固定 "restart-tool"。失败只 eprintln，不阻塞重启请求。
fn log_restart_to(path: &std::path::Path, reason: &str) {
    use std::io::Write;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = serde_json::json!({
        "ts": ts,
        "session": "restart-tool",
        "command": "restart",
        "reason": reason,
        "decision": "restart-tool",
        "scope": "",
    })
    .to_string();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let r = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| writeln!(f, "{line}"));
    if let Err(e) = r {
        eprintln!("[restart] 留痕日志追加失败（{path:?}）：{e}");
    }
}

#[async_trait::async_trait]
impl Tool for RestartTool {
    fn name(&self) -> &str {
        "restart"
    }

    fn description(&self) -> &str {
        "重启 R2 进程（官方重启通道）：进程映自替换、PID 不变，配置改动随之生效。本回合收尾后执行（最多等 10 秒），未完成的会话与任务重启后自动续跑恢复。改了配置需要生效、或用户要求重启时使用——绝不要再用 bash kill/pkill/systemctl 直接杀进程（会失联）。"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "reason": {"type": "string", "description": "重启原因（必填，如「应用新模型配置」），会写入重启回报与留痕日志"}
            },
            "required": ["reason"]
        })
    }

    async fn execute(&self, input: &serde_json::Value) -> String {
        let home = std::env::var("HOME").unwrap_or_default();
        let log_path = std::path::PathBuf::from(home).join(".r2/approvals.log");
        Self::do_execute(input, &log_path)
    }
}

impl RestartTool {
    /// 内层（日志路径可注入，测试用）
    fn do_execute(input: &serde_json::Value, log_path: &std::path::Path) -> String {
        let reason = input
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim();
        if reason.is_empty() {
            return "ERROR: reason 必填（重启原因，如「应用新模型配置」）".into();
        }
        crate::host::request(reason);
        log_restart_to(log_path, reason);
        format!(
            "🔄 重启已调度（原因：{reason}）：本回合收尾后执行，最多等 10 秒。\
             未完成的任务重启后由系统续跑机制自动继续；新配置随之生效。"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_restart_requires_reason() {
        let tool = RestartTool::new();
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("approvals.log");
        let r = RestartTool::do_execute(&serde_json::json!({}), &log);
        assert!(r.starts_with("ERROR:"), "{r}");
        let r = RestartTool::do_execute(&serde_json::json!({"reason": "  "}), &log);
        assert!(r.starts_with("ERROR:"), "{r}");
        // schema 声明 reason 必填
        assert_eq!(tool.schema()["required"], serde_json::json!(["reason"]));
    }

    #[tokio::test]
    async fn test_restart_requests_and_replies() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("approvals.log");
        let r = RestartTool::do_execute(&serde_json::json!({"reason": "单测重启"}), &log);
        assert!(r.contains("重启已调度"), "{r}");
        assert!(r.contains("单测重启"), "{r}");
        // 信号已发出 + 留痕落盘（decision = restart-tool）
        let body = std::fs::read_to_string(&log).unwrap();
        let v: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
        assert_eq!(v["decision"], "restart-tool");
        assert_eq!(v["reason"], "单测重启");
    }

    #[test]
    fn test_log_restart_to_jsonl() {
        // 留痕落盘：JSONL 一行，decision = restart-tool
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".r2/approvals.log");
        log_restart_to(&path, "单测留痕");
        log_restart_to(&path, "第二次");
        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v["decision"], "restart-tool");
        assert_eq!(v["reason"], "单测留痕");
        assert!(v["ts"].as_u64().unwrap() > 0);
    }
}
