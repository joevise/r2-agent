//! TOML 配置解析模块

use serde::{Deserialize, Serialize};

/// 配置加载错误类型
pub type ConfigResult<T> = Result<T, Box<dyn std::error::Error>>;

/// 顶层配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
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
    /// 会话最大累计 token 数
    #[serde(default = "default_max_total_tokens")]
    pub max_total_tokens: usize,
    /// 工作目录
    #[serde(default = "default_work_dir")]
    pub work_dir: String,
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
}

/// 会话存储配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SessionConfig {
    /// 会话文件目录（支持 ~ 展开）
    #[serde(default = "default_session_dir")]
    pub dir: String,
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
    500_000
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
    10
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
            model: ModelConfig::default(),
            agent: AgentConfig::default(),
            context: ContextConfig::default(),
            sandbox: SandboxConfig::default(),
            session: SessionConfig::default(),
        }
    }

    /// 从 TOML 文件加载配置，并做 ~ 展开与 provider 校验
    pub fn load_from_file(path: &str) -> ConfigResult<Self> {
        let content = std::fs::read_to_string(path)?;
        let mut config: Config = toml::from_str(&content)?;
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
        Ok(())
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
        assert_eq!(config.agent.max_total_tokens, 500000);
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
    fn test_override_none_keeps_config() {
        let mut config = Config::default_config();
        let model_before = config.model.openai_compat.model.clone();
        let dir_before = config.agent.work_dir.clone();
        apply_overrides(&mut config, None, None);
        assert_eq!(config.model.openai_compat.model, model_before);
        assert_eq!(config.agent.work_dir, dir_before);
    }
}
