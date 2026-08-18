//! task 工具：定时任务的起草与查询（v0.8）
//!
//! 信任模型的工具侧：本工具只能创建 pending 态任务——
//! active 的转换（批准）只能走 Console 的审批按钮（WS task_approve），
//! 代码路径上物理隔离"花钱的开关"。
//!
//! agent 用它响应两种场景：
//! · 用户口述「帮我建个每天7点复习教训的任务」→ owner=user 起草
//! · 自提议（反思发现周期性需求）→ owner=agent 起草
//! 两种都出审批卡，人点批准才生效。

use super::Tool;
use crate::tasks;

pub struct TaskTool;

impl TaskTool {
    pub fn new() -> Self {
        Self
    }

    fn do_create(&self, input: &serde_json::Value) -> String {
        let name = input.get("name").and_then(|v| v.as_str()).unwrap_or_default();
        let schedule = input
            .get("schedule")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let prompt = input.get("prompt").and_then(|v| v.as_str()).unwrap_or_default();
        let owner = match input.get("owner").and_then(|v| v.as_str()) {
            Some("agent") => "agent",
            _ => "user",
        };
        if !tasks::valid_name(name) {
            return "ERROR: name 必须是 1-48 位字母/数字/-/_（如 morning-review）".into();
        }
        if !tasks::valid_schedule(schedule) {
            return "ERROR: 周期规格不合法。格式：daily:HH:MM（每天）/ weekly:星期几:HH:MM（0=周日）/ every:Nh|Nm（间隔，最小5分钟）。示例：daily:07:00 · weekly:1:22:00（每周一22点） · every:6h".into();
        }
        if prompt.trim().is_empty() || prompt.len() > 2000 {
            return "ERROR: prompt 是后台会话要执行的任务描述，1-2000 字符".into();
        }
        let mut store = tasks::load_store();
        if store.tasks.iter().any(|t| t.name == name) {
            return format!("ERROR: 已存在同名任务 {name}（task list 查看，先删除再建）");
        }
        let tz = tasks::local_tz_offset_secs();
        let next_due = tasks::next_run(schedule, tasks::now_ts(), tz);
        let task = tasks::Task {
            id: format!("t{}", tasks::now_ts()),
            name: name.to_string(),
            schedule: schedule.to_string(),
            prompt: prompt.to_string(),
            owner: owner.to_string(),
            state: "pending".into(),
            created_ts: tasks::now_ts(),
            last_run: None,
            next_due,
            last_result: None,
        };
        let id = task.id.clone();
        let sched = task.schedule.clone();
        let tname = task.name.clone();
        store.tasks.push(task);
        if let Err(e) = tasks::save_store(&store) {
            return format!("ERROR: 保存任务失败：{e}");
        }
        format!(
            "OK: 定时任务「{tname}」已起草（{sched}，状态 pending）。⚠ 需要用户在界面点击批准后才会生效——工具无法（也不应）自行激活。界面会出现审批卡；用户也可以稍后在 GROWTH 页签处理。task id: {id}"
        )
    }

    fn do_list(&self) -> String {
        let store = tasks::load_store();
        if store.tasks.is_empty() {
            return "当前没有定时任务。create 新建（起草后需用户批准生效）。".into();
        }
        let mut out = format!("共 {} 个任务：\n", store.tasks.len());
        for t in &store.tasks {
            let state_badge = match t.state.as_str() {
                "pending" => "⏳待批准",
                "active" => "✅生效中",
                "paused" => "⏸已暂停",
                "rejected" => "❌已拒绝",
                _ => &t.state,
            };
            let owner_badge = if t.owner == "agent" { "🤖自提议" } else { "👤用户" };
            let next = t
                .next_due
                .map(|d| d.to_string())
                .unwrap_or_else(|| "-".into());
            out.push_str(&format!(
                "\n- {}（{}·{}·{}）下次:{}\n  任务：{}",
                t.name, state_badge, owner_badge, t.schedule, next, t.prompt
            ));
        }
        out
    }

