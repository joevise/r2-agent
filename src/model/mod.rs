//! 模型层抽象：ModelProvider trait + Provider 工厂

pub mod openai_compat;

use crate::types::{Message, StreamChunk, ToolCall, ToolSchema};
use futures_util::Stream;
use std::pin::Pin;

/// 模型层统一错误类型
pub type ModelResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
/// 流式响应块流
pub type ChunkStream = Pin<Box<dyn Stream<Item = ModelResult<StreamChunk>> + Send>>;

/// 模型提供商抽象
#[async_trait::async_trait]
pub trait ModelProvider: Send + Sync {
    /// Provider 标识
    fn id(&self) -> &str;

    /// 流式对话：messages + tools（可能为空） → StreamChunk 流
    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
    ) -> ModelResult<ChunkStream>;

    /// 从流式 chunks 合成完整响应：(文本, 工具调用列表)
    fn parse_response(&self, chunks: &[StreamChunk]) -> ModelResult<(String, Vec<ToolCall>)>;

    /// Token 计数（v0.1 用近似：字符数/2）
    fn count_tokens(&self, text: &str) -> usize {
        text.len() / 2
    }
}

/// 工厂函数：根据 Config 创建 Provider
pub fn create_provider(config: &crate::config::Config) -> ModelResult<Box<dyn ModelProvider>> {
    match config.model.provider.as_str() {
        "openai_compat" => Ok(Box::new(openai_compat::OpenAiCompatProvider::new(
            &config.model.openai_compat.base_url,
            &config.model.openai_compat.api_key,
            &config.model.openai_compat.model,
        ))),
        "anthropic" => Err("anthropic provider 尚未实现（P0.5）".into()),
        other => Err(format!("未知 provider: {other}").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_provider_openai_compat() {
        let config = crate::config::Config::default_config();
        let provider = create_provider(&config).unwrap();
        assert_eq!(provider.id(), "openai_compat");
    }

    #[test]
    fn test_create_provider_unknown() {
        let mut config = crate::config::Config::default_config();
        config.model.provider = "unknown".to_string();
        assert!(create_provider(&config).is_err());
    }
}
