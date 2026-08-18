//! 定时任务系统（v0.8 后台成长）
//!
//! 信任模型：口头=起草权，点击=签字权。
//! task 工具只能创建 pending 态；active 转换只能走 Console 的批准按钮（WS）。
//! 暂停/删除随时可用（停止花钱的事不需要签字）。
//!
//! 周期规格（对 UI 友好，不用 cron 原文）：
//!   daily:HH:MM      每天定点        daily:07:00
//!   weekly:D:HH:MM   每周定点 D=0..6（0=周日）  weekly:1:22:00
//!   every:Nh|Nm      间隔循环        every:6h / every:30m

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Task {
    pub id: String,
    pub name: String,
    /// 周期规格（见模块注释）
    pub schedule: String,
    /// 任务提示词（后台会话的 prompt）
    pub prompt: String,
    /// user（口述）/ agent（自提议）
    pub owner: String,
    /// pending / active / rejected / paused
    pub state: String,
    pub created_ts: u64,
    /// 上次运行（unix 秒；None=从未）
    pub last_run: Option<u64>,
    /// 下次应运行时刻（调度器持久化，重启不丢）
    pub next_due: Option<u64>,
    /// 上次结果摘要
    pub last_result: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TaskStore {
    pub tasks: Vec<Task>,
    /// 每日运行计数（全局预算护栏）
    pub meta_day: String,
    pub meta_runs: u64,
}

impl Default for Task {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            schedule: String::new(),
            prompt: String::new(),
            owner: "user".into(),
            state: "pending".into(),
            created_ts: 0,
            last_run: None,
            next_due: None,
            last_result: None,
        }
    }
}

fn store_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(format!("{home}/.r2/tasks.json"))
}

pub fn load_store() -> TaskStore {
    std::fs::read_to_string(store_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_store(s: &TaskStore) -> Result<(), String> {
    let path = store_path();
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let json = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())
}

// ── 周期规格解析与下次时刻计算（纯函数，可测）──

fn parse_hhmm(s: &str) -> Option<(u32, u32)> {
    let mut it = s.split(':');
    let h: u32 = it.next()?.parse().ok()?;
    let m: u32 = it.next()?.parse().ok()?;
    if it.next().is_some() || h > 23 || m > 59 {
        return None;
    }
    Some((h, m))
}

/// 校验周期规格；合法返回 true
pub fn valid_schedule(spec: &str) -> bool {
    next_run(spec, 0, 0).is_some()
}

/// 下次运行时刻（unix 秒，分钟粒度）。from_ts 起算。
/// 内部用简易 UTC 日历（day-of-week 由 1970-01-01 周四推算）。
/// 说明：后台例程以服务器本地体验为准，UTC 偏差对"每天7点"类需求
/// 的影响 = 时区差——调度器调用处传入本地化后的 ts（见 web.rs：用
/// 本地时区偏移修正），此处保持纯函数可测。
pub fn next_run(spec: &str, from_ts: u64, tz_offset_secs: i64) -> Option<u64> {
    let from_local = from_ts as i64 + tz_offset_secs;
    if let Some(hhmm) = spec.strip_prefix("daily:") {
        let (h, m) = parse_hhmm(hhmm)?;
        return next_at_time(from_local, h, m, None, tz_offset_secs);
    }
    if let Some(rest) = spec.strip_prefix("weekly:") {
        let mut it = rest.split(':');
        let d: u32 = it.next()?.parse().ok()?;
        let h: u32 = it.next()?.parse().ok()?;
        let m: u32 = it.next()?.parse().ok()?;
        if d > 6 || h > 23 || m > 59 {
            return None;
        }
        return next_at_time(from_local, h, m, Some(d), tz_offset_secs);
    }
    if let Some(rest) = spec.strip_prefix("every:") {
        let unit = rest.chars().last()?;
        let n: u64 = rest[..rest.len() - 1].parse().ok()?;
        let secs = match unit {
            'h' => n.checked_mul(3600)?,
            'm' => n.checked_mul(60)?,
            _ => return None,
        };
        if secs < 300 {
            return None; // 最小间隔 5 分钟（防失控循环）
        }
        return Some(from_ts + secs);
    }
    None
}

/// 找 ≥ from_local 的下一个 当日 h:m（可限定星期）
fn next_at_time(
    from_local: i64,
    h: u32,
    m: u32,
    weekday: Option<u32>,
    tz_offset_secs: i64,
) -> Option<u64> {
    let day_secs: i64 = 86400;
    let target_of_day: i64 = (h as i64) * 3600 + (m as i64) * 60;
    // days since epoch + weekday（1970-01-01 = 周四 = 4）
    for offset in 0..8 {
        let day_start = (from_local / day_secs) * day_secs + offset * day_secs;
        let candidate_local = day_start + target_of_day;
        if candidate_local < from_local + 60 {
            continue; // 已过（含 1 分钟保护窗）
        }
        if let Some(w) = weekday {
            let days = candidate_local.div_euclid(day_secs);
            let dow = ((days + 4).rem_euclid(7)) as u32;
            if dow != w {
                continue;
            }
        }
        return Some((candidate_local - tz_offset_secs) as u64);
    }
    None
}

/// 本地时区偏移（读 /etc/localtime 太重；从 TZ 环境或 chrono 都不入——
/// 用 libc::localtime_r 一次调用拿 tm_gmtoff，零新依赖）
pub fn local_tz_offset_secs() -> i64 {
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return 8 * 3600; // 默认中国时区
        }
        tm.tm_gmtoff as i64
    }
}

