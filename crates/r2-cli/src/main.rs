mod web;

use clap::{Parser, Subcommand};
use r2_core::config::{self, apply_overrides, Config};
use r2_core::rpc::{self, RpcOutcome, RpcServer};
use r2_core::{session, types, Agent, AgentSession};
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

    /// 从指定会话分叉（继承历史后进入新会话）
    #[arg(long)]
    branch: Option<String>,

    /// 分叉截止点：只继承父会话前 N 条消息（配合 --branch 使用）
    #[arg(long)]
    at: Option<usize>,

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
    /// JSON-RPC serve 模式：stdin/stdout 说行分隔 JSON-RPC 2.0（供任何语言嵌入）
    Serve,
    /// 列出内置模型注册表（上下文窗口 / 参考价格 / 工具支持）
    Models {
        /// 额外显示每模型的 API 端点与订阅计划说明
        #[arg(long)]
        verbose: bool,
    },
    /// Web 控制台：axum + WebSocket 壳（R2 Console，浏览器打开即用）
    Web {
        /// 监听端口（仅绑定 127.0.0.1）
        #[arg(long, default_value_t = 5290)]
        port: u16,
    },
    /// 跨会话记忆管理：list / search / delete / stats / migrate
    #[cfg(feature = "l3-memory")]
    Memory {
        #[command(subcommand)]
        action: MemoryAction,
    },
}

