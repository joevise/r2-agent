//! read 工具：读取工作目录内文件内容（UTF-8 文本，超 2000 行截断）

use super::{resolve_safe_path, Tool};
use std::path::PathBuf;

/// 最大返回行数，超过则截断
const MAX_LINES: usize = 2000;

/// read 工具：参数 {"path": "src/main.rs"}
pub struct ReadTool {
    work_dir: PathBuf,
}

impl ReadTool {
    pub fn new(work_dir: &str) -> Self {
        Self {
            work_dir: PathBuf::from(work_dir),
        }
    }
}

#[async_trait::async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "读取工作目录内的文件内容（UTF-8 文本）。超过 2000 行会截断。"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "文件路径（相对工作目录）"}
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, input: &serde_json::Value) -> String {
        let Some(path) = input.get("path").and_then(|v| v.as_str()) else {
            return "ERROR: 缺少 path 参数（字符串）".to_string();
        };
        let path = match resolve_safe_path(&self.work_dir, path) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => return format!("ERROR: 读取失败：{e}"),
        };
        let content = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => return "ERROR: 非 UTF-8 文件（可能是二进制文件），无法读取".to_string(),
        };
        let lines: Vec<&str> = content.lines().collect();
        if lines.len() > MAX_LINES {
            let mut out = lines[..MAX_LINES].join("\n");
            out.push_str(&format!("\n...(截断，共 {} 行)", lines.len()));
            out
        } else {
            content
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_read_ok() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "hello\nworld\n").unwrap();
        let tool = ReadTool::new(tmp.path().to_str().unwrap());
        let result = tool.execute(&serde_json::json!({"path": "a.txt"})).await;
        assert_eq!(result, "hello\nworld\n");
    }

    #[tokio::test]
    async fn test_read_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = ReadTool::new(tmp.path().to_str().unwrap());
        let result = tool
            .execute(&serde_json::json!({"path": "no_such.txt"}))
            .await;
        assert!(result.starts_with("ERROR: 读取失败"));
    }

    #[tokio::test]
    async fn test_read_path_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = ReadTool::new(tmp.path().to_str().unwrap());
        let result = tool
            .execute(&serde_json::json!({"path": "../outside.txt"}))
            .await;
        assert!(result.contains("路径越界"), "got: {result}");
    }

    #[tokio::test]
    async fn test_read_truncation() {
        let tmp = tempfile::tempdir().unwrap();
        let content: String = (1..=2500).map(|i| format!("line {i}\n")).collect();
        std::fs::write(tmp.path().join("big.txt"), &content).unwrap();
        let tool = ReadTool::new(tmp.path().to_str().unwrap());
        let result = tool.execute(&serde_json::json!({"path": "big.txt"})).await;
        assert!(result.contains("...(截断，共 2500 行)"));
        assert!(!result.contains("line 2500"));
        assert!(result.contains("line 2000"));
    }

    #[tokio::test]
    async fn test_read_missing_path_param() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = ReadTool::new(tmp.path().to_str().unwrap());
        let result = tool.execute(&serde_json::json!({})).await;
        assert!(result.starts_with("ERROR: 缺少 path"));
    }

    #[tokio::test]
    async fn test_read_binary_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("bin.dat"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
        let tool = ReadTool::new(tmp.path().to_str().unwrap());
        let result = tool.execute(&serde_json::json!({"path": "bin.dat"})).await;
        assert!(result.contains("非 UTF-8"));
    }

    /// 读目录而不是文件 → ERROR 不 panic
    #[tokio::test]
    async fn test_read_directory_errors() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("subdir")).unwrap();
        let tool = ReadTool::new(tmp.path().to_str().unwrap());
        let result = tool.execute(&serde_json::json!({"path": "subdir"})).await;
        assert!(result.starts_with("ERROR:"), "got: {result}");
    }

    /// path 参数类型错误（数字）→ ERROR 不 panic
    #[tokio::test]
    async fn test_read_path_wrong_type() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = ReadTool::new(tmp.path().to_str().unwrap());
        let result = tool.execute(&serde_json::json!({"path": 123})).await;
        assert!(result.starts_with("ERROR: 缺少 path"), "got: {result}");
    }
}
