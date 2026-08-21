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
        // 改 HOME 的测试必须持全仓共享锁（防并行踩踏）
        let _guard = crate::testutil::HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", tmp.path()); // 测试隔离
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

// ═══════════════════ v0.7 阶段2/3：版本化自我 · 使用记录 · 衰退 · 晋升 ═══════════════════

/// ~/.r2 的技能使用记录：~/.r2/skill_usage.json
/// {"技能名": {"count": 累计, "success": 成功, "fail": 失败, "last_used": ts}}
fn usage_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(format!("{home}/.r2/skill_usage.json"))
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SkillUseStat {
    pub count: u64,
    pub success: u64,
    pub fail: u64,
    pub last_used: u64,
}

/// 读全量使用记录
pub fn read_usage() -> std::collections::HashMap<String, SkillUseStat> {
    std::fs::read_to_string(usage_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_usage(u: &std::collections::HashMap<String, SkillUseStat>) {
    if let Ok(s) = serde_json::to_string_pretty(u) {
        let _ = std::fs::write(usage_path(), s);
    }
}

/// 衰退阈值：90 天未使用
const DECAY_DAYS: u64 = 90;

/// 记录一次技能使用（agent 检测到引用即记；成功与否由调用方给）
pub fn record_skill_use(name: &str, ok: bool) {
    let mut all = read_usage();
    let e = all.entry(name.to_string()).or_default();
    e.count += 1;
    if ok { e.success += 1 } else { e.fail += 1 }
    e.last_used = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    write_usage(&all);
    // 晋升检查（trial 技能统计达标 → 转正；失败过多 → 归档）
    check_promotion(name);
}

/// 从工具调用的参数里检测技能引用：匹配 "skills/<name>/SKILL.md" 模式
/// （v0.6.0 使用约定是 bash cat，read 相对路径也命中）
pub fn detect_skill_reference(tool_name: &str, arguments: &str) -> Option<String> {
    if tool_name != "bash" && tool_name != "read" {
        return None;
    }
    let mut rest = arguments;
    while let Some(pos) = rest.find("skills/") {
        let after = &rest[pos + 7..]; // "skills/" 7 字节（off-by-one 实测教训）
        // 取到下一个 '/' 的段作为技能名
        if let Some(slash) = after.find('/') {
            let name = &after[..slash];
            let valid = !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if valid && after[slash..].starts_with("/SKILL.md") {
                return Some(name.to_string());
            }
        }
        rest = &rest[pos + 7..];
    }
    None
}

/// 技能 frontmatter 里的 x-r2-status 字段（auto 技能生命周期）：
/// trial（合成试用）→ promoted（转正）/ archived（归档，目录移走）
fn skill_status_path(name: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let p = PathBuf::from(format!("{home}/.r2/skills/{name}/SKILL.md"));
    p.exists().then_some(p)
}

/// 读技能 frontmatter 的 x-r2-status（无字段 = 手写技能）
pub fn read_skill_status(name: &str) -> Option<String> {
    let path = skill_status_path(name)?;
    let content = std::fs::read_to_string(path).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    if lines.first().map(|l| l.trim()) != Some(&"---") {
        return None;
    }
    let end = lines[1..].iter().position(|l| l.trim() == "---")? + 1;
    for l in &lines[1..end] {
        if let Some(v) = l.trim().strip_prefix("x-r2-status:") {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// 改写技能 frontmatter 的 x-r2-status（晋升用）
fn write_skill_status(name: &str, status: &str) -> Result<(), String> {
    let path = skill_status_path(name).ok_or("技能文件不存在")?;
    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let lines: Vec<&str> = content.lines().collect();
    let mut out = String::new();
    let mut in_fm = false;
    let mut replaced = false;
    for l in &lines {
        let t = l.trim();
        if t == "---" && !in_fm {
            in_fm = true;
            out.push_str(l);
            out.push('\n');
            continue;
        }
        if in_fm {
            if t.starts_with("x-r2-status:") {
                out.push_str(&format!("x-r2-status: {status}\n"));
                replaced = true;
                continue;
            }
            if t == "---" {
                if !replaced {
                    out.push_str(&format!("x-r2-status: {status}\n"));
                }
                in_fm = false;
            }
        }
        out.push_str(l);
        out.push('\n');
    }
    std::fs::write(&path, out).map_err(|e| e.to_string())
}

pub fn now_ts_pub() -> u64 {
    now_ts()
}

fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn event(kind: &str, content: &str, evidence: &str) -> EvolutionEvent {
    EvolutionEvent {
        ts: now_ts(),
        kind: kind.into(),
        content: content.into(),
        evidence: evidence.into(),
        session_id: "self-evolution".into(),
    }
}

/// 晋升检查：trial 技能用满 5 次且成功率 ≥80% → promoted；
/// 失败 ≥3 次 → archived（目录移入 skills/.archived/）
fn check_promotion(name: &str) {
    if read_skill_status(name).as_deref() != Some("trial") {
        return;
    }
    let usage = read_usage();
    let Some(u) = usage.get(name) else { return };
    let total = u.success + u.fail;
    if u.fail >= 3 {
        // 归档：移目录 + 事件 + 快照
        let home = std::env::var("HOME").unwrap_or_default();
        let src = format!("{home}/.r2/skills/{name}");
        let dst_dir = format!("{home}/.r2/skills/.archived");
        let _ = std::fs::create_dir_all(&dst_dir);
        if std::fs::rename(&src, format!("{dst_dir}/{name}")).is_ok() {
            let _ = append_event(&event(
                "skill_archived",
                &format!("auto 技能「{name}」归档（失败 {}/{} 次）", u.fail, total),
                &format!("success={} fail={}", u.success, u.fail),
            ));
            let _ = snapshot_self(&format!("archived skill {name}"));
        }
        return;
    }
    if total >= 5 && u.success * 10 >= total * 8 {
        if write_skill_status(name, "promoted").is_ok() {
            let _ = append_event(&event(
                "skill_promoted",
                &format!("auto 技能「{name}」转正（{}/{} 成功）", u.success, total),
                &format!("success={} fail={} count={}", u.success, u.fail, u.count),
            ));
            let _ = snapshot_self(&format!("promoted skill {name}"));
        }
    }
}

/// 衰退检测：90 天未使用的技能清单（GROWTH 预警 + 注入降权用）
pub fn decayed_skills() -> Vec<(String, u64)> {
    let home = std::env::var("HOME").unwrap_or_default();
    let cutoff = now_ts().saturating_sub(DECAY_DAYS * 86400);
    let mut out = Vec::new();
    let usage = read_usage();
    if let Ok(entries) = std::fs::read_dir(format!("{home}/.r2/skills")) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || !e.path().join("SKILL.md").exists() {
                continue;
            }
            let last = usage.get(&name).map(|u| u.last_used).unwrap_or(0);
            if last < cutoff {
                out.push((name, last));
            }
        }
    }
    out
}

// ── git 版本化自我 ──

/// ~/.r2 仓库化（幂等）：只版本化"自我"文件（skills/ GOAL.md SOUL.md），
/// 数据流（sessions/memory/evolution/config 含密钥）全部排除
pub fn snapshot_self(reason: &str) -> Result<String, String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let dir = format!("{home}/.r2");
    // init（已存在时静默）
    let _ = std::process::Command::new("git")
        .args(["-C", &dir, "init", "-q"])
        .output();
    // 身份（local，不污染全局配置）
    let _ = std::process::Command::new("git")
        .args(["-C", &dir, "config", "user.email", "r2@self"])
        .output();
    let _ = std::process::Command::new("git")
        .args(["-C", &dir, "config", "user.name", "R2 Self"])
        .output();
    // .gitignore（幂等写入）
    let gi = "sessions/\nmemory.db\nevolution.jsonl\nskill_usage.json\nconfig.toml\nuploads/\n.sandbox-root/\n";
    let _ = std::fs::write(format!("{dir}/.gitignore"), gi);
    // add 自我文件
    for target in ["skills", "GOAL.md", "SOUL.md"] {
        let p = format!("{dir}/{target}");
        if std::path::Path::new(&p).exists() {
            let _ = std::process::Command::new("git")
                .args(["-C", &dir, "add", target])
                .output();
        }
    }
    let _ = std::process::Command::new("git")
        .args(["-C", &dir, "add", ".gitignore"])
        .output();
    // 有变化才 commit
    let status = std::process::Command::new("git")
        .args(["-C", &dir, "status", "--porcelain"])
        .output()
        .map_err(|e| e.to_string())?;
    if status.stdout.is_empty() {
        return Ok("无变化".into());
    }
    let out = std::process::Command::new("git")
        .args(["-C", &dir, "commit", "-q", "-m", reason])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("commit 失败：{}", String::from_utf8_lossy(&out.stderr)));
    }
    let hash = std::process::Command::new("git")
        .args(["-C", &dir, "rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    Ok(hash)
}

// ── 模式检测 + 技能起草（阶段2晋升通道的入口）──

/// CJK 判定（简化：统一表意文字+扩展+kana 范围——对聚类足够）
fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x2E80..=0x9FFF | 0x3040..=0x30FF | 0xF900..=0xFAFF | 0x20000..=0x2FA1F)
}

