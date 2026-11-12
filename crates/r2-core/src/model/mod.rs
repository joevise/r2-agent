//! 模型层抽象：ModelProvider trait + Provider 工厂

pub mod anthropic;
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

    /// 会话提示（v0.14.0）：有值的 provider 在 HTTP 请求头带 X-R2-Session，
    /// 排查假死类事故时可按会话聚合网关/provider 日志。
    /// 默认空实现——mock/NullProvider 等零改动。
    fn set_session_hint(&mut self, _id: &str) {}

    /// 输出预算覆盖（v0.15.2）：>0 时下一次 chat_stream 用该值作为 max_tokens。
    /// config 显式配置与 length 空响应自动重试（调大重发）共用此入口。
    /// 默认空实现——不支持的 provider（mock 等）零改动。
    fn set_max_tokens_override(&mut self, _n: usize) {}

    /// 当前生效的输出预算（0 = 未知/不支持；length 重试翻倍基数用）
    fn current_max_tokens(&self) -> usize {
        0
    }

    /// Token 计数（v0.1 用近似：字符数/2）
    fn count_tokens(&self, text: &str) -> usize {
        text.len() / 2
    }
}

/// 工厂函数：根据 Config 创建 Provider
pub fn create_provider(config: &crate::config::Config) -> ModelResult<Box<dyn ModelProvider>> {
    match config.model.provider.as_str() {
        "openai_compat" => {
            let mut p = openai_compat::OpenAiCompatProvider::new(
                &config.model.openai_compat.base_url,
                &config.model.openai_compat.api_key,
                &config.model.openai_compat.model,
            );
            // config 显式覆盖输出预算（>0 生效；0 = 用 models 注册表值）
            if config.model.openai_compat.max_output_tokens > 0 {
                p.set_max_tokens_override(config.model.openai_compat.max_output_tokens);
            }
            Ok(Box::new(p))
        }
        "anthropic" => Ok(Box::new(anthropic::AnthropicProvider::new(
            &config.model.anthropic.base_url,
            &config.model.anthropic.api_key,
            &config.model.anthropic.model,
        ))),
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

    /// v0.15.2：config max_output_tokens >0 时 override 生效；0 用注册表值
    #[test]
    fn test_create_provider_max_output_override() {
        let mut config = crate::config::Config::default_config();
        config.model.openai_compat.model = "glm-5.2".to_string();
        // 默认 0：注册表值 32768
        let p = create_provider(&config).unwrap();
        assert_eq!(p.current_max_tokens(), 32768);
        // 显式覆盖
        config.model.openai_compat.max_output_tokens = 50000;
        let p = create_provider(&config).unwrap();
        assert_eq!(p.current_max_tokens(), 50000);
    }
}
