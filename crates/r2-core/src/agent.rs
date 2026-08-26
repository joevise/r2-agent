//! Agent 循环引擎：用户输入 → 模型流式响应 → 工具执行 → 输出

use crate::config::Config;
use crate::context::ContextManager;
use crate::events::AgentEvent;
#[cfg(feature = "l3-memory")]
use crate::memory::{EmbeddingProvider, MemoryStore};
use crate::model::{create_provider, ModelProvider, ModelResult};
use crate::session::{Session, SessionEntry};
use crate::tools::ToolRegistry;
use crate::types::{Role, StreamChunk, ToolCall, UsageStats};
use futures_util::StreamExt;
use std::io::Write;

pub(crate) const SYSTEM_PROMPT: &str = "你是 R2，一个极简但可靠的 Rust Agent。";

/// 工具使用规范补充段：既定行为说明，跟随内核核心（不可覆盖）
const TOOL_USAGE_RULES: &str = "\
工具使用规范：
- 文件路径优先用相对 work_dir 的相对路径；不越出 work_dir 读写，除非用户明确要求。
- bash 命令有超时限制（默认 30 秒），长任务请拆分执行。
- 工具输出可能被截断，关键信息缺失时用更精确的范围重读。
- 执行有副作用的命令前，先向用户说明影响。
- 失败的输出也是信息：按错误提示调整策略，不要盲目重试。";

/// 单个人格/上下文文件的最大读取量：超过即截断，防撑爆上下文
const MAX_LAYER_BYTES: usize = 64 * 1024;

/// system prompt 分层结果（壳展示用）：core 恒有，其余层按命中情况填充
#[derive(Debug, Clone, PartialEq)]
pub struct PromptSections {
    pub core: String,
    pub soul: Option<String>,
    pub agents: Option<String>,
    pub custom: Option<String>,
    /// 已安装技能清单（动态扫描 ~/.r2/skills，每次构建 prompt 时刷新）
    pub skills: Option<String>,
}

/// 用给定 home 展开路径开头的 ~（测试可注入假 home，隔离真实 ~/.r2）
fn expand_with_home(path: &str, home: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else if path == "~" {
        home.to_string()
    } else {
        path.to_string()
    }
}

/// 读取一个人格/上下文文件：不存在/读失败/全空白 → None；超 64KB 截断并加提示
fn read_layer(path: &str) -> Option<String> {
    let mut content = std::fs::read_to_string(path).ok()?;
    if content.trim().is_empty() {
        return None;
    }
    if content.len() > MAX_LAYER_BYTES {
        // 退到最近的 UTF-8 字符边界再截断，避免切坏多字节字符
        let mut end = MAX_LAYER_BYTES;
        while end > 0 && !content.is_char_boundary(end) {
            end -= 1;
        }
        content.truncate(end);
        content.push_str("\n…（文件超过 64KB，已截断）");
    }
    Some(content)
}

/// 组装三层 system prompt（见 docs/web-harness-plan.md 第三节）：
/// 内核核心（不可覆盖）+ config 自定义（显式覆盖，优先）或 SOUL.md + AGENTS.md
/// 组装三层 system prompt（核心 + SOUL.md + AGENTS.md，可被 config.system_prompt 覆盖）。
/// 返回 (全文, 分段)；web 壳用分段做 PROMPT 面板展示。
pub fn build_system_prompt(config: &Config) -> (String, PromptSections) {
    let home = std::env::var("HOME").unwrap_or_default();
    build_system_prompt_with_home(config, &home)
}


/// 扫描技能目录 → 技能清单文本（system prompt 注入用）。
/// 双层归属（v0.9）：~/.r2/skills 全员共享 + {persona}/skills 个人私有（同名私有优先）。
/// 每个技能一行：名字 + frontmatter description（无则正文首行）。
fn scan_skills_layer(home: &str, persona_dir: Option<&str>) -> Option<String> {
    let mut dirs = Vec::new();
    if let Some(p) = persona_dir {
        dirs.push(format!("{p}/skills")); // 私有在前：同名时先入表
    }
    dirs.push(format!("{home}/.r2/skills"));
    // 收集（先到的同名胜出）
    let mut items: Vec<(String, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !seen.insert(name.clone()) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(entry.path().join("SKILL.md")) else {
                continue;
            };
            let desc = skill_frontmatter_desc(&content);
            items.push((name, desc));
        }
    }
    if items.is_empty() {
        return None;
    }
    // read_dir 顺序随文件系统状态漂移，不排序会让 system prompt 前缀抖动、
    // 直接击穿 KV-cache 前缀命中——按名字排序保证跨调用字节级稳定
    items.sort_by(|a, b| a.0.cmp(&b.0));
    let lines: Vec<String> = items.iter().map(|(n, d)| format!("- {n}：{d}")).collect();
    Some(format!(
        "以下技能已安装（共享目录 ~/.r2/skills）。任务匹配时主动使用：\n\
         先 bash 执行 cat ~/.r2/skills/<名字>/SKILL.md 阅读完整流程，再遵循执行。\n\
         安装新技能：写到 ~/.r2/skills/<名字>/SKILL.md（frontmatter 带 name/description）。\n\
         {}",
        lines.join("\n")
    ))
}


