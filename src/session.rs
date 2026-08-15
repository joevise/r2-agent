//! 会话持久化：JSONL 追加写 + 崩溃恢复
//!
//! 存储格式：每行一个完整 JSON 对象（带 type 字段的多态记录），
//! 文件位置 {session_dir}/{session_id}.jsonl。每行立即 flush，
//! 断电最多丢失当前正在写的半行；恢复时残行直接丢弃。

use crate::types::{Message, Role, ToolCall};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// 当前 Unix 时间戳（秒）
fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// JSONL 中的一行记录
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEntry {
    /// 用户 / assistant 消息（assistant 可携带 tool_calls）
    Message {
        role: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<ToolCall>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
        ts: u64,
    },
    /// 工具执行结果
    ToolResult {
        call_id: String,
        content: String,
        ts: u64,
    },
    /// 轮次检查点（v0.1 仅落盘，恢复时忽略）
    Checkpoint { turn: usize, ts: u64 },
}

impl SessionEntry {
    /// 构造一条普通消息记录（user / assistant）
    pub fn message(role: Role, content: &str) -> Self {
        Self::Message {
            role: role_to_str(&role).to_string(),
            content: content.to_string(),
            tool_calls: None,
            tool_call_id: None,
            ts: now_ts(),
        }
    }

    /// 构造一条 assistant 消息记录（可携带工具调用）
    pub fn assistant(content: &str, tool_calls: Vec<ToolCall>) -> Self {
        Self::Message {
            role: "assistant".to_string(),
            content: content.to_string(),
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
            tool_call_id: None,
            ts: now_ts(),
        }
    }

    /// 构造一条工具结果记录
    pub fn tool_result(call_id: &str, content: &str) -> Self {
        Self::ToolResult {
            call_id: call_id.to_string(),
            content: content.to_string(),
            ts: now_ts(),
        }
    }

    /// 构造一条轮次检查点记录
    pub fn checkpoint(turn: usize) -> Self {
        Self::Checkpoint {
            turn,
            ts: now_ts(),
        }
    }
}

