// 地基阶段：部分核心类型尚未被运行时消费，暂允许 dead_code
#![allow(dead_code)]

mod agent;
mod config;
mod context;
mod model;
mod types;

use agent::Agent;
use clap::Parser;
use config::Config;
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

#[tokio::main]
async fn main() -> MainResult<()> {
    let cli = Cli::parse();
    let config = load_config(&cli)?;
    check_api_key(&config)?;

    let mut agent = Agent::new(config)?;

    if let Some(question) = cli.once {
        agent.run(&question).await?;
        return Ok(());
    }

    println!("R2 Agent — 输入 /quit 或 /exit 退出");
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
        if input == "/quit" || input == "/exit" {
            break;
        }
        if let Err(e) = agent.run(input).await {
            eprintln!("错误：{e}");
        }
    }
    println!("再见！");
    Ok(())
}
