//! Agent 循环引擎：用户输入 → 模型流式响应 → 工具执行 → 输出

use crate::config::Config;
use crate::context::ContextManager;
use crate::model::{create_provider, ModelProvider, ModelResult};
use crate::tools::ToolRegistry;
use crate::types::{Role, StreamChunk};
use futures_util::StreamExt;
use std::io::Write;

/// R2 Agent：Provider + L1 上下文 + 工具注册表 + 配置
pub struct Agent {
    provider: Box<dyn ModelProvider>,
    context: ContextManager,
    tools: ToolRegistry,
    config: Config,
}

impl Agent {
    pub fn new(config: Config) -> ModelResult<Self> {
        let provider = create_provider(&config)?;
        let max_tokens = config.agent.max_total_tokens;
        let context = ContextManager::new("你是 R2，一个极简但可靠的 Rust Agent。", max_tokens);
        let tools = ToolRegistry::new_default(
            &config.agent.work_dir,
            config.sandbox.bash_timeout_secs,
        );
        Ok(Self {
            provider,
            context,
            tools,
            config,
        })
    }

    /// 处理一次用户输入，流式打印 assistant 输出，返回完整回复文本
    pub async fn run(&mut self, user_input: &str) -> ModelResult<String> {
        self.context.add_message(Role::User, user_input)?;

        let mut final_text = String::new();
        for turn in 0..self.config.agent.max_turns {
            let messages = self.context.build();
            let mut stream = self
                .provider
                .chat_stream(&messages, &self.tools.schemas())
                .await
                .map_err(|e| format!("模型请求失败（第 {} 轮）：{}", turn + 1, e))?;

            let mut chunks: Vec<StreamChunk> = Vec::new();
            while let Some(item) = stream.next().await {
                match item {
                    Ok(chunk) => {
                        if let StreamChunk::Delta(ref s) = chunk {
                            print!("{s}");
                            let _ = std::io::stdout().flush();
                        }
                        chunks.push(chunk);
                    }
                    Err(e) => {
                        println!();
                        return Err(format!("流式响应中断：{e}").into());
                    }
                }
            }
            println!();

            let (text, tool_calls) = self
                .provider
                .parse_response(&chunks)
                .map_err(|e| format!("解析模型响应失败：{e}"))?;
            self.context.add_assistant_with_tools(&text, tool_calls.clone())?;
            final_text = text;

            if tool_calls.is_empty() {
                break;
            }

            // 逐个执行工具调用，结果回灌上下文后继续下一轮循环
            for call in &tool_calls {
                let result = self.tools.execute(call).await;
                let preview: String = result.chars().take(80).collect();
                println!("\n[tool] {} → {}...", call.name, preview);
                self.context.add_tool_result(&call.id, &result)?;
            }
        }
        Ok(final_text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_construction() {
        let config = Config::default_config();
        let agent = Agent::new(config);
        assert!(agent.is_ok());
    }

    #[test]
    fn test_agent_construction_bad_provider() {
        let mut config = Config::default_config();
        config.model.provider = "unknown".to_string();
        assert!(Agent::new(config).is_err());
    }
}
