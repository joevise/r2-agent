//! # r2-core
//!
//! R2 Agent 的核心运行时库：模型 Provider、L1/L2 上下文管理、工具注册表、
//! 会话持久化、沙箱，以及面向嵌入方的 [`AgentSession`] 会话门面。
//!
//! ## 快速上手（嵌入用法）
//!
//! ```no_run
//! use r2_core::{AgentSession, AgentEvent, config::Config};
//!
//! # async fn demo() -> Result<(), String> {
//! let config = Config::default_config();
//! let mut session = AgentSession::new(config)?;
//! let mut events = session.subscribe();
//! tokio::spawn(async move {
//!     while let Ok(evt) = events.recv().await {
//!         if let AgentEvent::MessageUpdate(t) = evt {
//!             print!("{t}");
//!         }
//!     }
//! });
//! let reply = session.prompt("你好").await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## 特性（cargo features）
//!
//! - `l3-memory`：启用 L3 跨会话记忆（基于 SQLite 向量索引）
//! - `sandbox-strict`：启用 strict 级 seccomp 沙箱（系统需安装 libseccomp-dev）

pub mod agent;
pub mod agents;
pub mod config;
pub mod context;
mod events;
pub mod mcp;
#[cfg(feature = "l3-memory")]
pub mod memory;
pub mod model;
pub mod evolution;
pub mod groups;
pub mod models;
pub mod namespaces;
pub mod rpc;
pub mod sandbox;
pub mod tasks;
pub mod session;
mod session_api;
pub mod tools;
pub mod types;

pub use agent::Agent;
pub use config::Config;
pub use events::AgentEvent;
pub use session_api::AgentSession;

#[cfg(test)]
pub(crate) mod testutil {
    /// 全仓测试共享的 HOME 互斥锁：HOME 是进程级全局，
    /// 凡改 HOME 的测试（agents/evolution/task_tool）必须串行，防并行踩踏
    pub(crate) static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
