//! 工具系统：Tool trait + ToolRegistry + 4 个核心工具（read/write/edit/bash）

mod bash;
mod edit;
mod mcp_admin;
mod read;
mod write;

use crate::types::{ToolCall, ToolSchema};
use std::path::{Component, Path, PathBuf};

/// 工具抽象：名称 + 描述 + JSON Schema + 执行
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// 工具名（模型通过该名字调用）
    fn name(&self) -> &str;
    /// 工具功能描述（给模型看）
    fn description(&self) -> &str;
    /// 参数 JSON Schema
    fn schema(&self) -> serde_json::Value;
    /// 执行工具。input 是模型给的参数 JSON。
    /// 返回值永远是给模型看的字符串：成功=结果文本，失败=以 "ERROR: " 开头的错误描述
    async fn execute(&self, input: &serde_json::Value) -> String;
}

/// 工具注册表：持有全部工具，负责 schema 导出与分发执行
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    /// 创建默认注册表：read/write/edit/bash 四个核心工具
    ///
    /// bash 工具按 sandbox_cfg 构造沙箱；level 非法返回错误（正常路径下 config 加载已校验）
    pub fn new_default(
        work_dir: &str,
        sandbox_cfg: &crate::config::SandboxConfig,
        config_path: Option<&str>,
    ) -> Result<Self, String> {
        let sandbox = crate::sandbox::Sandbox::from_config(sandbox_cfg)?;
        Ok(Self {
            tools: vec![
                Box::new(read::ReadTool::new(work_dir)),
                Box::new(write::WriteTool::new(work_dir)),
                Box::new(edit::EditTool::new(work_dir)),
                Box::new(bash::BashTool::new(
                    work_dir,
                    sandbox_cfg.bash_timeout_secs,
                    sandbox,
                    sandbox_cfg.bash_restrict_workdir,
                )),
                Box::new(mcp_admin::McpAdminTool::new(config_path)),
            ],
        })
    }

    /// 注册一个外部工具（MCP 适配器用）
    pub(crate) fn push_tool(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    /// 导出全部工具的 schema（发给模型）
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools
            .iter()
            .map(|t| ToolSchema {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.schema(),
            })
            .collect()
    }

    /// 执行一次工具调用。任何失败都转成 "ERROR: ..." 字符串返回，不 panic
    pub async fn execute(&self, call: &ToolCall) -> String {
        let Some(tool) = self.tools.iter().find(|t| t.name() == call.name) else {
            return format!("ERROR: 未知工具：{}", call.name);
        };
        let input: serde_json::Value = match serde_json::from_str(&call.arguments) {
            Ok(v) => v,
            Err(e) => return format!("ERROR: 参数 JSON 解析失败：{e}"),
        };
        tool.execute(&input).await
    }
}

/// 词法层面规范化路径：解析 `.` 与 `..`（不触碰文件系统）
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// 路径安全检查：把模型给出的路径解析到 work_dir 内
///
/// canonicalize 后必须以 work_dir 的 canonicalize 为前缀，否则视为越界。
/// 目标文件不存在时（write 场景），先词法规范化做前缀校验，
/// 父目录存在时再 canonicalize 父目录防符号链接逃逸。
fn resolve_safe_path(work_dir: &Path, path: &str) -> Result<PathBuf, String> {
    if path.trim().is_empty() {
        return Err("ERROR: path 参数为空".to_string());
    }
    let base = work_dir
        .canonicalize()
        .map_err(|e| format!("ERROR: 工作目录不可访问：{e}"))?;
    let joined = normalize(&base.join(path));
    if !joined.starts_with(&base) {
        return Err(format!("ERROR: 路径越界：{path} 不在工作目录内"));
    }
    // 目标已存在：canonicalize 防符号链接逃逸
    if joined.exists() {
        let canonical = joined
            .canonicalize()
            .map_err(|e| format!("ERROR: 路径解析失败：{e}"))?;
        if !canonical.starts_with(&base) {
            return Err(format!("ERROR: 路径越界：{path} 不在工作目录内"));
        }
        return Ok(canonical);
    }
    // 目标不存在（write 新建场景）：父目录存在则 canonicalize 父目录再拼接
    if let Some(parent) = joined.parent() {
        if parent.exists() {
            let parent = parent
                .canonicalize()
                .map_err(|e| format!("ERROR: 父目录解析失败：{e}"))?;
            if !parent.starts_with(&base) {
                return Err(format!("ERROR: 路径越界：{path} 不在工作目录内"));
            }
            let name = joined
                .file_name()
                .ok_or_else(|| "ERROR: 非法路径".to_string())?;
            return Ok(parent.join(name));
        }
    }
    // 父目录也不存在（write 会自动 mkdir -p）：词法校验已通过，直接返回
    Ok(joined)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_registry(dir: &Path) -> ToolRegistry {
        let cfg = crate::config::SandboxConfig {
            level: "off".to_string(),
            ..Default::default()
        };
        ToolRegistry::new_default(dir.to_str().unwrap(), &cfg, None).unwrap()
    }

    #[tokio::test]
    async fn test_unknown_tool_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = make_registry(tmp.path());
        let call = ToolCall {
            id: "c1".to_string(),
            name: "no_such_tool".to_string(),
            arguments: "{}".to_string(),
        };
        let result = reg.execute(&call).await;
        assert!(result.starts_with("ERROR: 未知工具"));
    }

    #[tokio::test]
    async fn test_bad_arguments_json_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = make_registry(tmp.path());
        let call = ToolCall {
            id: "c2".to_string(),
            name: "read".to_string(),
            arguments: "{not valid json".to_string(),
        };
        let result = reg.execute(&call).await;
        assert!(result.starts_with("ERROR: 参数 JSON 解析失败"));
    }

    #[test]
    fn test_schemas_contains_core_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = make_registry(tmp.path());
        let schemas = reg.schemas();
        assert_eq!(schemas.len(), 5);
        let names: Vec<&str> = schemas.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"read"));
        assert!(names.contains(&"write"));
        assert!(names.contains(&"edit"));
        assert!(names.contains(&"bash"));
        assert!(names.contains(&"mcp"));
        for s in &schemas {
            assert!(s.parameters["type"] == "object");
        }
    }
}
