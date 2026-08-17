//! AgentSession：面向嵌入方的会话门面
//!
//! 用法：
//! ```no_run
//! # use r2_core::{AgentSession, config::Config};
//! # async fn demo() -> Result<(), String> {
//! let mut s = AgentSession::new(Config::default_config())?;
//! let mut events = s.subscribe();          // 广播通道
//! let reply = s.prompt("帮我看看这个项目").await?;
//! # Ok(())
//! # }
//! ```

use crate::agent::Agent;
use crate::config::Config;
use crate::events::AgentEvent;
use tokio::sync::{broadcast, mpsc};

/// 面向嵌入方的会话门面：包一层 Agent，输出走 broadcast 事件而非 stdout
pub struct AgentSession {
    agent: Agent,
    event_tx: broadcast::Sender<AgentEvent>,
    /// 中途转向发送端（接收端已注入 agent）
    steer_tx: mpsc::Sender<String>,
}

impl AgentSession {
    /// 新建会话（quiet 模式：不打印 stdout，事件照常广播）
    pub fn new(config: Config) -> Result<Self, String> {
        let agent = Agent::new(config).map_err(|e| e.to_string())?;
        Ok(Self::wrap(agent))
    }

    /// 恢复指定会话（带历史上下文继续对话）
    pub fn resume(config: Config, session_id: &str) -> Result<Self, String> {
        let agent = Agent::resume(config, session_id).map_err(|e| e.to_string())?;
        Ok(Self::wrap(agent))
    }

    /// 从指定会话分叉（继承父会话 upto 点之前的历史）
    pub fn branch_from(
        config: Config,
        parent_session_id: &str,
        upto: Option<usize>,
    ) -> Result<Self, String> {
        let agent =
            Agent::branch_from(config, parent_session_id, upto).map_err(|e| e.to_string())?;
        Ok(Self::wrap(agent))
    }

    fn wrap(mut agent: Agent) -> Self {
        let (event_tx, _) = broadcast::channel(256);
        let (steer_tx, steer_rx) = mpsc::channel(32);
        agent.set_emitter(event_tx.clone());
        agent.set_steer_channel(steer_rx);
        agent.set_quiet(true);
        Self {
            agent,
            event_tx,
            steer_tx,
        }
    }

    /// 当前会话 ID（用于恢复）
    /// 会话历史消息（Console 切换/分叉/刷新后的 UI 回放用）
    pub fn messages(&self) -> &[crate::types::Message] {
        self.agent.messages()
    }

    pub fn session_id(&self) -> Option<&str> {
        self.agent.session_id()
    }

    /// 订阅事件流（广播通道，capacity 256）
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.event_tx.subscribe()
    }

    /// 发送一轮输入，返回最终回复。事件同步广播给所有订阅者。
    pub async fn prompt(&mut self, input: &str) -> Result<String, String> {
        match self.agent.run(input).await {
            Ok(text) => Ok(text),
            Err(e) => {
                let msg = e.to_string();
                let _ = self.event_tx.send(AgentEvent::Error(msg.clone()));
                Err(msg)
            }
        }
    }

    /// 中途转向：prompt 运行中随时调用；非运行时注入的消息会被下次 run 开头丢弃
    pub async fn steer(&self, instruction: &str) -> Result<(), String> {
        self.steer_tx
            .send(instruction.to_string())
            .await
            .map_err(|e| e.to_string())
    }

    /// 克隆一份 steer 发送端（交互循环等场景持有，运行中随时注入指令）
    pub fn steer_handle(&self) -> mpsc::Sender<String> {
        self.steer_tx.clone()
    }

    /// 清空当前上下文（等价于 CLI 的 /clear）：新建会话文件 + 重置 L1
    pub fn reset_context(&mut self) {
        self.agent.reset_context();
    }

    /// 当前 L1 中的历史消息条数（不含 system prompt / L2 摘要）
    pub fn history_len(&self) -> usize {
        self.agent.history_len()
    }

    /// 测试用：把外部构造好的 Agent（注入了 MockProvider）包成会话
    #[cfg(test)]
    pub(crate) fn wrap_test(agent: Agent) -> Self {
        Self::wrap(agent)
    }
}
