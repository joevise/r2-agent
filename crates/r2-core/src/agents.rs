//! 多 Agent 档案（v0.9.0 地基）
//!
//! 一个 Agent 分身 = ~/.r2/agents/<name>/ 一个目录：
//!   AGENT.toml  档案（显示名/模型/状态/描述）
//!   SOUL.md     人格
//!   work/       工作空间（独立于主 agent）
//!   sessions/   会话历史
//!   skills/     私有技能（共享库 ~/.r2/skills 之外的个人特长）
//!
//! 信任模型（与 task 工具同款）：
//!   工具（summon）物理上只能创建 pending 档案——
//!   active 的转换（批准召唤）只能走 Console 的审批按钮（WS agent_approve），
//!   代码路径上物理隔离"造人"的开关。
//!
//! "main" 是保留名：指默认主 agent（~/.r2 根，无 persona_dir），不可创建/删除。

use serde::{Deserialize, Serialize};

/// 保留名：主 agent
pub const MAIN: &str = "main";

/// 飞书 DM 通道配置（AGENT.toml 的 [channel_feishu] 段；v0.10.0-B）。
/// 老档案无此段 → serde(default) 全部取默认（enabled=false），零影响。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelFeishu {
    /// 是否启用该 agent 的飞书机器人私聊桥
    pub enabled: bool,
    pub app_id: String,
    pub app_secret: String,
    /// 【v0.10.1 弃用】旧白名单语义：空=拒绝所有人；["*"]=开放。
    /// 仅为兼容老档案保留，读入后归一到 dm_policy；写入时不再使用
    pub allow_from: Vec<String>,
    /// DM 策略：deny_all=拒绝所有人 / allow_all=允许所有人 /
    /// allow_list=仅允许名单内 / deny_list=拒绝名单内（其余放行）
    pub dm_policy: String,
    /// 策略名单（allow_list/deny_list 用，open_id 列表）
    pub policy_list: Vec<String>,
    /// none=只发最终回复 compact=工具调用各发一行 full=思考流也发（分片）
    pub show_process: String,
}

impl Default for ChannelFeishu {
    fn default() -> Self {
        Self {
            enabled: false,
            app_id: String::new(),
            app_secret: String::new(),
            allow_from: Vec::new(),
            dm_policy: "deny_all".into(),
            policy_list: Vec::new(),
            show_process: "compact".into(),
        }
    }
}

impl ChannelFeishu {
    /// 有效的 DM 策略（allow_from 老语义归一：["*"] → allow_all，非空 → allow_list）
    pub fn effective_policy(&self) -> (&str, &[String]) {
        match self.dm_policy.as_str() {
            "allow_all" => ("allow_all", &self.policy_list),
            "allow_list" => ("allow_list", &self.policy_list),
            "deny_list" => ("deny_list", &self.policy_list),
            _ => {
                // 未设置（老档案）看 allow_from 归一
                if self.allow_from.iter().any(|x| x == "*") {
                    ("allow_all", &self.policy_list)
                } else if !self.allow_from.is_empty() {
                    ("allow_list", &self.allow_from)
                } else {
                    ("deny_all", &self.policy_list)
                }
            }
        }
    }
}

/// Agent 档案（AGENT.toml）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentProfile {
    /// 目录名（安全字符：字母数字-_，1-24 位）
    pub name: String,
    /// 显示名（如「CFO 参谋」）
    #[serde(default)]
    pub display_name: String,
    /// 模型覆盖（空 = 继承主配置当前模型）
    #[serde(default)]
    pub model: String,
    /// pending | active | rejected（工具只能造 pending）
    #[serde(default = "default_state")]
    pub state: String,
    /// 一句话职责描述（审批卡与切换器展示）
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub created_ts: u64,
    /// 飞书 DM 通道（老档案无此段 → 默认全关；读入后再写回时原样保留）
    #[serde(default)]
    pub channel_feishu: ChannelFeishu,
}

