//! 群会话数据模型（v0.9.1 群聊地基，纯数据层）
//!
//! 一个群 = 群会话根目录下 <id>/ 一个目录：
//!   group.json    群档案（成员表/状态/设置/轮次/token 账）
//!   stream.jsonl  共享消息流（追加式，每行一个 GroupEvent，坏行丢弃）
//!
//! 设计要点（已定稿）：
//!   轮数上限默认 2、token 预算闸默认 300000、@唤醒权只属于人、
//!   成员上限 5（含 main）、lead 委任子任务链深度默认 2。
//!
//! 本模块只含磁盘结构 + 校验 + 状态机，调度引擎在 web.rs（另任务）。
//! 全部 API 返回 Result<_, String>（"ERROR: ..." 风格），绝不 panic。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::agents;

/// 默认轮数上限
pub const DEFAULT_MAX_ROUNDS: u32 = 2;
/// 默认 token 预算
pub const DEFAULT_BUDGET_TOKENS: u64 = 300_000;
/// lead 子任务链默认剩余深度
pub const DEFAULT_TASK_DEPTH: u32 = 2;
/// 成员上限（含 main）
pub const MAX_MEMBERS: usize = 5;
/// 成员下限（含 main）
pub const MIN_MEMBERS: usize = 2;

fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 群成员：name 是分身名或 "main"（人）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Member {
    pub name: String,
    #[serde(default)]
    pub display_name: String,
    /// owner = 人（main）/ member / lead
    #[serde(default = "default_role")]
    pub role: String,
}

fn default_role() -> String {
    "member".into()
}

/// 群设置
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GroupSettings {
    /// 轮数上限（默认 2）
    #[serde(default = "default_max_rounds")]
    pub max_rounds: u32,
    /// token 预算闸（默认 300000，超限触发暂停）
    #[serde(default = "default_budget")]
    pub budget_tokens: u64,
    /// 轮流循环顺序 = 成员名列表
    #[serde(default)]
    pub turn_order: Vec<String>,
}

fn default_max_rounds() -> u32 {
    DEFAULT_MAX_ROUNDS
}
fn default_budget() -> u64 {
    DEFAULT_BUDGET_TOKENS
}

impl Default for GroupSettings {
    fn default() -> Self {
        Self {
            max_rounds: DEFAULT_MAX_ROUNDS,
            budget_tokens: DEFAULT_BUDGET_TOKENS,
            turn_order: Vec::new(),
        }
    }
}

/// 委任/讨论主题
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GroupTask {
    pub topic: String,
    /// "discussion" | "delegation"
    #[serde(default = "default_kind")]
    pub kind: String,
    /// 被委任的 lead（member 名）
    #[serde(default)]
    pub lead: Option<String>,
    /// lead 子任务链剩余深度（默认 2）
    #[serde(default = "default_depth")]
    pub depth_left: u32,
    #[serde(default)]
    pub started_ts: u64,
}

fn default_kind() -> String {
    "discussion".into()
}
fn default_depth() -> u32 {
    DEFAULT_TASK_DEPTH
}

/// 群档案（group.json）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GroupConfig {
    /// uuid v4
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub created_ts: u64,
    #[serde(default)]
    pub members: Vec<Member>,
    /// idle | discussing | paused | stopped | summarized
    #[serde(default = "default_state")]
    pub state: String,
    #[serde(default)]
    pub settings: GroupSettings,
    /// 当前轮次
    #[serde(default)]
    pub round: u32,
    /// 已耗 token
    #[serde(default)]
    pub used_tokens: u64,
    /// 当前发言者
    #[serde(default)]
    pub speaking: Option<String>,
    #[serde(default)]
    pub task: Option<GroupTask>,
}

fn default_state() -> String {
    "idle".into()
}

/// 共享消息流事件（stream.jsonl，每行一个）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GroupEvent {
    /// 普通发言，from = "user" 或成员名
    Message { from: String, text: String, ts: u64 },
    /// @提及（唤醒权只属于人，引擎层校验）
    Mention {
        from: String,
        text: String,
        ts: u64,
        mentions: Vec<String>,
    },
    /// lead 委任子任务
    Subtask {
        from: String,
        to: String,
        prompt: String,
        ts: u64,
        /// pending | approved | done
        state: String,
    },
    /// 讨论总结
    Summary { text: String, ts: u64 },
    /// 群状态迁移记录
    StateChange {
        from_state: String,
        to_state: String,
        ts: u64,
    },
    /// 错误记录（不中断流）
    Error { text: String, ts: u64 },
}

