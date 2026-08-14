//! OpenAI 兼容协议 Provider（适用于 OpenAI / 智谱 / DeepSeek 等兼容接口）

use super::{ChunkStream, ModelProvider, ModelResult};
use crate::types::{Message, Role, StreamChunk, ToolCall, ToolSchema};
use futures_util::StreamExt;
use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

/// 建连超时
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// 最大重试次数（不含首次请求）
const MAX_RETRIES: u32 = 2;

/// OpenAI 兼容协议 Provider
pub struct OpenAiCompatProvider {
    /// API 基础地址（不含尾部斜杠）
    base_url: String,
    /// API 密钥
    api_key: String,
    /// 模型名称
    model: String,
    /// HTTP 客户端
    client: reqwest::Client,
}

impl OpenAiCompatProvider {
    /// 创建 Provider 实例
    pub fn new(base_url: &str, api_key: &str, model: &str) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .expect("构建 reqwest client 失败");
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            client,
        }
    }
}

/// 把内部 Message 转成 OpenAI 协议 JSON
fn message_to_json(msg: &Message) -> serde_json::Value {
    let role = match msg.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    let mut obj = serde_json::json!({
        "role": role,
        "content": msg.content,
    });
    // assistant 携带的工具调用
    if let Some(calls) = &msg.tool_calls {
        let arr: Vec<serde_json::Value> = calls
            .iter()
            .map(|c| {
                serde_json::json!({
                    "id": c.id,
                    "type": "function",
                    "function": {
                        "name": c.name,
                        "arguments": c.arguments,
                    }
                })
            })
            .collect();
        obj["tool_calls"] = serde_json::Value::Array(arr);
    }
    // tool 结果消息的 tool_call_id 是顶层字段
    if msg.role == Role::Tool {
        if let Some(id) = &msg.tool_call_id {
            obj["tool_call_id"] = serde_json::Value::String(id.clone());
        }
    }
    obj
}

/// 构建 /chat/completions 请求体（tools 为空时不带 tools 字段）
fn build_request_body(model: &str, messages: &[Message], tools: &[ToolSchema]) -> serde_json::Value {
    let msgs: Vec<serde_json::Value> = messages.iter().map(message_to_json).collect();
    let mut body = serde_json::json!({
        "model": model,
        "messages": msgs,
        "stream": true,
    });
    if !tools.is_empty() {
        let arr: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect();
        body["tools"] = serde_json::Value::Array(arr);
    }
    body
}

/// 解析单个 SSE data 事件 payload，产出 0..n 个 StreamChunk
fn parse_data_payload(payload: &str) -> ModelResult<Vec<StreamChunk>> {
    let v: serde_json::Value = serde_json::from_str(payload)?;
    let mut chunks = Vec::new();
    let delta = &v["choices"][0]["delta"];
    if let Some(content) = delta["content"].as_str() {
        if !content.is_empty() {
            chunks.push(StreamChunk::Delta(content.to_string()));
        }
    }
    if let Some(tool_calls) = delta["tool_calls"].as_array() {
        for tc in tool_calls {
            let index = tc["index"].as_u64().unwrap_or(0) as usize;
            let id = tc["id"].as_str().map(|s| s.to_string());
            let name = tc["function"]["name"].as_str().map(|s| s.to_string());
            let arguments_delta = tc["function"]["arguments"]
                .as_str()
                .unwrap_or("")
                .to_string();
            chunks.push(StreamChunk::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            });
        }
    }
    Ok(chunks)
}

/// SSE 增量解析器：维护缓冲区，按事件边界（\n\n）切分，处理跨网络块的事件
struct SseParser {
    /// 未完整事件的残留字节
    buffer: String,
}

impl SseParser {
    fn new() -> Self {
        Self {
            buffer: String::new(),
        }
    }

    /// 处理单个完整事件文本
    fn handle_event(&mut self, event: &str, out: &mut Vec<ModelResult<StreamChunk>>) {
        for line in event.lines() {
            if let Some(payload) = line.strip_prefix("data:") {
                let payload = payload.trim_start();
                if payload == "[DONE]" {
                    out.push(Ok(StreamChunk::Done));
                } else if !payload.is_empty() {
                    match parse_data_payload(payload) {
                        Ok(chunks) => out.extend(chunks.into_iter().map(Ok)),
                        Err(e) => out.push(Err(e)),
                    }
                }
            }
        }
    }

    /// 喂入网络字节块，返回本次解析出的 chunks
    fn feed(&mut self, bytes: &[u8]) -> Vec<ModelResult<StreamChunk>> {
        self.buffer.push_str(&String::from_utf8_lossy(bytes));
        let mut out = Vec::new();
        while let Some(pos) = self.buffer.find("\n\n") {
            let event: String = self.buffer.drain(..pos).collect();
            // 去掉分隔用的 "\n\n"
            self.buffer.drain(..2.min(self.buffer.len()));
            self.handle_event(&event, &mut out);
        }
        out
    }

