//! Agent 循环引擎：用户输入 → 模型流式响应 → 工具执行 → 输出

use crate::config::Config;
use crate::context::ContextManager;
use crate::model::{create_provider, ModelProvider, ModelResult};
use crate::session::{Session, SessionEntry};
use crate::tools::ToolRegistry;
use crate::types::{Role, StreamChunk};
use futures_util::StreamExt;
use std::io::Write;

const SYSTEM_PROMPT: &str = "你是 R2，一个极简但可靠的 Rust Agent。";

/// R2 Agent：Provider + L1 上下文 + 工具注册表 + 配置 + 会话持久化
pub struct Agent {
    provider: Box<dyn ModelProvider>,
    context: ContextManager,
    tools: ToolRegistry,
    config: Config,
    /// 会话持久化（Option：会话目录不可写时不影响主流程，也保持既有测试不炸）
    session: Option<Session>,
}

impl Agent {
    pub fn new(config: Config) -> ModelResult<Self> {
        let provider = create_provider(&config)?;
        let max_tokens = config.agent.max_total_tokens;
        let context = ContextManager::new(SYSTEM_PROMPT, max_tokens, config.context.l1_threshold);
        let tools = ToolRegistry::new_default(
            &config.agent.work_dir,
            config.sandbox.bash_timeout_secs,
        );
        let session = Session::create(&crate::config::expand_tilde(&config.session.dir)).ok();
        Ok(Self {
            provider,
            context,
            tools,
            config,
            session,
        })
    }

    /// 恢复指定会话：读 JSONL 重建上下文，继续追加写
    pub fn resume(config: Config, session_id: &str) -> ModelResult<Self> {
        let provider = create_provider(&config)?;
        let max_tokens = config.agent.max_total_tokens;
        let session_dir = crate::config::expand_tilde(&config.session.dir);
        let (session, messages) = Session::recover(&session_dir, session_id)
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
        let count = messages.len();
        let context = ContextManager::from_messages(
            SYSTEM_PROMPT,
            messages,
            max_tokens,
            config.context.l1_threshold,
        );
        let tools = ToolRegistry::new_default(
            &config.agent.work_dir,
            config.sandbox.bash_timeout_secs,
        );
        println!("已恢复会话 {session_id}（{count} 条历史消息）");
        Ok(Self {
            provider,
            context,
            tools,
            config,
            session: Some(session),
        })
    }

    /// 当前会话 ID（用于提示用户如何恢复）
    pub fn session_id(&self) -> Option<&str> {
        self.session.as_ref().map(|s| s.id())
    }

    /// 追加会话记录；失败只告警不中断主流程
    fn log_session(&mut self, entry: &SessionEntry) {
        if let Some(session) = &mut self.session {
            if let Err(e) = session.append(entry) {
                tracing::warn!("会话持久化失败：{e}");
            }
        }
    }

    /// L2 压缩：把旧消息发给模型生成摘要
    ///
    /// v0.1 复用主模型做摘要（config.context.l2_summary_model 暂忽略，
    /// 独立的小模型做摘要是后续优化点）。
    /// 已有摘要时，让模型把旧摘要和新消息合并成一份新摘要。
    async fn summarize(&self, old_msgs: &[crate::types::Message]) -> ModelResult<String> {
        // 把待压缩消息转成可读对话文本
        let mut dialogue = String::new();
        for m in old_msgs {
            let role = match m.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            };
            dialogue.push_str(&format!("[{role}] {}\n", m.content));
            if let Some(calls) = &m.tool_calls {
                for c in calls {
                    dialogue.push_str(&format!("[tool_call] {}({})\n", c.name, c.arguments));
                }
            }
        }

        let prompt = match self.context.l2_summary() {
            Some(old_summary) => format!(
                "以下是已有的会话历史摘要和新的对话内容。请把它们合并成一份简洁摘要，保留：关键决策、结论、重要文件路径、未完成任务、用户偏好。直接输出摘要内容，不要任何前缀。\n\n【已有摘要】\n{old_summary}\n\n【新对话内容】\n{dialogue}"
            ),
            None => format!(
                "把以下对话历史压缩成简洁摘要，保留：关键决策、结论、重要文件路径、未完成任务、用户偏好。直接输出摘要内容，不要任何前缀。\n\n{dialogue}"
            ),
        };

        let req = vec![crate::types::Message {
            role: Role::User,
            content: prompt,
            tool_calls: None,
            tool_call_id: None,
        }];
        // 摘要请求不带 tools
        let mut stream = self.provider.chat_stream(&req, &[]).await?;
        let mut chunks = Vec::new();
        while let Some(item) = stream.next().await {
            chunks.push(item?);
        }
        let (text, _) = self.provider.parse_response(&chunks)?;
        if text.trim().is_empty() {
            return Err("摘要模型返回空内容".into());
        }
        Ok(text.trim().to_string())
    }

    /// L2 压缩：超阈值时把旧消息交给模型摘要，腾出 L1 空间。
    /// 失败只告警不中断（调用方决定是否继续）。
    async fn compress_if_needed(&mut self) {
        if !self.context.should_compress() {
            return;
        }
        let Some(old_msgs) = self.context.take_compressible() else {
            return;
        };
        match self.summarize(&old_msgs).await {
            Ok(summary) => {
                println!("\n[context] L1 超阈值，已压缩 {} 条历史消息", old_msgs.len());
                // 摘要落盘：append-only，恢复时由 from_messages 重建
                self.log_session(&SessionEntry::message(
                    Role::System,
                    &format!("{}\n{}", crate::context::SUMMARY_PREFIX, summary),
                ));
                self.context.set_summary(summary);
            }
            Err(e) => {
                tracing::warn!("L2 压缩失败（跳过本轮）：{e}");
            }
        }
    }

    /// 处理一次用户输入，流式打印 assistant 输出，返回完整回复文本
    pub async fn run(&mut self, user_input: &str) -> ModelResult<String> {
        // 关键：在 add_user 之前先压缩——否则上下文快满时用户消息会先撞限报错，
        // 压缩永远没机会触发
        self.compress_if_needed().await;

        self.context.add_message(Role::User, user_input)?;
        self.log_session(&SessionEntry::message(Role::User, user_input));

        let mut final_text = String::new();
        for turn in 0..self.config.agent.max_turns {
            // turn 循环内也保留检查（长回复多轮工具调用时 token 也会涨）
            self.compress_if_needed().await;

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
            self.log_session(&SessionEntry::assistant(&text, tool_calls.clone()));
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
                self.log_session(&SessionEntry::tool_result(&call.id, &result));
            }
            // 每轮结束落一个检查点
            self.log_session(&SessionEntry::checkpoint(turn + 1));
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
