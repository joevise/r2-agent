//! TOML 配置解析模块

use serde::{Deserialize, Serialize};

/// 配置加载错误类型
pub type ConfigResult<T> = Result<T, Box<dyn std::error::Error>>;

/// 顶层配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// 本配置的来源文件路径（运行时元数据：mcp 工具写回/Console 热刷新用）
    #[serde(skip)]
    pub source_path: Option<String>,
    /// 模型配置
    #[serde(default)]
    pub model: ModelConfig,
    /// Agent 运行参数
    #[serde(default)]
    pub agent: AgentConfig,
    /// 上下文管理配置
    #[serde(default)]
    pub context: ContextConfig,
    /// 沙箱配置
    #[serde(default)]
    pub sandbox: SandboxConfig,
    /// 会话存储配置
    #[serde(default)]
    pub session: SessionConfig,
    /// MCP 外部工具服务器配置
    #[serde(default)]
    pub mcp: McpConfig,
}

/// 模型配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelConfig {
    /// 提供商：openai_compat | anthropic
    #[serde(default = "default_provider")]
    pub provider: String,
    /// OpenAI 兼容接口配置
    #[serde(default)]
    pub openai_compat: OpenAiCompatConfig,
    /// Anthropic 接口配置
    #[serde(default)]
    pub anthropic: AnthropicConfig,
}

/// OpenAI 兼容接口配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OpenAiCompatConfig {
    /// API 基础地址
    #[serde(default = "default_openai_base_url")]
    pub base_url: String,
    /// API 密钥
    #[serde(default)]
    pub api_key: String,
    /// 模型名称
    #[serde(default = "default_openai_model")]
    pub model: String,
}

/// Anthropic 接口配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AnthropicConfig {
    /// API 基础地址
    #[serde(default = "default_anthropic_base_url")]
    pub base_url: String,
    /// API 密钥
    #[serde(default)]
    pub api_key: String,
    /// 模型名称
    #[serde(default = "default_anthropic_model")]
    pub model: String,
}

/// Agent 运行参数
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentConfig {
    /// 单轮对话最大循环次数
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,
    /// 会话最大累计 token 数。0 = 自动（按 models 注册表推导当前模型的上下文窗口）
    #[serde(default = "default_max_total_tokens")]
    pub max_total_tokens: usize,
    /// 工作目录
    #[serde(default = "default_work_dir")]
    pub work_dir: String,
    /// 自定义 system prompt：非空时显式覆盖 SOUL.md / AGENTS.md 两层（默认空）
    #[serde(default)]
    pub system_prompt: String,
}

/// 上下文管理配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContextConfig {
    /// L1 压缩触发阈值（占上下文窗口比例）
    #[serde(default = "default_l1_threshold")]
    pub l1_threshold: f64,
    /// L2 摘要使用的模型
    #[serde(default = "default_l2_summary_model")]
    pub l2_summary_model: String,
    /// 是否启用 L3 跨会话记忆
    #[serde(default)]
    pub l3_enabled: bool,
    /// L3 嵌入后端：hash（默认，零依赖）| api（OpenAI 兼容 embedding API）
    #[serde(default = "default_l3_embedding")]
    pub l3_embedding: String,
    /// API 嵌入后端配置（l3_embedding = "api" 时生效）
    #[serde(default)]
    pub embedding: EmbeddingConfig,
}

/// API 嵌入后端配置（OpenAI 兼容 /embeddings 协议）
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EmbeddingConfig {
    /// API 基础地址（如 https://open.bigmodel.cn/api/paas/v4）
    #[serde(default)]
    pub base_url: String,
    /// API 密钥
    #[serde(default)]
    pub api_key: String,
    /// 嵌入模型名（如 embedding-3）
    #[serde(default)]
    pub model: String,
    /// 同主题覆盖阈值：新记忆与旧记忆相似度超过该值时，旧记忆标记为被覆盖。
    /// hash 后端语义弱，0.92 几乎只在字面全同时命中；语义 API 后端才会真正触发。
    #[serde(default = "default_supersede_threshold")]
    pub supersede_threshold: f64,
}

