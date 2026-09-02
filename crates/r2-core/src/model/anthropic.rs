//! Anthropic Messages API 协议 Provider

use super::{ChunkStream, ModelProvider, ModelResult};
use crate::types::{Message, Role, StreamChunk, ToolCall, ToolSchema};
use futures_util::StreamExt;
use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

/// 建连超时
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// 最大重试次数（不含首次请求）
const MAX_RETRIES: u32 = 2;
/// Anthropic API 版本头
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// max_tokens（Anthropic 强制要求）
const MAX_TOKENS: u32 = 4096;

/// Anthropic Messages API Provider
pub struct AnthropicProvider {
    /// API 基础地址（不含尾部斜杠）
    base_url: String,
    /// API 密钥
    api_key: String,
    /// 模型名称
    model: String,
    /// HTTP 客户端
    client: reqwest::Client,
}

impl AnthropicProvider {
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

/// 把内部 Message 列表转成 Anthropic 协议的 (system, messages)
///
/// Anthropic 与 OpenAI 的重大差异：
/// - system 是顶层字段，不在 messages 数组里
/// - assistant 的 tool_calls 要转成 content 数组里的 tool_use 块
/// - tool 结果转成 role=user + content 数组里的 tool_result 块
/// - role 必须 user/assistant 交替，连续多个 tool 结果要合并进同一条 user 消息
fn messages_to_anthropic(messages: &[Message]) -> (Option<String>, Vec<serde_json::Value>) {
    // 提取所有 system 消息文本，拼接为顶层 system 字段
    let system_texts: Vec<&str> = messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| m.content.as_str())
        .collect();
    let system = if system_texts.is_empty() {
        None
    } else {
        Some(system_texts.join("\n"))
    };

    let mut msgs: Vec<serde_json::Value> = Vec::new();
    for msg in messages {
        match msg.role {
            Role::System => {}
            Role::User => {
                msgs.push(serde_json::json!({
                    "role": "user",
                    "content": msg.content,
                }));
            }
            Role::Assistant => {
                if let Some(calls) = &msg.tool_calls {
                    // assistant 携带工具调用：content 为 text + tool_use 块数组
                    let mut content: Vec<serde_json::Value> = Vec::new();
                    if !msg.content.is_empty() {
                        content.push(serde_json::json!({
                            "type": "text",
                            "text": msg.content,
                        }));
                    }
                    for c in calls {
                        // arguments 是 JSON 字符串，解析为对象放入 input
                        let input: serde_json::Value =
                            serde_json::from_str(&c.arguments).unwrap_or(serde_json::json!({}));
                        content.push(serde_json::json!({
                            "type": "tool_use",
                            "id": c.id,
                            "name": c.name,
                            "input": input,
                        }));
                    }
                    msgs.push(serde_json::json!({
                        "role": "assistant",
                        "content": content,
                    }));
                } else {
                    msgs.push(serde_json::json!({
                        "role": "assistant",
                        "content": msg.content,
                    }));
                }
            }
            Role::Tool => {
                // tool 结果：role=user + content 数组里的 tool_result 块
                let block = serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": msg.tool_call_id.clone().unwrap_or_default(),
                    "content": msg.content,
                });
                // 相邻的 tool 结果合并到上一条 user 消息（Anthropic 要求 role 严格交替）
                let mergeable = matches!(
                    msgs.last(),
                    Some(last) if last["role"] == "user" && last["content"].is_array()
                );
                if mergeable {
                    msgs.last_mut().unwrap()["content"]
                        .as_array_mut()
                        .unwrap()
                        .push(block);
                } else {
                    msgs.push(serde_json::json!({
                        "role": "user",
                        "content": [block],
                    }));
                }
            }
        }
    }
    (system, msgs)
}

/// 构建 /v1/messages 请求体（tools 为空时不带 tools 字段）
fn build_request_body(model: &str, messages: &[Message], tools: &[ToolSchema]) -> serde_json::Value {
    let (system, msgs) = messages_to_anthropic(messages);
    let mut body = serde_json::json!({
        "model": model,
        "messages": msgs,
        "max_tokens": MAX_TOKENS,
        "stream": true,
    });
    if let Some(system) = system {
        // prompt caching 断点打在 system 末尾：system 是会话内最大稳定前缀，
        // 断点在此可覆盖 全部 system 内容 进入 KV-cache（messages/tools 结构不动）
        body["system"] = serde_json::json!([{
            "type": "text",
            "text": system,
            "cache_control": {"type": "ephemeral"},
        }]);
    }
    if !tools.is_empty() {
        // 注意：Anthropic 参数字段叫 input_schema（OpenAI 叫 parameters）
        let arr: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                })
            })
            .collect();
        body["tools"] = serde_json::Value::Array(arr);
    }
    body
}

