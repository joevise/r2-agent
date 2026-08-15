//! L1 工作记忆：最简上下文管理 + L2 压缩摘要

use crate::model::ModelResult;
use crate::types::{Message, Role, ToolCall};

/// 压缩时保留的最近消息条数（约 6 轮对话）
pub const KEEP_RECENT: usize = 12;

/// 摘要消息的内容前缀（恢复会话时靠它识别摘要）
pub const SUMMARY_PREFIX: &str = "【会话历史摘要】";

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

/// L1 工作记忆管理器（含 L2 压缩摘要）
pub struct ContextManager {
    system_prompt: String,
    messages: Vec<Message>,
    token_count: usize,
    max_tokens: usize,
    /// L2 压缩摘要（None = 尚未压缩）
    l2_summary: Option<String>,
    /// L1 压缩触发阈值（占 max_tokens 比例，默认 0.7）
    l1_threshold: f64,
}

impl ContextManager {
    pub fn new(system_prompt: &str, max_tokens: usize, l1_threshold: f64) -> Self {
        Self {
            system_prompt: system_prompt.to_string(),
            messages: Vec::new(),
            token_count: 0,
            max_tokens,
            l2_summary: None,
            l1_threshold,
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
    ///
    /// L2 摘要重建：JSONL 是 append-only 的，压缩时摘要会以一条 role=system、
    /// 内容以 SUMMARY_PREFIX 开头的消息追加到文件末尾，而被压缩的旧消息仍留在文件里。
    /// 因此恢复时取【最后一条】摘要消息为当前摘要，并丢弃它及其之前的所有消息
    /// （它们已被压缩进摘要，无需再灌入上下文）。
    pub fn from_messages(
        system_prompt: &str,
        messages: Vec<Message>,
        max_tokens: usize,
        l1_threshold: f64,
    ) -> Self {
        let mut l2_summary = None;
        let mut messages = messages;
        // 找最后一条摘要消息的位置
        if let Some(pos) = messages.iter().rposition(|m| {
            m.role == Role::System && m.content.starts_with(SUMMARY_PREFIX)
        }) {
            let summary = messages[pos]
                .content
                .trim_start_matches(SUMMARY_PREFIX)
                .trim_start()
                .to_string();
            l2_summary = Some(summary);
            // 摘要及其之前的消息都已压缩，丢弃
            messages.drain(..=pos);
        }
        let token_count = messages.iter().map(message_tokens).sum();
        Self {
            system_prompt: system_prompt.to_string(),
            messages,
            token_count,
            max_tokens,
            l2_summary,
            l1_threshold,
        }
    }

    /// 是否需要触发 L2 压缩：token 超阈值且尚有可压缩的旧消息
    pub fn should_compress(&self) -> bool {
        self.token_count as f64 > self.max_tokens as f64 * self.l1_threshold
            && self.compressible_end().is_some()
    }

    /// 当前 L2 摘要（供 Agent 做合并摘要时读取）
    pub fn l2_summary(&self) -> Option<&str> {
        self.l2_summary.as_deref()
    }

    /// 计算可压缩消息的结束下标（[0, end) 进入压缩，[end, len) 保留）
    ///
    /// 切分点对齐规则：不能把 assistant(tool_calls) 和它的 tool 结果切开。
    /// 若切分点落在 tool 结果消息上（即该 tool 组的 assistant 在切分点之前），
    /// 把切分点向后推到这组 tool 结果之后——整组一起进压缩，
    /// 避免保留区出现没有对应 tool_calls 的孤儿 tool 消息（OpenAI 会直接报错）。
    fn compressible_end(&self) -> Option<usize> {
        let len = self.messages.len();
        // 保留条数自适应阶梯：优先 12 条；消息太少切不动时逐级减少
        // （小窗口场景：消息少而大），至少保留 2 条（最近一轮 user+assistant）
        for &keep in &[KEEP_RECENT, 8, 4, 2] {
            let mut end = len.saturating_sub(keep);
            // 切分点后推跳过 tool 结果（对齐 tool 组，避免孤儿 tool 消息）
            while end < len && self.messages[end].role == Role::Tool {
                end += 1;
            }
            if end > 0 && end < len {
                return Some(end);
            }
            // end==0：这个保留量下没有可压缩的旧消息，试下一档
        }
        None
    }

    /// 取出待压缩的旧消息（从 messages 里移除并返回）；take 之后应调 set_summary
    pub fn take_compressible(&mut self) -> Option<Vec<Message>> {
        let end = self.compressible_end()?;
        let taken: Vec<Message> = self.messages.drain(..end).collect();
        let taken_tokens: usize = taken.iter().map(message_tokens).sum();
        self.token_count = self.token_count.saturating_sub(taken_tokens);
        Some(taken)
    }

    /// 注入压缩摘要（二次压缩时直接替换旧摘要，不叠加——
    /// 合并旧摘要的工作由 Agent 在生成新摘要时完成）
    pub fn set_summary(&mut self, summary: String) {
        self.l2_summary = Some(summary);
    }

    /// 构建发给模型的消息序列：system_prompt + 摘要消息 + 剩余历史
    ///
    /// 摘要消息用 role=System：OpenAI 兼容多条 system 消息；
    /// Anthropic 侧 messages_to_anthropic 会把所有 system 消息合并进顶层 system 字段，
    /// 两家都不会报错，比 role=User 伪装更贴近语义。
    pub fn build(&self) -> Vec<Message> {
        let extra = if self.l2_summary.is_some() { 2 } else { 1 };
        let mut msgs = Vec::with_capacity(self.messages.len() + extra);
        msgs.push(Message {
            role: Role::System,
            content: self.system_prompt.clone(),
            tool_calls: None,
            tool_call_id: None,
        });
        if let Some(summary) = &self.l2_summary {
            msgs.push(Message {
                role: Role::System,
                content: format!("{SUMMARY_PREFIX}\n{summary}"),
                tool_calls: None,
                tool_call_id: None,
            });
        }
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

    const THRESHOLD: f64 = 0.7;

    #[test]
    fn test_build_system_first() {
        let mut ctx = ContextManager::new("你是 R2", 10_000, THRESHOLD);
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
        let mut ctx = ContextManager::new("sys", 10_000, THRESHOLD);
        assert_eq!(ctx.token_count(), 0);
        ctx.add_message(Role::User, "hello world").unwrap();
        let c1 = ctx.token_count();
        assert!(c1 > 0);
        ctx.add_message(Role::Assistant, "hi there").unwrap();
        assert!(ctx.token_count() > c1);
    }

    #[test]
    fn test_context_overflow() {
        let mut ctx = ContextManager::new("sys", 10, THRESHOLD);
        let result = ctx.add_message(Role::User, "这是一条非常非常长的消息，肯定会超过十个 token 的限制");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("上下文超限"));
    }

    #[test]
    fn test_tool_messages() {
        let mut ctx = ContextManager::new("sys", 10_000, THRESHOLD);
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

    /// 灌入 n 条短消息（user/assistant 交替）
    fn fill(ctx: &mut ContextManager, n: usize) {
        for i in 0..n {
            let role = if i % 2 == 0 { Role::User } else { Role::Assistant };
            ctx.add_message(role, &format!("消息 {i}")).unwrap();
        }
    }

    #[test]
    fn test_should_compress_below_threshold() {
        let mut ctx = ContextManager::new("sys", 10_000, THRESHOLD);
        fill(&mut ctx, 20);
        assert!(!ctx.should_compress());
    }

    #[test]
    fn test_should_compress_above_threshold() {
        let mut ctx = ContextManager::new("sys", 100, THRESHOLD);
        fill(&mut ctx, 20); // 20 条 × 约 4 token ≈ 80 > 100*0.7
        assert!(ctx.should_compress());
    }

    #[test]
    fn test_should_compress_false_when_nothing_compressible() {
        // 超阈值但消息数 <= KEEP_RECENT，无可压缩旧消息
        let mut ctx = ContextManager::new("sys", 10, THRESHOLD);
        for i in 0..KEEP_RECENT {
            let role = if i % 2 == 0 { Role::User } else { Role::Assistant };
            ctx.add_message(role, &format!("{i}")).unwrap();
        }
        assert!(!ctx.should_compress());
    }

    #[test]
    fn test_take_compressible_keeps_recent() {
        let mut ctx = ContextManager::new("sys", 10_000, THRESHOLD);
        fill(&mut ctx, 20);
        let taken = ctx.take_compressible().unwrap();
        assert_eq!(taken.len(), 20 - KEEP_RECENT);
        let msgs = ctx.build();
        // system + 12 条保留
        assert_eq!(msgs.len(), 1 + KEEP_RECENT);
        assert_eq!(msgs[1].content, format!("消息 {}", 20 - KEEP_RECENT));
    }

    #[test]
    fn test_take_compressible_aligns_tool_boundary() {
        // 构造：22 条消息，切分点（22 - 12 = 10）恰好落在 tool 结果上
        let mut ctx = ContextManager::new("sys", 10_000, THRESHOLD);
        for i in 0..9 {
            let role = if i % 2 == 0 { Role::User } else { Role::Assistant };
            ctx.add_message(role, &format!("消息 {i}")).unwrap();
        }
        let tc = ToolCall {
            id: "call_w".to_string(),
            name: "bash".to_string(),
            arguments: "{}".to_string(),
        };
        ctx.add_assistant_with_tools("执行", vec![tc]).unwrap(); // index 9
        ctx.add_tool_result("call_w", "ok").unwrap(); // index 10 = 切分点
        fill(&mut ctx, 11); // index 11..22

        let taken = ctx.take_compressible().unwrap();
        // 切分点后推到 tool 结果之后：tool 组（assistant@9 + tool@10）整组进压缩
        assert_eq!(taken.len(), 11);
        assert_eq!(taken[9].role, Role::Assistant);
        assert!(taken[9].tool_calls.is_some());
        assert_eq!(taken[10].role, Role::Tool);
        // 保留区 11 条，第一条不是孤儿 tool 消息
        let msgs = ctx.build();
        assert_eq!(msgs.len(), 1 + 11);
        assert_ne!(msgs[1].role, Role::Tool);
    }

    #[test]
    fn test_set_summary_and_build_position() {
        let mut ctx = ContextManager::new("sys", 10_000, THRESHOLD);
        fill(&mut ctx, 4);
        ctx.set_summary("之前聊了 Rust".to_string());
        let msgs = ctx.build();
        // system_prompt + 摘要 + 4 条
        assert_eq!(msgs.len(), 6);
        assert_eq!(msgs[1].role, Role::System);
        assert!(msgs[1].content.starts_with(SUMMARY_PREFIX));
        assert!(msgs[1].content.contains("之前聊了 Rust"));
        assert_eq!(msgs[2].role, Role::User);
    }

    #[test]
    fn test_second_compression_replaces_summary() {
        let mut ctx = ContextManager::new("sys", 10_000, THRESHOLD);
        fill(&mut ctx, 30);
        let _ = ctx.take_compressible().unwrap();
        ctx.set_summary("摘要 v1".to_string());
        fill(&mut ctx, 20); // 剩 12 + 新 20 = 32 条，再次可压缩
        let _ = ctx.take_compressible().unwrap();
        ctx.set_summary("摘要 v2（合并 v1）".to_string());
        assert_eq!(ctx.l2_summary(), Some("摘要 v2（合并 v1）"));
        let msgs = ctx.build();
        // 只有一条摘要消息
        let summary_count = msgs
            .iter()
            .filter(|m| m.content.starts_with(SUMMARY_PREFIX))
            .count();
        assert_eq!(summary_count, 1);
        assert!(msgs[1].content.contains("摘要 v2"));
    }

    #[test]
    fn test_from_messages_rebuilds_summary() {
        // 模拟恢复：JSONL 里有被压缩的旧消息 + 末尾的摘要消息 + 之后的新消息
        let mut history: Vec<Message> = (0..10)
            .map(|i| Message {
                role: if i % 2 == 0 { Role::User } else { Role::Assistant },
                content: format!("旧消息 {i}"),
                tool_calls: None,
                tool_call_id: None,
            })
            .collect();
        history.push(Message {
            role: Role::System,
            content: format!("{SUMMARY_PREFIX}\n旧内容摘要"),
            tool_calls: None,
            tool_call_id: None,
        });
        history.push(Message {
            role: Role::User,
            content: "压缩后的新消息".to_string(),
            tool_calls: None,
            tool_call_id: None,
        });

        let ctx = ContextManager::from_messages("sys", history, 10_000, THRESHOLD);
        assert_eq!(ctx.l2_summary(), Some("旧内容摘要"));
        let msgs = ctx.build();
        // system_prompt + 摘要 + 1 条新消息（旧 10 条已被摘要覆盖，不重复灌入）
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[2].content, "压缩后的新消息");
    }

    #[test]
    fn test_compression_state_machine_end_to_end() {
        // 状态机全流程：灌入超量消息 → should_compress → take → set → build 结构正确
        let mut ctx = ContextManager::new("sys", 200, THRESHOLD);
        fill(&mut ctx, 40); // 40 × 约4token ≈ 160 > 200*0.7
        assert!(ctx.should_compress());

        let taken = ctx.take_compressible().unwrap();
        assert_eq!(taken.len(), 40 - KEEP_RECENT);
        ctx.set_summary("前 28 条摘要".to_string());

        let msgs = ctx.build();
        assert_eq!(msgs.len(), 1 + 1 + KEEP_RECENT);
        assert_eq!(msgs[0].content, "sys");
        assert!(msgs[1].content.starts_with(SUMMARY_PREFIX));
        assert_eq!(msgs[2].content, format!("消息 {}", 40 - KEEP_RECENT));

        // token_count 已扣除被压缩消息，低于阈值后不再触发
        assert!(!ctx.should_compress());
    }

    /// l1_threshold = 0：只要有 token 且有可压缩消息，永远触发压缩
    #[test]
    fn test_threshold_zero_always_compresses() {
        let mut ctx = ContextManager::new("sys", 10_000, 0.0);
        fill(&mut ctx, 20);
        assert!(ctx.should_compress());
    }

    /// l1_threshold = 1.5（>1）：token 数被 check_limit 限制在 max 内，永不触发
    #[test]
    fn test_threshold_above_one_never_compresses() {
        let mut ctx = ContextManager::new("sys", 100, 1.5);
        fill(&mut ctx, 20); // 约 80 token < 100*1.5
        assert!(!ctx.should_compress());
    }

    /// l1_threshold 为负数：阈值恒小于 0，行为等同 0（永远触发），不 panic
    #[test]
    fn test_threshold_negative_behaves_like_zero() {
        let mut ctx = ContextManager::new("sys", 10_000, -0.5);
        fill(&mut ctx, 20);
        assert!(ctx.should_compress());
        // 无可压缩消息时仍不触发（1 条消息，保留阶梯最小为 2，切不出旧消息）
        let mut ctx2 = ContextManager::new("sys", 10_000, -0.5);
        fill(&mut ctx2, 1);
        assert!(!ctx2.should_compress());
    }

    /// max_total_tokens = 0：任何非空消息都报“上下文超限”（符合逻辑）
    #[test]
    fn test_zero_max_tokens_rejects_everything() {
        let mut ctx = ContextManager::new("sys", 0, THRESHOLD);
        let result = ctx.add_message(Role::User, "hi");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("上下文超限"));
        let result = ctx.add_tool_result("c1", "ok");
        assert!(result.is_err());
        // 空内容估算为 0 token，0 > 0 不成立，仍可加入（不 panic 即可）
        assert!(ctx.add_message(Role::User, "").is_ok());
    }
}
