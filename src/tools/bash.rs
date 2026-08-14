//! bash 工具：在工作目录内执行 shell 命令（v0.1 沙箱：超时 + 输出限制）

use super::Tool;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;

/// 输出上限：64KB
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
/// 超时上限（秒）：参数覆盖也不能超过该值
const MAX_TIMEOUT_SECS: u64 = 120;

/// bash 工具：参数 {"command": "ls -la", "timeout_secs": 可选}
pub struct BashTool {
    work_dir: PathBuf,
    default_timeout_secs: u64,
}

impl BashTool {
    pub fn new(work_dir: &str, default_timeout_secs: u64) -> Self {
        Self {
            work_dir: PathBuf::from(work_dir),
            default_timeout_secs,
        }
    }
}

#[async_trait::async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "在工作目录内执行 bash 命令。返回 exit_code 和 stdout+stderr（超 64KB 截断）。\
         默认超时 30s，可用 timeout_secs 覆盖（上限 120s）。"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "要执行的 bash 命令"},
                "timeout_secs": {"type": "integer", "description": "超时秒数（可选，默认 30，上限 120）"}
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, input: &serde_json::Value) -> String {
        let Some(command) = input.get("command").and_then(|v| v.as_str()) else {
            return "ERROR: 缺少 command 参数（字符串）".to_string();
        };
        if command.trim().is_empty() {
            return "ERROR: command 为空".to_string();
        }
        let timeout_secs = input
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(self.default_timeout_secs)
            .min(MAX_TIMEOUT_SECS);

        // v0.1 沙箱：仅 cwd + 超时 + 输出限制，不做 namespace/seccomp/chroot
        let mut cmd = Command::new("bash");
        cmd.arg("-c")
            .arg(command)
            .current_dir(&self.work_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // 子进程设为进程组组长，超时可整组 kill
            .process_group(0)
            .kill_on_drop(true);

        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return format!("ERROR: 启动进程失败：{e}"),
        };
        let pid = child.id();
        let fut = child.wait_with_output();
        tokio::pin!(fut);

        match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), &mut fut).await {
            Ok(Ok(output)) => {
                let code = output.status.code().unwrap_or(-1);
                let mut buf = Vec::with_capacity(output.stdout.len() + output.stderr.len());
                buf.extend_from_slice(&output.stdout);
                buf.extend_from_slice(&output.stderr);
                let truncated = buf.len() > MAX_OUTPUT_BYTES;
                if truncated {
                    buf.truncate(MAX_OUTPUT_BYTES);
                }
                let mut text = String::from_utf8_lossy(&buf).into_owned();
                if truncated {
                    text.push_str("\n...(输出截断，超过 64KB)");
                }
                format!("exit_code={code}\n\n{text}")
            }
            Ok(Err(e)) => format!("ERROR: 命令执行失败：{e}"),
            Err(_) => {
                // 超时：kill 整个进程组（负 pid = 进程组）
                if let Some(pid) = pid {
                    let _ = std::process::Command::new("kill")
                        .args(["-KILL", &format!("-{pid}")])
                        .status();
                }
                format!("ERROR: 命令超时({timeout_secs}s)被终止")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_bash_echo_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = BashTool::new(tmp.path().to_str().unwrap(), 30);
        let result = tool
            .execute(&serde_json::json!({"command": "echo hello"}))
            .await;
        assert!(result.starts_with("exit_code=0"), "got: {result}");
        assert!(result.contains("hello"));
    }

    #[tokio::test]
    async fn test_bash_nonzero_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = BashTool::new(tmp.path().to_str().unwrap(), 30);
        let result = tool
            .execute(&serde_json::json!({"command": "exit 42"}))
            .await;
        assert!(result.starts_with("exit_code=42"), "got: {result}");
    }

    #[tokio::test]
    async fn test_bash_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = BashTool::new(tmp.path().to_str().unwrap(), 30);
        let result = tool
            .execute(&serde_json::json!({"command": "sleep 5", "timeout_secs": 1}))
            .await;
        assert!(result.contains("命令超时(1s)被终止"), "got: {result}");
    }

    #[tokio::test]
    async fn test_bash_output_truncation() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = BashTool::new(tmp.path().to_str().unwrap(), 30);
        let result = tool
            .execute(&serde_json::json!({"command": "head -c 100000 /dev/zero | tr '\\0' 'a'"}))
            .await;
        assert!(result.contains("输出截断，超过 64KB"), "got len: {}", result.len());
    }

    #[tokio::test]
    async fn test_bash_runs_in_work_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("marker.txt"), "x").unwrap();
        let tool = BashTool::new(tmp.path().to_str().unwrap(), 30);
        let result = tool
            .execute(&serde_json::json!({"command": "ls marker.txt"}))
            .await;
        assert!(result.contains("marker.txt"), "got: {result}");
    }

    #[tokio::test]
    async fn test_bash_missing_command() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = BashTool::new(tmp.path().to_str().unwrap(), 30);
        let result = tool.execute(&serde_json::json!({})).await;
        assert!(result.starts_with("ERROR: 缺少 command"));
    }
}