/// 解析单个 SSE data 事件 payload，产出 0..n 个 StreamChunk
///
/// 只看 data: 行 JSON 里的 type 字段，event: 行本身不需要解析
fn parse_data_payload(payload: &str) -> ModelResult<Vec<StreamChunk>> {
    let v: serde_json::Value = serde_json::from_str(payload)?;
    let mut chunks = Vec::new();
    match v["type"].as_str().unwrap_or("") {
        "content_block_start" => {
            // tool_use 块开始：携带 id 和 name，arguments 增量为空
            if v["content_block"]["type"] == "tool_use" {
                let index = v["index"].as_u64().unwrap_or(0) as usize;
                let id = v["content_block"]["id"].as_str().map(|s| s.to_string());
                let name = v["content_block"]["name"].as_str().map(|s| s.to_string());
                chunks.push(StreamChunk::ToolCallDelta {
                    index,
                    id,
                    name,
                    arguments_delta: String::new(),
                });
            }
        }
        "content_block_delta" => {
            let index = v["index"].as_u64().unwrap_or(0) as usize;
            match v["delta"]["type"].as_str().unwrap_or("") {
                "text_delta" => {
                    if let Some(text) = v["delta"]["text"].as_str() {
                        if !text.is_empty() {
                            chunks.push(StreamChunk::Delta(text.to_string()));
                        }
                    }
                }
                "input_json_delta" => {
                    let partial = v["delta"]["partial_json"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    chunks.push(StreamChunk::ToolCallDelta {
                        index,
                        id: None,
                        name: None,
                        arguments_delta: partial,
                    });
                }
                _ => {}
            }
        }
        "message_start" => {
            // message 对象携带 input 侧真实用量（含缓存读/写明细）
            let usage = &v["message"]["usage"];
            let input_tokens = usage["input_tokens"].as_u64().unwrap_or(0);
            let cached_tokens = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
            if input_tokens > 0 || cached_tokens > 0 {
                chunks.push(StreamChunk::Usage {
                    input_tokens,
                    output_tokens: 0,
                    cached_tokens,
                });
            }
        }
        "message_delta" => {
            // 流尾部携带累计 output_tokens（真实值，覆盖客户端估算）
            let output_tokens = v["usage"]["output_tokens"].as_u64().unwrap_or(0);
            if output_tokens > 0 {
                chunks.push(StreamChunk::Usage {
                    input_tokens: 0,
                    output_tokens,
                    cached_tokens: 0,
                });
            }
        }
        "message_stop" => chunks.push(StreamChunk::Done),
        // content_block_stop / ping 等忽略
        _ => {}
    }
    Ok(chunks)
}

/// SSE 增量解析器：维护缓冲区，按事件边界（\n\n）切分，处理跨网络块的事件

/// 字节缓冲中找 SSE 事件分隔符 b"\n\n" 的位置（切分点天然在 ASCII 边界）
fn find_event_sep(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
}

struct SseParser {
    /// 未完整事件的残留字节（字节级：切分前不转 String，防多字节字符被
    /// 网络 chunk 边界切开产生 U+FFFD）
    buffer: Vec<u8>,
}

impl SseParser {
    fn new() -> Self {
        Self {
            buffer: Vec::new(),
        }
    }

    /// 处理单个完整事件文本（容忍 event: 行，直接跳过）
    fn handle_event(&mut self, event: &str, out: &mut Vec<ModelResult<StreamChunk>>) {
        for line in event.lines() {
            if let Some(payload) = line.strip_prefix("data:") {
                let payload = payload.trim_start();
                if !payload.is_empty() {
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
        // 字节级缓冲：网络 chunk 边界不保证落在 UTF-8 字符边界——先转 String
        // 会把被 TCP 切开的汉字永久替换成 U+FFFD（用户实测"两个问号"根因）。
        // 攒字节、按完整事件（\n\n 分隔，ASCII 安全边界）切出后再转字符串
        self.buffer.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(pos) = find_event_sep(&self.buffer) {
            let event_bytes: Vec<u8> = self.buffer.drain(..pos).collect();
            // 去掉分隔用的 "\n\n"
            self.buffer.drain(..2.min(self.buffer.len()));
            let event = String::from_utf8_lossy(&event_bytes).into_owned();
            self.handle_event(&event, &mut out);
        }
        out
    }

    /// 流结束时冲刷残留缓冲区（末尾可能没有 \n\n）
    fn finish(&mut self) -> Vec<ModelResult<StreamChunk>> {
        let mut out = Vec::new();
        let rest = String::from_utf8_lossy(&self.buffer).into_owned();
        self.buffer.clear();
        let trimmed = rest.trim();
        if !trimmed.is_empty() {
            self.handle_event(trimmed, &mut out);
        }
        out
    }
}

#[async_trait::async_trait]
impl ModelProvider for AnthropicProvider {
    fn id(&self) -> &str {
        "anthropic"
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
    ) -> ModelResult<ChunkStream> {
        let url = format!("{}/v1/messages", self.base_url);
        let body = build_request_body(&self.model, messages, tools);

        // 建连阶段重试：429/5xx 指数退避（1s/2s/4s），流开始后不重试
        let mut attempt: u32 = 0;
        let response = loop {
            let result = self
                .client
                .post(&url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("Content-Type", "application/json")
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
                StreamChunk::Reasoning(_) => {} // 思考不进正文（仅展示/用量，agent 层已计）
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
                StreamChunk::Usage { .. } => {} // 用量由 agent 层校正，不进正文/工具拼装
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

    /// 完整的 SSE 流文本（text + tool_use 开始 + input_json_delta 分片 + message_stop）
    const SSE_TEXT: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"role\":\"assistant\"}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"我需要执行命令\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01\",\"name\":\"bash\",\"input\":{}}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"cmd\\\":\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"ls\\\"}\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

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
        // Delta(text) + ToolCallDelta(start) + 2 × ToolCallDelta(json) + Done
        assert_eq!(chunks.len(), 5);
        match &chunks[0] {
            StreamChunk::Delta(s) => assert_eq!(s, "我需要执行命令"),
            other => panic!("期望 Delta，实际 {other:?}"),
        }
        match &chunks[1] {
            StreamChunk::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => {
                assert_eq!(*index, 1);
                assert_eq!(id.as_deref(), Some("toolu_01"));
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
                assert_eq!(*index, 1);
                assert!(id.is_none());
                assert!(name.is_none());
                assert_eq!(arguments_delta, "{\"cmd\":");
            }
            other => panic!("期望 ToolCallDelta，实际 {other:?}"),
        }
        match &chunks[3] {
            StreamChunk::ToolCallDelta {
                arguments_delta, ..
            } => assert_eq!(arguments_delta, "\"ls\"}"),
            other => panic!("期望 ToolCallDelta，实际 {other:?}"),
        }
        assert!(matches!(chunks[4], StreamChunk::Done));
    }

    /// 恶意/畸形 SSE 输入 fuzz：解析器绝不能 panic（产出什么都行，核心是不崩）
    #[test]
    fn test_sse_malformed_never_panics() {
        let cases: Vec<&str> = vec![
            "",                                    // 空
            "data:",                               // 空 payload
            "data: \n\n",                          // 空白 payload
            "data: {broken json\n\n",              // 半截 JSON
            "data: {}\n\n",                        // type 字段缺失
            "data: {\"type\":null}\n\n",           // type 为 null
            "data: {\"type\":123}\n\n",            // type 为数字
            "data: {\"type\":\"content_block_delta\"}\n\n",  // 缺 index / delta
            "data: {\"type\":\"content_block_delta\",\"index\":\"abc\"}\n\n",  // index 类型错
            "data: {\"type\":\"content_block_delta\",\"index\":-1,\"delta\":null}\n\n",  // 负 index + null delta
            "data: {\"type\":\"content_block_start\",\"content_block\":{\"type\":\"tool_use\"},\"index\":-1}\n\n",
            "data: {\"type\":\"content_block_start\",\"content_block\":null}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",  // 正常停止
            "data: \x00\x01binary garbage\n\n",    // 二进制垃圾
            // 孤代理对（serde_json 应解析失败而非 panic）
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"\\ud800\"}}\n\n",
            "event: message\ndata: {}\ndata: not json\n\n",  // 事件行混合
            "\n\n\n\ndata: {}\n\n\n",              // 多空行
        ];
        for case in &cases {
            // 整块喂入
            let mut parser = SseParser::new();
            let mut out = parser.feed(case.as_bytes());
            out.extend(parser.finish());
            drop(out);
            // 逐字节喂入（跨块边界也不许崩）
            let mut parser = SseParser::new();
            for b in case.as_bytes() {
                drop(parser.feed(&[*b]));
            }
            drop(parser.finish());
        }
    }

    #[test]
    fn test_request_body_system_has_cache_control_breakpoint() {
        let msgs = vec![
            Message {
                role: Role::System,
                content: "你是 R2".to_string(),
                tool_calls: None,
                tool_call_id: None,
            },
            Message {
                role: Role::User,
                content: "hi".to_string(),
                tool_calls: None,
                tool_call_id: None,
            },
        ];
        let body = build_request_body("claude-sonnet-4-20250514", &msgs, &[]);
        let system = body["system"].as_array().expect("system 应为块数组");
        assert_eq!(system.len(), 1);
        assert_eq!(system[0]["type"], "text");
        assert_eq!(system[0]["text"], "你是 R2");
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn test_sse_parse_message_start_and_delta_usage() {
        // message_start 带 input 侧用量（含缓存明细），message_delta 带 output
        let text = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"role\":\"assistant\",\"usage\":{\"input_tokens\":1500,\"cache_read_input_tokens\":1200,\"cache_creation_input_tokens\":300,\"output_tokens\":1}}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":42}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let chunks = parse_all(text);
        assert_eq!(chunks.len(), 4);
        match &chunks[0] {
            StreamChunk::Usage {
                input_tokens,
                output_tokens,
                cached_tokens,
            } => {
                assert_eq!(*input_tokens, 1500);
                assert_eq!(*output_tokens, 0);
                assert_eq!(*cached_tokens, 1200);
            }
            other => panic!("期望 Usage(message_start)，实际 {other:?}"),
        }
        match &chunks[2] {
            StreamChunk::Usage {
                input_tokens,
                output_tokens,
                cached_tokens,
            } => {
                assert_eq!(*input_tokens, 0);
                assert_eq!(*output_tokens, 42);
                assert_eq!(*cached_tokens, 0);
            }
            other => panic!("期望 Usage(message_delta)，实际 {other:?}"),
        }
        assert!(matches!(chunks[3], StreamChunk::Done));
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
            StreamChunk::Delta(s) => assert_eq!(s, "我需要执行命令"),
            other => panic!("期望 Delta，实际 {other:?}"),
        }
    }

    #[test]
    fn test_messages_system_extracted_to_top_level() {
        let msgs = vec![
            Message {
                role: Role::System,
                content: "你是 R2".to_string(),
                tool_calls: None,
                tool_call_id: None,
            },
            Message {
                role: Role::User,
                content: "hi".to_string(),
                tool_calls: None,
                tool_call_id: None,
            },
        ];
        let (system, converted) = messages_to_anthropic(&msgs);
        assert_eq!(system.as_deref(), Some("你是 R2"));
        // system 不进 messages 数组
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0]["role"], "user");
        assert_eq!(converted[0]["content"], "hi");
    }

    #[test]
    fn test_messages_assistant_tool_calls_to_tool_use() {
        let msgs = vec![Message {
            role: Role::Assistant,
            content: "我需要执行命令".to_string(),
            tool_calls: Some(vec![ToolCall {
                id: "toolu_01".to_string(),
                name: "bash".to_string(),
                arguments: "{\"cmd\":\"ls\"}".to_string(),
            }]),
            tool_call_id: None,
        }];
        let (_, converted) = messages_to_anthropic(&msgs);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0]["role"], "assistant");
        let content = converted[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "我需要执行命令");
        assert_eq!(content[1]["type"], "tool_use");
        assert_eq!(content[1]["id"], "toolu_01");
        assert_eq!(content[1]["name"], "bash");
        // arguments 字符串被解析为 input 对象
        assert_eq!(content[1]["input"]["cmd"], "ls");
    }

    #[test]
    fn test_messages_consecutive_tool_results_merged() {
        let msgs = vec![
            Message {
                role: Role::Assistant,
                content: String::new(),
                tool_calls: Some(vec![
                    ToolCall {
                        id: "toolu_01".to_string(),
                        name: "bash".to_string(),
                        arguments: "{}".to_string(),
                    },
                    ToolCall {
                        id: "toolu_02".to_string(),
                        name: "read".to_string(),
                        arguments: "{}".to_string(),
                    },
                ]),
                tool_call_id: None,
            },
            Message {
                role: Role::Tool,
                content: "输出一".to_string(),
                tool_calls: None,
                tool_call_id: Some("toolu_01".to_string()),
            },
            Message {
                role: Role::Tool,
                content: "输出二".to_string(),
                tool_calls: None,
                tool_call_id: Some("toolu_02".to_string()),
            },
        ];
        let (_, converted) = messages_to_anthropic(&msgs);
        // assistant + 合并后的一条 user 消息
        assert_eq!(converted.len(), 2);
        assert_eq!(converted[0]["role"], "assistant");
        assert_eq!(converted[1]["role"], "user");
        let content = converted[1]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[0]["tool_use_id"], "toolu_01");
        assert_eq!(content[0]["content"], "输出一");
        assert_eq!(content[1]["type"], "tool_result");
        assert_eq!(content[1]["tool_use_id"], "toolu_02");
        assert_eq!(content[1]["content"], "输出二");
    }

    #[test]
    fn test_request_body_tools_serialization() {
        let tools = vec![ToolSchema {
            name: "bash".to_string(),
            description: "执行命令".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"cmd": {"type": "string"}},
            }),
        }];
        let msgs = vec![Message {
            role: Role::User,
            content: "hi".to_string(),
            tool_calls: None,
            tool_call_id: None,
        }];
        let body = build_request_body("claude-sonnet-4-20250514", &msgs, &tools);
        assert_eq!(body["model"], "claude-sonnet-4-20250514");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], MAX_TOKENS);
        // tools 格式：name / description / input_schema（不是 parameters）
        assert_eq!(body["tools"][0]["name"], "bash");
        assert_eq!(body["tools"][0]["description"], "执行命令");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert!(body["tools"][0].get("parameters").is_none());
    }

    #[test]
    fn test_request_body_tools_omitted_when_empty() {
        let msgs = vec![Message {
            role: Role::User,
            content: "hi".to_string(),
            tool_calls: None,
            tool_call_id: None,
        }];
        let body = build_request_body("claude-sonnet-4-20250514", &msgs, &[]);
        assert!(body.get("tools").is_none());
        assert!(body.get("system").is_none());
    }

    #[test]
    fn test_parse_response_mixed() {
        let provider = AnthropicProvider::new("https://api.anthropic.com", "k", "m");
        let chunks = vec![
            StreamChunk::Delta("我来执行".to_string()),
            StreamChunk::Delta("命令".to_string()),
            StreamChunk::ToolCallDelta {
                index: 2,
                id: Some("toolu_02".to_string()),
                name: Some("read".to_string()),
                arguments_delta: String::new(),
            },
            StreamChunk::ToolCallDelta {
                index: 1,
                id: Some("toolu_01".to_string()),
                name: Some("bash".to_string()),
                arguments_delta: String::new(),
            },
            StreamChunk::ToolCallDelta {
                index: 1,
                id: None,
                name: None,
                arguments_delta: "{\"cmd\":".to_string(),
            },
            StreamChunk::ToolCallDelta {
                index: 2,
                id: None,
                name: None,
                arguments_delta: "{\"path\":\"a.txt\"}".to_string(),
            },
            StreamChunk::ToolCallDelta {
                index: 1,
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
        assert_eq!(calls[0].id, "toolu_01");
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].arguments, "{\"cmd\":\"ls\"}");
        assert_eq!(calls[1].id, "toolu_02");
        assert_eq!(calls[1].name, "read");
        assert_eq!(calls[1].arguments, "{\"path\":\"a.txt\"}");
    }
}
