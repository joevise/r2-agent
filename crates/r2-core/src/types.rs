//! 核心数据结构定义

use serde::{Deserialize, Serialize};

/// 消息角色
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// 对话消息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    /// 消息文本内容（tool 结果消息此字段为工具输出）
    pub content: String,
    /// 工具调用（仅 assistant 消息携带）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// 工具调用 ID（仅 role=Tool 时存在，对应 assistant 消息里的 tool_call id）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// 工具调用请求（模型发起）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// JSON 格式的参数字符串（OpenAI 惯例是字符串）
    pub arguments: String,
}

/// 工具定义（注册到 ToolRegistry 的 schema）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    /// JSON Schema 格式的参数定义
    pub parameters: serde_json::Value,
}

/// 一轮对话的用量统计
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageStats {
    pub input_tokens: u64,  // 发给模型的全部消息（含system）估算（服务端上报后被真实值校正）
    pub output_tokens: u64, // 模型回复估算（服务端上报后被真实值校正）
    pub llm_calls: u64,     // 模型调用次数（含摘要/重试）
    /// KV-cache 命中 token 数（服务端真实上报；命中率是第一生产指标）
    #[serde(default)]
    pub cached_tokens: u64,
}

/// 流式响应块
#[derive(Debug, Clone)]
pub enum StreamChunk {
    /// 文本增量
    Delta(String),
    /// 工具调用增量（index 用于拼装同一工具调用的多个分片）
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments_delta: String,
    },
    /// 模型思考增量（GLM reasoning_content 等；不计入正文，仅展示/用量）
    Reasoning(String),
    /// 服务端上报的真实用量（SSE 尾部 usage 对象；有则覆盖本轮估算）
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cached_tokens: u64,
    },
    /// 流结束
    Done,
}
