//! Agent 循环引擎：用户输入 → 模型流式响应 → 工具执行 → 输出

use crate::config::Config;
use crate::context::ContextManager;
use crate::events::AgentEvent;
#[cfg(feature = "l3-memory")]
use crate::memory::MemoryStore;
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
    /// L3 跨会话记忆（l3_enabled=false 或打开失败时为 None）
    #[cfg(feature = "l3-memory")]
    memory: Option<MemoryStore>,
    /// 事件广播（库形态嵌入时由 AgentSession 注入；CLI 下为 None，行为不变）
    emitter: Option<tokio::sync::broadcast::Sender<AgentEvent>>,
    /// 中途转向通道（AgentSession / CLI 注入；未注入时行为与原来完全一致）
    steer_rx: Option<tokio::sync::mpsc::Receiver<String>>,
    /// 静音模式：为 true 时不向 stdout 打印（事件照常广播）
    quiet: bool,
}

impl Agent {
    pub fn new(config: Config) -> ModelResult<Self> {
        #[cfg(not(feature = "l3-memory"))]
        Self::warn_l3_not_compiled(&config);
        let provider = create_provider(&config)?;
        let max_tokens = config.agent.max_total_tokens;
        let context = ContextManager::new(SYSTEM_PROMPT, max_tokens, config.context.l1_threshold);
        let tools = ToolRegistry::new_default(&config.agent.work_dir, &config.sandbox)
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
        let session = Session::create(&crate::config::expand_tilde(&config.session.dir)).ok();
        #[cfg(feature = "l3-memory")]
        let memory = Self::init_memory(&config);
        Ok(Self {
            provider,
            context,
            tools,
            config,
            session,
            #[cfg(feature = "l3-memory")]
            memory,
            emitter: None,
            steer_rx: None,
            quiet: false,
        })
    }

    /// 恢复指定会话：读 JSONL 重建上下文，继续追加写
    pub fn resume(config: Config, session_id: &str) -> ModelResult<Self> {
        #[cfg(not(feature = "l3-memory"))]
        Self::warn_l3_not_compiled(&config);
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
        let tools = ToolRegistry::new_default(&config.agent.work_dir, &config.sandbox)
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
        println!("已恢复会话 {session_id}（{count} 条历史消息）");
        #[cfg(feature = "l3-memory")]
        let memory = Self::init_memory(&config);
        Ok(Self {
            provider,
            context,
            tools,
            config,
            session: Some(session),
            #[cfg(feature = "l3-memory")]
            memory,
            emitter: None,
            steer_rx: None,
            quiet: false,
        })
    }

    /// 从某会话分叉并继续（上下文 = 父会话 upto 点 + 后续新对话）
    ///
    /// 流程：Session::branch 新建分支会话文件 → 继承消息灌入 L1 → 组装 Agent。
    /// 之后对话追加写到新会话文件，不碰父文件。
    pub fn branch_from(config: Config, parent_session_id: &str, upto: Option<usize>) -> ModelResult<Self> {
        #[cfg(not(feature = "l3-memory"))]
        Self::warn_l3_not_compiled(&config);
        let provider = create_provider(&config)?;
        let max_tokens = config.agent.max_total_tokens;
        let session_dir = crate::config::expand_tilde(&config.session.dir);
        let (session, messages) = Session::branch(&session_dir, parent_session_id, upto)
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
        let count = messages.len();
        let new_id = session.id().to_string();
        let context = ContextManager::from_messages(
            SYSTEM_PROMPT,
            messages,
            max_tokens,
            config.context.l1_threshold,
        );
        let tools = ToolRegistry::new_default(&config.agent.work_dir, &config.sandbox)
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
        println!("已从会话 {parent_session_id} 分叉（继承 {count} 条消息，新会话 {new_id}）");
        #[cfg(feature = "l3-memory")]
        let memory = Self::init_memory(&config);
        Ok(Self {
            provider,
            context,
            tools,
            config,
            session: Some(session),
            #[cfg(feature = "l3-memory")]
            memory,
            emitter: None,
            steer_rx: None,
            quiet: false,
        })
    }

