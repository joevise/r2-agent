//! edit 工具：str_replace 机制的唯一匹配替换

use super::{resolve_safe_path, Tool};
use std::path::PathBuf;

/// edit 工具：参数 {"path": "...", "old_text": "...", "new_text": "..."}
pub struct EditTool {
    work_dir: PathBuf,
}

impl EditTool {
    pub fn new(work_dir: &str) -> Self {
        Self {
            work_dir: PathBuf::from(work_dir),
        }
    }
}

#[async_trait::async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "在文件中查找 old_text 并替换为 new_text。old_text 必须在文件中恰好出现一次，\
         否则报错。new_text 为空字符串表示删除 old_text。"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "文件路径（相对工作目录）"},
                "old_text": {"type": "string", "description": "要被替换的原文（必须唯一匹配）"},
                "new_text": {"type": "string", "description": "替换后的新文本（可为空串表示删除）"}
            },
            "required": ["path", "old_text", "new_text"]
        })
    }

    async fn execute(&self, input: &serde_json::Value) -> String {
        let Some(path_str) = input.get("path").and_then(|v| v.as_str()) else {
            return "ERROR: 缺少 path 参数（字符串）".to_string();
        };
        let Some(old_text) = input.get("old_text").and_then(|v| v.as_str()) else {
            return "ERROR: 缺少 old_text 参数（字符串）".to_string();
        };
        let Some(new_text) = input.get("new_text").and_then(|v| v.as_str()) else {
            return "ERROR: 缺少 new_text 参数（字符串）".to_string();
        };
        if old_text.is_empty() {
            return "ERROR: old_text 不能为空".to_string();
        }
        if old_text == new_text {
            return "ERROR: 内容相同，无需修改".to_string();
        }
        let path = match resolve_safe_path(&self.work_dir, path_str) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => return format!("ERROR: 读取失败：{e}"),
        };
        let content = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => return "ERROR: 非 UTF-8 文件（可能是二进制文件），无法编辑".to_string(),
        };
        let count = content.matches(old_text).count();
        match count {
            0 => "ERROR: 未找到 old_text，请确认文本与文件内容完全一致".to_string(),
            1 => {
                let new_content = content.replacen(old_text, new_text, 1);
                match std::fs::write(&path, &new_content) {
                    Ok(()) => format!(
                        "OK: 已替换 (old {} 字节 → new {} 字节)",
                        old_text.len(),
                        new_text.len()
                    ),
                    Err(e) => format!("ERROR: 写入失败：{e}"),
                }
            }
            n => format!("ERROR: old_text 匹配到 {n} 处，请增加上下文使其唯一"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, EditTool) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "foo bar baz\n").unwrap();
        let tool = EditTool::new(tmp.path().to_str().unwrap());
        (tmp, tool)
    }

    #[tokio::test]
    async fn test_edit_single_replace() {
        let (tmp, tool) = setup();
        let result = tool
            .execute(&serde_json::json!({"path": "f.txt", "old_text": "bar", "new_text": "qux"}))
            .await;
        assert!(result.starts_with("OK: 已替换 (old 3 字节 → new 3 字节)"));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("f.txt")).unwrap(),
            "foo qux baz\n"
        );
    }

    #[tokio::test]
    async fn test_edit_multiple_matches() {
        let (tmp, tool) = setup();
        std::fs::write(tmp.path().join("m.txt"), "aa aa aa").unwrap();
        let result = tool
            .execute(&serde_json::json!({"path": "m.txt", "old_text": "aa", "new_text": "bb"}))
            .await;
        assert!(result.contains("匹配到 3 处"), "got: {result}");
    }

    #[tokio::test]
    async fn test_edit_not_found() {
        let (_tmp, tool) = setup();
        let result = tool
            .execute(&serde_json::json!({"path": "f.txt", "old_text": "not_here", "new_text": "x"}))
            .await;
        assert!(result.contains("未找到 old_text"), "got: {result}");
    }

    #[tokio::test]
    async fn test_edit_delete_with_empty_new_text() {
        let (tmp, tool) = setup();
        let result = tool
            .execute(&serde_json::json!({"path": "f.txt", "old_text": " bar", "new_text": ""}))
            .await;
        assert!(result.starts_with("OK: 已替换"), "got: {result}");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("f.txt")).unwrap(),
            "foo baz\n"
        );
    }

    #[tokio::test]
    async fn test_edit_same_content() {
        let (_tmp, tool) = setup();
        let result = tool
            .execute(&serde_json::json!({"path": "f.txt", "old_text": "bar", "new_text": "bar"}))
            .await;
        assert!(result.contains("内容相同"), "got: {result}");
    }

    #[tokio::test]
    async fn test_edit_path_escape() {
        let (_tmp, tool) = setup();
        let result = tool
            .execute(&serde_json::json!({"path": "../f.txt", "old_text": "a", "new_text": "b"}))
            .await;
        assert!(result.contains("路径越界"), "got: {result}");
    }
}
