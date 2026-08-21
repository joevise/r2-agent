//! summon 工具：多 Agent 召唤的起草与查询（v0.9.0）
//!
//! 信任模型的工具侧（与 task 工具同款）：本工具物理上只能创建 pending 档案——
//! active 的转换（批准召唤）只能走 Console 的审批按钮（WS agent_approve），
//! 代码路径上物理隔离"造人"的开关。
//!
//! 不提供 approve/reject/remove：agent 不能自己批准自己召唤的分身。

use super::Tool;
use crate::agents;

pub struct SummonTool;

impl SummonTool {
    pub fn new() -> Self {
        Self
    }

    fn do_create(&self, input: &serde_json::Value) -> String {
        let name = input.get("name").and_then(|v| v.as_str()).unwrap_or_default();
        let display_name = input
            .get("display_name")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let description = input
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let model = input.get("model").and_then(|v| v.as_str()).unwrap_or_default();
        let soul = input.get("soul").and_then(|v| v.as_str()).unwrap_or_default();
        if name.trim().is_empty() {
            return "ERROR: 缺少 name（agent 目录名，1-24 位字母/数字/-/_，main 保留）".into();
        }
        if !agents::valid_name(name) {
            return format!("ERROR: 非法名字 {name}（1-24 位字母/数字/-/_，main 保留）");
        }
        if display_name.trim().is_empty() {
            return "ERROR: 缺少 display_name（显示名，如「CFO 参谋」，最长 32 字）".into();
        }
        if description.trim().is_empty() {
            return "ERROR: 缺少 description（一句话职责描述，审批卡展示，最长 200 字）".into();
        }
        let soul = if soul.trim().is_empty() {
            format!("你是 {display_name}。职责：{description}。忠诚服务用户，先想清楚再行动，遇事如实汇报。")
        } else {
            soul.to_string()
        };
        match agents::draft_profile(name, display_name, model, description, &soul) {
            Ok(p) => format!(
                "OK: agent「{}」（{}）已起草(pending)·等待用户在 Console 审批。⚠ 工具无法（也不应）自行激活分身——用户会在界面看到审批卡，批准后才生效。summon list 可查看当前全部分身。",
                p.display_name, p.name
            ),
            Err(e) => format!("ERROR: {e}"),
        }
    }

    fn do_list(&self) -> String {
        let profiles = agents::list_profiles();
        if profiles.is_empty() {
            return "当前没有 agent 分身。create 新建（起草后需用户在 Console 批准生效）。".into();
        }
        let mut out = format!("共 {} 个分身：\n", profiles.len());
        for p in &profiles {
            out.push_str(&format!(
                "\n{} | {} | {} | {}",
                p.name, p.display_name, p.state, p.model
            ));
        }
        out
    }
}

#[async_trait::async_trait]
impl Tool for SummonTool {
    fn name(&self) -> &str {
        "summon"
    }

    fn description(&self) -> &str {
        "召唤新 agent 分身（多 Agent 协作）。action=create 起草新分身（name+display_name+description 必填，model 可选=模型覆盖，soul 可选=人格），起草后恒为 pending 状态，必须等用户在 Console 批准才生效——工具物理上造不出 active；action=list 查看当前全部分身（每行 name | 显示名 | 状态 | 模型）。不提供 approve/reject/remove：分身批准只能走 Console，删除由用户在界面操作。"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["create", "list"],
                    "description": "create=起草新分身 / list=查看全部"},
                "name": {"type": "string", "description": "分身目录名（1-24 位字母/数字/-/_，如 cfo；main 为保留名）"},
                "display_name": {"type": "string", "description": "显示名（如「CFO 参谋」，最长 32 字）"},
                "description": {"type": "string", "description": "一句话职责描述（审批卡与切换器展示，最长 200 字）"},
                "model": {"type": "string", "description": "模型覆盖（可选，空 = 继承主配置当前模型）"},
                "soul": {"type": "string", "description": "SOUL 人格（可选，最长 8000 字，缺省给通用职责描述）"}
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: &serde_json::Value) -> String {
        match input.get("action").and_then(|v| v.as_str()) {
            Some("create") => self.do_create(input),
            Some("list") => self.do_list(),
            other => format!("ERROR: action 必须是 create/list，收到 {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HOME 是进程级全局：持全仓共享锁防并行踩踏（agents/task_tool 同锁串行）。
    fn isolate_home() -> (tempfile::TempDir, std::sync::MutexGuard<'static, ()>) {
        let guard = crate::testutil::HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", tmp.path());
        (tmp, guard)
    }

    #[tokio::test]
    async fn test_create_pends_and_writes_soul() {
        let (_h, _g) = isolate_home();
        let tool = SummonTool::new();
        let r = tool
            .execute(&serde_json::json!({
                "action":"create","name":"cfo","display_name":"CFO 参谋","description":"管钱"
            }))
            .await;
        assert!(r.starts_with("OK"), "{r}");
        assert!(r.contains("pending") && r.contains("审批"), "必须声明 pending+审批卡语义: {r}");
        // 档案恒 pending（信任模型：工具造不出 active）
        let p = agents::load_profile("cfo").unwrap();
        assert_eq!(p.state, "pending");
        assert_eq!(p.model, ""); // 空 = 继承主配置
        // 目录与 SOUL.md 落盘
        let dir = agents::profile_dir("cfo");
        assert!(dir.join("AGENT.toml").is_file());
        assert!(dir.join("SOUL.md").is_file());
    }

    #[tokio::test]
    async fn test_list_shows_new_agent() {
        let (_h, _g) = isolate_home();
        let tool = SummonTool::new();
        let r = tool
            .execute(&serde_json::json!({
                "action":"create","name":"cfo","display_name":"CFO 参谋","description":"管钱","model":"glm-5.2-flash"
            }))
            .await;
        assert!(r.starts_with("OK"), "{r}");
        let out = tool.execute(&serde_json::json!({"action":"list"})).await;
        assert!(out.contains("cfo | CFO 参谋 | pending | glm-5.2-flash"), "{out}");
    }

    #[tokio::test]
    async fn test_create_rejects_bad_input() {
        let (_h, _g) = isolate_home();
        let tool = SummonTool::new();
        // 缺 action
        let r = tool.execute(&serde_json::json!({})).await;
        assert!(r.starts_with("ERROR"), "{r}");
        // 非法名（保留名）
        let r = tool
            .execute(&serde_json::json!({"action":"create","name":"main","display_name":"X","description":"y"}))
            .await;
        assert!(r.starts_with("ERROR"), "{r}");
        // 非法字符
        let r = tool
            .execute(&serde_json::json!({"action":"create","name":"a/b","display_name":"X","description":"y"}))
            .await;
        assert!(r.starts_with("ERROR"), "{r}");
        // 缺 display_name / description
        let r = tool
            .execute(&serde_json::json!({"action":"create","name":"cfo","description":"y"}))
            .await;
        assert!(r.starts_with("ERROR"), "{r}");
        let r = tool
            .execute(&serde_json::json!({"action":"create","name":"cfo","display_name":"X"}))
            .await;
        assert!(r.starts_with("ERROR"), "{r}");
        // 重名
        tool.execute(&serde_json::json!({"action":"create","name":"cfo","display_name":"CFO","description":"y"}))
            .await;
        let r = tool
            .execute(&serde_json::json!({"action":"create","name":"cfo","display_name":"CFO2","description":"y"}))
            .await;
        assert!(r.starts_with("ERROR"), "{r}");
        assert!(r.contains("同名"), "{r}");
    }
}