/// Role 枚举 → 字符串（持久化用）
fn role_to_str(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// 会话：id + append 模式的 JSONL 文件句柄
pub struct Session {
    id: String,
    file: BufWriter<File>,
}

impl Session {
    /// 创建新会话（uuid v4）。自动创建会话目录。
    pub fn create(session_dir: &str) -> Result<Self, String> {
        fs::create_dir_all(session_dir)
            .map_err(|e| format!("创建会话目录失败（{session_dir}）：{e}"))?;
        let id = uuid::Uuid::new_v4().to_string();
        let path = session_path(session_dir, &id);
        let file = open_append(&path)?;
        Ok(Self {
            id,
            file: BufWriter::new(file),
        })
    }

    /// 恢复已有会话：读 JSONL 重建消息列表。
    ///
    /// 崩溃安全核心：最后一行如果不完整（解析失败），丢弃它；
    /// 中间坏行同样跳过（日志 warn，历史可能被手动编辑过）。
    pub fn recover(session_dir: &str, session_id: &str) -> Result<(Self, Vec<Message>), String> {
        let path = session_path(session_dir, session_id);
        if !path.exists() {
            return Err(format!("会话不存在：{session_id}"));
        }
        let content = fs::read_to_string(&path)
            .map_err(|e| format!("读取会话文件失败（{}）：{e}", path.display()))?;

        let lines: Vec<&str> = content.lines().collect();
        let mut messages = Vec::new();
        for (i, raw) in lines.iter().enumerate() {
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            let is_last = i == lines.len() - 1;
            match serde_json::from_str::<SessionEntry>(line) {
                Ok(SessionEntry::Message {
                    role,
                    content,
                    tool_calls,
                    tool_call_id,
                    ..
                }) => {
                    let role = match role.as_str() {
                        "user" => Role::User,
                        "assistant" => Role::Assistant,
                        "system" => Role::System,
                        other => {
                            tracing::warn!("会话 {session_id} 第 {} 行 role 未知（{other}），跳过", i + 1);
                            continue;
                        }
                    };
                    messages.push(Message {
                        role,
                        content,
                        tool_calls,
                        tool_call_id,
                    });
                }
                Ok(SessionEntry::ToolResult {
                    call_id, content, ..
                }) => messages.push(Message {
                    role: Role::Tool,
                    content,
                    tool_calls: None,
                    tool_call_id: Some(call_id),
                }),
                Ok(SessionEntry::Checkpoint { .. }) => {
                    // v0.1 不需要检查点，忽略
                }
                Err(e) => {
                    if is_last {
                        // 崩溃残行：静默丢弃
                        tracing::debug!("会话 {session_id} 末尾残行已丢弃");
                    } else {
                        tracing::warn!("会话 {session_id} 第 {} 行解析失败，跳过：{e}", i + 1);
                    }
                }
            }
        }

        let file = open_append(&path)?;
        Ok((
            Self {
                id: session_id.to_string(),
                file: BufWriter::new(file),
            },
            messages,
        ))
    }

    /// 追加一条记录。每行立即 flush（崩溃安全：断电最多丢当前行）。
    pub fn append(&mut self, entry: &SessionEntry) -> Result<(), String> {
        let line = serde_json::to_string(entry).map_err(|e| format!("序列化会话记录失败：{e}"))?;
        writeln!(self.file, "{line}").map_err(|e| format!("写入会话文件失败：{e}"))?;
        self.file.flush().map_err(|e| format!("刷新会话文件失败：{e}"))?;
        Ok(())
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

/// 会话摘要（sessions 列表用）
pub struct SessionSummary {
    pub id: String,
    /// 消息条数（message + tool_result 记录数）
    pub message_count: usize,
    /// 首条用户消息预览（截断）
    pub first_user_preview: String,
    /// 最后一条记录的时间戳
    pub last_ts: u64,
}

/// 列出会话目录下的所有会话摘要，按最后活跃时间倒序
pub fn list_sessions(session_dir: &str) -> Result<Vec<SessionSummary>, String> {
    let dir = Path::new(session_dir);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut summaries = Vec::new();
    let entries = fs::read_dir(dir).map_err(|e| format!("读取会话目录失败：{e}"))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
            continue;
        };
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let mut summary = SessionSummary {
            id,
            message_count: 0,
            first_user_preview: String::new(),
            last_ts: 0,
        };
        for line in content.lines() {
            let Ok(entry) = serde_json::from_str::<SessionEntry>(line.trim()) else {
                continue; // 坏行 / 残行不影响列表
            };
            match entry {
                SessionEntry::Message {
                    ref role,
                    ref content,
                    ts,
                    ..
                } => {
                    summary.message_count += 1;
                    summary.last_ts = summary.last_ts.max(ts);
                    if role == "user" && summary.first_user_preview.is_empty() {
                        summary.first_user_preview = content.chars().take(40).collect();
                    }
                }
                SessionEntry::ToolResult { ts, .. } => {
                    summary.message_count += 1;
                    summary.last_ts = summary.last_ts.max(ts);
                }
                SessionEntry::Checkpoint { ts, .. } => {
                    summary.last_ts = summary.last_ts.max(ts);
                }
            }
        }
        summaries.push(summary);
    }
    summaries.sort_by(|a, b| b.last_ts.cmp(&a.last_ts));
    Ok(summaries)
}

fn session_path(session_dir: &str, session_id: &str) -> PathBuf {
    Path::new(session_dir).join(format!("{session_id}.jsonl"))
}

fn open_append(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("打开会话文件失败（{}）：{e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_entry_serde_roundtrip() {
        let entry = SessionEntry::assistant(
            "好的",
            vec![ToolCall {
                id: "call_1".to_string(),
                name: "bash".to_string(),
                arguments: r#"{"command":"ls"}"#.to_string(),
            }],
        );
        let line = serde_json::to_string(&entry).unwrap();
        assert!(line.contains(r#""type":"message""#));
        let back: SessionEntry = serde_json::from_str(&line).unwrap();
        match back {
            SessionEntry::Message {
                role, tool_calls, ..
            } => {
                assert_eq!(role, "assistant");
                assert_eq!(tool_calls.unwrap()[0].name, "bash");
            }
            _ => panic!("应为 message 记录"),
        }
    }

    #[test]
    fn test_entry_type_tags() {
        let tr = serde_json::to_string(&SessionEntry::tool_result("c1", "ok")).unwrap();
        assert!(tr.contains(r#""type":"tool_result""#));
        let cp = serde_json::to_string(&SessionEntry::checkpoint(3)).unwrap();
        assert!(cp.contains(r#""type":"checkpoint""#));
    }
}
