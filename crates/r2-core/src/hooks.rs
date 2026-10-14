//! 行为钩子体系（v0.13.0 模块化阶段一）：核心循环只在点位上调 hooks，
//! 注册表不存状态、hook 不持久化（每次 Agent 构造重建）。
//! 第一个 Hook 实现是 EvolutionHook（见 evolution.rs）——反思/技能草稿/技能追踪
//! 从 agent.rs 内嵌逻辑降级为可插拔 hook。

use crate::events::AgentEvent;
use crate::model::ModelProvider;
use crate::types::ToolCall;

/// 钩子上下文（点位调用时传给 hook）
pub struct HookCtx<'a> {
    pub provider: &'a mut dyn ModelProvider,
    /// 本回合硬信号快照（工具成败计数等；由 agent 循环维护）
    pub signals: &'a crate::evolution::TurnSignals,
    /// 回合任务摘要（TurnEnd 点位才有值；其他点位为空串）
    pub task_summary: &'a str,
    /// 工作目录（技能草稿写盘等需要）
    pub work_dir: &'a str,
    /// 触发来源会话 ID（进化事件落盘的归属字段）
    pub session_id: &'a str,
    /// 事件广播（hook 产出成长事件时用；CLI 下为 None）
    pub emitter: Option<&'a tokio::sync::broadcast::Sender<AgentEvent>>,
    /// 本轮开始时的目标快照（TurnEnd 的 goal_set 记账基线）
    pub goal_baseline: Option<&'a str>,
    /// 本轮开始时的技能名快照（TurnEnd 的沉淀记账基线）
    pub skills_baseline: &'a [String],
}

