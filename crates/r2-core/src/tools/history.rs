//! 历史会话检索工具（v0.11.0 记忆层）：agent 主动查过去的会话记录。
//!
//! 数据源：{session_dir}/*.jsonl（每行一个 SessionEntry，含 user/assistant
//! 消息与工具结果）。飞书 DM 会话通过 {persona}/dm/*.sid 指针文件标记
//! （文件名=open_id，内容=session id）——列表能看出"这是跟哪个飞书用户聊的"。
//!
//! 三个动作：
//!   list   —— 列出全部历史会话（相对时间/条数/主题预览/飞书标记）
//!   search —— 跨会话关键词搜索（大小写不敏感，返回会话+上下文片段）
//!   read   —— 读指定会话尾部（user/assistant 消息流，跳过工具噪音）
//!
//! 设计原则：全文关键词检索（JSONL 是结构化文本），零 embedding 依赖——
//! 单用户量级（几十个会话文件）grep 式搜索足够快也足够准。

use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

pub struct HistoryTool {
    session_dir: PathBuf,
}

impl HistoryTool {
    pub fn new(session_dir: &str) -> Self {
        Self {
            session_dir: PathBuf::from(crate::config::expand_tilde(session_dir)),
        }
    }

    /// 飞书 DM 会话标记：{persona}/dm/*.sid（文件名=open_id → 内容=session_id）。
    /// 零侵入复用会话指针文件——不用改 JSONL 格式
    fn dm_markers(&self) -> HashMap<String, String> {
        let mut map = HashMap::new();
        let Some(parent) = self.session_dir.parent() else {
            return map;
        };
        let Ok(entries) = std::fs::read_dir(parent.join("dm")) else {
            return map;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(oid) = name.strip_suffix(".sid") {
                if let Ok(sid) = std::fs::read_to_string(e.path()) {
                    let sid = sid.trim().to_string();
                    if !sid.is_empty() {
                        map.insert(sid, oid.to_string());
                    }
                }
            }
        }
        map
    }

    fn do_list(&self) -> String {
        let dir = self.session_dir.to_string_lossy().to_string();
        let Ok(mut list) = crate::session::list_sessions(&dir) else {
            return "ERROR: 读会话目录失败".into();
        };
        if list.is_empty() {
            return "还没有历史会话。".into();
        }
        list.sort_by_key(|s| std::cmp::Reverse(s.last_ts));
        let dm = self.dm_markers();
        let mut out = format!("共 {} 个历史会话（新→旧，最多列 50）：\n", list.len());
        for s in list.iter().take(50) {
            let src = dm
                .get(&s.id)
                .map(|oid| format!(" 📱飞书({}…)", &oid[..8.min(oid.len())]))
                .unwrap_or_default();
            let preview: String = s.first_user_preview.chars().take(60).collect();
            out.push_str(&format!(
                "\n· {} · {}条 · {}{src}\n  主题：{}",
                &s.id[..8.min(s.id.len())],
                s.message_count,
                rel_time(s.last_ts),
                preview
            ));
        }
        out.push_str("\n\n读全文：history {\"action\":\"read\",\"session_id\":\"<前8位>\"}");
        out
    }

    fn do_search(&self, keyword: &str) -> String {
        if keyword.trim().is_empty() {
            return "ERROR: 缺少 keyword".into();
        }
        let kw = keyword.to_lowercase();
        let dm = self.dm_markers();
        let Ok(entries) = std::fs::read_dir(&self.session_dir) else {
            return "ERROR: 读会话目录失败".into();
        };
        let mut hits: Vec<String> = Vec::new();
        let mut files = 0usize;
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.ends_with(".jsonl") {
                continue;
            }
            files += 1;
            let sid = name.trim_end_matches(".jsonl").to_string();
            let Ok(content) = std::fs::read_to_string(e.path()) else {
                continue;
            };
            for line in content.lines() {
                let Ok(v) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                // 只搜 user/assistant 消息行——工具调用/结果太吵
                let role = v.get("role").and_then(|x| x.as_str()).unwrap_or("");
                if role != "user" && role != "assistant" {
                    continue;
                }
                let text = v.get("content").and_then(|x| x.as_str()).unwrap_or("");
                let lower = text.to_lowercase();
                if let Some(pos) = lower.find(&kw) {
                    let src = dm.get(&sid).map(|_| "📱").unwrap_or("");
                    let start = pos.saturating_sub(40);
                    let snip: String = text.chars().skip(start).take(140).collect();
                    hits.push(format!(
                        "· {}{src} [{}] …{}…",
                        &sid[..8.min(sid.len())],
                        role,
                        snip.replace('\n', " ")
                    ));
                    if hits.len() >= 40 {
                        return format!(
                            "「{keyword}」命中 ≥40 条（截断显示 40）：\n\n{}",
                            hits.join("\n")
                        );
                    }
                }
            }
        }
        if hits.is_empty() {
            format!("「{keyword}」在 {files} 个会话文件中没有命中。")
        } else {
            format!(
                "「{keyword}」命中 {} 条：\n\n{}",
                hits.len(),
                hits.join("\n")
            )
        }
    }

    fn do_read(&self, sid_prefix: &str, tail: usize) -> String {
        let Ok(entries) = std::fs::read_dir(&self.session_dir) else {
            return "ERROR: 读会话目录失败".into();
        };
        let mut matched: Option<(String, PathBuf)> = None;
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(id) = name.strip_suffix(".jsonl") {
                if id.starts_with(sid_prefix) {
                    matched = Some((id.to_string(), e.path()));
                    break;
                }
            }
        }
        let Some((sid, path)) = matched else {
            return format!("ERROR: 没有以 {sid_prefix} 开头的会话（list 查看全部）");
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            return "ERROR: 读会话文件失败".into();
        };
        let mut msgs: Vec<(String, String, u64)> = Vec::new();
        for line in content.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let role = v.get("role").and_then(|x| x.as_str()).unwrap_or("");
            if role != "user" && role != "assistant" {
                continue;
            }
            let text = v.get("content").and_then(|x| x.as_str()).unwrap_or("");
            let ts = v.get("ts").and_then(|x| x.as_u64()).unwrap_or(0);
            msgs.push((role.to_string(), text.to_string(), ts));
        }
        let tail = tail.clamp(1, 100);
        let skip = msgs.len().saturating_sub(tail);
        let dm = self.dm_markers();
        let src = dm
            .get(&sid)
            .map(|o| format!("（📱飞书 {}…）", &o[..8.min(o.len())]))
            .unwrap_or_default();
        let mut out = format!(
            "会话 {}{src}（共 {} 条消息，显示尾部 {} 条）：",
            sid,
            msgs.len(),
            msgs.len() - skip
        );
        for (role, text, ts) in msgs.iter().skip(skip) {
            let t = if *ts > 0 {
                rel_time(*ts)
            } else {
                String::new()
            };
            let snip: String = text.chars().take(600).collect();
            out.push_str(&format!("\n[{} {}] {}", role, t, snip));
        }
        out
    }
}