/// 提取 SKILL.md 描述：frontmatter description 优先，回退正文首条非空行
fn skill_frontmatter_desc(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.first().map(|l| l.trim()) == Some(&"---") {
        if let Some(end) = lines[1..].iter().position(|l| l.trim() == "---") {
            for l in &lines[1..=end] {
                let t = l.trim();
                if let Some(v) = t.strip_prefix("description:") {
                    let v = v.trim().trim_matches('"').trim_matches('\'');
                    if !v.is_empty() {
                        return v.chars().take(120).collect();
                    }
                }
            }
        }
    }
    let start = if lines.first().map(|l| l.trim()) == Some(&"---") {
        lines.iter().position(|l| l.trim() == "---").map(|p| p + 1).unwrap_or(0)
    } else {
        0
    };
    lines[start..]
        .iter()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .trim_start_matches('#')
        .trim()
        .chars()
        .take(120)
        .collect()
}

fn build_system_prompt_with_home(config: &Config, home: &str) -> (String, PromptSections) {
    let core = format!("{SYSTEM_PROMPT}\n\n{TOOL_USAGE_RULES}");
    let mut full = core.clone();
    let mut sections = PromptSections {
        core,
        soul: None,
        agents: None,
        custom: None,
        skills: None,
    };

    // 技能层：动态扫描 ~/.r2/skills（装了自动可见，删了自动消失，零维护）。
    // 放最前且不受 custom 覆盖——能力感知必须始终可见，
    // 否则 agent 装了 skill 也不知道去哪找（实测病灶：去翻 ~/.claude/skills）。
    let persona_dir = config.agent.persona_dir.as_deref().map(|p| expand_with_home(p, home));
    let skills = scan_skills_layer(home, persona_dir.as_deref());
    if let Some(ref s) = skills {
        full.push_str("\n\n[已安装技能]\n");
        full.push_str(s);
        sections.skills = Some(s.clone());
    }
    // 成长系统自我认知层（v0.7.2）：没有这段，agent 不知道自己的进化机制存在
    // （实测病灶：用户口述目标，它只能塞进工作目录笔记；学完的东西不沉淀）。
    // 宪法保护 = 语义约束（不得自行更改目标），而非文件权限——代笔权给 agent。
    full.push_str("\n\n[成长系统]\n");
    full.push_str(
        "你有自我成长机制，用户口头表达时请主动使用：\n\
         · 目标代笔：用户说出目标/身份/期望（如「我希望你成为…」）时，把原话忠实写入 ~/.r2/GOAL.md\n\
           （用 bash 写入），写入前先向用户复述确认。未经用户明确表达，绝不创建或修改目标。\n\
         · 学习沉淀：完成有意义的学习（方法论/框架/流程/领域知识）后，主动蒸馏成技能文件\n\
           写入 ~/.r2/skills/<名字>/SKILL.md（frontmatter 含 name 和 description），\n\
           下次会话自动生效——学习不沉淀等于白学。\n\
         · 你的目标与技能会出现在成长档案里，持续积累成为你的一部分。",
    );

    // config [agent] system_prompt 非空：显式覆盖，SOUL / AGENTS 两层跳过
    let custom = config.agent.system_prompt.trim();
    if !custom.is_empty() {
        full.push_str("\n\n[自定义配置]\n");
        full.push_str(custom);
        sections.custom = Some(custom.to_string());
        return (full, sections);
    }

    // SOUL 层（v0.9）：分身优先自己的 {persona}/SOUL.md（标题随之说明归属），
    // 无个人 SOUL 时回退全局人格——诚实降级，不静默吞掉人格。
    let soul_path = persona_dir
        .as_ref()
        .map(|p| format!("{p}/SOUL.md"))
        .unwrap_or_else(|| expand_with_home("~/.r2/SOUL.md", home));
    let soul_title = if persona_dir.is_some() { "[SOUL.md 分身人格]" } else { "[SOUL.md 全局人格]" };
    if let Some(soul) = read_layer(&soul_path) {
        full.push_str(&format!("\n\n{soul_title}\n"));
        full.push_str(&soul);
        sections.soul = Some(soul);
    }
    let work_dir = expand_with_home(&config.agent.work_dir, home);
    if let Some(agents) = read_layer(&format!("{work_dir}/AGENTS.md")) {
        full.push_str("\n\n[AGENTS.md 项目上下文]\n");
        full.push_str(&agents);
        sections.agents = Some(agents);
    }
    (full, sections)
}

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
    /// L3 嵌入后端（与 memory 同生共死；API 后端失败时降级跳过记忆读写）
    #[cfg(feature = "l3-memory")]
    embedding: Option<Box<dyn EmbeddingProvider>>,
    /// 事件广播（库形态嵌入时由 AgentSession 注入；CLI 下为 None，行为不变）
    emitter: Option<tokio::sync::broadcast::Sender<AgentEvent>>,
    /// 中途转向通道（AgentSession / CLI 注入；未注入时行为与原来完全一致）
    steer_rx: Option<tokio::sync::mpsc::Receiver<String>>,
    /// 静音模式：为 true 时不向 stdout 打印（事件照常广播）
    quiet: bool,
    /// 会话生命周期的累计用量统计
    usage: UsageStats,
    /// 本轮 run 的硬信号采集（反思钩子原料）
    turn_signals: crate::evolution::TurnSignals,
    /// 本轮开始时的目标快照（结尾 diff → goal_set 记账）
    turn_start_goal: Option<String>,
    /// 本轮开始时的技能名快照（结尾 diff → 沉淀记账）
    turn_start_skills: Vec<String>,
    /// 本轮失败过的工具名（重试成功的检测依据）
    failed_tools: std::collections::HashSet<String>,
    /// 组装好的三层 system prompt 全文（壳展示用 effective_system_prompt）
    system_prompt: String,
}