/// 沙箱配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SandboxConfig {
    /// 沙箱级别：off | container | strict
    #[serde(default = "default_sandbox_level")]
    pub level: String,
    /// bash 命令超时（秒）
    #[serde(default = "default_bash_timeout")]
    pub bash_timeout_secs: u64,
    /// 最大进程数
    #[serde(default = "default_max_processes")]
    pub max_processes: usize,
    /// 最大内存（MB）
    #[serde(default = "default_max_memory_mb")]
    pub max_memory_mb: usize,
    /// CPU 时间上限（秒，RLIMIT_CPU）
    #[serde(default = "default_cpu_time_secs")]
    pub cpu_time_secs: u32,
    /// 单文件写入上限（MB，RLIMIT_FSIZE，防写爆磁盘）
    #[serde(default = "default_max_file_size_mb")]
    pub max_file_size_mb: u32,
    /// cgroup v2 pids 限制开关（默认开；无 root / 非 v2 / 只读时自动降级回 rlimits）
    #[serde(default = "default_cgroup")]
    pub cgroup: bool,
    /// bash 高危命令启发式拦截（默认关，向后兼容；开启后拦截 rm -rf / 等明显逃逸模式）
    #[serde(default)]
    pub bash_restrict_workdir: bool,
}

/// 会话存储配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SessionConfig {
    /// 会话文件目录（支持 ~ 展开）
    #[serde(default = "default_session_dir")]
    pub dir: String,
}

/// MCP 配置：要连接的外部 MCP server 列表（默认空，向后兼容）
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct McpConfig {
    /// MCP server 列表
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

/// 单个 MCP server（stdio 传输：command + args 起子进程）
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpServerConfig {
    /// server 名（用于工具名前缀 mcp_{name}_{tool}）
    pub name: String,
    /// 启动命令（如 npx / uvx）
    pub command: String,
    /// 命令参数
    #[serde(default)]
    pub args: Vec<String>,
}

fn default_provider() -> String {
    "openai_compat".to_string()
}
fn default_openai_base_url() -> String {
    "https://api.openai.com/v1".to_string()
}
fn default_openai_model() -> String {
    "gpt-4o".to_string()
}
fn default_anthropic_base_url() -> String {
    "https://api.anthropic.com".to_string()
}
fn default_anthropic_model() -> String {
    "claude-sonnet-4-20250514".to_string()
}
fn default_max_turns() -> usize {
    50
}
fn default_max_total_tokens() -> usize {
    // 0 = 自动：post_process / Agent 构造时按 models 注册表推导窗口预算
    0
}
fn default_work_dir() -> String {
    ".".to_string()
}
fn default_l1_threshold() -> f64 {
    0.7
}
fn default_l2_summary_model() -> String {
    "gpt-4o-mini".to_string()
}
fn default_l3_embedding() -> String {
    "hash".to_string()
}
fn default_supersede_threshold() -> f64 {
    0.92
}
fn default_sandbox_level() -> String {
    "container".to_string()
}
fn default_bash_timeout() -> u64 {
    30
}
fn default_max_processes() -> usize {
    // 0 = 不设 RLIMIT_NPROC。默认关闭：RLIMIT_NPROC 按真实 UID 全部线程计数，
    // 桌面/共享 uid 机器上（飞书/Cursor 等 GUI 线程数轻易上千）设任何小值都会让
    // bash 工具 fork 全部失败。仅 r2 独占 uid 的容器部署建议显式设 64-256。
    0
}
fn default_max_memory_mb() -> usize {
    512
}
fn default_cpu_time_secs() -> u32 {
    60
}
fn default_max_file_size_mb() -> u32 {
    100
}
fn default_cgroup() -> bool {
    // 默认开：cgroup v2 可用时硬限 pids（fork 炸弹真空填补）；不可用自动降级不失败
    true
}
fn default_session_dir() -> String {
    "~/.r2/sessions".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self::default_config()
    }
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            openai_compat: OpenAiCompatConfig::default(),
            anthropic: AnthropicConfig::default(),
        }
    }
}

impl Default for OpenAiCompatConfig {
    fn default() -> Self {
        Self {
            base_url: default_openai_base_url(),
            api_key: String::new(),
            model: default_openai_model(),
        }
    }
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            base_url: default_anthropic_base_url(),
            api_key: String::new(),
            model: default_anthropic_model(),
        }
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_turns: default_max_turns(),
            max_total_tokens: default_max_total_tokens(),
            work_dir: default_work_dir(),
            system_prompt: String::new(),
        }
    }
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            l1_threshold: default_l1_threshold(),
            l2_summary_model: default_l2_summary_model(),
            l3_enabled: false,
            l3_embedding: default_l3_embedding(),
            embedding: EmbeddingConfig::default(),
        }
    }
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            supersede_threshold: default_supersede_threshold(),
        }
    }
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            level: default_sandbox_level(),
            bash_timeout_secs: default_bash_timeout(),
            max_processes: default_max_processes(),
            max_memory_mb: default_max_memory_mb(),
            cpu_time_secs: default_cpu_time_secs(),
            max_file_size_mb: default_max_file_size_mb(),
            cgroup: default_cgroup(),
            bash_restrict_workdir: false,
        }
    }
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            dir: default_session_dir(),
        }
    }
}

