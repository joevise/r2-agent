//! L1 工作记忆：最简上下文管理

use crate::model::ModelResult;
use crate::types::{Message, Role, ToolCall};

/// 近似 token 计数：字符数 / 2（中英文混合够用）
fn estimate_tokens(text: &str) -> usize {
    text.len() / 2
}

/// 估算一条完整消息的 token 数（content + 工具调用参数）
fn message_tokens(msg: &Message) -> usize {
    let mut tokens = estimate_tokens(&msg.content);
    if let Some(calls) = &msg.tool_calls {
        for tc in calls {
            tokens += estimate_tokens(&tc.arguments) + estimate_tokens(&tc.name);
        }
    }
    tokens
}

/// L1 工作记忆管理器
pub struct ContextManager {
    system_prompt: String,
    messages: Vec<Message>,
    token_count: usize,
    max_tokens: usize,
}

impl ContextManager {
    pub fn new(system_prompt: &str, max_tokens: usize) -> Self {
        Self {
            system_prompt: system_prompt.to_string(),
            messages: Vec::new(),
            token_count: 0,
            max_tokens,
        }
    }

    /// 追加一条普通消息，更新 token 计数；超限报错
    pub fn add_message(&mut self, role: Role, content: &str) -> ModelResult<()> {
        let tokens = estimate_tokens(content);
        self.check_limit(tokens)?;
        self.messages.push(Message {
            role,
            content: content.to_string(),
            tool_calls: None,
            tool_call_id: None,
        });
        self.token_count += tokens;
        Ok(())
    }

    /// 追加 assistant 消息（可携带工具调用）
    pub fn add_assistant_with_tools(
        &mut self,
        content: &str,
        tool_calls: Vec<ToolCall>,
    ) -> ModelResult<()> {
        let mut tokens = estimate_tokens(content);
        for tc in &tool_calls {
            tokens += estimate_tokens(&tc.arguments) + estimate_tokens(&tc.name);
        }
        self.check_limit(tokens)?;
        self.messages.push(Message {
            role: Role::Assistant,
            content: content.to_string(),
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
            tool_call_id: None,
        });
        self.token_count += tokens;
        Ok(())
    }

    /// 追加工具执行结果
    pub fn add_tool_result(&mut self, tool_call_id: &str, content: &str) -> ModelResult<()> {
        let tokens = estimate_tokens(content);
        self.check_limit(tokens)?;
        self.messages.push(Message {
            role: Role::Tool,
            content: content.to_string(),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.to_string()),
        });
        self.token_count += tokens;
        Ok(())
    }

    /// 从恢复的消息列表重建 L1（system_prompt 用当前配置的，历史消息直接灌入）
    pub fn from_messages(system_prompt: &str, messages: Vec<Message>, max_tokens: usize) -> Self {
        let token_count = messages.iter().map(message_tokens).sum();
        Self {
            system_prompt: system_prompt.to_string(),
            messages,
            token_count,
            max_tokens,
        }
    }

    /// 构建发给模型的消息序列：system + 全部历史
    pub fn build(&self) -> Vec<Message> {
        let mut msgs = Vec::with_capacity(self.messages.len() + 1);
        msgs.push(Message {
            role: Role::System,
            content: self.system_prompt.clone(),
            tool_calls: None,
            tool_call_id: None,
        });
        msgs.extend(self.messages.iter().cloned());
        msgs
    }

    pub fn token_count(&self) -> usize {
        self.token_count
    }

    fn check_limit(&self, additional: usize) -> ModelResult<()> {
        if self.token_count + additional > self.max_tokens {
            return Err(format!(
                "上下文超限：当前 {} tokens + 新增 {} tokens 超过上限 {} tokens",
                self.token_count, additional, self.max_tokens
            )
            .into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_system_first() {
        let mut ctx = ContextManager::new("你是 R2", 10_000);
        ctx.add_message(Role::User, "你好").unwrap();
        ctx.add_message(Role::Assistant, "你好！有什么可以帮你？").unwrap();

        let msgs = ctx.build();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, Role::System);
        assert_eq!(msgs[0].content, "你是 R2");
        assert_eq!(msgs[1].role, Role::User);
        assert_eq!(msgs[2].role, Role::Assistant);
    }

    #[test]
    fn test_token_count_monotonic() {
        let mut ctx = ContextManager::new("sys", 10_000);
        assert_eq!(ctx.token_count(), 0);
        ctx.add_message(Role::User, "hello world").unwrap();
        let c1 = ctx.token_count();
        assert!(c1 > 0);
        ctx.add_message(Role::Assistant, "hi there").unwrap();
        assert!(ctx.token_count() > c1);
    }

    #[test]
    fn test_context_overflow() {
        let mut ctx = ContextManager::new("sys", 10);
        let result = ctx.add_message(Role::User, "这是一条非常非常长的消息，肯定会超过十个 token 的限制");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("上下文超限"));
    }

    #[test]
    fn test_tool_messages() {
        let mut ctx = ContextManager::new("sys", 10_000);
        let tc = ToolCall {
            id: "call_1".to_string(),
            name: "bash".to_string(),
            arguments: r#"{"command":"ls"}"#.to_string(),
        };
        ctx.add_assistant_with_tools("我来执行", vec![tc]).unwrap();
        ctx.add_tool_result("call_1", "file1.rs").unwrap();

        let msgs = ctx.build();
        assert_eq!(msgs[1].role, Role::Assistant);
        assert!(msgs[1].tool_calls.is_some());
        assert_eq!(msgs[2].role, Role::Tool);
        assert_eq!(msgs[2].tool_call_id.as_deref(), Some("call_1"));
    }
}
