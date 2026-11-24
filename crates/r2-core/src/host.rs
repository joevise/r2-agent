//! 进程级重启信号（v0.17.0）：restart 工具 / 飞书 /restart / Console /api/restart
//! 统一发信号，宿主（web 壳）订阅后执行 graceful 收尾 + exec 自替换。
//!
//! 工具（Box<dyn Tool>）构造时拿不到 Agent/壳层句柄；重启是整进程语义
//! （跟 steer 的会话内语义不同），故选全局静态信号。watch 只存最新值：
//! 多次 request 覆盖（重启只需一次），壳层另有 restarting 防重入 flag 兜底。

use std::sync::OnceLock;
use tokio::sync::watch;

/// 重启信号量（value = reason；None = 尚未请求）。独立结构便于测试构造
/// 隔离实例（全局单例跨并行测试会互相覆盖最新值）
struct RestartSignal {
    tx: watch::Sender<Option<String>>,
}

impl RestartSignal {
    fn new() -> Self {
        let (tx, _) = watch::channel(None);
        Self { tx }
    }
    fn request(&self, reason: &str) {
        let _ = self.tx.send(Some(reason.to_string()));
    }
    fn subscribe(&self) -> watch::Receiver<Option<String>> {
        self.tx.subscribe()
    }
}

static RESTART: OnceLock<RestartSignal> = OnceLock::new();

fn signal() -> &'static RestartSignal {
    RESTART.get_or_init(RestartSignal::new)
}

/// 请求重启（覆盖语义：重复请求只留最新 reason，重启只需一次）
pub fn request(reason: &str) {
    signal().request(reason);
}

/// 壳层订阅（web 启动时 spawn 信号泵消费）
pub fn subscribe() -> watch::Receiver<Option<String>> {
    signal().subscribe()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_request_subscribe_roundtrip() {
        // 隔离实例（不碰全局单例）：request → subscribe 端收到
        let sig = RestartSignal::new();
        let mut rx = sig.subscribe();
        assert!(rx.borrow().is_none());
        sig.request("t-roundtrip");
        rx.changed().await.unwrap();
        assert_eq!(rx.borrow().as_deref(), Some("t-roundtrip"));
    }

    #[tokio::test]
    async fn test_request_overwrite_semantics() {
        // 覆盖语义：连续 request 只留最新；晚订阅者也能读到最新值（watch 语义）
        let sig = RestartSignal::new();
        let mut rx = sig.subscribe();
        sig.request("t-old");
        sig.request("t-new");
        rx.changed().await.unwrap();
        assert_eq!(rx.borrow().as_deref(), Some("t-new"));
        let late = sig.subscribe();
        assert_eq!(late.borrow().as_deref(), Some("t-new"));
    }
}
