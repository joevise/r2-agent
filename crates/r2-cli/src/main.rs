use clap::{Parser, Subcommand};
use r2_core::config::{self, apply_overrides, Config};
use r2_core::{session, types, Agent};
use std::io::Write;

/// R2 Agent — 极简但可靠的 Rust Agent
#[derive(Parser)]
#[command(name = "r2", version, about = "R2 Agent — Small droid, big jobs.")]
struct Cli {
    /// 单次执行：回答该问题后退出
    #[arg(long)]
    once: Option<String>,

    /// 指定配置文件路径
    #[arg(long)]
    config: Option<String>,

    /// 恢复指定会话（带历史上下文继续对话）
    #[arg(long)]
    session: Option<String>,

    /// 列出所有历史会话后退出（等价于 `r2 sessions`，保留作向后兼容）
    #[arg(long)]
    list_sessions: bool,

    /// 覆盖配置里的模型名（作用于当前 provider）
    #[arg(long)]
    model: Option<String>,

    /// 覆盖工作目录（工具操作的根目录，支持 ~ 展开）
    #[arg(long)]
    work_dir: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// 会话管理：列出 / 导出 / 查看历史会话
    Sessions {
        #[command(subcommand)]
        action: Option<SessionsAction>,
    },
}

#[derive(Subcommand)]
enum SessionsAction {
    /// 导出会话为 JSON
    Export {
        /// 会话 ID
        id: String,
        /// 输出到文件（缺省打印到 stdout）
        #[arg(long)]
        out: Option<String>,
    },
    /// 查看会话内容（人类可读格式）
    Show {
        /// 会话 ID
        id: String,
    },
}

const CONFIG_EXAMPLE: &str = r#"最小配置示例（~/.r2/config.toml）：

[model]
provider = "openai_compat"

[model.openai_compat]
base_url = "https://open.bigmodel.cn/api/coding/paas/v4"
api_key = "你的key"
model = "glm-5.2"
"#;

type MainResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn load_config(cli: &Cli) -> MainResult<Config> {
    if let Some(path) = &cli.config {
        if !std::path::Path::new(path).exists() {
            return Err(format!("指定的配置文件不存在：{path}").into());
        }
        return Config::load_from_file(path)
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.to_string().into() });
    }
    let default_path = config::expand_tilde("~/.r2/config.toml");
    if std::path::Path::new(&default_path).exists() {
        return Config::load_from_file(&default_path)
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.to_string().into() });
    }
    Ok(Config::default_config())
}

fn check_api_key(config: &Config) -> MainResult<()> {
    let key = match config.model.provider.as_str() {
        "openai_compat" => &config.model.openai_compat.api_key,
        "anthropic" => &config.model.anthropic.api_key,
        _ => return Ok(()),
    };
    if key.is_empty() {
        eprintln!("未配置 API Key。请创建 ~/.r2/config.toml\n");
        eprintln!("{CONFIG_EXAMPLE}");
        std::process::exit(1);
    }
    Ok(())
}

/// 校验会话目录可用，返回展开后的路径
fn sessions_dir(config: &Config) -> MainResult<String> {
    let dir = config::expand_tilde(&config.session.dir);
    if !std::path::Path::new(&dir).is_dir() {
        return Err(format!("会话目录不可用：{dir}（还没有任何会话记录）").into());
    }
    Ok(dir)
}

/// 导出会话为 JSON：复用 Session::recover 的重建逻辑，导出干净的恢复后消息列表
fn export_session(config: &Config, id: &str, out: Option<&str>) -> MainResult<()> {
    let dir = sessions_dir(config)?;
    let path = std::path::Path::new(&dir).join(format!("{id}.jsonl"));
    if !path.exists() {
        return Err(format!("会话不存在：{id}").into());
    }
    // created_at 取文件 mtime（v0.1 没有单独的创建时间元数据）
    let created_at = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (_session, messages) = session::Session::recover(&dir, id)
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
    let json = serde_json::json!({
        "session_id": id,
        "created_at": created_at,
        "messages": messages,
    });
    let pretty = serde_json::to_string_pretty(&json)?;
    match out {
        Some(file) => {
            std::fs::write(file, &pretty)?;
            println!("已导出到 {file}");
        }
        None => println!("{pretty}"),
    }
    Ok(())
}