    /// 注入事件广播通道（嵌入方使用；CLI 不调用，输出行为不变）
    pub fn set_emitter(&mut self, emitter: tokio::sync::broadcast::Sender<AgentEvent>) {
        self.emitter = Some(emitter);
    }

    /// 注入 steer 通道（AgentSession / CLI 用）：运行中可接收用户中途转向指令
    pub fn set_steer_channel(&mut self, rx: tokio::sync::mpsc::Receiver<String>) {
        self.steer_rx = Some(rx);
    }

    /// 测试注入 Mock Provider（不走 create_provider 工厂）
    #[cfg(test)]
    pub(crate) fn set_provider(&mut self, p: Box<dyn ModelProvider>) {
        self.provider = p;
    }

    /// 静音开关：true 时不向 stdout 打印（事件照常广播）
    pub fn set_quiet(&mut self, quiet: bool) {
        self.quiet = quiet;
    }

    /// 广播一条事件（无订阅者时忽略错误）
    fn emit(&self, event: AgentEvent) {
        if let Some(tx) = &self.emitter {
            let _ = tx.send(event);
        }
    }

    /// 输出一行提示：quiet 时只发事件不打印；否则打印 + 发事件
    fn notice(&self, text: String) {
        if !self.quiet {
            println!("{text}");
        }
        self.emit(AgentEvent::MessageUpdate(format!("{text}\n")));
    }

    /// 初始化 L3 跨会话记忆：l3_enabled=false 或打开失败时为 None
    #[cfg(feature = "l3-memory")]
    fn init_memory(config: &Config) -> Option<MemoryStore> {
        if !config.context.l3_enabled {
            return None;
        }
        let path = format!("{}/memory.db", crate::config::expand_tilde(&config.session.dir));
        match MemoryStore::open(&path) {
            Ok(m) => Some(m),
            Err(e) => {
                tracing::warn!("L3 记忆库初始化失败（跳过）：{e}");
                None
            }
        }
    }

    /// 没开 feature 但配置开了 l3_enabled：启动时提示一行
    #[cfg(not(feature = "l3-memory"))]
    fn warn_l3_not_compiled(config: &Config) {
        if config.context.l3_enabled {
            eprintln!("[memory] l3_enabled=true 但 l3-memory 未编译（需 cargo build --features l3-memory），已跳过");
        }
    }

    /// 当前会话 ID（用于提示用户如何恢复）
    pub fn session_id(&self) -> Option<&str> {
        self.session.as_ref().map(|s| s.id())
    }

    /// 当前 L1 中的历史消息条数（不含 system prompt / L2 摘要）
    pub fn history_len(&self) -> usize {
        self.context.history_len()
    }