impl Agent {
    pub fn new(config: Config) -> ModelResult<Self> {
        #[cfg(not(feature = "l3-memory"))]
        Self::warn_l3_not_compiled(&config);
        // 直接构造的 Config（未走 load_from_file）也在这里兜底做窗口自动预算
        let mut config = config;
        config.resolve_auto_budget();
        let provider = create_provider(&config)?;
        let max_tokens = config.agent.max_total_tokens;
        let (system_prompt, _sections) = build_system_prompt(&config);
        let context = ContextManager::new(&system_prompt, max_tokens, config.context.l1_threshold);
        let mut tools = ToolRegistry::new_default(&config.agent.work_dir, &config.sandbox, config.mcp_write_path().as_deref())
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
        tools.connect_mcp(&config.mcp);
        let session = Session::create(&crate::config::expand_tilde(&config.session.dir)).ok();
        #[cfg(feature = "l3-memory")]
        let (memory, embedding) = Self::init_memory(&config);
        Ok(Self {
            provider,
            context,
            tools,
            config,
            session,
            #[cfg(feature = "l3-memory")]
            memory,
            #[cfg(feature = "l3-memory")]
            embedding,
            emitter: None,
            steer_rx: None,
            quiet: false,
            usage: UsageStats::default(),
            turn_signals: crate::evolution::TurnSignals::default(),
            turn_start_goal: None,
            turn_start_skills: Vec::new(),
            failed_tools: std::collections::HashSet::new(),
            system_prompt,
        })
    }

    /// 恢复指定会话：读 JSONL 重建上下文，继续追加写
    pub fn resume(config: Config, session_id: &str) -> ModelResult<Self> {
        #[cfg(not(feature = "l3-memory"))]
        Self::warn_l3_not_compiled(&config);
        let mut config = config;
        config.resolve_auto_budget();
        let provider = create_provider(&config)?;
        let max_tokens = config.agent.max_total_tokens;
        let session_dir = crate::config::expand_tilde(&config.session.dir);
        let (session, messages) = Session::recover(&session_dir, session_id)
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
        // 累计用量一并恢复（取 JSONL 里最后一条 usage 快照）
        let usage = Session::recover_usage(&session_dir, session_id);
        let count = messages.len();
        let (system_prompt, _sections) = build_system_prompt(&config);
        let context = ContextManager::from_messages(
            &system_prompt,
            messages,
            max_tokens,
            config.context.l1_threshold,
        );
        let mut tools = ToolRegistry::new_default(&config.agent.work_dir, &config.sandbox, config.mcp_write_path().as_deref())
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
        tools.connect_mcp(&config.mcp);
        println!("已恢复会话 {session_id}（{count} 条历史消息）");
        #[cfg(feature = "l3-memory")]
        let (memory, embedding) = Self::init_memory(&config);
        Ok(Self {
            provider,
            context,
            tools,
            config,
            session: Some(session),
            #[cfg(feature = "l3-memory")]
            memory,
            #[cfg(feature = "l3-memory")]
            embedding,
            emitter: None,
            steer_rx: None,
            quiet: false,
            usage,
            turn_signals: crate::evolution::TurnSignals::default(),
            turn_start_goal: None,
            turn_start_skills: Vec::new(),
            failed_tools: std::collections::HashSet::new(),
            system_prompt,
        })
    }