/// 相对时间（免 chrono 依赖；agent 看得懂就行）
fn rel_time(ts: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let d = now.saturating_sub(ts);
    if d < 60 {
        "刚刚".into()
    } else if d < 3600 {
        format!("{}分钟前", d / 60)
    } else if d < 86400 {
        format!("{}小时前", d / 3600)
    } else {
        format!("{}天前", d / 86400)
    }
}

#[async_trait]
impl super::Tool for HistoryTool {
    fn name(&self) -> &str {
        "history"
    }

    fn description(&self) -> &str {
        "查历史会话记录（跨会话记忆）。action=list 列出全部会话（时间/主题/\
         飞书标记）；action=search keyword=关键词 跨会话全文搜索；\
         action=read session_id=<前8位> tail=<条数> 读指定会话尾部。\
         用户问「之前说过什么 / 你还记得吗」时先用本工具查，禁止凭空编造。"
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["list", "search", "read"],
                    "description": "list=列出会话 / search=关键词搜全文 / read=读会话"},
                "keyword": {"type": "string", "description": "search 时的关键词"},
                "session_id": {"type": "string", "description": "read 时的会话 id（前 8 位即可）"},
                "tail": {"type": "integer", "description": "read 时尾部条数（默认 30，上限 100）"}
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: &Value) -> String {
        let action = input.get("action").and_then(|v| v.as_str()).unwrap_or("");
        match action {
            "list" => self.do_list(),
            "search" => self.do_search(input.get("keyword").and_then(|v| v.as_str()).unwrap_or("")),
            "read" => {
                let sid = input
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if sid.is_empty() {
                    return "ERROR: 缺少 session_id".into();
                }
                let tail = input.get("tail").and_then(|v| v.as_u64()).unwrap_or(30) as usize;
                self.do_read(sid, tail)
            }
            _ => "ERROR: action 必须是 list / search / read".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool; // execute 是 trait 方法，测试里要显式引入

    fn mk_session(dir: &std::path::Path, id: &str, lines: &[(&str, &str)]) {
        let mut content = String::new();
        for (role, text) in lines {
            content.push_str(&format!(
                "{{\"type\":\"message\",\"role\":\"{role}\",\"content\":\"{text}\",\"ts\":1787800000}}\n"
            ));
        }
        std::fs::write(dir.join(format!("{id}.jsonl")), content).unwrap();
    }

    #[tokio::test]
    async fn test_search_and_read() {
        let tmp = tempfile::tempdir().unwrap();
        let sd = tmp.path().join("sessions");
        std::fs::create_dir_all(&sd).unwrap();
        mk_session(
            &sd,
            "aaaa1111-2222",
            &[
                ("user", "帮我分析一下 besureAI 的架构"),
                ("assistant", "好的，besureAI 分三层：vault、索引、语义检索"),
            ],
        );
        mk_session(
            &sd,
            "bbbb3333-4444",
            &[("user", "今天天气不错"), ("assistant", "是的")],
        );
        // dm 标记：persona/dm/ou_test123.sid → aaaa1111 会话
        let dm_dir = tmp.path().join("dm");
        std::fs::create_dir_all(&dm_dir).unwrap();
        std::fs::write(dm_dir.join("ou_testopen_id.sid"), "aaaa1111-2222").unwrap();

        let tool = HistoryTool::new(sd.to_str().unwrap());

        // search 命中 + 飞书标记
        let out = tool
            .execute(&serde_json::json!({"action":"search","keyword":"besureAI"}))
            .await;
        assert!(out.contains("aaaa1111"), "应命中第一个会话：{out}");
        assert!(out.contains("📱"), "应带飞书标记：{out}");
        // search 未命中
        let out = tool
            .execute(&serde_json::json!({"action":"search","keyword":"不存在词xyz"}))
            .await;
        assert!(out.contains("没有命中"), "{out}");
        // read 前缀匹配 + 内容
        let out = tool
            .execute(&serde_json::json!({"action":"read","session_id":"aaaa"}))
            .await;
        assert!(out.contains("三层"), "应含会话内容：{out}");
        assert!(out.contains("飞书"), "应标来源：{out}");
        // read 不存在
        let out = tool
            .execute(&serde_json::json!({"action":"read","session_id":"zzzz"}))
            .await;
        assert!(out.starts_with("ERROR"), "{out}");
    }

    #[test]
    fn test_rel_time() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(rel_time(now - 120), "2分钟前");
        assert_eq!(rel_time(now - 7200), "2小时前");
    }
}
