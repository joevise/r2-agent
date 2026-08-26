//! 飞书工具族（v0.10.4）：agent 用**自己通道的机器人身份**调飞书 API。
//!
//! 凭证零重复：不读任何配置文件，直接复用 CHANNEL_REGISTRY 里频道管理器
//! 登记的 FeishuClient（token 缓存/长连接全共享）。通道没启用 → 工具报
//! 明确错误（提示到 Console 频道页配置），不会误导 agent 去要 AppID。
//!
//! 用户实测教训（8/26）：lisa 被问"能不能发飞书"时自己去装了 lark SDK
//! 并向用户要凭证——工具层缺位导致 agent 绕路。正解是通道即身份。

use crate::channels;
use async_trait::async_trait;
use serde_json::Value;

/// 发消息工具：以本 agent 的机器人身份私聊任意飞书用户
pub struct FeishuSendTool {
    agent: String,
}

impl FeishuSendTool {
    pub fn new(agent: &str) -> Self {
        Self {
            agent: agent.to_string(),
        }
    }
}

#[async_trait]
impl super::Tool for FeishuSendTool {
    fn name(&self) -> &str {
        "feishu_send_message"
    }

    fn description(&self) -> &str {
        "以自己的飞书机器人身份给指定用户发私聊消息。参数 open_id 是对方 \
         的飞书 open_id（ou_ 开头），text 是消息内容（支持 \\n 换行）。\n\
         注意：需要本分身已在 Console「频道」页启用飞书通道。"
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "open_id": { "type": "string", "description": "接收者 open_id（ou_ 开头）" },
                "text": { "type": "string", "description": "消息文本" }
            },
            "required": ["open_id", "text"]
        })
    }

    async fn execute(&self, input: &Value) -> String {
        let (Some(open_id), Some(text)) = (
            input.get("open_id").and_then(|v| v.as_str()),
            input.get("text").and_then(|v| v.as_str()),
        ) else {
            return "ERROR: 缺少参数 open_id / text".into();
        };
        let Some(client) = channels::channel_client(&self.agent) else {
            return format!(
                "ERROR: 本分身（{}）未启用飞书通道——请到 Console「频道」页配置并启用后重试",
                self.agent
            );
        };
        match client.send_text(open_id, text).await {
            Ok(()) => format!("✅ 已发送给 {open_id}"),
            Err(e) => format!("ERROR: 发送失败：{e}"),
        }
    }
}

/// 建文档工具：把 Markdown 转成飞书在线文档（标题/列表/代码块/正文），返回链接
pub struct FeishuDocTool {
    agent: String,
}

impl FeishuDocTool {
    pub fn new(agent: &str) -> Self {
        Self {
            agent: agent.to_string(),
        }
    }
}

#[async_trait]
impl super::Tool for FeishuDocTool {
    fn name(&self) -> &str {
        "feishu_create_doc"
    }

    fn description(&self) -> &str {
        "把 Markdown 内容创建为飞书在线文档并返回链接（标题/加粗保留语义，\
         代码块原样）。适合输出报告、日报、长文——发链接比刷屏消息体面。\n\
         参数 title 文档标题，markdown 正文（# ## ### 标题 / - 列表 / ``` 代码块）。"
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "文档标题" },
                "markdown": { "type": "string", "description": "正文 Markdown" }
            },
            "required": ["title", "markdown"]
        })
    }

    async fn execute(&self, input: &Value) -> String {
        let (Some(title), Some(markdown)) = (
            input.get("title").and_then(|v| v.as_str()),
            input.get("markdown").and_then(|v| v.as_str()),
        ) else {
            return "ERROR: 缺少参数 title / markdown".into();
        };
        let Some(client) = channels::channel_client(&self.agent) else {
            return format!(
                "ERROR: 本分身（{}）未启用飞书通道——请到 Console「频道」页配置并启用后重试",
                self.agent
            );
        };
        match client.create_doc(title, markdown).await {
            Ok(url) => format!("✅ 文档已创建：{url}"),
            Err(e) => format!("ERROR: 建文档失败：{e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_send_tool_schema_required() {
        let t = FeishuSendTool::new("lisa");
        let s = t.schema();
        assert!(s["required"].as_array().unwrap().contains(&serde_json::json!("open_id")));
        assert_eq!(t.name(), "feishu_send_message");
    }

    #[test]
    fn test_doc_tool_no_channel_error() {
        // 注册表里没有 main2 → 明确报错（不 panic）
        let t = FeishuDocTool::new("main2");
        let out = futures_lite_block(t.execute(&serde_json::json!({
            "title": "t", "markdown": "# h"
        })));
        assert!(out.starts_with("ERROR:"));
        assert!(out.contains("未启用飞书通道"));
    }

    /// 最小 async 执行器（测试里 tokio runtime 可能没起）
    fn futures_lite_block<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }
}