impl GroupEvent {
    pub fn message(from: &str, text: &str) -> Self {
        Self::Message {
            from: from.into(),
            text: text.into(),
            ts: now_ts(),
        }
    }

    pub fn mention(from: &str, text: &str, mentions: Vec<String>) -> Self {
        Self::Mention {
            from: from.into(),
            text: text.into(),
            ts: now_ts(),
            mentions,
        }
    }

    pub fn summary(text: &str) -> Self {
        Self::Summary {
            text: text.into(),
            ts: now_ts(),
        }
    }

    pub fn error(text: &str) -> Self {
        Self::Error {
            text: text.into(),
            ts: now_ts(),
        }
    }
}

/// 群目录：<root>/<id>
pub fn group_dir(root: &Path, id: &str) -> PathBuf {
    root.join(id)
}

fn config_path(dir: &Path) -> PathBuf {
    dir.join("group.json")
}

fn stream_path(dir: &Path) -> PathBuf {
    dir.join("stream.jsonl")
}

/// 成员名合法性：复用 agents::valid_name 语义，但 "main"（人）允许入群
fn valid_member_name(name: &str) -> bool {
    name == agents::MAIN || agents::valid_name(name)
}

/// 追加一条事件到 stream.jsonl（每行 flush；文件不存在则创建）
pub fn append_event(dir: &Path, event: &GroupEvent) -> Result<(), String> {
    use std::io::Write;
    let line =
        serde_json::to_string(event).map_err(|e| format!("ERROR: 序列化事件失败：{e}"))?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(stream_path(dir))
        .map_err(|e| format!("ERROR: 打开消息流失败：{e}"))?;
    writeln!(f, "{line}").map_err(|e| format!("ERROR: 写入消息流失败：{e}"))?;
    f.flush().map_err(|e| format!("ERROR: 刷盘失败：{e}"))
}

/// 读整条消息流（坏行/残行跳过，日志 warn）
pub fn read_stream(dir: &Path) -> Vec<GroupEvent> {
    let Ok(content) = std::fs::read_to_string(stream_path(dir)) else {
        return Vec::new();
    };
    content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            match serde_json::from_str(line) {
                Ok(e) => Some(e),
                Err(e) => {
                    eprintln!("[groups] 警告：跳过坏行（{e}）：{line}");
                    None
                }
            }
        })
        .collect()
}

/// 建群：member_names = (成员名, 显示名) 列表（含 main 计入人数，main 自动 role=owner）。
/// 校验：人数 2-5、名字合法、不重名。落盘 group.json；turn_order = 成员顺序。
pub fn create_group(
    root: &Path,
    title: &str,
    member_names: &[(&str, &str)],
) -> Result<GroupConfig, String> {
    if member_names.len() < MIN_MEMBERS {
        return Err(format!(
            "ERROR: 群成员至少 {MIN_MEMBERS} 人（含 main），当前 {} 人",
            member_names.len()
        ));
    }
    if member_names.len() > MAX_MEMBERS {
        return Err(format!(
            "ERROR: 群成员最多 {MAX_MEMBERS} 人（含 main），当前 {} 人",
            member_names.len()
        ));
    }
    let mut members: Vec<Member> = Vec::new();
    for (name, display) in member_names {
        if !valid_member_name(name) {
            return Err(format!(
                "ERROR: 非法成员名：{name}（1-24 位字母/数字/-/_，main 保留给人）"
            ));
        }
        if members.iter().any(|m| m.name == *name) {
            return Err(format!("ERROR: 成员重名：{name}"));
        }
        let role = if *name == agents::MAIN { "owner" } else { "member" };
        members.push(Member {
            name: name.to_string(),
            display_name: display.to_string(),
            role: role.into(),
        });
    }
    let g = GroupConfig {
        id: uuid::Uuid::new_v4().to_string(),
        title: title.into(),
        created_ts: now_ts(),
        members,
        state: "idle".into(),
        settings: GroupSettings {
            turn_order: member_names.iter().map(|(n, _)| n.to_string()).collect(),
            ..Default::default()
        },
        round: 0,
        used_tokens: 0,
        speaking: None,
        task: None,
    };
    let dir = group_dir(root, &g.id);
    std::fs::create_dir_all(&dir).map_err(|e| format!("ERROR: 创建群目录失败：{e}"))?;
    save_group(&dir, &g)?;
    Ok(g)
}

