//! write 工具：覆盖写入工作目录内文件（自动创建父目录）

use super::{resolve_safe_path, Tool};
use std::path::PathBuf;

/// write 工具：参数 {"path": "src/foo.rs", "content": "文件全部内容"}
pub struct WriteTool {
    work_dir: PathBuf,
}

impl WriteTool {
    pub fn new(work_dir: &str) -> Self {
        Self {
            work_dir: PathBuf::from(work_dir),
        }
    }
}

#[async_trait::async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "将 content 完整写入工作目录内的文件（覆盖已有内容，自动创建父目录）。"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "文件路径（相对工作目录）"},
                "content": {"type": "string", "description": "要写入的文件全部内容"}
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, input: &serde_json::Value) -> String {
        let Some(path_str) = input.get("path").and_then(|v| v.as_str()) else {
            return "ERROR: 缺少 path 参数（字符串）".to_string();
        };
        let Some(content) = input.get("content").and_then(|v| v.as_str()) else {
            return "ERROR: content 缺失或非字符串".to_string();
        };
        let path = match resolve_safe_path(&self.work_dir, path_str) {
            Ok(p) => p,
            Err(e) => return e,
        };
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return format!("ERROR: 创建父目录失败：{e}");
            }
        }
        match std::fs::write(&path, content) {
            Ok(()) => format!("OK: 写入 {} ({} 字节)", path_str, content.len()),
            Err(e) => format!("ERROR: 写入失败：{e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_write_new_file_with_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = WriteTool::new(tmp.path().to_str().unwrap());
        let result = tool
            .execute(&serde_json::json!({"path": "sub/dir/f.txt", "content": "abc"}))
            .await;
        assert!(result.starts_with("OK: 写入 sub/dir/f.txt (3 字节)"));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("sub/dir/f.txt")).unwrap(),
            "abc"
        );
    }

    #[tokio::test]
    async fn test_write_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "old").unwrap();
        let tool = WriteTool::new(tmp.path().to_str().unwrap());
        let result = tool
            .execute(&serde_json::json!({"path": "f.txt", "content": "new content"}))
            .await;
        assert!(result.starts_with("OK:"));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("f.txt")).unwrap(),
            "new content"
        );
    }

    #[tokio::test]
    async fn test_write_path_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = WriteTool::new(tmp.path().to_str().unwrap());
        let result = tool
            .execute(&serde_json::json!({"path": "../evil.txt", "content": "x"}))
            .await;
        assert!(result.contains("路径越界"), "got: {result}");
    }

    #[tokio::test]
    async fn test_write_missing_content() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = WriteTool::new(tmp.path().to_str().unwrap());
        let result = tool.execute(&serde_json::json!({"path": "f.txt"})).await;
        assert!(result.starts_with("ERROR: content 缺失"));
    }

    /// path 指向已存在的目录 → ERROR 不 panic
    #[tokio::test]
    async fn test_write_to_directory_errors() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("subdir")).unwrap();
        let tool = WriteTool::new(tmp.path().to_str().unwrap());
        let result = tool
            .execute(&serde_json::json!({"path": "subdir", "content": "x"}))
            .await;
        assert!(result.starts_with("ERROR:"), "got: {result}");
    }

    /// path 参数类型错误（数字）→ ERROR 不 panic
    #[tokio::test]
    async fn test_write_path_wrong_type() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = WriteTool::new(tmp.path().to_str().unwrap());
        let result = tool
            .execute(&serde_json::json!({"path": 123, "content": "x"}))
            .await;
        assert!(result.starts_with("ERROR: 缺少 path"), "got: {result}");
    }
}