/// 教训 token 化（零依赖聚类的地基）。
/// E2E 实测教训：中文没有词分隔符，按非字母数字切会把整句中文切成一个巨型
/// token——三句同族教训 Jaccard 只有 0.11，聚类永远不触发。
/// 修法（标准技巧）：拉丁词段照常（≥2字符）；CJK 连续段切**二元组**
/// （"必须唯一"→必须/须唯/唯一），中文相似度恢复可用。
fn key_tokens(s: &str) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    let mut latin: Vec<char> = Vec::new();
    let mut cjk_run: Vec<char> = Vec::new();
    for c in s.chars() {
        if is_cjk(c) {
            if !latin.is_empty() {
                if latin.len() >= 2 {
                    out.insert(latina(&latin));
                }
                latin.clear();
            }
            cjk_run.push(c);
        } else if c.is_alphanumeric() {
            if !cjk_run.is_empty() {
                flush_cjk(&cjk_run, &mut out);
                cjk_run.clear();
            }
            latin.push(c);
        } else {
            if latin.len() >= 2 {
                out.insert(latina(&latin));
            }
            latin.clear();
            if !cjk_run.is_empty() {
                flush_cjk(&cjk_run, &mut out);
                cjk_run.clear();
            }
        }
    }
    if latin.len() >= 2 {
        out.insert(latina(&latin));
    }
    if !cjk_run.is_empty() {
        flush_cjk(&cjk_run, &mut out);
    }
    out
}