// ── 状态机（approve/reject 只能由壳层调——信任模型的实现）──

pub fn transition(store: &mut TaskStore, id: &str, to: &str) -> Result<(), String> {
    let t = store
        .tasks
        .iter_mut()
        .find(|t| t.id == id)
        .ok_or_else(|| format!("任务不存在：{id}"))?;
    let allowed = match (t.state.as_str(), to) {
        ("pending", "active") | ("pending", "rejected") => true,
        ("active", "paused") | ("paused", "active") => true,
        _ => false,
    };
    if !allowed {
        return Err(format!("不允许的状态转换：{} → {}", t.state, to));
    }
    t.state = to.into();
    Ok(())
}

pub fn remove_task(store: &mut TaskStore, id: &str) -> Result<(), String> {
    let before = store.tasks.len();
    store.tasks.retain(|t| t.id != id);
    if store.tasks.len() == before {
        return Err(format!("任务不存在：{id}"));
    }
    Ok(())
}

/// 每日预算检查 + 计数（后台运行前调用；超限拒绝）
pub fn budget_gate(store: &mut TaskStore, max_per_day: u64) -> Result<(), String> {
    let today = chrono_like_today();
    if store.meta_day != today {
        store.meta_day = today;
        store.meta_runs = 0;
    }
    if store.meta_runs >= max_per_day {
        return Err(format!("今日后台任务已达上限 {max_per_day} 次（预算护栏）"));
    }
    store.meta_runs += 1;
    Ok(())
}

/// "YYYY-MM-DD"（本地）——用 localtime_r，零新依赖
fn chrono_like_today() -> String {
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return "unknown".into();
        }
        format!(
            "{:04}-{:02}-{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday
        )
    }
}

pub fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 48
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schedule_validation() {
        assert!(valid_schedule("daily:07:00"));
        assert!(valid_schedule("weekly:1:22:00"));
        assert!(valid_schedule("every:6h"));
        assert!(valid_schedule("every:30m"));
        assert!(!valid_schedule("daily:25:00"));
        assert!(!valid_schedule("every:1m")); // 最小 5 分钟
        assert!(!valid_schedule("0 7 * * *")); // 不收 cron 原文
        assert!(!valid_schedule("随便"));
    }

    #[test]
    fn test_next_run_daily() {
        // 2026-08-19 06:30 UTC+8 = 1787099400 - ... 用固定锚点：
        // 假设 from = 某日 10:30 本地，daily:07:00 → 次日本地 07:00
        let tz: i64 = 8 * 3600;
        // 1970-01-02 10:30 本地(UTC+8) → unix = (86400*2+10.5h) - tz
        let from = (86400 * 2 + 10 * 3600 + 30 * 60) - tz as u64;
        let next = next_run("daily:07:00", from, tz).unwrap();
        let next_local = next as i64 + tz;
        let day_start_local = (next_local / 86400) * 86400;
        assert_eq!(next_local - day_start_local, 7 * 3600);
        assert!(next > from + 3600); // 确实是明天
    }

    #[test]
    fn test_next_run_every() {
        let from = 1_000_000u64;
        assert_eq!(next_run("every:6h", from, 0), Some(from + 21600));
        assert_eq!(next_run("every:30m", from, 0), Some(from + 1800));
    }

    #[test]
    fn test_next_run_weekly_picks_right_day() {
        let tz: i64 = 0;
        // 找 2026-08-19（周三=3）00:00 UTC 的 unix
        // 2026-08-19 = days since epoch: 计算——1970→2026 约 56 年
        // 直接用已知锚点验证行为：从周一早上找 weekly:3（周三）
        // 2024-01-01 是周一（已知），unix = 1704067200
        let monday = 1704067200u64;
        let next = next_run("weekly:3:09:00", monday + 3600, tz).unwrap();
        // 应落在周三 09:00 = monday + 2*86400 + 9h
        assert_eq!(next, monday + 2 * 86400 + 9 * 3600);
    }

    #[test]
    fn test_transition_state_machine() {
        let mut s = TaskStore::default();
        s.tasks.push(Task {
            id: "t1".into(),
            state: "pending".into(),
            ..Default::default()
        });
        // pending → active ✓ / rejected ✓ / paused ✗
        assert!(transition(&mut s, "t1", "active").is_ok());
        assert!(transition(&mut s, "t1", "paused").is_ok());
        assert!(transition(&mut s, "t1", "active").is_ok());
        assert!(transition(&mut s, "t1", "rejected").is_err(), "active 不能直接 rejected（先删）");
        // 不存在
        assert!(transition(&mut s, "nope", "active").is_err());
    }

    #[test]
    fn test_budget_gate() {
        let mut s = TaskStore::default();
        for i in 0..3 {
            budget_gate(&mut s, 3).unwrap();
            assert_eq!(s.meta_runs, i + 1);
        }
        assert!(budget_gate(&mut s, 3).is_err(), "超限必须拒绝");
    }
}