/// 展开路径开头的 ~ 为用户主目录
pub fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{}/{}", home, rest);
        }
    } else if path == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return home;
        }
    }
    path.to_string()
}

impl Config {
    /// 生成默认配置
    pub fn default_config() -> Self {
        Self {
            source_path: None,
            model: ModelConfig::default(),
            agent: AgentConfig::default(),
            context: ContextConfig::default(),
            sandbox: SandboxConfig::default(),
            session: SessionConfig::default(),
            mcp: McpConfig::default(),
        }
    }

    /// 从 TOML 文件加载配置，并做 ~ 展开与 provider 校验
    pub fn load_from_file(path: &str) -> ConfigResult<Self> {
        let content = std::fs::read_to_string(path)?;
        let mut config: Config = toml::from_str(&content)?;
        config.source_path = Some(path.to_string());
        config.post_process()?;
        Ok(config)
    }

    /// 后处理：~ 展开 + 字段校验
    fn post_process(&mut self) -> ConfigResult<()> {
        if self.model.provider != "openai_compat" && self.model.provider != "anthropic" {
            return Err(format!(
                "非法 provider: \"{}\"，仅支持 \"openai_compat\" 或 \"anthropic\"",
                self.model.provider
            )
            .into());
        }
        crate::sandbox::SandboxLevel::parse(&self.sandbox.level)?;
        self.session.dir = expand_tilde(&self.session.dir);
        self.agent.work_dir = expand_tilde(&self.agent.work_dir);
        self.resolve_auto_budget();
        Ok(())
    }

    /// 窗口自动预算：max_total_tokens == 0 时按 models 注册表推导。
    /// 幂等：显式设置过（非 0）时不做任何事。
    pub fn resolve_auto_budget(&mut self) {
        if self.agent.max_total_tokens != 0 {
            return;
        }
        let model = self.current_model().to_string();
        match crate::models::lookup(&model) {
            Some(info) => {
                // L1 预算 = 上下文窗口；压缩线由 context.l1_threshold 控制
                self.agent.max_total_tokens = info.context_window;
            }
            None => {
                self.agent.max_total_tokens = 128_000;
                tracing::warn!(
                    "未知模型 {model}，预算默认 128K，建议 config 显式设置 max_total_tokens 或用 `r2 models` 查看支持列表"
                );
            }
        }
    }

    /// 当前 provider 生效的模型名
    pub fn current_model(&self) -> &str {
        match self.model.provider.as_str() {
            "anthropic" => &self.model.anthropic.model,
            _ => &self.model.openai_compat.model,
        }
    }

    /// 供测试使用：从 TOML 字符串解析
    #[cfg(test)]
    pub fn load_from_str(content: &str) -> ConfigResult<Self> {
        let mut config: Config = toml::from_str(content)?;
        config.post_process()?;
        Ok(config)
    }
}