fn latina(v: &[char]) -> String {
    v.iter().collect::<String>().to_lowercase()
}

/// CJK 段切二元组（≥2 字才切）
fn flush_cjk(run: &[char], out: &mut std::collections::HashSet<String>) {
    if run.len() == 1 {
        return; // 单字信息量不足，丢弃
    }
    for w in run.windows(2) {
        out.insert(w.iter().collect::<String>());
    }
}

fn jaccard(a: &std::collections::HashSet<String>, b: &std::collections::HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    inter / union
}

/// 同族教训聚类（贪心）：CJK二元组 Jaccard ≥ 0.14 归同族（同族实测0.15-0.3，异族<0.05）；返回大小 ≥ min_size 的族
pub fn detect_lesson_clusters(min_size: usize) -> Vec<Vec<String>> {
    let lessons: Vec<String> = read_events(500)
        .into_iter()
        .filter(|e| e.kind == "lesson")
        .map(|e| e.content)
        .collect();
    // 单链聚类：与族内**任一成员**的 Jaccard 达标即入族。
    // 不用并集质心——成员加入会稀释质心，后来的边缘成员永远够不着
    // （E2E 实测：J12=0.16 入族后质心稀释，J13=0.08 的第三条被拒）。
    // 单链的代价（链式漂移）由阈值+族大小下限兜底。
    let mut clusters: Vec<Vec<(std::collections::HashSet<String>, String)>> = Vec::new();
    for l in lessons {
        let tk = key_tokens(&l);
        let mut target: Option<usize> = None;
        for (i, members) in clusters.iter().enumerate() {
            if members
                .iter()
                .any(|(mtk, _)| jaccard(&tk, mtk) >= 0.14)
            {
                target = Some(i);
                break;
            }
        }
        match target {
            Some(i) => clusters[i].push((tk, l)),
            None => clusters.push(vec![(tk, l)]),
        }
    }
    clusters
        .into_iter()
        .filter(|m| m.len() >= min_size)
        .map(|m| m.into_iter().map(|(_, l)| l).collect())
        .collect()
}