fn default_state() -> String {
    "pending".into()
}

/// agents 根目录：~/.r2/agents
pub fn agents_root() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    std::path::PathBuf::from(format!("{home}/.r2/agents"))
}

/// 档案名合法性：1-24 位字母数字-_，且不等于保留名
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 24
        && !name.eq_ignore_ascii_case(MAIN)
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// 单个档案目录
pub fn profile_dir(name: &str) -> std::path::PathBuf {
    agents_root().join(name)
}

fn toml_path(name: &str) -> std::path::PathBuf {
    profile_dir(name).join("AGENT.toml")
}

/// 列出全部档案（按名排序；目录损坏的跳过）
pub fn list_profiles() -> Vec<AgentProfile> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(agents_root()) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !valid_name(&name) {
            continue; // 非档案目录（如缓存）跳过
        }
        if let Some(p) = load_profile(&name) {
            out.push(p);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// 读单个档案（AGENT.toml 解析失败/不存在 → None）
pub fn load_profile(name: &str) -> Option<AgentProfile> {
    if !valid_name(name) {
        return None;
    }
    let content = std::fs::read_to_string(toml_path(name)).ok()?;
    let mut p: AgentProfile = toml::from_str(&content).ok()?;
    p.name = name.to_string(); // 以目录名为准，防 toml 内漂移
    Some(p)
}

/// 保存档案（AGENT.toml）
pub fn save_profile(p: &AgentProfile) -> Result<(), String> {
    if !valid_name(&p.name) {
        return Err(format!("非法档案名：{}", p.name));
    }
    let dir = profile_dir(&p.name);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录失败：{e}"))?;
    let body = toml::to_string_pretty(p).map_err(|e| e.to_string())?;
    std::fs::write(toml_path(&p.name), body).map_err(|e| e.to_string())
}

/// 起草新档案（state 恒为 pending——信任模型：工具造不出 active）。
/// 同名已存在 → 报错（先删再建）。
pub fn draft_profile(
    name: &str,
    display_name: &str,
    model: &str,
    description: &str,
    soul: &str,
) -> Result<AgentProfile, String> {
    if !valid_name(name) {
        return Err("名字必须是 1-24 位字母/数字/-/_（main 保留）".into());
    }
    if toml_path(name).exists() {
        return Err(format!("已存在同名 agent：{name}（换个名字或先删除旧的）"));
    }
    if display_name.chars().count() > 32 {
        return Err("display_name 最长 32 字".into());
    }
    if description.chars().count() > 200 {
        return Err("description 最长 200 字".into());
    }
    if soul.chars().count() > 8000 {
        return Err("SOUL 人格最长 8000 字".into());
    }
    let profile = AgentProfile {
        name: name.to_string(),
        display_name: display_name.to_string(),
        model: model.to_string(),
        state: "pending".into(),
        description: description.to_string(),
        created_ts: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        channel_feishu: ChannelFeishu::default(),
    };
    save_profile(&profile)?;
    std::fs::write(profile_dir(name).join("SOUL.md"), soul).map_err(|e| e.to_string())?;
    Ok(profile)
}

/// 状态迁移（Console 审批通道专用）：
/// approve: pending→active（同时建 work/sessions 目录）
/// reject:  pending→rejected
pub fn approve(name: &str) -> Result<AgentProfile, String> {
    let Some(mut p) = load_profile(name) else {
        return Err(format!("agent 不存在：{name}"));
    };
    if p.state != "pending" {
        return Err(format!("{} 当前状态 {}，只有 pending 可批准", p.name, p.state));
    }
    p.state = "active".into();
    save_profile(&p)?;
    let dir = profile_dir(name);
    for sub in ["work", "sessions"] {
        std::fs::create_dir_all(dir.join(sub)).map_err(|e| e.to_string())?;
    }
    Ok(p)
}

pub fn reject(name: &str) -> Result<(), String> {
    let Some(mut p) = load_profile(name) else {
        return Err(format!("agent 不存在：{name}"));
    };
    if p.state != "pending" {
        return Err(format!("{} 当前状态 {}，只有 pending 可拒绝", p.name, p.state));
    }
    p.state = "rejected".into();
    save_profile(&p)
}

/// 删除档案（整个目录）。active 状态需 force（前端二次确认）。
pub fn remove_profile(name: &str, force: bool) -> Result<(), String> {
    if !valid_name(name) {
        return Err("非法档案名".into());
    }
    if let Some(p) = load_profile(name) {
        if p.state == "active" && !force {
            return Err(format!("{} 是生效中的 agent，需确认后才能删除", p.name));
        }
    }
    let dir = profile_dir(name);
    if !dir.exists() {
        return Err(format!("agent 不存在：{name}"));
    }
    std::fs::remove_dir_all(dir).map_err(|e| format!("删除失败：{e}"))
}

/// 给 Console 的档案摘要 JSON（不含 SOUL 全文，列表够用）
pub fn profile_json(p: &AgentProfile) -> serde_json::Value {
    serde_json::json!({
        "name": p.name,
        "display_name": p.display_name,
        "model": p.model,
        "state": p.state,
        "description": p.description,
        "created_ts": p.created_ts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HOME 是进程级全局：并行测试会互相踩踏（写 A 家读 B 家）。
    /// 全仓共享锁见 crate::testutil::HOME_LOCK（agents/evolution/task_tool 同锁串行）。
    fn isolate_home() -> (tempfile::TempDir, std::sync::MutexGuard<'static, ()>) {
        let guard = crate::testutil::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", tmp.path());
        (tmp, guard)
    }

    #[test]
    fn test_valid_name() {
        assert!(valid_name("cfo"));
        assert!(valid_name("code-monkey_2"));
        assert!(!valid_name("main"));
        assert!(!valid_name("MAIN"));
        assert!(!valid_name(""));
        assert!(!valid_name("a/b"));
        assert!(!valid_name(".."));
        assert!(!valid_name(&"x".repeat(25)));
    }

    #[test]
    fn test_draft_always_pending() {
        let (_h, _g) = isolate_home();
        let p = draft_profile("cfo", "CFO 参谋", "", "管钱", "你是谨慎的 CFO。").unwrap();
        assert_eq!(p.state, "pending"); // 信任模型：起草恒 pending
        // SOUL 落盘
        let soul = std::fs::read_to_string(profile_dir("cfo").join("SOUL.md")).unwrap();
        assert!(soul.contains("CFO"));
        // 同名拒绝
        assert!(draft_profile("cfo", "x", "", "y", "z").is_err());
    }

    #[test]
    fn test_lifecycle() {
        let (_h, _g) = isolate_home();
        draft_profile("bob", "Bob", "glm-5.2-flash", "干活", "soul").unwrap();
        // 非 pending 不可重复批准
        approve("bob").unwrap();
        assert!(approve("bob").is_err());
        // active 目录齐备
        assert!(profile_dir("bob").join("work").is_dir());
        assert!(profile_dir("bob").join("sessions").is_dir());
        // active 删除需 force
        assert!(remove_profile("bob", false).is_err());
        remove_profile("bob", true).unwrap();
        assert!(!profile_dir("bob").exists());
    }

    #[test]
    fn test_reject_then_list() {
        let (_h, _g) = isolate_home();
        draft_profile("x1", "X", "", "", "").unwrap();
        draft_profile("x2", "Y", "", "", "").unwrap();
        reject("x1").unwrap();
        let list = list_profiles();
        assert_eq!(list.len(), 2);
        assert!(list.iter().any(|p| p.name == "x1" && p.state == "rejected"));
        assert!(list.iter().any(|p| p.name == "x2" && p.state == "pending"));
    }
}