/// 读群档案（不存在/解析失败 → None）
pub fn load_group(dir: &Path) -> Option<GroupConfig> {
    let content = std::fs::read_to_string(config_path(dir)).ok()?;
    serde_json::from_str(&content).ok()
}

/// 写群档案（group.json，pretty JSON）
pub fn save_group(dir: &Path, g: &GroupConfig) -> Result<(), String> {
    let body =
        serde_json::to_string_pretty(g).map_err(|e| format!("ERROR: 序列化群档案失败：{e}"))?;
    std::fs::write(config_path(dir), body).map_err(|e| format!("ERROR: 写群档案失败：{e}"))
}

/// 状态机合法迁移：
///   idle → discussing
///   discussing → paused | stopped | summarized
///   paused → discussing | stopped（恢复/终止）
///   stopped / summarized 为终态
pub fn state_allowed(from: &str, to: &str) -> bool {
    matches!(
        (from, to),
        ("idle", "discussing")
            | ("discussing", "paused")
            | ("discussing", "stopped")
            | ("discussing", "summarized")
            | ("paused", "discussing")
            | ("paused", "stopped")
    )
}

/// 状态迁移（非法迁移报错；成功时向 stream 追加 state_change 事件）
pub fn set_state(dir: &Path, to: &str) -> Result<GroupConfig, String> {
    let mut g = load_group(dir).ok_or_else(|| "ERROR: 群档案不存在或损坏".to_string())?;
    if !state_allowed(&g.state, to) {
        return Err(format!("ERROR: 非法状态迁移：{} → {to}", g.state));
    }
    let from = std::mem::replace(&mut g.state, to.to_string());
    save_group(dir, &g)?;
    append_event(
        dir,
        &GroupEvent::StateChange {
            from_state: from,
            to_state: to.into(),
            ts: now_ts(),
        },
    )?;
    Ok(g)
}

/// 轮流出下一个发言者：turn_order 循环里当前 speaking 之后的成员；
/// lead 在场时 lead 最后发言；skip 指定者本轮跳过。无人可讲 → None。
pub fn next_speaker(g: &GroupConfig, skip: Option<&str>) -> Option<String> {
    let mut order = g.settings.turn_order.clone();
    if let Some(lead) = g
        .members
        .iter()
        .find(|m| m.role == "lead")
        .map(|m| m.name.clone())
    {
        if let Some(pos) = order.iter().position(|n| *n == lead) {
            let l = order.remove(pos);
            order.push(l);
        }
    }
    if order.is_empty() {
        return None;
    }
    let start = match &g.speaking {
        Some(cur) => order
            .iter()
            .position(|n| n == cur)
            .map(|i| i + 1)
            .unwrap_or(0),
        None => 0,
    };
    for k in 0..order.len() {
        let cand = &order[(start + k) % order.len()];
        if Some(cand.as_str()) == g.speaking.as_deref() {
            continue;
        }
        if Some(cand.as_str()) == skip {
            continue;
        }
        return Some(cand.clone());
    }
    None
}

/// token 加账：超预算返回 false（= 触发暂停闸）
pub fn add_tokens(g: &mut GroupConfig, n: u64) -> bool {
    g.used_tokens = g.used_tokens.saturating_add(n);
    g.used_tokens <= g.settings.budget_tokens
}

