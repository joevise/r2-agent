//! Agent 运行时事件：供嵌入方订阅（broadcast 通道）

use crate::types::UsageStats;

/// Agent 一轮运行中广播的事件
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// 一轮开始
    AgentStart,
    /// 文本增量（流式）
    MessageUpdate(String),
    /// 模型思考增量（流式；GLM reasoning_content，仅展示不入历史）
    Thinking(String),
    /// 工具调用开始：名称 + 参数
    ToolCall { name: String, arguments: String },
    /// 工具执行结果
    ToolResult { name: String, output: String },
    /// 用户中途转向指令（steering）
    Steered(String),
    /// 一轮结束
    Done { final_text: String },
    /// 用量统计更新（Done 前发出，含会话累计值）
    UsageUpdate(UsageStats),
    /// 出错
    Error(String),
}
