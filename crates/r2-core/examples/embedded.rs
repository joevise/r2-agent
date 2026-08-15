//! 嵌入示例：演示 r2-core 库形态的用法（AgentSession + 事件订阅）

use r2_core::config::{expand_tilde, Config};
use r2_core::{AgentEvent, AgentSession};

fn load_config() -> Result<Config, String> {
    let default_path = expand_tilde("~/.r2/config.toml");
    if std::path::Path::new(&default_path).exists() {
        return Config::load_from_file(&default_path).map_err(|e| e.to_string());
    }
    Err(format!("未找到配置文件：{default_path}"))
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let config = load_config()?;
    let mut session = AgentSession::new(config)?;
    println!("会话: {:?}", session.session_id());
    let mut events = session.subscribe();

    let handle = tokio::spawn(async move {
        while let Ok(evt) = events.recv().await {
            match evt {
                AgentEvent::MessageUpdate(t) => print!("{t}"),
                AgentEvent::ToolCall { name, arguments } => {
                    let args: String = arguments.chars().take(60).collect();
                    println!("\n[事件] 工具调用 {name}: {args}");
                }
                AgentEvent::ToolResult { name, output } => {
                    let out: String = output.chars().take(80).collect();
                    println!("[事件] 工具结果 {name}: {out}");
                }
                AgentEvent::Done { .. } => break,
                AgentEvent::Error(e) => {
                    println!("[事件] 错误: {e}");
                    break;
                }
                _ => {}
            }
        }
    });

    let reply = session.prompt("读取当前目录的 Cargo.toml，告诉我包名").await?;
    handle.await.ok();
    println!("\n\n最终回复: {reply}");
    Ok(())
}