/// 应用命令行覆盖项：model 作用于当前 provider，work_dir 覆盖并展开 ~
pub fn apply_overrides(config: &mut Config, model: Option<&str>, work_dir: Option<&str>) {
    if let Some(model) = model {
        match config.model.provider.as_str() {
            "anthropic" => config.model.anthropic.model = model.to_string(),
            _ => config.model.openai_compat.model = model.to_string(),
        }
    }
    if let Some(dir) = work_dir {
        config.agent.work_dir = expand_tilde(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_TOML: &str = r#"
[model]
provider = "openai_compat"

[model.openai_compat]
base_url = "https://open.bigmodel.cn/api/coding/paas/v4"
api_key = "sk-xxx"
model = "glm-5.2"

[agent]
max_turns = 50
max_total_tokens = 500000
work_dir = "."

[context]
l1_threshold = 0.7
l2_summary_model = "glm-4.5-flash"
l3_enabled = false

[sandbox]
level = "container"
bash_timeout_secs = 30
max_processes = 10
max_memory_mb = 512
cpu_time_secs = 60
max_file_size_mb = 100

[session]
dir = "~/.r2/sessions"
"#;

    #[test]
    fn test_default_config() {
        let config = Config::default_config();
        assert_eq!(config.model.provider, "openai_compat");
        assert_eq!(config.agent.max_turns, 50);
        assert_eq!(config.sandbox.level, "container");
    }

    #[test]
    fn test_parse_full_toml() {
        let config = Config::load_from_str(FULL_TOML).unwrap();
        assert_eq!(config.model.provider, "openai_compat");
        assert_eq!(
            config.model.openai_compat.base_url,
            "https://open.bigmodel.cn/api/coding/paas/v4"
        );
        assert_eq!(config.model.openai_compat.api_key, "sk-xxx");
        assert_eq!(config.model.openai_compat.model, "glm-5.2");
        assert_eq!(config.agent.max_turns, 50);
        assert_eq!(config.agent.max_total_tokens, 500000);
        assert!((config.context.l1_threshold - 0.7).abs() < f64::EPSILON);
        assert_eq!(config.context.l2_summary_model, "glm-4.5-flash");
        assert!(!config.context.l3_enabled);
        assert_eq!(config.sandbox.bash_timeout_secs, 30);
        assert_eq!(config.sandbox.max_processes, 10);
        assert_eq!(config.sandbox.max_memory_mb, 512);
        assert_eq!(config.sandbox.cpu_time_secs, 60);
        assert_eq!(config.sandbox.max_file_size_mb, 100);
    }

    #[test]
    fn test_auto_budget_known_model() {
        // 不显式设置 max_total_tokens（默认 0=自动）：mock 模型 → 窗口 8192
        let toml_str = r#"
[model.openai_compat]
model = "mock"
"#;
        let config = Config::load_from_str(toml_str).unwrap();
        assert_eq!(config.agent.max_total_tokens, 8_192);
    }

    #[test]
    fn test_auto_budget_unknown_model_fallback() {
        let toml_str = r#"
[model.openai_compat]
model = "no-such-model-xyz"
"#;
        let config = Config::load_from_str(toml_str).unwrap();
        assert_eq!(config.agent.max_total_tokens, 128_000);
    }

    #[test]
    fn test_auto_budget_explicit_not_overridden() {
        // 显式设置不受自动推导影响（FULL_TOML 模型是 glm-5.2，窗口 200K，显式 500K 保留）
        let config = Config::load_from_str(FULL_TOML).unwrap();
        assert_eq!(config.agent.max_total_tokens, 500_000);
        // 幂等：再调一次也不变
        let mut config = config;
        config.resolve_auto_budget();
        assert_eq!(config.agent.max_total_tokens, 500_000);
    }

    #[test]
    fn test_invalid_sandbox_level() {
        let toml_str = FULL_TOML.replace("level = \"container\"", "level = \"docker\"");
        let result = Config::load_from_str(&toml_str);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("docker"));
    }

    #[test]
    fn test_tilde_expansion() {
        let config = Config::load_from_str(FULL_TOML).unwrap();
        let home = std::env::var("HOME").unwrap();
        assert_eq!(config.session.dir, format!("{}/.r2/sessions", home));
        assert!(!config.session.dir.contains('~'));
    }

    #[test]
    fn test_invalid_provider() {
        let toml_str = FULL_TOML.replace("openai_compat\"\n", "bad_provider\"\n");
        let result = Config::load_from_str(&toml_str);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("bad_provider"));
    }

    #[test]
    fn test_embedding_config_defaults() {
        // 旧配置（无 embedding 字段）向后兼容：默认 hash 后端 + 0.92 覆盖阈值
        let config = Config::load_from_str(FULL_TOML).unwrap();
        assert_eq!(config.context.l3_embedding, "hash");
        assert!(config.context.embedding.base_url.is_empty());
        assert!(config.context.embedding.api_key.is_empty());
        assert!(config.context.embedding.model.is_empty());
        assert!((config.context.embedding.supersede_threshold - 0.92).abs() < f64::EPSILON);
    }

    #[test]
    fn test_embedding_config_parse() {
        let toml_str = r#"
[context]
l3_enabled = true
l3_embedding = "api"

[context.embedding]
base_url = "https://open.bigmodel.cn/api/paas/v4"
api_key = "sk-emb"
model = "embedding-3"
supersede_threshold = 0.95
"#;
        let config = Config::load_from_str(toml_str).unwrap();
        assert!(config.context.l3_enabled);
        assert_eq!(config.context.l3_embedding, "api");
        assert_eq!(
            config.context.embedding.base_url,
            "https://open.bigmodel.cn/api/paas/v4"
        );
        assert_eq!(config.context.embedding.api_key, "sk-emb");
        assert_eq!(config.context.embedding.model, "embedding-3");
        assert!((config.context.embedding.supersede_threshold - 0.95).abs() < f64::EPSILON);
    }

    #[test]
    fn test_missing_fields_use_defaults() {
        let minimal = r#"
[model]
provider = "anthropic"
"#;
        let config = Config::load_from_str(minimal).unwrap();
        assert_eq!(config.model.provider, "anthropic");
        // 缺失字段应使用默认值
        assert_eq!(config.agent.max_turns, 50);
        // max_total_tokens 默认 0（自动）：anthropic 默认模型不在注册表 → 兜底 128K
        assert_eq!(config.agent.max_total_tokens, 128_000);
        assert_eq!(config.sandbox.level, "container");
        assert_eq!(config.sandbox.bash_timeout_secs, 30);
        assert_eq!(
            config.model.anthropic.base_url,
            "https://api.anthropic.com"
        );
    }

    #[test]
    fn test_override_model_openai_compat() {
        let mut config = Config::default_config();
        apply_overrides(&mut config, Some("glm-5.2"), None);
        assert_eq!(config.model.openai_compat.model, "glm-5.2");
        // anthropic 侧不受影响
        assert_ne!(config.model.anthropic.model, "glm-5.2");
    }

    #[test]
    fn test_override_model_anthropic() {
        let mut config = Config::default_config();
        config.model.provider = "anthropic".to_string();
        apply_overrides(&mut config, Some("claude-opus-4"), None);
        assert_eq!(config.model.anthropic.model, "claude-opus-4");
    }

    #[test]
    fn test_override_work_dir_tilde() {
        let mut config = Config::default_config();
        apply_overrides(&mut config, None, Some("~/proj"));
        let home = std::env::var("HOME").unwrap();
        assert_eq!(config.agent.work_dir, format!("{home}/proj"));
    }

    #[test]
    fn test_sandbox_new_fields_defaults() {
        // 旧配置（无 cgroup / bash_restrict_workdir 字段）向后兼容
        let config = Config::load_from_str(FULL_TOML).unwrap();
        assert!(config.sandbox.cgroup);
        assert!(!config.sandbox.bash_restrict_workdir);
    }

    #[test]
    fn test_sandbox_new_fields_parse() {
        let toml_str = r#"
[sandbox]
cgroup = false
bash_restrict_workdir = true
"#;
        let config = Config::load_from_str(toml_str).unwrap();
        assert!(!config.sandbox.cgroup);
        assert!(config.sandbox.bash_restrict_workdir);
    }

    #[test]
    fn test_mcp_config_defaults_empty() {
        // 旧配置（无 mcp 字段）向后兼容：servers 默认为空
        let config = Config::load_from_str(FULL_TOML).unwrap();
        assert!(config.mcp.servers.is_empty());
    }

    #[test]
    fn test_mcp_config_parse() {
        let toml_str = r#"
[[mcp.servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[[mcp.servers]]
name = "fetch"
command = "uvx"
args = ["mcp-server-fetch"]
"#;
        let config = Config::load_from_str(toml_str).unwrap();
        assert_eq!(config.mcp.servers.len(), 2);
        assert_eq!(config.mcp.servers[0].name, "filesystem");
        assert_eq!(config.mcp.servers[0].command, "npx");
        assert_eq!(config.mcp.servers[0].args.len(), 3);
        assert_eq!(config.mcp.servers[1].name, "fetch");
        assert_eq!(config.mcp.servers[1].args, vec!["mcp-server-fetch"]);
    }

    #[test]
    fn test_override_none_keeps_config() {
        let mut config = Config::default_config();
        let model_before = config.model.openai_compat.model.clone();
        let dir_before = config.agent.work_dir.clone();
        apply_overrides(&mut config, None, None);
        assert_eq!(config.model.openai_compat.model, model_before);
        assert_eq!(config.agent.work_dir, dir_before);
    }
}