/// 人类可读格式打印会话内容
fn show_session(config: &Config, id: &str) -> MainResult<()> {
    let dir = sessions_dir(config)?;
    let (_session, messages) = session::Session::recover(&dir, id)
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
    println!("会话 {id}（{} 条消息）", messages.len());
    for m in &messages {
        let role = match m.role {
            types::Role::System => "system",
            types::Role::User => "user",
            types::Role::Assistant => "assistant",
            types::Role::Tool => "tool",
        };
        println!("\n[{role}]");
        if !m.content.is_empty() {
            println!("{}", m.content);
        }
        if let Some(calls) = &m.tool_calls {
            for c in calls {
                // 参数摘要：截断到 120 字符，避免刷屏
                let args: String = c.arguments.chars().take(120).collect();
                println!("  → 工具调用 {}({})", c.name, args);
            }
        }
    }
    Ok(())
}

/// 打印会话列表（--list-sessions）
fn print_sessions(config: &Config) -> MainResult<()> {
    let dir = config::expand_tilde(&config.session.dir);
    let sessions = session::list_sessions(&dir)
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
    if sessions.is_empty() {
        println!("暂无历史会话");
        return Ok(());
    }
    for s in sessions {
        let preview = if s.first_user_preview.is_empty() {
            "（无用户消息）"
        } else {
            &s.first_user_preview
        };
        println!(
            "{}  |  {} 条消息  |  ts={}  |  {}",
            s.id, s.message_count, s.last_ts, preview
        );
    }
    Ok(())
}

#[tokio::main]
async fn main() -> MainResult<()> {
    let cli = Cli::parse();
    let mut config = load_config(&cli)?;
    apply_overrides(&mut config, cli.model.as_deref(), cli.work_dir.as_deref());

    // sessions 子命令（无子命令 = 列出全部）
    if let Some(Commands::Sessions { action }) = &cli.command {
        return match action {
            None => print_sessions(&config),
            Some(SessionsAction::Export { id, out }) => export_session(&config, id, out.as_deref()),
            Some(SessionsAction::Show { id }) => show_session(&config, id),
        };
    }
    if cli.list_sessions {
        return print_sessions(&config);
    }

    check_api_key(&config)?;

    let mut agent = if let Some(session_id) = &cli.session {
        Agent::resume(config, session_id)?
    } else {
        Agent::new(config)?
    };

    if let Some(question) = cli.once {
        agent.run(&question).await?;
        return Ok(());
    }

    // 启动横幅：单行
    match agent.session_id() {
        Some(id) => println!("R2 v{} | 会话 {} | /help 帮助", env!("CARGO_PKG_VERSION"), id),
        None => println!("R2 v{} | /help 帮助", env!("CARGO_PKG_VERSION")),
    }
    let stdin = std::io::stdin();
    loop {
        print!("> ");
        std::io::stdout().flush()?;

        let mut input = String::new();
        let n = stdin.read_line(&mut input)?;
        if n == 0 {
            println!();
            break;
        }
        let input = input.trim();
        if input.is_empty() {
            continue;
        }
        match input {
            "/quit" | "/exit" => break,
            "/help" => {
                print!("{HELP_TEXT}");
                continue;
            }
            "/clear" => {
                agent.reset_context();
                match agent.session_id() {
                    Some(id) => println!("已清空上下文，新会话：{id}"),
                    None => println!("已清空上下文"),
                }
                continue;
            }
            _ => {}
        }
        if let Err(e) = agent.run(input).await {
            eprintln!("错误：{e}");
        }
    }
    println!("再见！");
    Ok(())
}

const HELP_TEXT: &str = "可用命令：
  /help    显示本帮助
  /quit    退出
  /exit    退出
  /clear   清空当前上下文（开新会话文件，但保持进程）
";