    /// 清空当前上下文（/clear）：新建会话文件 + 重置 L1。
    /// L3 跨会话记忆（若启用）刻意保留不动——它是跨会话的。
    pub fn reset_context(&mut self) {
        self.session =
            Session::create(&crate::config::expand_tilde(&self.config.session.dir)).ok();
        self.context = ContextManager::new(
            SYSTEM_PROMPT,
            self.config.agent.max_total_tokens,
            self.config.context.l1_threshold,
        );
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
                self.notice(format!(
                    "\n[context] L1 超阈值，已压缩 {} 条历史消息",
                    old_msgs.len()
                ));
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
        self.emit(AgentEvent::AgentStart);
        // 排空上一轮残留的 steer 消息——非运行时注入的指令不应影响本轮
        if let Some(rx) = self.steer_rx.as_mut() {
            while rx.try_recv().is_ok() {}
        }
        // 关键：在 add_user 之前先压缩——否则上下文快满时用户消息会先撞限报错，
        // 压缩永远没机会触发
        self.compress_if_needed().await;

        self.context.add_message(Role::User, user_input)?;
        self.log_session(&SessionEntry::message(Role::User, user_input));

        // L3：检索跨会话记忆（排除当前会话——它已在上下文里）
        #[cfg(feature = "l3-memory")]
        let memory_msg = self.recall_memory(user_input);

        let mut final_text = String::new();
        for turn in 0..self.config.agent.max_turns {
            // turn 循环内也保留检查（长回复多轮工具调用时 token 也会涨）
            self.compress_if_needed().await;

            #[allow(unused_mut)]
            let mut messages = self.context.build();
            // 瞬态注入：记忆消息插在 system_prompt 之后（index 1），
            // 不进 context.messages（不污染历史、不落盘 JSONL），只在 turn 0 注入一次
            #[cfg(feature = "l3-memory")]
            if turn == 0 {
                if let Some(msg) = &memory_msg {
                    messages.insert(1, msg.clone());
                }
            }
            let mut stream = self
                .provider
                .chat_stream(&messages, &self.tools.schemas())
                .await
                .map_err(|e| format!("模型请求失败（第 {} 轮）：{}", turn + 1, e))?;

            let mut chunks: Vec<StreamChunk> = Vec::new();
            // 把 steer 通道临时拿出 self：select 的流分支要借用 self（emit/quiet），
            // 不能同时持有 self.steer_rx 的可变借用。循环结束后放回。
            let mut steer_rx = self.steer_rx.take();
            let mut steered_msg: Option<String> = None;
            loop {
                tokio::select! {
                    item = stream.next() => match item {
                        Some(Ok(chunk)) => {
                            if let StreamChunk::Delta(ref s) = chunk {
                                if !self.quiet {
                                    print!("{s}");
                                    let _ = std::io::stdout().flush();
                                }
                                self.emit(AgentEvent::MessageUpdate(s.clone()));
                            }
                            chunks.push(chunk);
                        }
                        Some(Err(e)) => {
                            if !self.quiet {
                                println!();
                            }
                            self.steer_rx = steer_rx;
                            return Err(format!("流式响应中断：{e}").into());
                        }
                        None => break,   // 流正常结束
                    },
                    // 没装 steer 通道时此分支永远 pending，行为与原来完全一致
                    msg = async {
                        match steer_rx.as_mut() {
                            Some(rx) => rx.recv().await,
                            None => std::future::pending().await,
                        }
                    } => match msg {
                        Some(m) => {
                            steered_msg = Some(m);
                            break; // 抛弃当前流（stream drop 即断开连接）
                        }
                        None => steer_rx = None, // 发送端已关闭：当作无 steer 通道
                    },
                }
            }
            self.steer_rx = steer_rx;
            if !self.quiet {
                println!();
            }

            // steer 处理：流被放弃后，带着部分输出 + 新指令继续下一轮
            if let Some(steer) = steered_msg {
                self.handle_steer(&chunks, &steer)?;
                continue;
            }

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
                self.emit(AgentEvent::ToolCall {
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                });
                let result = self.tools.execute(call).await;
                let preview: String = result.chars().take(80).collect();
                if !self.quiet {
                    println!("\n[tool] {} → {}...", call.name, preview);
                }
                self.emit(AgentEvent::ToolResult {
                    name: call.name.clone(),
                    output: result.clone(),
                });
                self.context.add_tool_result(&call.id, &result)?;
                self.log_session(&SessionEntry::tool_result(&call.id, &result));
            }
            // 每轮结束落一个检查点
            self.log_session(&SessionEntry::checkpoint(turn + 1));

            // 工具间隙检查 steer（非阻塞）。刻意放在整轮工具执行完之后检查：
            // 中途丢弃剩余 tool_calls 会让 assistant 消息挂着没结果的工具调用，
            // 破坏上下文完整性。语义：本轮全部工具结果已入上下文，
            // steer 作为新 user 消息追加后直接继续外层 turn 循环。
            let mut gap_msgs: Vec<String> = Vec::new();
            if let Some(rx) = self.steer_rx.as_mut() {
                while let Ok(msg) = rx.try_recv() {
                    gap_msgs.push(msg);
                }
            }
            if !gap_msgs.is_empty() {
                self.handle_steer(&[], &gap_msgs.join("\n"))?;
                continue;
            }
        }

