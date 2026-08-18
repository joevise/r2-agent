//! 进化事件流 + 反思钩子（v0.7 自进化内核）
//!
//! 设计依据（闭环逻辑拆解的结论）：
//! - 反馈是发动机：会话经历是原料，硬信号采集器把"预测误差"留下，其余蒸发
//! - 铁律（挑战2补丁）：LLM 只翻译硬信号成教训措辞，不负责判断好坏——
//!   判断已由硬信号（exit code / ERROR / 用户纠正）完成
//! - 一切进化落 evolution.jsonl（追加式，与会话 JSONL 同构）→ 可观测性原料

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 一轮会话内采集的硬信号（反思的原料）
#[derive(Debug, Default, Clone)]
pub struct TurnSignals {
    /// 工具失败：(工具名, 错误预览)
    pub tool_errors: Vec<(String, String)>,
    /// 用户中途转向（= 显式纠正）
    pub steers: Vec<String>,
    /// 同一工具失败后重试成功的次数（"踩坑后爬出来"的最强信号）
    pub retries_recovered: usize,
}

impl TurnSignals {
    pub fn is_empty(&self) -> bool {
        self.tool_errors.is_empty() && self.steers.is_empty() && self.retries_recovered == 0
    }

    /// 信号摘要（反思 prompt 用；不含敏感参数，只取预览）
    fn summary(&self) -> String {
        let mut s = String::new();
        for (name, err) in self.tool_errors.iter().take(6) {
            s.push_str(&format!("- 工具 {name} 失败：{}\n", err.chars().take(120).collect::<String>()));
        }
        for st in self.steers.iter().take(4) {
            s.push_str(&format!("- 用户中途纠正：「{}」\n", st.chars().take(120).collect::<String>()));
        }
        if self.retries_recovered > 0 {
            s.push_str(&format!("- 失败后重试成功 {} 次\n", self.retries_recovered));
        }
        s
    }
}

/// 进化事件（一条 = 一次可观测的自我改变）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionEvent {
    /// Unix 秒
    pub ts: u64,
    /// 事件类型：lesson（教训）/ skill_draft / skill_promoted / goal_set / mcp_installed / decay
    pub kind: String,
    /// 事件内容（如教训文本）
    pub content: String,
    /// 证据（硬信号引用）
    pub evidence: String,
    /// 触发来源会话
    pub session_id: String,
}

/// 进化事件流文件：~/.r2/evolution.jsonl
fn evolution_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(format!("{home}/.r2/evolution.jsonl"))
}

/// 追加一条进化事件（不可变流水，只增不改）
pub fn append_event(event: &EvolutionEvent) -> Result<(), String> {
    use std::io::Write;
    let path = evolution_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let line = serde_json::to_string(event).map_err(|e| e.to_string())?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("打开进化事件流失败：{e}"))?;
    writeln!(f, "{line}").map_err(|e| e.to_string())
}

/// 读最近 N 条事件（时间正序，最新的在尾部）
pub fn read_events(limit: usize) -> Vec<EvolutionEvent> {
    let path = evolution_path();
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let mut events: Vec<EvolutionEvent> = content
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    if events.len() > limit {
        events = events.split_off(events.len() - limit);
    }
    events
}

/// 反思 prompt：把 LLM 钉死在"翻译器"角色上（防自我吹捧式假学习）
///
/// 铁律的 prompt 化：不问"学到了什么"（开放式=邀请编造），
/// 只问"这些硬信号里有没有可提取的操作性教训"。
pub fn reflection_messages(signals: &TurnSignals, task_summary: &str) -> Vec<crate::types::Message> {
    use crate::types::{Message, Role};
    let sys = "你是信号翻译器，不是评判者。给你一轮 Agent 会话中采集的硬信号（工具失败/用户纠正/重试记录）和任务摘要。\
你的唯一职责：判断信号里是否含有一条【可操作的教训】——必须具体到命令、参数、路径或流程步骤，\
能指导下次同类任务直接避开这个坑。\n\n\
规则：\n\
1. 只翻译硬信号，禁止推断、禁止美化、禁止总结优点\n\
2. 教训必须是操作性的（'用 X 时要先 Y'），不是认知性的（'要小心'）\n\
3. 信号不足以提炼教训时，只输出 NO_LESSON\n\
4. 输出格式（严格遵守）：\n\
LESSON: <一句教训，30字内>\n\
EVIDENCE: <引用哪条信号>";
    let user = format!("任务摘要：{task_summary}\n\n硬信号：\n{}", signals.summary());
    vec![
        Message { role: Role::System, content: sys.to_string(), tool_calls: None, tool_call_id: None },
        Message { role: Role::User, content: user, tool_calls: None, tool_call_id: None },
    ]
}

/// 解析反思输出：LESSON 行 + EVIDENCE 行；无教训返回 None
pub fn parse_reflection(text: &str) -> Option<(String, String)> {
    if text.contains("NO_LESSON") {
        return None;
    }
    let mut lesson = None;
    let mut evidence = None;
    for line in text.lines() {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("LESSON:") {
            let v = v.trim().trim_matches('"');
            if !v.is_empty() {
                lesson = Some(v.to_string());
            }
        } else if let Some(v) = t.strip_prefix("EVIDENCE:") {
            let v = v.trim().trim_matches('"');
            if !v.is_empty() {
                evidence = Some(v.to_string());
            }
        }
    }
    match (lesson, evidence) {
        (Some(l), Some(e)) => Some((l, e)),
        (Some(l), None) => Some((l, "（信号见事件流）".to_string())),
        _ => None,
    }
}

/// 读 GOAL.md（目标宪法；用户写，agent 只读）
pub fn read_goal() -> Option<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let content = std::fs::read_to_string(format!("{home}/.r2/GOAL.md")).ok()?;
    let t = content.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_signals_summary_and_empty() {
        let mut s = TurnSignals::default();
        assert!(s.is_empty());
        s.tool_errors.push(("bash".into(), "ERROR: exit_code=2".into()));
        assert!(!s.is_empty());
        assert!(s.summary().contains("bash"));
    }

    #[test]
    fn test_parse_reflection_formats() {
        // 正常提取
        let r = parse_reflection("LESSON: sed -i 前先备份，用 cp x x.bak\nEVIDENCE: 工具 edit 失败");
        assert_eq!(r.as_ref().unwrap().0, "sed -i 前先备份，用 cp x x.bak");
        // NO_LESSON
        assert!(parse_reflection("NO_LESSON").is_none());
        // 有 LESSON 无 EVIDENCE（宽容）
        let r2 = parse_reflection("LESSON: 路径带空格要加引号");
        assert!(r2.is_some());
        // 认知性废话不成教训格式 → None（无 LESSON 前缀）
        assert!(parse_reflection("以后要更小心").is_none());
    }

    #[test]
    fn test_evolution_event_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", tmp.path()); // 测试隔离（单线程场景）
        let ev = EvolutionEvent {
            ts: 1787000000,
            kind: "lesson".into(),
            content: "测试教训".into(),
            evidence: "exit 1".into(),
            session_id: "s-test".into(),
        };
        append_event(&ev).unwrap();
        let events = read_events(10);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].content, "测试教训");
        // HOME 恢复由进程隔离保证（测试二进制内其他测试不应依赖真实 ~/.r2）
    }
}