#[cfg(feature = "l3-memory")]
#[derive(Subcommand)]
enum MemoryAction {
    /// 重建记忆索引（嵌入模型变更后使用，逐条重新 embed）
    Migrate,
    /// 列出记忆（query 预览 / 时间 / 嵌入后端）
    List {
        /// 最多列出条数
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// 手动检索测试（打印衰减后分数）
    Search {
        /// 检索关键词
        keyword: String,
        /// 返回条数
        #[arg(long, default_value_t = 5)]
        k: usize,
    },
    /// 删除指定记忆（id 来自 memory list）
    Delete {
        /// 记忆 ID
        #[arg(long)]
        id: i64,
    },
    /// 记忆库统计（总数 / 各后端分布 / 时间跨度）
    Stats,
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
    /// 列出会话及分支关系
    Tree,
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
    // 累计用量（JSONL 里有 usage 记录才显示）；成本按当前配置模型价估算并注明
    let usage = session::Session::recover_usage(&dir, id);
    if usage.llm_calls > 0 {
        let model = config.current_model();
        let base = format!(
            "累计用量：输入 {} tok · 输出 {} tok · 调用 {} 次",
            r2_core::models::format_tokens(usage.input_tokens),
            r2_core::models::format_tokens(usage.output_tokens),
            usage.llm_calls
        );
        match r2_core::models::estimate_cost(model, &usage) {
            Some(cost) => println!("\n{base} · 成本 ≈ ¥{cost:.2}（按 {model} 估算）"),
            None => println!("\n{base}"),
        }
    }
    Ok(())
}

/// 打印模型注册表（r2 models）
fn print_models(verbose: bool) {
    if verbose {
        println!(
            "{:<18} {:>10} {:>10} {:>10} {:>6} {:>6}  提供商  端点",
            "模型名", "窗口", "输入价", "输出价", "工具", "订阅"
        );
    } else {
        println!(
            "{:<18} {:>10} {:>10} {:>10} {:>6} {:>6}  提供商",
            "模型名", "窗口", "输入价", "输出价", "工具", "订阅"
        );
    }
    for m in r2_core::models::registry() {
        if verbose {
            println!(
                "{:<18} {:>10} {:>10} {:>10} {:>6} {:>6}  {}  {}",
                m.display_name,
                r2_core::models::format_tokens(m.context_window as u64),
                m.input_price_per_m,
                m.output_price_per_m,
                if m.tool_support { "✓" } else { "✗" },
                if m.coding_plan.is_empty() { "-" } else { "✓" },
                m.provider_hint,
                if m.endpoint.is_empty() { "-" } else { m.endpoint },
            );
        } else {
            println!(
                "{:<18} {:>10} {:>10} {:>10} {:>6} {:>6}  {}",
                m.display_name,
                r2_core::models::format_tokens(m.context_window as u64),
                m.input_price_per_m,
                m.output_price_per_m,
                if m.tool_support { "✓" } else { "✗" },
                if m.coding_plan.is_empty() { "-" } else { "✓" },
                m.provider_hint
            );
        }
    }
    println!("\n订阅/Coding Plan：");
    println!("· 智谱 Coding Plan：open.bigmodel.cn/api/coding/paas/v4（GLM 系列包月）");
    println!("· Kimi Coding Plan：api.kimi.com/coding（k3 / kimi-for-coding 包月）");
    println!("· GitHub Copilot：claude-sonnet-4.6 / gpt-5.4 等（$10/月档含额度）");
    println!("· 小米 Token Plan：token-plan-cn.xiaomimimo.com/v1");
    println!("\n价格单位：元/百万 token。模型名匹配规则：包含即命中（大小写不敏感），价格仅供参考，以官网为准。");
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

/// 打印会话树（sessions tree）：简单列表 + 分支指示
fn print_sessions_tree(config: &Config) -> MainResult<()> {
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
        match (&s.branch_from, s.branch_upto) {
            (Some(parent), Some(upto)) => {
                // 父会话 id 截断显示，避免行过长
                let short: String = parent.chars().take(8).collect();
                println!("{}  {}条  {}  ← 分支自 {}@{}", s.id, s.message_count, preview, short, upto);
            }
            _ => println!("{}  {}条  {}", s.id, s.message_count, preview),
        }
    }
    Ok(())
}

/// memory 子命令组：打开记忆库 + 当前配置的嵌入后端，分发到各管理操作
#[cfg(feature = "l3-memory")]
async fn run_memory(config: &Config, action: &MemoryAction) -> MainResult<()> {
    use r2_core::memory::{self, MemoryStore};

    let provider = memory::build_embedding_provider(config);
    let path = memory::memory_db_path(config);
    let store = MemoryStore::open(&path, provider.id())
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;

    match action {
        MemoryAction::Migrate => {
            let report = store
                .migrate(provider.as_ref(), &|done, total| {
                    println!("重建记忆索引 {done}/{total}");
                })
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            println!(
                "迁移完成：共 {} 条，成功 {} 条，跳过（失败）{} 条",
                report.total, report.migrated, report.failed
            );
        }
        MemoryAction::List { limit } => {
            let entries = store
                .list(*limit)
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            if entries.is_empty() {
                println!("暂无记忆");
                return Ok(());
            }
            for e in &entries {
                let preview: String = e.query.chars().take(50).collect();
                let flag = if e.superseded { "  [已被覆盖]" } else { "" };
                println!(
                    "id={}  |  ts={}  |  {}  |  {}{}",
                    e.id, e.created_at, e.embed_id, preview, flag
                );
            }
        }
        MemoryAction::Search { keyword, k } => {
            let qv = provider
                .embed(keyword)
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            let hits = store
                .search(&qv, *k, 0.0, "")
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            if hits.is_empty() {
                println!("无匹配记忆");
                return Ok(());
            }
            for h in &hits {
                let answer: String = h.answer.chars().take(80).collect();
                println!("score={:.3}  |  问：{}", h.score, h.query);
                println!("           答：{answer}");
            }
        }
        MemoryAction::Delete { id } => {
            let deleted = store
                .delete(*id)
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            if deleted {
                println!("已删除记忆 id={id}");
            } else {
                println!("记忆不存在：id={id}");
            }
        }
        MemoryAction::Stats => {
            let s = store
                .stats()
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            println!("记忆总数：{}", s.total);
            if !s.by_embed_id.is_empty() {
                println!("嵌入后端分布：");
                for (id, count) in &s.by_embed_id {
                    println!("  {id}: {count} 条");
                }
            }
            match (s.oldest, s.newest) {
                (Some(oldest), Some(newest)) => {
                    println!("时间跨度：ts={oldest} ~ ts={newest}");
                }
                _ => println!("时间跨度：（空库）"),
            }
            if let Some(msg) = store.mismatch() {
                println!("警告：{msg}");
            }
        }
    }
    Ok(())
}

/// serve 主循环：stdin 读行 → RpcServer 路由 → stdout 单行写出
///
/// 结构：
/// - stdin 独立线程读行 → mpsc（阻塞 IO 不进 tokio 调度器）
/// - 单一写任务持有 mpsc<String> 收端口，逐行 println + flush（避免多写者交错）
/// - prompt 在 tokio 任务里异步执行：边跑边把 AgentEvent 转成通知行送入写通道；
///   完成后会话 + 结果经 done 通道交还主循环，由主循环写最终响应（保证响应在
///   最后一条 done 通知之后）
async fn run_serve(config_path: Option<String>) -> MainResult<()> {
    let mut server = RpcServer::new();
    if let Some(path) = config_path {
        server.set_config_path(path);
    }

    // 单写者：所有输出行都经这个通道汇聚
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<String>(256);
    let writer = tokio::spawn(async move {
        while let Some(line) = out_rx.recv().await {
            // StdoutLock 不是 Send，不能跨 await 持有：每行现取现放（通道已保证单写者）
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            if writeln!(lock, "{line}").and_then(|_| lock.flush()).is_err() {
                break; // stdout 已关闭：宿主消失，写出线程退出
            }
        }
    });

    // stdin 读行线程：EOF 时关闭通道通知主循环
    let (line_tx, mut line_rx) = tokio::sync::mpsc::channel::<String>(64);
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        loop {
            let mut buf = String::new();
            match stdin.read_line(&mut buf) {
                Ok(0) => break, // EOF
                Ok(_) => {
                    if line_tx.blocking_send(buf).is_err() {
                        break; // 主循环已退出
                    }
                }
                Err(_) => break,
            }
        }
    });

    // prompt 完成回流：(会话, 请求 id, 结果)
    type PromptDone = (AgentSession, u64, Result<String, String>);
    let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<PromptDone>(4);

    let mut stdin_open = true;
    loop {
        tokio::select! {
            maybe_line = line_rx.recv(), if stdin_open => match maybe_line {
                Some(line) => match server.handle_line(&line) {
                    RpcOutcome::Line(l) => {
                        let _ = out_tx.send(l).await;
                    }
                    RpcOutcome::None => {}
                    RpcOutcome::Shutdown(l) => {
                        let _ = out_tx.send(l).await;
                        break;
                    }
                    RpcOutcome::PendingPrompt { id, input } => {
                        match server.begin_prompt() {
                            Some((session, mut events)) => {
                                let out = out_tx.clone();
                                let done = done_tx.clone();
                                tokio::spawn(async move {
                                    let mut session = session;
                                    let result = {
                                        let fut = session.prompt(&input);
                                        tokio::pin!(fut);
                                        loop {
                                            tokio::select! {
                                                r = &mut fut => break r,
                                                evt = events.recv() => match evt {
                                                    Ok(e) => {
                                                        let _ = out.send(rpc::event_notification(&e)).await;
                                                    }
                                                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                                        eprintln!("[serve] 事件通道滞后，丢弃 {n} 条事件");
                                                    }
                                                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                                                }
                                            }
                                        }
                                    };
                                    // prompt 返回前发出的 Done 等事件可能还压在缓冲里，排空后再交还
                                    while let Ok(e) = events.try_recv() {
                                        let _ = out.send(rpc::event_notification(&e)).await;
                                    }
                                    let _ = done.send((session, id, result)).await;
                                });
                            }
                            None => {
                                // 理论不可达（handle_prompt 已确保会话存在），回滚状态并报错
                                server.cancel_prompt();
                                let _ = out_tx
                                    .send(rpc::error_line(Some(id), rpc::SESSION_ERROR, "会话不可用"))
                                    .await;
                            }
                        }
                    }
                },
                None => {
                    // stdin EOF：不再接受新请求；若有在途 prompt 则等它结束
                    stdin_open = false;
                    if !server.in_flight() {
                        break;
                    }
                }
            },
            done = done_rx.recv() => {
                let Some((session, id, result)) = done else { break };
                server.end_prompt(session);
                let line = match result {
                    Ok(text) => rpc::result_line(id, serde_json::json!({"final_text": text})),
                    Err(e) => rpc::error_line(Some(id), rpc::SESSION_ERROR, &e),
                };
                let _ = out_tx.send(line).await;
                if !stdin_open {
                    break; // EOF 后在途 prompt 已完成，优雅退出
                }
            }
        }
    }

    drop(out_tx);
    let _ = writer.await;
    Ok(())
}