        // L3：存一轮 Q&A（session 可能为 None——持久化失败场景，用 "unknown" 代替）
        #[cfg(feature = "l3-memory")]
        if let Some(memory) = &self.memory {
            let session_id = self.session_id().unwrap_or("unknown");
            if let Err(e) = memory.store(session_id, user_input, &final_text) {
                tracing::warn!("L3 记忆写入失败：{e}");
            }
        }
        self.emit(AgentEvent::Done {
            final_text: final_text.clone(),
        });
        Ok(final_text)
    }

    /// steer 统一处理：保留已收到的文本部分（半截工具调用 JSON 不可用，全部丢弃），
    /// 追加中断标注和 [用户中途指令] 消息，广播 Steered 事件
    fn handle_steer(&mut self, chunks: &[StreamChunk], steer: &str) -> ModelResult<()> {
        // 只取文本部分拼 partial_text
        let partial_text: String = chunks
            .iter()
            .filter_map(|c| match c {
                StreamChunk::Delta(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        if !partial_text.trim().is_empty() {
            // 追加标注——让模型知道上次没说完
            let annotated = format!("{}\n(此回复被用户中途打断)", partial_text);
            self.context.add_assistant_with_tools(&annotated, vec![])?;
            self.log_session(&SessionEntry::assistant(&annotated, vec![]));
        }
        let steer_msg = format!("[用户中途指令] {steer}");
        self.context.add_message(Role::User, &steer_msg)?;
        self.log_session(&SessionEntry::message(Role::User, &steer_msg));
        self.emit(AgentEvent::Steered(steer.to_string()));
        if !self.quiet {
            println!("\n[steer] 收到中途指令，转向中…");
        }
        Ok(())
    }

    /// L3：检索跨会话记忆，非空则构造一条注入消息（不进 context.messages）
    #[cfg(feature = "l3-memory")]
    fn recall_memory(&self, user_input: &str) -> Option<crate::types::Message> {
        let memory = self.memory.as_ref()?;
        let current = self.session_id().unwrap_or("unknown");
        let hits = memory.search(user_input, 3, 0.30, current).ok()?;
        if hits.is_empty() {
            return None;
        }
        self.notice(format!("\n[memory] 唤起 {} 条跨会话记忆", hits.len()));
        let mut content = String::from("【跨会话记忆】以下是你（R2）在之前会话中的相关经历：");
        for hit in &hits {
            let answer: String = hit.answer.chars().take(400).collect();
            content.push_str(&format!("\n- 用户曾问：{}\n  你答：{}", hit.query, answer));
        }
        Some(crate::types::Message {
            role: Role::System,
            content,
            tool_calls: None,
            tool_call_id: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ChunkStream;
    use crate::types::{Message, ToolCall, ToolSchema};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Mock Provider：第 0 次调用先吐"第一段"再挂起等 gate（steer 测试在此打断）；
    /// 后续调用直接吐"最终回复"结束。parse_response 只聚合文本（不产生工具调用）。
    struct MockProvider {
        gate: tokio::sync::watch::Receiver<bool>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ModelProvider for MockProvider {
        fn id(&self) -> &str {
            "mock"
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolSchema],
        ) -> ModelResult<ChunkStream> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let gate = self.gate.clone();
            let stream = futures_util::stream::unfold((n, 0usize), move |(n, step)| {
                let mut gate = gate.clone();
                async move {
                    match (n, step) {
                        (0, 0) => Some((Ok(StreamChunk::Delta("第一段".to_string())), (n, 1))),
                        (0, 1) => {
                            // 挂起等 gate 打开；steer 打断时这里永远不会恢复
                            while !*gate.borrow() {
                                if gate.changed().await.is_err() {
                                    break; // 发送端关闭：放行，避免挂死
                                }
                            }
                            Some((Ok(StreamChunk::Delta("第二段".to_string())), (n, 2)))
                        }
                        (0, 2) => Some((Ok(StreamChunk::Done), (n, 3))),
                        (_, 0) => Some((Ok(StreamChunk::Delta("最终回复".to_string())), (n, 1))),
                        (_, 1) => Some((Ok(StreamChunk::Done), (n, 2))),
                        _ => None,
                    }
                }
            });
            Ok(Box::pin(stream))
        }

        fn parse_response(&self, chunks: &[StreamChunk]) -> ModelResult<(String, Vec<ToolCall>)> {
            let text: String = chunks
                .iter()
                .filter_map(|c| match c {
                    StreamChunk::Delta(s) => Some(s.as_str()),
                    _ => None,
                })
                .collect();
            Ok((text, vec![]))
        }
    }

    fn test_agent(tmp: &tempfile::TempDir, gate: tokio::sync::watch::Receiver<bool>) -> Agent {
        let mut config = Config::default_config();
        config.session.dir = tmp.path().to_string_lossy().to_string();
        let mut agent = Agent::new(config).unwrap();
        agent.set_provider(Box::new(MockProvider {
            gate,
            calls: Arc::new(AtomicUsize::new(0)),
        }));
        agent.set_quiet(true);
        agent
    }

    #[tokio::test]
    async fn test_steer_during_stream() {
        let tmp = tempfile::tempdir().unwrap();
        let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
        let mut agent = test_agent(&tmp, gate_rx);
        let (steer_tx, steer_rx) = tokio::sync::mpsc::channel(32);
        agent.set_steer_channel(steer_rx);

        // 用块包裹：run future 持有 agent 的可变借用，出块即释放
        let reply = {
            let run = agent.run("任务");
            tokio::pin!(run);
            let driver = async {
                // 等 run 进入流式等待 gate，再注入 steer
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                steer_tx.send("改口令".to_string()).await.unwrap();
                // 兜底：即使 steer 没生效也打开 gate 让流程走完（断言失败而不是挂死）
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let _ = gate_tx.send(true);
            };
            let (result, _) = tokio::join!(run, driver);
            result.expect("run 应成功完成")
        };
        assert_eq!(reply, "最终回复");

        let messages = agent.context.build();
        assert!(
            messages
                .iter()
                .any(|m| m.role == Role::User && m.content == "[用户中途指令] 改口令"),
            "上下文应包含 [用户中途指令]，实际：{messages:?}"
        );
        assert!(
            messages.iter().any(|m| m.role == Role::Assistant
                && m.content.contains("第一段")
                && m.content.contains("(此回复被用户中途打断)")),
            "上下文应保留半截文本并带中断标注，实际：{messages:?}"
        );
    }

    #[tokio::test]
    async fn test_stale_steer_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        // gate 常开：流不挂起，run 正常走完
        let (_gate_tx, gate_rx) = tokio::sync::watch::channel(true);
        let mut agent = test_agent(&tmp, gate_rx);
        let (steer_tx, steer_rx) = tokio::sync::mpsc::channel(32);
        agent.set_steer_channel(steer_rx);
        // 无 run 时注入一条——应被 run 开头排空丢弃
        steer_tx.send("陈旧指令".to_string()).await.unwrap();

        let reply = agent.run("正常任务").await.expect("run 应成功");
        assert_eq!(reply, "第一段第二段");
        let messages = agent.context.build();
        assert!(
            !messages.iter().any(|m| m.content.contains("陈旧指令")),
            "陈旧 steer 不应进入上下文，实际：{messages:?}"
        );
    }

    // 注：工具间隙 steer 的确定性测试需要 mock 出完整工具调用 JSON + ToolRegistry 配合，
    // 构造复杂度高、收益低，v0.2 跳过。

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

    #[test]
    fn test_reset_context() {
        // 会话目录指向临时目录，避免污染真实数据
        let tmp = tempfile::tempdir().unwrap();
        let mut config = Config::default_config();
        config.session.dir = tmp.path().to_string_lossy().to_string();
        let mut agent = Agent::new(config).unwrap();
        let old_id = agent.session_id().map(|s| s.to_string());
        agent
            .context
            .add_message(Role::User, "你好")
            .expect("加消息应成功");
        agent.reset_context();
        // 上下文已清空（build 只剩 system prompt 一条）
        assert_eq!(agent.context.build().len(), 1);
        // 会话换成了新 id，且新文件已创建
        let new_id = agent.session_id().expect("reset 后应有新会话");
        assert!(old_id.as_deref() != Some(new_id));
        assert!(tmp.path().join(format!("{new_id}.jsonl")).exists());
    }
}