/// 技能起草 prompt：把同族教训合成 SKILL.md（翻译器角色的延伸——
/// 只归纳教训里已有的操作性规则，禁止发明）
pub fn draft_messages(lessons: &[String]) -> Vec<crate::types::Message> {
    use crate::types::{Message, Role};
    let sys = "你是技能合成器。给你同一主题的多条操作性教训（来自真实会话的硬信号提炼）。\
把它们归纳成一份 SKILL.md。规则：\n\
1. 只归纳教训中已有的操作规则，禁止发明新规则\n\
2. frontmatter 格式：\n---\nname: auto-<短名>\ndescription: <何时触发本技能，30字内>\nx-r2-status: trial\n---\n\
3. 正文：## 何时用\\n## 怎么做（要点列表）\\n## 来源教训（逐条列出）\n\
4. 直接输出 SKILL.md 全文，不要代码块包裹";
    let user = lessons
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{}. {}", i + 1, l))
        .collect::<Vec<_>>()
        .join("\n");
    vec![
        Message { role: Role::System, content: sys.into(), tool_calls: None, tool_call_id: None },
        Message { role: Role::User, content: user, tool_calls: None, tool_call_id: None },
    ]
}

/// 解析起草输出的技能名（frontmatter name: auto-xxx）
pub fn parse_draft_name(text: &str) -> Option<String> {
    for line in text.lines() {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("name:") {
            let v = v.trim();
            if v.starts_with("auto-") {
                let valid = v.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
                if valid {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// 落盘草稿技能（名字冲突时跳过——同名技能已在成长中）
pub fn write_draft_skill(name: &str, content: &str) -> Result<(), String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let dir = format!("{home}/.r2/skills/{name}");
    if std::path::Path::new(&dir).exists() {
        return Err(format!("技能 {name} 已存在（成长中，不重复起草）"));
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(format!("{dir}/SKILL.md"), content).map_err(|e| e.to_string())
}


/// 技能名清单（成长变化检测的基线）
pub fn list_skill_names() -> Vec<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(format!("{home}/.r2/skills")) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with('.') && e.path().join("SKILL.md").exists() {
                out.push(name);
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod v2_tests {
    use super::*;

    #[test]
    fn test_detect_skill_reference() {
        assert_eq!(
            detect_skill_reference("bash", "cat ~/.r2/skills/brainstorming/SKILL.md"),
            Some("brainstorming".into())
        );
        assert_eq!(
            detect_skill_reference("read", "{\"path\":\"skills/quant-report/SKILL.md\"}"),
            Some("quant-report".into())
        );
        assert_eq!(detect_skill_reference("bash", "ls skills/"), None);
        assert_eq!(detect_skill_reference("edit", "skills/x/SKILL.md"), None);
    }

    #[test]
    fn test_key_tokens_jaccard() {
        let a = key_tokens("sed -i 前先备份 cp x x.bak");
        let b = key_tokens("sed -i 前先备份，用 cp 复制一份");
        let c = key_tokens("docker 网络要 inspect 看 bridge");
        assert!(jaccard(&a, &b) > 0.4, "同主题教训应高相似");
        assert!(jaccard(&a, &c) < 0.15, "不同主题应低相似");
    }

    #[test]
    fn test_parse_draft_name() {
        assert_eq!(
            parse_draft_name("---\nname: auto-sed-backup\ndescription: x\n---\n正文"),
            Some("auto-sed-backup".into())
        );
        assert_eq!(parse_draft_name("---\nname: hand-written\n---"), None);
    }

    #[test]
    fn test_draft_messages_shape() {
        let msgs = draft_messages(&["教训A".into(), "教训B".into()]);
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0].content.contains("禁止发明"));
    }
}