#[tokio::main]
async fn main() -> MainResult<()> {
    let cli = Cli::parse();

    // serve 子命令：不进交互/单次流程，自己管配置加载（initialize 请求可覆盖）
    if let Some(Commands::Serve) = &cli.command {
        return run_serve(cli.config.clone()).await;
    }

    let mut config = load_config(&cli)?;
    apply_overrides(&mut config, cli.model.as_deref(), cli.work_dir.as_deref());

    // sessions 子命令（无子命令 = 列出全部）
    if let Some(Commands::Sessions { action }) = &cli.command {
        return match action {
            None => print_sessions(&config),
            Some(SessionsAction::Export { id, out }) => export_session(&config, id, out.as_deref()),
            Some(SessionsAction::Show { id }) => show_session(&config, id),
            Some(SessionsAction::Tree) => print_sessions_tree(&config),
        };
    }
    if cli.list_sessions {
        return print_sessions(&config);
    }

    // models 子命令：只读注册表，不需要 api_key
    if let Some(Commands::Models { verbose }) = &cli.command {
        print_models(*verbose);
        return Ok(());
    }

    // web 子命令：起服务时不强制 api_key（浏览器里可先看界面，prompt 时才会用到）
    if let Some(Commands::Web { port }) = &cli.command {
        return web::run(config, *port).await;
    }

    // memory 子命令（需 l3-memory feature；只做记忆管理，不校验模型 api_key）
    #[cfg(feature = "l3-memory")]
    if let Some(Commands::Memory { action }) = &cli.command {
        return run_memory(&config, action).await;
    }

    check_api_key(&config)?;

    if cli.at.is_some() && cli.branch.is_none() {
        eprintln!("警告：--at 仅在配合 --branch 时生效，已忽略");
    }

    let mut agent = if let Some(session_id) = &cli.session {
        Agent::resume(config, session_id)?
    } else if let Some(parent_id) = &cli.branch {
        Agent::branch_from(config, parent_id, cli.at)?
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
    // 独立线程读 stdin 行 → tokio mpsc：这样 run 运行中用户也能随时打字
    let (line_tx, mut line_rx) = tokio::sync::mpsc::channel::<String>(32);
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        loop {
            let mut buf = String::new();
            match stdin.read_line(&mut buf) {
                Ok(0) => break, // EOF
                Ok(_) => {
                    if line_tx.blocking_send(buf).is_err() {
                        break; // 主循环已退出
                    }
                }
                Err(_) => break,
            }
        }
    });

    // steer 通道：运行中收到的行注入 Agent（中途转向）
    let (steer_tx, steer_rx) = tokio::sync::mpsc::channel::<String>(32);
    agent.set_steer_channel(steer_rx);

    let mut quit_after_run = false;
    loop {
        print!("> ");
        std::io::stdout().flush()?;

        let Some(line) = line_rx.recv().await else {
            println!();
            break; // stdin EOF
        };
        let input = line.trim();
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

        // 运行中循环 select：run 完成 → 回到提示符；又来一行 → 注入 steer
        let run = agent.run(input);
        tokio::pin!(run);
        let result = loop {
            tokio::select! {
                res = &mut run => break res,
                maybe_line = line_rx.recv() => match maybe_line {
                    Some(l) => {
                        let l = l.trim();
                        if l.is_empty() {
                            continue;
                        }
                        match l {
                            "/quit" | "/exit" => {
                                // v0.2 简化：运行中 /quit = 等当前 run 完成后退出
                                println!("\n等待当前任务结束后退出…");
                                quit_after_run = true;
                            }
                            _ => {
                                let _ = steer_tx.send(l.to_string()).await;
                                let preview: String = l.chars().take(60).collect();
                                println!("\n[steer] 指令已注入: {preview}");
                            }
                        }
                    }
                    None => {
                        // 运行中 stdin 关闭：等当前任务结束后退出
                        quit_after_run = true;
                        break run.await;
                    }
                },
            }
        };
        if let Err(e) = result {
            eprintln!("错误：{e}");
        }
        if quit_after_run {
            break;
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