    /// 流结束时冲刷残留缓冲区（末尾可能没有 \n\n）
    fn finish(&mut self) -> Vec<ModelResult<StreamChunk>> {
        let mut out = Vec::new();
        let rest = std::mem::take(&mut self.buffer);
        let trimmed = rest.trim();
        if !trimmed.is_empty() {
            self.handle_event(trimmed, &mut out);
        }
        out
    }
}

#[async_trait::async_trait]
impl ModelProvider for OpenAiCompatProvider {
    fn id(&self) -> &str {
        "openai_compat"
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
    ) -> ModelResult<ChunkStream> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = build_request_body(&self.model, messages, tools);

        // 建连阶段重试：429/5xx 指数退避（1s/2s/4s），流开始后不重试
        let mut attempt: u32 = 0;
        let response = loop {
            let result = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await;
            match result {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        break resp;
                    }
                    let retryable = status.as_u16() == 429 || status.is_server_error();
                    if retryable && attempt < MAX_RETRIES {
                        tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                        attempt += 1;
                        continue;
                    }
                    let text = resp.text().await.unwrap_or_default();
                    return Err(format!("HTTP {status}: {text}").into());
                }
                Err(e) => {
                    if attempt < MAX_RETRIES {
                        tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(e.into());
                }
            }
        };

        // 字节流装箱，避免依赖 reqwest 内部流类型
        type ByteStream = std::pin::Pin<
            Box<dyn futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send>,
        >;
        let byte_stream: ByteStream = Box::pin(response.bytes_stream());
        type State = (
            ByteStream,
            SseParser,
            VecDeque<ModelResult<StreamChunk>>,
            bool,
        );
        let stream = futures_util::stream::unfold(
            (byte_stream, SseParser::new(), VecDeque::new(), false),
            |(mut bs, mut parser, mut pending, mut finished): State| async move {
                loop {
                    if let Some(item) = pending.pop_front() {
                        return Some((item, (bs, parser, pending, finished)));
                    }
                    if finished {
                        return None;
                    }
                    match bs.next().await {
                        Some(Ok(bytes)) => {
                            pending.extend(parser.feed(&bytes));
                        }
                        Some(Err(e)) => {
                            finished = true;
                            pending.push_back(Err(e.into()));
                        }
                        None => {
                            finished = true;
                            pending.extend(parser.finish());
                        }
                    }
                }
            },
        );
        Ok(Box::pin(stream))
    }

    fn parse_response(&self, chunks: &[StreamChunk]) -> ModelResult<(String, Vec<ToolCall>)> {
        let mut text = String::new();
        // 按 index 分组拼装工具调用（BTreeMap 保证按 index 排序输出）
        let mut calls: BTreeMap<usize, (Option<String>, Option<String>, String)> = BTreeMap::new();
        for chunk in chunks {
            match chunk {
                StreamChunk::Delta(s) => text.push_str(s),
                StreamChunk::ToolCallDelta {
                    index,
                    id,
                    name,
                    arguments_delta,
                } => {
                    let entry = calls.entry(*index).or_default();
                    // id / name 取第一个 Some
                    if entry.0.is_none() {
                        entry.0 = id.clone();
                    }
                    if entry.1.is_none() {
                        entry.1 = name.clone();
                    }
                    entry.2.push_str(arguments_delta);
                }
                StreamChunk::Done => {}
            }
        }
        let tool_calls = calls
            .into_iter()
            .map(|(_, (id, name, arguments))| ToolCall {
                id: id.unwrap_or_default(),
                name: name.unwrap_or_default(),
                arguments,
            })
            .collect();
        Ok((text, tool_calls))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 完整的 SSE 流文本（文本增量 + 工具调用分片 + 结束）
    const SSE_TEXT: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"\"}}]}}]}\n\ndata: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"cm\"}}]}}]}\n\ndata: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"d\\\":\\\"ls\\\"}\"}}]}}]}\n\ndata: {\"choices\":[{\"finish_reason\":\"tool_calls\",\"delta\":{}}]}\n\ndata: [DONE]\n\n";

    /// 从 SSE 文本提取所有 Ok chunk（喂入整块）
    fn parse_all(text: &str) -> Vec<StreamChunk> {
        let mut parser = SseParser::new();
        let results: Vec<ModelResult<StreamChunk>> = parser
            .feed(text.as_bytes())
            .into_iter()
            .chain(parser.finish())
            .collect();
        results
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .expect("SSE 解析失败")
    }

    #[test]
    fn test_sse_parse_full_stream() {
        let chunks = parse_all(SSE_TEXT);
        assert_eq!(chunks.len(), 5);
        match &chunks[0] {
            StreamChunk::Delta(s) => assert_eq!(s, "hello"),
            other => panic!("期望 Delta，实际 {other:?}"),
        }
        match &chunks[1] {
            StreamChunk::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => {
                assert_eq!(*index, 0);
                assert_eq!(id.as_deref(), Some("call_1"));
                assert_eq!(name.as_deref(), Some("bash"));
                assert_eq!(arguments_delta, "");
            }
            other => panic!("期望 ToolCallDelta，实际 {other:?}"),
        }
        match &chunks[2] {
            StreamChunk::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => {
                assert_eq!(*index, 0);
                assert!(id.is_none());
                assert!(name.is_none());
                assert_eq!(arguments_delta, "{\"cm");
            }
            other => panic!("期望 ToolCallDelta，实际 {other:?}"),
        }
        match &chunks[3] {
            StreamChunk::ToolCallDelta {
                arguments_delta, ..
            } => assert_eq!(arguments_delta, "d\":\"ls\"}"),
            other => panic!("期望 ToolCallDelta，实际 {other:?}"),
        }
        assert!(matches!(chunks[4], StreamChunk::Done));
    }

    #[test]
    fn test_sse_parse_cross_chunk_boundary() {
        // 把一个 JSON 事件拆到两个网络块里，验证缓冲区能正确拼装
        let mid = SSE_TEXT.len() / 2;
        let mut parser = SseParser::new();
        let mut results = parser.feed(SSE_TEXT[..mid].as_bytes());
        results.extend(parser.feed(SSE_TEXT[mid..].as_bytes()));
        results.extend(parser.finish());
        let chunks: Vec<StreamChunk> = results
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .expect("SSE 解析失败");
        assert_eq!(chunks.len(), 5);
        assert!(matches!(chunks[4], StreamChunk::Done));
        match &chunks[0] {
            StreamChunk::Delta(s) => assert_eq!(s, "hello"),
            other => panic!("期望 Delta，实际 {other:?}"),
        }
    }

    #[test]
    fn test_parse_response_mixed() {
        let provider = OpenAiCompatProvider::new("https://api.example.com/v1", "k", "m");
        let chunks = vec![
            StreamChunk::Delta("我来执行".to_string()),
            StreamChunk::Delta("命令".to_string()),
            StreamChunk::ToolCallDelta {
                index: 1,
                id: Some("call_2".to_string()),
                name: Some("read".to_string()),
                arguments_delta: String::new(),
            },
            StreamChunk::ToolCallDelta {
                index: 0,
                id: Some("call_1".to_string()),
                name: Some("bash".to_string()),
                arguments_delta: String::new(),
            },
            StreamChunk::ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments_delta: "{\"cmd\":".to_string(),
            },
            StreamChunk::ToolCallDelta {
                index: 1,
                id: None,
                name: None,
                arguments_delta: "{\"path\":\"a.txt\"}".to_string(),
            },
            StreamChunk::ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments_delta: "\"ls\"}".to_string(),
            },
            StreamChunk::Done,
        ];
        let (text, calls) = provider.parse_response(&chunks).unwrap();
        assert_eq!(text, "我来执行命令");
        // 按 index 排序
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].arguments, "{\"cmd\":\"ls\"}");
        assert_eq!(calls[1].id, "call_2");
        assert_eq!(calls[1].name, "read");
        assert_eq!(calls[1].arguments, "{\"path\":\"a.txt\"}");
    }

    #[test]
    fn test_message_serialization_tool_role() {
        let msg = Message {
            role: Role::Tool,
            content: "输出结果".to_string(),
            tool_calls: None,
            tool_call_id: Some("call_1".to_string()),
        };
        let json = message_to_json(&msg);
        assert_eq!(json["role"], "tool");
        assert_eq!(json["tool_call_id"], "call_1");
        assert_eq!(json["content"], "输出结果");
        assert!(json.get("tool_calls").is_none());
    }

    #[test]
    fn test_message_serialization_assistant_with_tool_calls() {
        let msg = Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: Some(vec![ToolCall {
                id: "call_1".to_string(),
                name: "bash".to_string(),
                arguments: "{\"cmd\":\"ls\"}".to_string(),
            }]),
            tool_call_id: None,
        };
        let json = message_to_json(&msg);
        assert_eq!(json["role"], "assistant");
        assert_eq!(json["content"], "");
        assert_eq!(json["tool_calls"][0]["id"], "call_1");
        assert_eq!(json["tool_calls"][0]["type"], "function");
        assert_eq!(json["tool_calls"][0]["function"]["name"], "bash");
        assert_eq!(
            json["tool_calls"][0]["function"]["arguments"],
            "{\"cmd\":\"ls\"}"
        );
    }

    #[test]
    fn test_request_body_tools_omitted_when_empty() {
        let msgs = vec![Message {
            role: Role::User,
            content: "hi".to_string(),
            tool_calls: None,
            tool_call_id: None,
        }];
        let body = build_request_body("glm-5.2", &msgs, &[]);
        assert!(body.get("tools").is_none());
        assert_eq!(body["stream"], true);
        assert_eq!(body["model"], "glm-5.2");
        assert_eq!(body["messages"][0]["role"], "user");
    }
}