/// 委任 lead：member → lead（全群仅一个 lead；已有 lead 或成员不存在报错）
pub fn promote_lead(dir: &Path, name: &str) -> Result<GroupConfig, String> {
    let mut g = load_group(dir).ok_or_else(|| "ERROR: 群档案不存在或损坏".to_string())?;
    if g.members.iter().any(|m| m.role == "lead") {
        return Err("ERROR: 已存在 lead，先 revoke_lead 再委任".into());
    }
    let Some(m) = g.members.iter_mut().find(|m| m.name == name) else {
        return Err(format!("ERROR: 成员不存在：{name}"));
    };
    if m.role == "owner" {
        return Err("ERROR: owner（人）不能被委任为 lead".into());
    }
    m.role = "lead".into();
    save_group(dir, &g)?;
    Ok(g)
}

/// 撤委任：lead → member（没有 lead 报错；state 若 discussing 保持不变）
pub fn revoke_lead(dir: &Path) -> Result<GroupConfig, String> {
    let mut g = load_group(dir).ok_or_else(|| "ERROR: 群档案不存在或损坏".to_string())?;
    let Some(m) = g.members.iter_mut().find(|m| m.role == "lead") else {
        return Err("ERROR: 当前没有 lead".into());
    };
    m.role = "member".into();
    save_group(dir, &g)?;
    Ok(g)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建一个临时群根 + 三人群（main + 两个分身），返回 (root, config)
    fn make_group() -> (tempfile::TempDir, GroupConfig) {
        let root = tempfile::tempdir().unwrap();
        let g = create_group(
            root.path(),
            "评审会",
            &[("main", "主人"), ("cfo", "CFO"), ("cto", "CTO")],
        )
        .unwrap();
        (root, g)
    }

    #[test]
    fn test_create_group_roundtrip() {
        let (root, g) = make_group();
        assert_eq!(g.state, "idle");
        assert_eq!(g.settings.max_rounds, DEFAULT_MAX_ROUNDS);
        assert_eq!(g.settings.budget_tokens, DEFAULT_BUDGET_TOKENS);
        assert_eq!(g.settings.turn_order, vec!["main", "cfo", "cto"]);
        assert_eq!(g.members[0].role, "owner"); // main = 人
        assert_eq!(g.members[1].role, "member");
        assert_eq!(g.round, 0);
        assert_eq!(g.used_tokens, 0);
        assert!(g.speaking.is_none());
        assert!(g.task.is_none());
        // 落盘 roundtrip
        let loaded = load_group(&group_dir(root.path(), &g.id)).unwrap();
        assert_eq!(loaded, g);
    }

    #[test]
    fn test_create_member_count_limits() {
        let root = tempfile::tempdir().unwrap();
        // 少于 2 人
        assert!(create_group(root.path(), "t", &[("main", "主人")]).is_err());
        // 多于 5 人
        let six: Vec<(&str, &str)> = vec![
            ("main", "m"),
            ("a1", ""),
            ("a2", ""),
            ("a3", ""),
            ("a4", ""),
            ("a5", ""),
        ];
        assert!(create_group(root.path(), "t", &six).is_err());
        // 恰好 5 人合法
        let five = &six[..5];
        assert!(create_group(root.path(), "t", five).is_ok());
    }

    #[test]
    fn test_create_duplicate_rejected() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            create_group(root.path(), "t", &[("main", "m"), ("cfo", "a"), ("cfo", "b")]).is_err()
        );
    }

    #[test]
    fn test_create_name_rules() {
        let root = tempfile::tempdir().unwrap();
        // 非法字符
        assert!(create_group(root.path(), "t", &[("main", "m"), ("a/b", "x")]).is_err());
        // main 允许入群（agents::valid_name 拒绝 main，群里放开）
        assert!(create_group(root.path(), "t", &[("main", "m"), ("cfo", "x")]).is_ok());
    }

    #[test]
    fn test_state_machine_legal_chain() {
        let (root, g) = make_group();
        let dir = group_dir(root.path(), &g.id);
        let g = set_state(&dir, "discussing").unwrap();
        assert_eq!(g.state, "discussing");
        let g = set_state(&dir, "paused").unwrap();
        assert_eq!(g.state, "paused");
        // paused 可恢复
        let g = set_state(&dir, "discussing").unwrap();
        assert_eq!(g.state, "discussing");
        let g = set_state(&dir, "stopped").unwrap();
        assert_eq!(g.state, "stopped");
        // discussing → summarized 也合法
        let (root2, g2) = make_group();
        let dir2 = group_dir(root2.path(), &g2.id);
        set_state(&dir2, "discussing").unwrap();
        assert_eq!(set_state(&dir2, "summarized").unwrap().state, "summarized");
        // paused → stopped 合法
        let (root3, g3) = make_group();
        let dir3 = group_dir(root3.path(), &g3.id);
        set_state(&dir3, "discussing").unwrap();
        set_state(&dir3, "paused").unwrap();
        assert_eq!(set_state(&dir3, "stopped").unwrap().state, "stopped");
    }

    #[test]
    fn test_state_machine_illegal_rejected() {
        let (root, g) = make_group();
        let dir = group_dir(root.path(), &g.id);
        // idle 只能 → discussing
        assert!(set_state(&dir, "paused").is_err());
        assert!(set_state(&dir, "stopped").is_err());
        assert!(set_state(&dir, "summarized").is_err());
        assert!(set_state(&dir, "idle").is_err()); // 同态拒绝
        set_state(&dir, "discussing").unwrap();
        assert!(set_state(&dir, "idle").is_err());
        set_state(&dir, "stopped").unwrap();
        // 终态不可再迁
        assert!(set_state(&dir, "discussing").is_err());
        // summarized 终态
        let (root2, g2) = make_group();
        let dir2 = group_dir(root2.path(), &g2.id);
        set_state(&dir2, "discussing").unwrap();
        set_state(&dir2, "summarized").unwrap();
        assert!(set_state(&dir2, "idle").is_err());
    }

    #[test]
    fn test_state_change_logged_to_stream() {
        let (root, g) = make_group();
        let dir = group_dir(root.path(), &g.id);
        set_state(&dir, "discussing").unwrap();
        let events = read_stream(&dir);
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            GroupEvent::StateChange {
                from_state: "idle".into(),
                to_state: "discussing".into(),
                ts: match events[0] {
                    GroupEvent::StateChange { ts, .. } => ts,
                    _ => unreachable!(),
                },
            }
        );
    }

    #[test]
    fn test_next_speaker_cycles() {
        let (_root, mut g) = make_group();
        // 无人发言 → 队首
        assert_eq!(next_speaker(&g, None).as_deref(), Some("main"));
        g.speaking = Some("main".into());
        assert_eq!(next_speaker(&g, None).as_deref(), Some("cfo"));
        g.speaking = Some("cfo".into());
        assert_eq!(next_speaker(&g, None).as_deref(), Some("cto"));
        // 队尾循环回队首
        g.speaking = Some("cto".into());
        assert_eq!(next_speaker(&g, None).as_deref(), Some("main"));
        // speaking 不在 turn_order → 从队首起
        g.speaking = Some("ghost".into());
        assert_eq!(next_speaker(&g, None).as_deref(), Some("main"));
    }

    #[test]
    fn test_next_speaker_skip() {
        let (_root, mut g) = make_group();
        g.speaking = Some("main".into());
        // 跳过下一个 → 落到再下一个
        assert_eq!(next_speaker(&g, Some("cfo")).as_deref(), Some("cto"));
        // 全员跳过（skip + speaking 覆盖两人之外只剩一个可选）
        g.speaking = Some("cfo".into());
        assert_eq!(next_speaker(&g, Some("cto")).as_deref(), Some("main"));
        // 单人无效循环：speaking 与 skip 覆盖全部 → None
        g.settings.turn_order = vec!["cfo".into()];
        g.speaking = Some("cfo".into());
        assert_eq!(next_speaker(&g, None), None);
    }

    #[test]
    fn test_next_speaker_lead_last() {
        let (root, g) = make_group();
        let dir = group_dir(root.path(), &g.id);
        promote_lead(&dir, "cfo").unwrap();
        let mut g = load_group(&dir).unwrap();
        // lead=cfo 被挪到队尾：顺序 main → cto → cfo
        g.speaking = None;
        assert_eq!(next_speaker(&g, None).as_deref(), Some("main"));
        g.speaking = Some("main".into());
        assert_eq!(next_speaker(&g, None).as_deref(), Some("cto"));
        g.speaking = Some("cto".into());
        assert_eq!(next_speaker(&g, None).as_deref(), Some("cfo"));
        g.speaking = Some("cfo".into());
        assert_eq!(next_speaker(&g, None).as_deref(), Some("main"));
    }

    #[test]
    fn test_budget_gate() {
        let (_root, mut g) = make_group();
        assert!(add_tokens(&mut g, 100_000));
        assert_eq!(g.used_tokens, 100_000);
        // 顶到预算边缘仍放行（<=）
        assert!(add_tokens(&mut g, 200_000));
        assert_eq!(g.used_tokens, 300_000);
        // 超 1 个 token 触发闸
        assert!(!add_tokens(&mut g, 1));
        assert_eq!(g.used_tokens, 300_001);
    }

    #[test]
    fn test_lead_promote_and_revoke() {
        let (root, g) = make_group();
        let dir = group_dir(root.path(), &g.id);
        // 提升
        let g = promote_lead(&dir, "cfo").unwrap();
        assert_eq!(g.members.iter().find(|m| m.name == "cfo").unwrap().role, "lead");
        // 重复提升报错（已存在 lead）
        assert!(promote_lead(&dir, "cto").is_err());
        assert!(promote_lead(&dir, "cfo").is_err());
        // owner 不可委任
        let (root2, g2) = make_group();
        let dir2 = group_dir(root2.path(), &g2.id);
        assert!(promote_lead(&dir2, "main").is_err());
        // 不存在的成员
        assert!(promote_lead(&dir, "ghost").is_err());
        // 撤销
        let g = revoke_lead(&dir).unwrap();
        assert_eq!(g.members.iter().find(|m| m.name == "cfo").unwrap().role, "member");
        // 无 lead 撤销报错
        assert!(revoke_lead(&dir).is_err());
    }

    #[test]
    fn test_lead_revoke_keeps_discussing_state() {
        let (root, g) = make_group();
        let dir = group_dir(root.path(), &g.id);
        set_state(&dir, "discussing").unwrap();
        promote_lead(&dir, "cfo").unwrap();
        let g = revoke_lead(&dir).unwrap();
        assert_eq!(g.state, "discussing"); // 撤 lead 不动状态
    }

    #[test]
    fn test_stream_append_and_read() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        // 不存在时读 = 空
        assert!(read_stream(dir).is_empty());
        append_event(dir, &GroupEvent::message("user", "大家好")).unwrap();
        append_event(dir, &GroupEvent::mention("user", "@cfo 报数", vec!["cfo".into()])).unwrap();
        append_event(
            dir,
            &GroupEvent::Subtask {
                from: "cfo".into(),
                to: "analyst".into(),
                prompt: "拉数据".into(),
                ts: 1,
                state: "pending".into(),
            },
        )
        .unwrap();
        append_event(dir, &GroupEvent::summary("已对齐")).unwrap();
        append_event(dir, &GroupEvent::error("模型超时")).unwrap();
        let events = read_stream(dir);
        assert_eq!(events.len(), 5);
        assert_eq!(
            events[0],
            GroupEvent::Message {
                from: "user".into(),
                text: "大家好".into(),
                ts: match events[0] {
                    GroupEvent::Message { ts, .. } => ts,
                    _ => unreachable!(),
                },
            }
        );
        match &events[1] {
            GroupEvent::Mention { mentions, .. } => assert_eq!(mentions, &vec!["cfo".to_string()]),
            other => panic!("期望 mention，实际 {other:?}"),
        }
        match &events[2] {
            GroupEvent::Subtask { state, .. } => assert_eq!(state, "pending"),
            other => panic!("期望 subtask，实际 {other:?}"),
        }
    }

    #[test]
    fn test_stream_bad_line_recovery() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        append_event(dir, &GroupEvent::message("user", "第一条")).unwrap();
        // 手动塞入坏行（模拟崩溃残行/手改历史）
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("stream.jsonl"))
            .unwrap();
        writeln!(f, "{{not-json").unwrap();
        writeln!(f).unwrap();
        append_event(dir, &GroupEvent::message("cfo", "第三条")).unwrap();
        let events = read_stream(dir);
        assert_eq!(events.len(), 2); // 坏行与空行被丢弃
        match &events[1] {
            GroupEvent::Message { from, .. } => assert_eq!(from, "cfo"),
            other => panic!("期望 message，实际 {other:?}"),
        }
    }
}