/// 钩子 trait：全部默认空实现，hook 按需覆盖
#[async_trait::async_trait]
pub trait Hook: Send + Sync {
    fn name(&self) -> &str;
    /// 工具调用前（工具参数解析成功后）；返回 Some(拦截文案) 则不执行该工具，
    /// 文案直接作为工具结果（审批/防护类 hook 用；evolution 不用）
    async fn on_tool_start(&self, _call: &ToolCall, _ctx: &HookCtx<'_>) -> Option<String> {
        None
    }
    /// 工具结束后（含成败）
    async fn on_tool_end(&self, _call: &ToolCall, _ok: bool, _ctx: &HookCtx<'_>) {}
    /// 回合收尾（run 成功完成后；失败路径不调）。
    /// 需要 &mut ctx：hook 内部可经 ctx.provider 调模型（反思/草稿合成）
    async fn on_turn_end(&self, _ctx: &mut HookCtx<'_>) {}
}

/// 钩子注册表：Vec + 顺序调用（不存状态，随 Agent 构造重建）
#[derive(Default)]
pub struct HookRegistry {
    hooks: Vec<std::sync::Arc<dyn Hook>>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self { hooks: Vec::new() }
    }

    pub fn register(&mut self, h: std::sync::Arc<dyn Hook>) {
        self.hooks.push(h);
    }

    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// 顺序调用，返回第一个 Some 的拦截文案
    pub async fn fire_tool_start(&self, call: &ToolCall, ctx: &HookCtx<'_>) -> Option<String> {
        for h in &self.hooks {
            if let Some(msg) = h.on_tool_start(call, ctx).await {
                return Some(msg);
            }
        }
        None
    }

    pub async fn fire_tool_end(&self, call: &ToolCall, ok: bool, ctx: &HookCtx<'_>) {
        for h in &self.hooks {
            h.on_tool_end(call, ok, ctx).await;
        }
    }

    pub async fn fire_turn_end(&self, ctx: &mut HookCtx<'_>) {
        for h in &self.hooks {
            h.on_turn_end(ctx).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChunkStream, ModelResult};
    use crate::types::{Message, StreamChunk, ToolSchema};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// 空壳 Provider：hook 机制测试只需要填满 HookCtx，不会真调模型
    struct NullProvider {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ModelProvider for NullProvider {
        fn id(&self) -> &str {
            "null"
        }
        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolSchema],
        ) -> ModelResult<ChunkStream> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Box::pin(futures_util::stream::iter(vec![
                Ok(StreamChunk::Done),
            ])))
        }
        fn parse_response(&self, _chunks: &[StreamChunk]) -> ModelResult<(String, Vec<ToolCall>)> {
            Ok((String::new(), vec![]))
        }
    }

    fn fixture<'a>(
        ctx_provider: &'a mut NullProvider,
        signals: &'a crate::evolution::TurnSignals,
    ) -> HookCtx<'a> {
        HookCtx {
            provider: ctx_provider,
            signals,
            task_summary: "",
            work_dir: "",
            session_id: "s-test",
            emitter: None,
            goal_baseline: None,
            skills_baseline: &[],
        }
    }

    fn bash_call() -> ToolCall {
        ToolCall {
            id: "t1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        }
    }

    /// 记录型 mock hook：把点位调用序写进共享日志
    struct RecordingHook {
        tag: &'static str,
        log: Arc<Mutex<Vec<String>>>,
        intercept: Option<String>,
    }

    #[async_trait::async_trait]
    impl Hook for RecordingHook {
        fn name(&self) -> &str {
            self.tag
        }
        async fn on_tool_start(&self, call: &ToolCall, _ctx: &HookCtx<'_>) -> Option<String> {
            self.log
                .lock()
                .unwrap()
                .push(format!("{}.tool_start.{}", self.tag, call.name));
            self.intercept.clone()
        }
        async fn on_tool_end(&self, call: &ToolCall, ok: bool, _ctx: &HookCtx<'_>) {
            self.log
                .lock()
                .unwrap()
                .push(format!("{}.tool_end.{}.{}", self.tag, call.name, ok));
        }
        async fn on_turn_end(&self, _ctx: &mut HookCtx<'_>) {
            self.log.lock().unwrap().push(format!("{}.turn_end", self.tag));
        }
    }

    #[tokio::test]
    async fn test_empty_registry_fire_is_noop() {
        // 空注册表：三点位直接返回，无拦截、零模型调用
        let calls = Arc::new(AtomicUsize::new(0));
        let mut provider = NullProvider { calls: calls.clone() };
        let signals = crate::evolution::TurnSignals::default();
        let reg = HookRegistry::new();
        assert!(reg.is_empty());
        let call = bash_call();
        {
            let ctx = fixture(&mut provider, &signals);
            assert!(reg.fire_tool_start(&call, &ctx).await.is_none());
            reg.fire_tool_end(&call, true, &ctx).await;
        }
        {
            let mut ctx = fixture(&mut provider, &signals);
            reg.fire_turn_end(&mut ctx).await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0, "空注册表不得触碰 provider");
    }

    #[tokio::test]
    async fn test_hooks_called_in_registration_order() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut reg = HookRegistry::new();
        for tag in ["h1", "h2", "h3"] {
            reg.register(Arc::new(RecordingHook {
                tag,
                log: log.clone(),
                intercept: None,
            }));
        }
        assert_eq!(reg.len(), 3);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut provider = NullProvider { calls };
        let signals = crate::evolution::TurnSignals::default();
        let call = bash_call();
        {
            let ctx = fixture(&mut provider, &signals);
            assert!(reg.fire_tool_start(&call, &ctx).await.is_none());
            reg.fire_tool_end(&call, false, &ctx).await;
        }
        {
            let mut ctx = fixture(&mut provider, &signals);
            reg.fire_turn_end(&mut ctx).await;
        }
        assert_eq!(
            *log.lock().unwrap(),
            vec![
                "h1.tool_start.bash",
                "h2.tool_start.bash",
                "h3.tool_start.bash",
                "h1.tool_end.bash.false",
                "h2.tool_end.bash.false",
                "h3.tool_end.bash.false",
                "h1.turn_end",
                "h2.turn_end",
                "h3.turn_end",
            ],
            "点位必须按注册序调用"
        );
    }

    #[tokio::test]
    async fn test_tool_start_intercept_first_some_wins() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut reg = HookRegistry::new();
        reg.register(Arc::new(RecordingHook {
            tag: "h1",
            log: log.clone(),
            intercept: None,
        }));
        reg.register(Arc::new(RecordingHook {
            tag: "h2",
            log: log.clone(),
            intercept: Some("已被防护拦截".into()),
        }));
        reg.register(Arc::new(RecordingHook {
            tag: "h3",
            log: log.clone(),
            intercept: Some("不应到达".into()),
        }));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut provider = NullProvider { calls };
        let signals = crate::evolution::TurnSignals::default();
        let call = bash_call();
        let ctx = fixture(&mut provider, &signals);
        let r = reg.fire_tool_start(&call, &ctx).await;
        assert_eq!(r.as_deref(), Some("已被防护拦截"), "第一个 Some 胜出");
        assert_eq!(
            *log.lock().unwrap(),
            vec!["h1.tool_start.bash", "h2.tool_start.bash"],
            "拦截后短路，后续 hook 不调"
        );
    }
}