    fn do_remove(&self, input: &serde_json::Value) -> String {
        let name = input.get("name").and_then(|v| v.as_str()).unwrap_or_default();
        let mut store = tasks::load_store();
        // 支持按 name 删（对 agent 自然）
        let id = match store.tasks.iter().find(|t| t.name == name) {
            Some(t) => t.id.clone(),
            None => return format!("ERROR: 任务不存在：{name}"),
        };
        match tasks::remove_task(&mut store, &id) {
            Ok(()) => {
                let _ = tasks::save_store(&store);
                format!("OK: 任务 {name} 已删除")
            }
            Err(e) => format!("ERROR: {e}"),
        }
    }
}

#[async_trait::async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }

    fn description(&self) -> &str {
        "管理定时任务（后台成长例程）。action=create 起草新任务（name+schedule+prompt，schedule 格式 daily:HH:MM / weekly:星期几:HH:MM / every:Nh），创建后需用户在界面批准才生效；action=list 查看；action=remove 删除。用户口述「每天几点做什么」时用本工具起草；你自己在反思中发现周期性需求（同类查询反复出现）时也可提议（owner=agent），同样需要用户批准。"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["create", "list", "remove"],
                    "description": "create=起草 / list=查看 / remove=删除"},
                "name": {"type": "string", "description": "任务名（字母数字-_，如 morning-review）"},
                "schedule": {"type": "string",
                    "description": "周期：daily:07:00（每天7点）/ weekly:1:22:00（每周一22点，0=周日）/ every:6h（每6小时）"},
                "prompt": {"type": "string", "description": "任务内容：后台会话将收到的指令（1-2000字符）"},
                "owner": {"type": "string", "enum": ["user", "agent"],
                    "description": "user=用户口述创建 / agent=你主动提议（默认 user）"}
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: &serde_json::Value) -> String {
        match input.get("action").and_then(|v| v.as_str()) {
            Some("create") => self.do_create(input),
            Some("list") => self.do_list(),
            Some("remove") => self.do_remove(input),
            other => format!("ERROR: action 必须是 create/list/remove，收到 {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_create_validates() {
        let tool = TaskTool::new();
        // 坏周期
        let r = tool
            .execute(&serde_json::json!({"action":"create","name":"x","schedule":"0 7 * * *","prompt":"p"}))
            .await;
        assert!(r.starts_with("ERROR"), "{r}");
        // 坏名字
        let r = tool
            .execute(&serde_json::json!({"action":"create","name":"坏 名字","schedule":"daily:07:00","prompt":"p"}))
            .await;
        assert!(r.starts_with("ERROR"), "{r}");
        // 空 prompt
        let r = tool
            .execute(&serde_json::json!({"action":"create","name":"ok-task","schedule":"daily:07:00","prompt":" "}))
            .await;
        assert!(r.starts_with("ERROR"), "{r}");
    }

    #[tokio::test]
    async fn test_create_pends_not_activates() {
        // 隔离 HOME
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", tmp.path());
        let tool = TaskTool::new();
        let r = tool
            .execute(&serde_json::json!({
                "action":"create","name":"morning-review",
                "schedule":"daily:07:00","prompt":"晨间成长例程"
            }))
            .await;
        assert!(r.starts_with("OK"), "{r}");
        assert!(r.contains("pending") && r.contains("批准"), "工具输出必须声明 pending 语义: {r}");
        // 存储里的状态必须是 pending（信任模型：工具不能造 active）
        let store = tasks::load_store();
        let t = store.tasks.iter().find(|t| t.name == "morning-review").unwrap();
        assert_eq!(t.state, "pending");
        // 重复名拒绝
        let r2 = tool
            .execute(&serde_json::json!({
                "action":"create","name":"morning-review",
                "schedule":"daily:07:00","prompt":"x"
            }))
            .await;
        assert!(r2.starts_with("ERROR"));
    }
}