    /// 从某会话分叉并继续（上下文 = 父会话 upto 点 + 后续新对话）
    ///
    /// 流程：Session::branch 新建分支会话文件 → 继承消息灌入 L1 → 组装 Agent。
    /// 之后对话追加写到新会话文件，不碰父文件。
    pub fn branch_from(config: Config, parent_session_id: &str, upto: Option<usize>) -> ModelResult<Self> {
        #[cfg(not(feature = "l3-memory"))]
        Self::warn_l3_not_compiled(&config);
        let mut config = config;
        config.resolve_auto_budget();
        let provider = create_provider(&config)?;
        let max_tokens = config.agent.max_total_tokens;
        let session_dir = crate::config::expand_tilde(&config.session.dir);
        let (session, messages) = Session::branch(&session_dir, parent_session_id, upto)
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
        let count = messages.len();
        let new_id = session.id().to_string();
        let (system_prompt, _sections) = build_system_prompt(&config);
        let context = ContextManager::from_messages(
            &system_prompt,
            messages,
            max_tokens,
            config.context.l1_threshold,
        );
        let mut tools = ToolRegistry::new_default(&config.agent.work_dir, &config.sandbox, config.mcp_write_path().as_deref())
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
        tools.connect_mcp(&config.mcp);
        println!("已从会话 {parent_session_id} 分叉（继承 {count} 条消息，新会话 {new_id}）");
        #[cfg(feature = "l3-memory")]
        let (memory, embedding) = Self::init_memory(&config);
        Ok(Self {
            provider,
            context,
            tools,
            config,
            session: Some(session),
            #[cfg(feature = "l3-memory")]
            memory,
            #[cfg(feature = "l3-memory")]
            embedding,
            emitter: None,
            steer_rx: None,
            quiet: false,
            // 分支会话的用量从头累计（不继承父会话的用量快照）
            usage: UsageStats::default(),
            turn_signals: crate::evolution::TurnSignals::default(),
            turn_start_goal: None,
            turn_start_skills: Vec::new(),
            failed_tools: std::collections::HashSet::new(),
            system_prompt,
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

    /// 初始化 L3 跨会话记忆 + 嵌入后端：l3_enabled=false 或打开失败时为 (None, None)
    #[cfg(feature = "l3-memory")]
    fn init_memory(config: &Config) -> (Option<MemoryStore>, Option<Box<dyn EmbeddingProvider>>) {
        if !config.context.l3_enabled {
            return (None, None);
        }
        let embedding = crate::memory::build_embedding_provider(config);
        let path = crate::memory::memory_db_path(config);
        match MemoryStore::open(&path, embedding.id()) {
            Ok(m) => (Some(m), Some(embedding)),
            Err(e) => {
                tracing::warn!("L3 记忆库初始化失败（跳过）：{e}");
                (None, None)
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
    /// 会话历史消息（Console 切换/分叉后的 UI 回放用）
    pub fn messages(&self) -> &[crate::types::Message] {
        self.context.history()
    }

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
            &self.system_prompt,
            self.config.agent.max_total_tokens,
            self.config.context.l1_threshold,
        );
        // 新会话文件：用量统计一并清零
        self.usage = UsageStats::default();
    }

    /// 当前会话的累计用量统计
    pub fn usage(&self) -> &UsageStats {
        &self.usage
    }

    /// 组装好的三层 system prompt 全文（壳展示用）
    pub fn effective_system_prompt(&self) -> &str {
        &self.system_prompt
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

    /// 反思钩子（v0.7 自进化核心环）：本轮有硬信号时，让模型把信号翻译成
    /// 一条可操作教训，落进化事件流。失败静默（进化是副业，不打扰主业）。
    /// 铁律：模型只翻译硬信号，不评判（prompt 见 evolution::reflection_messages）。
    async fn reflect_and_record(&mut self, task_summary: &str) {
        if self.turn_signals.is_empty() {
            return;
        }
        let msgs = crate::evolution::reflection_messages(&self.turn_signals, task_summary);
        let stream = match self.provider.chat_stream(&msgs, &[]).await {
            Ok(s) => s,
            Err(_) => return, // 反思失败不影响主流程
        };
        let mut chunks = Vec::new();
        let mut st = stream;
        use futures_util::StreamExt;
        while let Some(item) = st.next().await {
            match item {
                Ok(c) => chunks.push(c),
                Err(_) => return,
            }
        }
        let Ok((text, _)) = self.provider.parse_response(&chunks) else {
            return;
        };
        if let Some((lesson, evidence)) = crate::evolution::parse_reflection(&text) {
            let ev = crate::evolution::EvolutionEvent {
                ts: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                kind: "lesson".into(),
                content: lesson.clone(),
                evidence,
                session_id: self.session_id().unwrap_or("unknown").to_string(),
            };
            if crate::evolution::append_event(&ev).is_ok() {
                self.emit(AgentEvent::Evolved(format!("🌱 学到教训：{lesson}")));
            }
        }
    }


    /// 成长变化记账：本轮内目标被设定/修改、技能被沉淀 → 事件流 + git 快照 + 广播。
    /// 与反思钩子互补：反思管"教训"，这里管"自我档案的变化"。
    async fn record_growth_changes(&mut self) {
        let now = crate::evolution::now_ts_pub();
        // 目标 diff
        let goal_after = crate::evolution::read_goal();
        let goal_changed = match (&self.turn_start_goal, &goal_after) {
            (None, Some(g)) => Some(format!("目标已设定：{}", truncate_str(g, 80))),
            (Some(a), Some(b)) if a != b => Some(format!("目标已更新：{}", truncate_str(b, 80))),
            _ => None,
        };
        if let Some(content) = goal_changed {
            let _ = crate::evolution::append_event(&crate::evolution::EvolutionEvent {
                ts: now,
                kind: "goal_set".into(),
                content,
                evidence: "用户口述 · agent 代笔".into(),
                session_id: self.session_id().unwrap_or("unknown").to_string(),
            });
            let _ = crate::evolution::snapshot_self("goal updated");
            self.emit(AgentEvent::Evolved("🎯 目标已记录".into()));
        }
        // 技能 diff（新增 = 沉淀；删除不记账——归档机制已有自己的事件）
        let after = crate::evolution::list_skill_names();
        let added: Vec<&String> = after.iter().filter(|n| !self.turn_start_skills.contains(n)).collect();
        if let Some(first) = added.first() {
            // 只对非 auto- 前缀记账（auto 起草有自己的 skill_draft 事件，避免双记）
            if !first.starts_with("auto-") {
                let names: Vec<String> = added.iter().map(|s| s.to_string()).collect();
                let _ = crate::evolution::append_event(&crate::evolution::EvolutionEvent {
                    ts: now,
                    kind: "skill_created".into(),
                    content: format!("学习沉淀为技能：{}", names.join("、")),
                    evidence: "agent 主动蒸馏（成长系统注入指引）".into(),
                    session_id: self.session_id().unwrap_or("unknown").to_string(),
                });
                let _ = crate::evolution::snapshot_self(&format!("skill沉淀 {names:?}"));
                self.emit(AgentEvent::Evolved(format!("⭐ 学习沉淀：{}", names.join("、"))));
            }
        }
    }

    /// 同族教训 ≥3 条 → LLM 合成 SKILL.md 草稿（trial 状态）→ 事件 + git 快照。
    /// 每轮最多起草 1 个（防突发成批）；名字冲突 = 已在成长中，跳过。
    async fn maybe_draft_skill(&mut self) {
        let clusters = crate::evolution::detect_lesson_clusters(3);
        let Some(lessons) = clusters.first() else { return };
        let msgs = crate::evolution::draft_messages(lessons);
        let Ok(mut stream) = self.provider.chat_stream(&msgs, &[]).await else {
            return;
        };
        let mut chunks = Vec::new();
        use futures_util::StreamExt;
        while let Some(item) = stream.next().await {
            match item {
                Ok(c) => chunks.push(c),
                Err(_) => return,
            }
        }
        let Ok((text, _)) = self.provider.parse_response(&chunks) else {
            return;
        };
        let Some(name) = crate::evolution::parse_draft_name(&text) else {
            return;
        };
        if crate::evolution::write_draft_skill(&name, &text).is_ok() {
            let n = lessons.len();
            let _ = crate::evolution::append_event(&crate::evolution::EvolutionEvent {
                ts: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                kind: "skill_draft".into(),
                content: format!("同族教训 ×{n} 自动合成技能「{name}」（试用中）"),
                evidence: lessons.first().cloned().unwrap_or_default(),
                session_id: self.session_id().unwrap_or("unknown").to_string(),
            });
            let _ = crate::evolution::snapshot_self(&format!("draft skill {name}"));
            self.emit(AgentEvent::Evolved(format!("✏️ 教训攒够 {n} 条，自动起草技能 {name}（试用）")));
        }
    }

    async fn summarize(&mut self, old_msgs: &[crate::types::Message]) -> ModelResult<String> {
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
        // L2 摘要也计入用量：明细标不了就并入总数
        self.usage.input_tokens += crate::context::message_tokens(&req[0]) as u64;
        self.usage.llm_calls += 1;
        let mut stream = self.provider.chat_stream(&req, &[]).await?;
        let mut chunks = Vec::new();
        while let Some(item) = stream.next().await {
            chunks.push(item?);
        }
        let (text, _) = self.provider.parse_response(&chunks)?;
        self.usage.output_tokens += crate::context::estimate_tokens(&text) as u64;
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
        // 硬信号采集开始（每轮独立）
        self.turn_signals = crate::evolution::TurnSignals::default();
        self.failed_tools.clear();
        // 成长变化基线：本轮开始时的目标与技能（结尾 diff 记账）
        self.turn_start_goal = crate::evolution::read_goal();
        self.turn_start_skills = crate::evolution::list_skill_names();
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
        let memory_msg = self.recall_memory(user_input).await;

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
            // 用量统计：输入 = 本次发给模型的全部消息（含 system prompt / 摘要 / 记忆注入）
            self.usage.input_tokens += messages
                .iter()
                .map(crate::context::message_tokens)
                .sum::<usize>() as u64;
            self.usage.llm_calls += 1;
            // 本轮用量基线快照：服务端真实 usage（StreamChunk::Usage）到达时
            // 按「基线 + 服务端值」覆盖本轮估算；cached_tokens 为累计值直接累加
            let turn_base_input = self.usage.input_tokens;
            let turn_base_output = self.usage.output_tokens;
            let mut server_usage_seen = false;
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
                            match &chunk {
                                StreamChunk::Delta(s) => {
                                    if !self.quiet {
                                        print!("{s}");
                                        let _ = std::io::stdout().flush();
                                    }
                                    self.emit(AgentEvent::MessageUpdate(s.clone()));
                                }
                                // 思考流：不打印 stdout（保持 CLI 干净），广播事件给壳层展示，
                                // 并计入输出用量（思考 token 是计费的）
                                StreamChunk::Reasoning(s) => {
                                    self.usage.output_tokens +=
                                        crate::context::estimate_tokens(s) as u64;
                                    self.emit(AgentEvent::Thinking(s.clone()));
                                }
                                // 服务端真实用量：校正本轮估算（0 值字段不动，
                                // 兼容 Anthropic start/delta 分块上报）
                                StreamChunk::Usage {
                                    input_tokens,
                                    output_tokens,
                                    cached_tokens,
                                } => {
                                    if *input_tokens > 0 {
                                        self.usage.input_tokens =
                                            turn_base_input + input_tokens;
                                    }
                                    if *output_tokens > 0 {
                                        self.usage.output_tokens =
                                            turn_base_output + output_tokens;
                                    }
                                    self.usage.cached_tokens += cached_tokens;
                                    server_usage_seen = true;
                                }
                                _ => {}
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
            // 空响应守卫（v0.5.3）：流被提前掐断时正文与工具调用双空。
            // 已知触发：GLM 内容审查（code 1301）在思考阶段掐流——HTTP 200、
            // reasoning_content 正常流动、无 finish_reason 直接 EOF；工具结果
            // （如新闻检索）含敏感内容时必触发，且该内容留在会话历史里会
            // 持续触发。不拦截的话回合循环会静默写空消息结束——用户侧表现为
            // "调了工具但不回复"。
            validate_turn_output(&text, &tool_calls)?;
            // 用量统计：输出 = 回复文本 + 工具调用参数
            // （服务端已上报真实 output 时跳过估算，避免双计）
            if !server_usage_seen {
                self.usage.output_tokens += crate::context::estimate_tokens(&text) as u64;
                for tc in &tool_calls {
                    self.usage.output_tokens += (crate::context::estimate_tokens(&tc.arguments)
                        + crate::context::estimate_tokens(&tc.name))
                        as u64;
                }
            }
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
                // 技能使用追踪：引用即记，成败入统计（晋升/衰退原料）
                if let Some(sk) = crate::evolution::detect_skill_reference(&call.name, &call.arguments) {
                    crate::evolution::record_skill_use(&sk, !result.starts_with("ERROR"));
                }
                // 硬信号采集：ERROR 前缀 = 工具失败；曾失败的工具成功 = 重试爬出
                if result.starts_with("ERROR") {
                    let preview_e: String = result.chars().take(160).collect();
                    self.turn_signals.tool_errors.push((call.name.clone(), preview_e));
                    self.failed_tools.insert(call.name.clone());
                } else if self.failed_tools.remove(call.name.as_str()) {
                    self.turn_signals.retries_recovered += 1;
                }
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
        if let (Some(memory), Some(embedding)) = (&self.memory, &self.embedding) {
            let session_id = self.session_id().unwrap_or("unknown");
            // query / answer 两次嵌入并行；API 后端失败时降级：本轮不存，不阻塞主流程
            let (qv, av) = tokio::join!(
                embedding.embed(user_input),
                embedding.embed(&final_text)
            );
            match (qv, av) {
                (Ok(qv), Ok(av)) => {
                    if let Err(e) = memory
                        .store(
                            session_id,
                            user_input,
                            &final_text,
                            &qv,
                            &av,
                            self.config.context.embedding.supersede_threshold,
                        )
                        .await
                    {
                        tracing::warn!("L3 记忆写入失败：{e}");
                    }
                }
                (q, a) => {
                    let err = q.err().or_else(|| a.err()).unwrap_or_default();
                    tracing::warn!("L3 嵌入计算失败（本轮不写入记忆）：{err}");
                }
            }
        }
        // 用量：UsageUpdate 事件在 Done 前发出（嵌入方可订阅），累计值落盘 JSONL
        self.emit(AgentEvent::UsageUpdate(self.usage.clone()));
        self.log_session(&SessionEntry::usage(&self.usage));
        self.emit(AgentEvent::Done {
            final_text: final_text.clone(),
        });
        if !self.quiet {
            println!("[usage] {}", self.usage_line());
        }
        // 反思钩子：Done 之后跑（用户已见到回复），教训落进化流
        let task_summary: String = user_input.chars().take(200).collect();
        // 收尾钩子 30s 上限：副业（进化）不得无限占用会话锁——
        // 8/23 大Joe 实测：回复已显示，但钩子的 LLM 调用还在锁内跑，
        // 此时发消息吃 "prompt in flight" 闭门羹
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.reflect_and_record(&task_summary),
        )
        .await;
        // 聚类检查（无条件）：种子教训可能来自历史会话，不能只靠"新教训落流"触发；
        // 幂等保护 = 同名技能已存在时 write_draft_skill 拒绝（E2E 实测发现的 gap）
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.maybe_draft_skill(),
        )
        .await;
        // 成长变化记账：目标被设定/修改、技能被沉淀 → 事件流 + git 快照
        self.record_growth_changes().await;
        Ok(final_text)
    }

    /// 组装 [usage] 行：token 统计恒有；命中注册表时追加成本估算
    fn usage_line(&self) -> String {
        let base = format!(
            "输入 {} tok · 输出 {} tok · 调用 {} 次",
            crate::models::format_tokens(self.usage.input_tokens),
            crate::models::format_tokens(self.usage.output_tokens),
            self.usage.llm_calls
        );
        match crate::models::estimate_cost(self.config.current_model(), &self.usage) {
            Some(cost) => format!("{base} · 累计成本 ≈ ¥{cost:.2}"),
            None => base, // 未知模型只显示 token 不显示价格
        }
    }

    /// steer 统一处理：保留已收到的文本部分（半截工具调用 JSON 不可用，全部丢弃），
    /// 追加中断标注和 [用户中途指令] 消息，广播 Steered 事件
    fn handle_steer(&mut self, chunks: &[StreamChunk], steer: &str) -> ModelResult<()> {
        // 用户中途转向 = 最强纠正信号（反思原料）
        self.turn_signals.steers.push(steer.to_string());
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
    ///
    /// 嵌入/检索失败（含模型签名不匹配）只告警并跳过——记忆是增强不是依赖。
    #[cfg(feature = "l3-memory")]
    async fn recall_memory(&self, user_input: &str) -> Option<crate::types::Message> {
        let memory = self.memory.as_ref()?;
        let embedding = self.embedding.as_ref()?;
        let current = self.session_id().unwrap_or("unknown");
        let qv = match embedding.embed(user_input).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("L3 查询嵌入失败（跳过记忆检索）：{e}");
                return None;
            }
        };
        let hits = match memory.search(&qv, 3, 0.30, current).await {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!("L3 记忆检索失败：{e}");
                return None;
            }
        };
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


fn truncate_str(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// 空响应守卫：正文与工具调用双空 = 流被掐断的显式错误（不静默成功）。
/// 已知触发：GLM 内容审查（1301）在思考阶段掐流——HTTP 200、reasoning 正常、
/// 无 finish_reason 直接 EOF；工具结果（如新闻检索）含敏感内容时必触发，
/// 且该内容留在会话历史会持续触发。不拦截则回合循环静默写空消息——
/// 用户侧表现为"调了工具但不回复"。
fn validate_turn_output(
    text: &str,
    tool_calls: &[ToolCall],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if text.trim().is_empty() && tool_calls.is_empty() {
        return Err(
            "模型返回空响应（正文与工具调用均为空）。常见原因：\n\
             1) 内容审查拦截（如 GLM 1301：会话上下文可能含敏感内容，例如新闻/检索结果——新建会话即可恢复，旧会话历史会持续触发）\n\
             2) 流被网络中断提前掐断（直接重试即可）"
                .into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_validate_turn_output_empty_rejected() {
        // 双空 = 掐流，必须显式报错（不静默）
        let r = validate_turn_output("", &[]);
        assert!(r.is_err(), "双空必须报错");
        let msg = r.unwrap_err().to_string();
        assert!(
            msg.contains("内容审查") && msg.contains("新建会话"),
            "错误信息要给出路：{msg}"
        );
    }

    #[test]
    fn test_validate_turn_output_valid_passes() {
        // 有正文 / 有工具调用 / 两者都有 → 均通过
        assert!(validate_turn_output("你好", &[]).is_ok());
        let call = ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        };
        assert!(validate_turn_output("", std::slice::from_ref(&call)).is_ok());
        // 纯空白正文 + 无调用 = 仍算空
        assert!(validate_turn_output("   \n  ", &[]).is_err());
    }

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
        // work_dir 也指向临时目录：避免拾取仓库根 AGENTS.md 污染 system prompt 相关断言
        config.agent.work_dir = tmp.path().to_string_lossy().to_string();
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

    #[tokio::test]
    async fn test_usage_stats_and_jsonl_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        // gate 常开：一轮直接走完（1 次模型调用）
        let (_gate_tx, gate_rx) = tokio::sync::watch::channel(true);
        let mut agent = test_agent(&tmp, gate_rx);
        let session_id = agent.session_id().unwrap().to_string();

        agent.run("统计用量").await.expect("run 应成功");
        assert!(agent.usage().input_tokens > 0);
        assert!(agent.usage().output_tokens > 0);
        assert_eq!(agent.usage().llm_calls, 1);

        // usage 记录已落盘 JSONL（每轮结束 append 累计值）
        let dir = tmp.path().to_string_lossy().to_string();
        let disk = Session::recover_usage(&dir, &session_id);
        assert_eq!(disk.llm_calls, 1);
        assert_eq!(disk.input_tokens, agent.usage().input_tokens);
        assert_eq!(disk.output_tokens, agent.usage().output_tokens);

        // 恢复会话：usage 一并恢复，再跑一轮在累计值上继续
        let mut config = Config::default_config();
        config.session.dir = dir;
        let mut agent2 = Agent::resume(config, &session_id).expect("resume 应成功");
        assert_eq!(agent2.usage().llm_calls, 1);
        let (_g2, gate_rx2) = tokio::sync::watch::channel(true);
        agent2.set_provider(Box::new(MockProvider {
            gate: gate_rx2,
            calls: Arc::new(AtomicUsize::new(1)), // 直接走"最终回复"分支
        }));
        agent2.set_quiet(true);
        agent2.run("继续").await.expect("run 应成功");
        assert_eq!(agent2.usage().llm_calls, 2);
        assert!(agent2.usage().input_tokens > disk.input_tokens);
    }

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

    /// 造一个 work_dir 指向临时目录的配置，home 用另一个临时目录隔离真实 ~/.r2
    fn prompt_fixture() -> (tempfile::TempDir, tempfile::TempDir, Config) {
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let mut config = Config::default_config();
        config.agent.work_dir = work.path().to_string_lossy().to_string();
        (home, work, config)
    }

    #[test]
    fn test_prompt_core_only() {
        // 无 SOUL / AGENTS / 自定义配置 → 只有内核核心段
        let (home, _work, config) = prompt_fixture();
        let (full, sections) = build_system_prompt_with_home(&config, &home.path().to_string_lossy());
        assert!(full.starts_with(SYSTEM_PROMPT));
        assert!(full.contains("工具使用规范"));
        assert!(!full.contains("[SOUL.md"));
        assert!(!full.contains("[AGENTS.md"));
        assert!(!full.contains("[自定义配置]"));
        // v0.7.2 起内核恒附[成长系统]段（fixture 无技能目录）：core 是前缀而非全部
        assert!(full.starts_with(sections.core.as_str()), "内核段必须是前缀");
        assert!(full.contains("[成长系统]"));
        assert!(sections.soul.is_none() && sections.agents.is_none() && sections.custom.is_none());
    }

    #[test]
    fn test_prompt_with_soul() {
        let (home, _work, config) = prompt_fixture();
        std::fs::create_dir_all(home.path().join(".r2")).unwrap();
        std::fs::write(home.path().join(".r2/SOUL.md"), "温和而坚定").unwrap();
        let (full, sections) = build_system_prompt_with_home(&config, &home.path().to_string_lossy());
        assert!(full.contains("[SOUL.md 全局人格]\n温和而坚定"));
        assert_eq!(sections.soul.as_deref(), Some("温和而坚定"));
        assert!(sections.agents.is_none());
    }

    #[test]
    fn test_prompt_with_agents() {
        let (home, work, config) = prompt_fixture();
        std::fs::write(work.path().join("AGENTS.md"), "本项目用 Rust").unwrap();
        let (full, sections) = build_system_prompt_with_home(&config, &home.path().to_string_lossy());
        assert!(full.contains("[AGENTS.md 项目上下文]\n本项目用 Rust"));
        assert_eq!(sections.agents.as_deref(), Some("本项目用 Rust"));
        assert!(sections.soul.is_none());
    }

    #[test]
    fn test_prompt_soul_then_agents_order() {
        // 两层都有：SOUL 在前，AGENTS 在后
        let (home, work, config) = prompt_fixture();
        std::fs::create_dir_all(home.path().join(".r2")).unwrap();
        std::fs::write(home.path().join(".r2/SOUL.md"), "人格").unwrap();
        std::fs::write(work.path().join("AGENTS.md"), "项目").unwrap();
        let (full, _) = build_system_prompt_with_home(&config, &home.path().to_string_lossy());
        let i_soul = full.find("[SOUL.md 全局人格]").unwrap();
        let i_agents = full.find("[AGENTS.md 项目上下文]").unwrap();
        assert!(i_soul < i_agents, "SOUL 段应在 AGENTS 段之前：{full}");
    }

    #[test]
    fn test_prompt_custom_overrides_layers() {
        // system_prompt 配置非空：只留 核心+[自定义配置]，SOUL/AGENTS 被跳过
        let (home, work, mut config) = prompt_fixture();
        std::fs::create_dir_all(home.path().join(".r2")).unwrap();
        std::fs::write(home.path().join(".r2/SOUL.md"), "人格").unwrap();
        std::fs::write(work.path().join("AGENTS.md"), "项目").unwrap();
        config.agent.system_prompt = "只听我\u{7684}".to_string();
        let (full, sections) = build_system_prompt_with_home(&config, &home.path().to_string_lossy());
        assert!(full.contains("[自定义配置]\n只听我\u{7684}"));
        assert!(!full.contains("[SOUL.md"));
        assert!(!full.contains("[AGENTS.md"));
        assert_eq!(sections.custom.as_deref(), Some("只听我\u{7684}"));
        assert!(sections.soul.is_none() && sections.agents.is_none());
    }

    #[test]
    fn test_prompt_layer_truncated_at_64kb() {
        // 超过 64KB 的文件截断并带提示
        let (home, work, config) = prompt_fixture();
        std::fs::write(work.path().join("AGENTS.md"), "a".repeat(100 * 1024)).unwrap();
        let (full, sections) = build_system_prompt_with_home(&config, &home.path().to_string_lossy());
        let agents = sections.agents.expect("应有 AGENTS 段");
        assert!(agents.len() <= MAX_LAYER_BYTES + 64, "截断后超长：{}", agents.len());
        assert!(agents.ends_with("已截断）"));
        // 成长系统段（~400B）恒定附加于 core 之后：边界放宽到 +1024 覆盖恒定段
        assert!(full.len() < sections.core.len() + MAX_LAYER_BYTES + 1024);
    }

    #[test]
    fn test_prompt_work_dir_tilde_expand() {
        // work_dir = "~/proj" 时用注入的 home 展开并读到 AGENTS.md
        let (home, _work, mut config) = prompt_fixture();
        config.agent.work_dir = "~/proj".to_string();
        std::fs::create_dir_all(home.path().join("proj")).unwrap();
        std::fs::write(home.path().join("proj/AGENTS.md"), "家目录项目").unwrap();
        let (full, sections) = build_system_prompt_with_home(&config, &home.path().to_string_lossy());
        assert!(full.contains("家目录项目"));
        assert!(sections.agents.is_some());
        // 纯函数自身的展开行为
        assert_eq!(expand_with_home("~/x", "/h"), "/h/x");
        assert_eq!(expand_with_home("~", "/h"), "/h");
        assert_eq!(expand_with_home("./rel", "/h"), "./rel");
    }

    #[test]
    fn test_agent_effective_system_prompt() {
        // Agent 构造后可通过访问器拿到拼好的全文
        let tmp = tempfile::tempdir().unwrap();
        let (_gate_tx, gate_rx) = tokio::sync::watch::channel(true);
        let agent = test_agent(&tmp, gate_rx);
        assert!(agent.effective_system_prompt().starts_with(SYSTEM_PROMPT));
        assert!(agent.effective_system_prompt().contains("工具使用规范"));
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
