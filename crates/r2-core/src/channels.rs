//! 飞书 Provider 传输层（v0.10.0-A，纯传输，不含 agent 路由）
//!
//! 每个 agent 绑一个飞书机器人做私聊闭环。本模块只负责：
//!   1. tenant_access_token 获取/缓存/自动刷新（HTTP）
//!   2. WS 长连接（接入点协商 → protobuf 帧 → 心跳 → 分片重组 → ACK → 重连）
//!   3. 文本消息下发（HTTP，4000 字符自动分片）
//!
//! 协议规格逆向自 @larksuiteoapi/node-sdk：
//!   - 接入点：POST {domain}/callback/ws/endpoint 拿 wss URL + 心跳/重连参数
//!   - 帧格式：Frame protobuf（本文件 pb 模块手写最小编解码，不引 prost）
//!   - 心跳：每 PingInterval 秒发 control 帧 type=ping，2 倍间隔无入站则主动断连
//!   - 事件：data 帧可能分片（sum/seq），重组后必须原帧回敬 ACK 并追加 biz_rt，
//!     否则服务端会重投；重投用 message_id LRU（512 条）去重
//!
//! 全部 API 不 panic，错误走 Result<_, String>，日志用 eprintln!（项目无 log 框架）。

use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message;

/// 默认域名（海外版传 https://open.larksuite.com）
const DEFAULT_DOMAIN: &str = "https://open.feishu.cn";
/// PingInterval 最小钳制（防服务端给 0 导致心跳风暴）
const MIN_PING_INTERVAL_SECS: u64 = 10;
/// 心跳超时倍数：超过 N×PingInterval 无入站帧 → 主动断开重连
const HEARTBEAT_TIMEOUT_FACTOR: u64 = 2;
/// message_id 去重 LRU 容量
const DEDUP_CAP: usize = 512;
/// 分片缓存过期（秒）：超时未齐的碎片丢弃，防内存泄漏
const FRAG_TTL_SECS: u64 = 60;
/// 单条消息正文最大字符数（超出自动分片逐条发送）
const MAX_TEXT_CHARS: usize = 4000;
/// token 过期前提前刷新余量（秒）
const TOKEN_REFRESH_MARGIN_SECS: u64 = 300;
/// token 获取失败重试次数
const TOKEN_RETRY: usize = 3;
/// 鉴权类错误码：接入点协商遇之不可重试
const FATAL_AUTH_CODES: &[i64] = &[99992402, 99991663, 99991664];

/// 飞书机器人配置
#[derive(Debug, Clone)]
pub struct FeishuConfig {
    pub app_id: String,
    pub app_secret: String,
    /// 默认 https://open.feishu.cn，空串自动补默认
    pub domain: String,
}

/// 一条私聊消息（回调给上层）
#[derive(Debug, Clone, Serialize)]
pub struct FeishuDm {
    pub open_id: String,
    pub message_id: String,
    /// message_type=="text" 时解析出的正文；其它类型为空串
    pub text: String,
}

/// 通道状态
#[derive(Debug, Clone, Serialize, PartialEq)]
pub enum ChannelStatus {
    Connecting,
    Connected,
    Reconnecting,
    Failed(String),
}

/// 心跳/重连运行参数（接入点协商 + pong 帧可动态更新，单位秒）
#[derive(Debug, Clone, Copy)]
struct ClientParams {
    ping_interval: u64,
    reconnect_count: i64,
    reconnect_interval: u64,
    reconnect_nonce: u64,
}

impl Default for ClientParams {
    fn default() -> Self {
        Self {
            ping_interval: 30,
            reconnect_count: 3600,
            reconnect_interval: 15,
            reconnect_nonce: 3,
        }
    }
}

impl ClientParams {
    /// ping 间隔最小钳制
    fn clamped(mut self) -> Self {
        if self.ping_interval < MIN_PING_INTERVAL_SECS {
            self.ping_interval = MIN_PING_INTERVAL_SECS;
        }
        self
    }
}

/// 接入点协商结果
struct EndpointInfo {
    url: String,
    service_id: i32,
    params: ClientParams,
}

/// 接入点协商错误：鉴权类不可重试，其它可重试
enum EndpointError {
    Fatal(String),
    Retry(String),
}

/// 缓存的 tenant_access_token
struct CachedToken {
    token: String,
    expire_at: Instant,
}

/// message_id 去重 LRU：check_and_insert 首次出现返回 true（应处理），重复返回 false
struct DedupLru {
    cap: usize,
    list: VecDeque<String>,
}

impl DedupLru {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            list: VecDeque::new(),
        }
    }

    fn check_and_insert(&mut self, id: &str) -> bool {
        if id.is_empty() {
            return true; // 无 id 不参与去重，直接放行
        }
        if self.list.iter().any(|x| x == id) {
            return false;
        }
        self.list.push_back(id.to_string());
        while self.list.len() > self.cap {
            self.list.pop_front();
        }
        true
    }
}

/// 事件分片重组缓存：message_id -> (sum, seq->bytes)
/// 按 SDK 的 DataCache 语义：buffer[seq]=data，全齐后按 seq 升序拼接
struct FragmentCache {
    map: HashMap<String, FragEntry>,
}

struct FragEntry {
    sum: u32,
    parts: HashMap<u32, Vec<u8>>,
    at: Instant,
}

impl FragmentCache {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    /// 塞入一片；齐了返回完整 payload，未齐返回 None
    fn insert(&mut self, id: &str, sum: u32, seq: u32, data: Vec<u8>) -> Option<Vec<u8>> {
        if sum <= 1 {
            return Some(data);
        }
        // 顺带清理过期碎片，防对端异常导致内存泄漏
        self.map
            .retain(|_, e| e.at.elapsed() < Duration::from_secs(FRAG_TTL_SECS));
        let entry = self.map.entry(id.to_string()).or_insert_with(|| FragEntry {
            sum,
            parts: HashMap::new(),
            at: Instant::now(),
        });
        entry.parts.insert(seq, data);
        if (entry.parts.len() as u32) >= entry.sum {
            let mut keys: Vec<u32> = entry.parts.keys().copied().collect();
            keys.sort_unstable();
            let mut out = Vec::new();
            for k in keys {
                if let Some(p) = entry.parts.get(&k) {
                    out.extend_from_slice(p);
                }
            }
            self.map.remove(id);
            Some(out)
        } else {
            None
        }
    }
}

/// 内部共享状态
struct Inner {
    cfg: FeishuConfig,
    http: reqwest::Client,
    status: Mutex<ChannelStatus>,
    params: Mutex<ClientParams>,
    token: Mutex<Option<CachedToken>>,
    dedup: Mutex<DedupLru>,
    on_message: Mutex<Option<Arc<dyn Fn(FeishuDm) + Send + Sync>>>,
    on_status: Mutex<Option<Arc<dyn Fn(ChannelStatus) + Send + Sync>>>,
    /// CancellationToken 语义：置位 + Notify 唤醒所有阻塞点
    stop_flag: AtomicBool,
    notify: Notify,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Inner {
    fn stopped(&self) -> bool {
        self.stop_flag.load(Ordering::SeqCst)
    }

    fn set_status(&self, st: ChannelStatus) {
        let cb = {
            let mut s = self.status.lock().unwrap();
            if *s == st {
                return; // 状态没变不回调，防抖
            }
            *s = st.clone();
            self.on_status.lock().unwrap().clone()
        };
        if let Some(cb) = cb {
            cb(st);
        }
    }

    fn emit_message(&self, dm: FeishuDm) {
        let cb = self.on_message.lock().unwrap().clone();
        if let Some(cb) = cb {
            cb(dm);
        }
    }

    fn current_params(&self) -> ClientParams {
        self.params.lock().unwrap().clamped()
    }

    fn update_params(&self, p: ClientParams) {
        *self.params.lock().unwrap() = p.clamped();
    }

    /// 可中断睡眠：stop 信号立即返回 false（表示被打断）
    async fn interruptible_sleep(&self, secs: u64) -> bool {
        if secs == 0 {
            return !self.stopped();
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(secs)) => !self.stopped(),
            _ = self.notify.notified() => false,
        }
    }
}

/// 飞书客户端：内部全 Arc，clone 廉价，生命周期由 stop/start 管理
pub struct FeishuClient {
    inner: Arc<Inner>,
}

impl FeishuClient {
    pub fn new(mut cfg: FeishuConfig) -> Self {
        if cfg.domain.is_empty() {
            cfg.domain = DEFAULT_DOMAIN.to_string();
        }
        Self {
            inner: Arc::new(Inner {
                cfg,
                http: reqwest::Client::new(),
                status: Mutex::new(ChannelStatus::Connecting),
                params: Mutex::new(ClientParams::default()),
                token: Mutex::new(None),
                dedup: Mutex::new(DedupLru::new(DEDUP_CAP)),
                on_message: Mutex::new(None),
                on_status: Mutex::new(None),
                stop_flag: AtomicBool::new(true),
                notify: Notify::new(),
                task: Mutex::new(None),
            }),
        }
    }

    /// 启动：spawn WS 循环任务。重复 start 先停旧的。
    /// 需在 tokio runtime 内调用。
    pub fn start(
        &self,
        on_message: Box<dyn Fn(FeishuDm) + Send + Sync>,
        on_status: Box<dyn Fn(ChannelStatus) + Send + Sync>,
    ) {
        self.stop();
        self.inner.stop_flag.store(false, Ordering::SeqCst);
        *self.inner.on_message.lock().unwrap() = Some(Arc::from(on_message));
        *self.inner.on_status.lock().unwrap() = Some(Arc::from(on_status));
        self.inner.set_status(ChannelStatus::Connecting);
        let inner = self.inner.clone();
        let handle = tokio::spawn(async move {
            ws_loop(inner).await;
        });
        *self.inner.task.lock().unwrap() = Some(handle);
    }

    /// 停止（幂等）
    pub fn stop(&self) {
        self.inner.stop_flag.store(true, Ordering::SeqCst);
        self.inner.notify.notify_waiters();
        if let Some(h) = self.inner.task.lock().unwrap().take() {
            h.abort();
        }
    }

    pub fn status(&self) -> ChannelStatus {
        self.inner.status.lock().unwrap().clone()
    }

    /// 发文本（自动按 4000 字符分片，逐条发送）
    /// 给指定消息贴表情（收到确认/完成确认用；需 im:message.reaction:write 权限）
    /// emoji_type 参考：Typing（输入中动画）/ THUMBSUP（👍）。
    /// 失败静默由调用方处理，不影响主链路
    pub async fn add_reaction(&self, message_id: &str, emoji_type: &str) -> Result<(), String> {
        let token = get_token(&self.inner).await?;
        let url = format!(
            "{}/open-apis/im/v1/messages/{message_id}/reactions",
            self.inner.cfg.domain
        );
        let body = serde_json::json!({ "reaction_type": { "emoji_type": emoji_type } });
        let resp = self
            .inner
            .http
            .post(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("ERROR: 贴表情请求失败: {e}"))?;
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("ERROR: 贴表情响应解析失败: {e}"))?;
        let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("unknown");
            return Err(format!("ERROR: 贴表情失败 code={code} msg={msg}"));
        }
        Ok(())
    }

    pub async fn send_text(&self, open_id: &str, text: &str) -> Result<(), String> {
        let token = get_token(&self.inner).await?;
        let url = format!(
            "{}/open-apis/im/v1/messages?receive_id_type=open_id",
            self.inner.cfg.domain
        );
        for chunk in chunk_text(text, MAX_TEXT_CHARS) {
            let content = serde_json::json!({ "text": chunk }).to_string();
            let body = serde_json::json!({
                "receive_id": open_id,
                "msg_type": "text",
                "content": content,
            });
            let resp = self
                .inner
                .http
                .post(&url)
                .bearer_auth(&token)
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("ERROR: 发送消息请求失败: {e}"))?;
            let v: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| format!("ERROR: 发送消息响应解析失败: {e}"))?;
            let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
            if code != 0 {
                let msg = v
                    .get("msg")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                return Err(format!("ERROR: 发送消息失败 code={code} msg={msg}"));
            }
        }
        Ok(())
    }

    /// 发送一张 CardKit 流式卡片并返回控制器（v0.10.2：思考小字+主区 markdown 流式）。
    /// 建卡（streaming_mode=true + note 小字区）→ 发进聊天 → 拿到 card_id/message_id。
    /// 后续 append_content/update_note/finalize 原地更新，打字机效果由飞书端渲染。
    /// 任一步失败返回 Err（调用方降级 send_text 纯文本路径）
    pub async fn start_streaming_card(
        &self,
        open_id: &str,
        with_note: bool,
    ) -> Result<StreamingCard, String> {
        let token = get_token(&self.inner).await?;
        // ① 建卡实体（Card JSON 2.0；note 区带灰色小字体思考流）
        let mut elements = vec![serde_json::json!({
            "tag": "markdown", "content": "", "element_id": "content"
        })];
        if with_note {
            elements.push(serde_json::json!({"tag": "hr"}));
            elements.push(serde_json::json!({
                "tag": "markdown", "content": "<font color='grey'>…</font>", "element_id": "note"
            }));
        }
        let card_json = serde_json::json!({
            "schema": "2.0",
            "config": {
                "streaming_mode": true,
                "summary": { "content": "[生成中…]" },
                "streaming_config": {
                    "print_frequency_ms": { "default": 50 },
                    "print_step": { "default": 1 }
                }
            },
            "body": { "elements": elements }
        });
        let create_url = format!("{}/open-apis/cardkit/v1/cards", self.inner.cfg.domain);
        // ⚠️ 包装格式（8/26 实测踩坑）：外层 {type:"card_json", data:"<卡JSON字符串>"}——
        //    data 必须二次序列化为字符串，传嵌套对象会被 99992402 字段校验拒收
        let resp = self
            .inner
            .http
            .post(&create_url)
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "type": "card_json",
                "data": card_json.to_string(),
            }))
            .send()
            .await
            .map_err(|e| format!("ERROR: 建卡请求失败: {e}"))?;
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("ERROR: 建卡响应解析失败: {e}"))?;
        let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("unknown");
            return Err(format!("ERROR: 建卡失败 code={code} msg={msg}（检查 cardkit:card:write 权限）"));
        }
        let card_id = v
            .pointer("/data/card_id")
            .and_then(|x| x.as_str())
            .ok_or("ERROR: 建卡响应缺 card_id")?
            .to_string();
        // ② 发进聊天（msg_type=interactive，content 引 card_id）
        let send_url = format!(
            "{}/open-apis/im/v1/messages?receive_id_type=open_id",
            self.inner.cfg.domain
        );
        let content = serde_json::json!({ "type": "card", "data": { "card_id": card_id } }).to_string();
        let body = serde_json::json!({
            "receive_id": open_id,
            "msg_type": "interactive",
            "content": content,
        });
        let resp = self
            .inner
            .http
            .post(&send_url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("ERROR: 发送卡片请求失败: {e}"))?;
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("ERROR: 发送卡片响应解析失败: {e}"))?;
        let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("unknown");
            return Err(format!("ERROR: 发送卡片失败 code={code} msg={msg}"));
        }
        Ok(StreamingCard {
            inner: self.inner.clone(),
            card_id,
            sequence: std::sync::atomic::AtomicU64::new(1),
            has_note: with_note,
            content_text: String::new(),
            note_text: String::new(),
            last_flush: std::sync::Mutex::new(std::time::Instant::now()),
        })
    }
}

/// 流式卡片控制器：每次更新传**全量**文本（飞书端做增量渲染：旧文本是新文本前缀
/// 时打字机续写，否则整体上屏）。节流 160ms（OpenClaw 同款值，防 API 抖动/限流）
pub struct StreamingCard {
    inner: Arc<Inner>,
    card_id: String,
    sequence: std::sync::atomic::AtomicU64,
    has_note: bool,
    content_text: String,
    note_text: String,
    last_flush: std::sync::Mutex<std::time::Instant>,
}

impl StreamingCard {
    /// 全量替换主区 markdown（带节流：距上次 <160ms 时本地缓冲，下次再刷）
    pub async fn update_content(&mut self, text: &str) -> Result<(), String> {
        self.content_text = text.to_string();
        self.flush(false).await
    }

    /// 全量替换 note 小字区（思考流；内容套灰色字体）
    pub async fn update_note(&mut self, text: &str) -> Result<(), String> {
        if !self.has_note {
            return Ok(());
        }
        self.note_text = text.to_string();
        self.flush(true).await
    }

    /// 收尾（不节流）：主区定格 final_text + note 变身状态行（note_override 传 Some 时
    /// 替换 note 内容——思考流退场，状态行登场）+ 关闭 streaming_mode。
    /// 错误/超时路径同样用它（final_text 传错误消息，用户在卡片主区直接看到）
    pub async fn finalize(
        &mut self,
        final_text: &str,
        note_override: Option<&str>,
    ) -> Result<(), String> {
        self.content_text = final_text.to_string();
        if let Some(n) = note_override {
            self.note_text = n.to_string();
        }
        if self.has_note {
            self.put_note().await?;
        }
        self.put_content().await?;
        self.close_streaming().await
    }

    /// 这张卡有没有 note 小字区（调用方决定状态行放哪）
    pub fn has_note(&self) -> bool {
        self.has_note
    }

    /// 关闭流式模式（卡片定格；finalize 内部调用，也可单独用）。
    /// ⚠️ 必须带 sequence+uuid（8/26 实测：缺 sequence 报 99992402 field validation）
    pub async fn close_streaming(&self) -> Result<(), String> {
        let token = get_token(&self.inner).await?;
        let seq = self.sequence.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let url = format!(
            "{}/open-apis/cardkit/v1/cards/{}/settings",
            self.inner.cfg.domain, self.card_id
        );
        let settings = serde_json::json!({ "config": { "streaming_mode": false } });
        let resp = self
            .inner
            .http
            .patch(&url)
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "settings": settings.to_string(),
                "sequence": seq,
                "uuid": format!("c_{}_{}", self.card_id, seq),
            }))
            .send()
            .await
            .map_err(|e| format!("ERROR: 关闭流式模式失败: {e}"))?;
        let v: serde_json::Value = resp.json().await.unwrap_or_default();
        let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("unknown");
            return Err(format!("ERROR: 关闭流式失败 code={code} msg={msg}（seq={seq}）"));
        }
        Ok(())
    }

    /// 节流判断：距上次实际 API 刷新不足 160ms 时跳过（finalize 不节流）
    async fn flush(&mut self, is_note: bool) -> Result<(), String> {
        {
            let mut t = self.last_flush.lock().unwrap();
            if t.elapsed() < std::time::Duration::from_millis(160) {
                return Ok(());
            }
            *t = std::time::Instant::now();
        }
        if is_note {
            self.put_note().await
        } else {
            self.put_content().await
        }
    }

    async fn put_content(&self) -> Result<(), String> {
        let token = get_token(&self.inner).await?;
        let seq = self.sequence.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let url = format!(
            "{}/open-apis/cardkit/v1/cards/{}/elements/content/content",
            self.inner.cfg.domain, self.card_id
        );
        let body = serde_json::json!({
            "content": self.content_text,
            "sequence": seq,
            "uuid": format!("s_{}_{}", self.card_id, seq),
        });
        let resp = self
            .inner
            .http
            .put(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("ERROR: 流式更新主区失败: {e}"))?;
        let v: serde_json::Value = resp.json().await.unwrap_or_default();
        let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("unknown");
            return Err(format!("ERROR: 流式更新主区 code={code} msg={msg}"));
        }
        Ok(())
    }

    async fn put_note(&self) -> Result<(), String> {
        let token = get_token(&self.inner).await?;
        let seq = self.sequence.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let url = format!(
            "{}/open-apis/cardkit/v1/cards/{}/elements/note/content",
            self.inner.cfg.domain, self.card_id
        );
        let body = serde_json::json!({
            "content": format!("<font color='grey'>{}</font>", self.note_text),
            "sequence": seq,
            "uuid": format!("n_{}_{}", self.card_id, seq),
        });
        let resp = self
            .inner
            .http
            .put(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("ERROR: 思考区更新失败: {e}"))?;
        let v: serde_json::Value = resp.json().await.unwrap_or_default();
        let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("unknown");
            return Err(format!("ERROR: 思考区更新 code={code} msg={msg}"));
        }
        Ok(())
    }
}

impl Drop for FeishuClient {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 按字符数切分文本（不切 UTF-8 边界）
fn chunk_text(text: &str, n: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cnt = 0usize;
    for ch in text.chars() {
        cur.push(ch);
        cnt += 1;
        if cnt >= n {
            out.push(std::mem::take(&mut cur));
            cnt = 0;
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// 伪随机 [0, n]（不引 rand crate，用系统时间纳米扰动）
fn rand_below_inclusive(n: u64) -> u64 {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() ^ (d.subsec_nanos() as u64) << 17)
        .unwrap_or(0);
    seed % (n + 1)
}

/// 从 wss URL query 解析 service_id（ping 帧的 service 字段用），找不到默认 0
fn parse_service_id(url: &str) -> i32 {
    let Some(q) = url.split('?').nth(1) else {
        return 0;
    };
    for pair in q.split('&') {
        if let Some(v) = pair.strip_prefix("service_id=") {
            // query 值可能带 #fragment 或其它字符，只取数字前缀
            let digits: String = v.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = digits.parse::<i32>() {
                return n;
            }
        }
    }
    0
}

/// 接入点协商：POST /callback/ws/endpoint
async fn fetch_endpoint(inner: &Arc<Inner>) -> Result<EndpointInfo, EndpointError> {
    let url = format!("{}/callback/ws/endpoint", inner.cfg.domain);
    let body = serde_json::json!({
        "AppID": inner.cfg.app_id,
        "AppSecret": inner.cfg.app_secret,
    });
    let resp = inner
        .http
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| EndpointError::Retry(format!("接入点请求失败: {e}")))?;
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| EndpointError::Retry(format!("接入点响应解析失败: {e}")))?;
    let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = v
            .get("msg")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown")
            .to_string();
        if FATAL_AUTH_CODES.contains(&code) {
            return Err(EndpointError::Fatal(format!(
                "接入点鉴权失败 code={code} msg={msg}"
            )));
        }
        return Err(EndpointError::Retry(format!(
            "接入点返回错误 code={code} msg={msg}"
        )));
    }
    let data = v.get("data").cloned().unwrap_or(serde_json::Value::Null);
    let ws_url = data
        .get("URL")
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .to_string();
    if ws_url.is_empty() {
        return Err(EndpointError::Retry("接入点响应缺少 URL".to_string()));
    }
    let cc = data.get("ClientConfig").cloned().unwrap_or_default();
    let get_u64 = |k: &str, def: u64| {
        cc.get(k)
            .and_then(|x| x.as_i64())
            .map(|x| if x < 0 { def } else { x as u64 })
            .unwrap_or(def)
    };
    let mut params = ClientParams::default();
    params.ping_interval = get_u64("PingInterval", params.ping_interval);
    params.reconnect_count = cc
        .get("ReconnectCount")
        .and_then(|x| x.as_i64())
        .unwrap_or(params.reconnect_count);
    params.reconnect_interval = get_u64("ReconnectInterval", params.reconnect_interval);
    params.reconnect_nonce = get_u64("ReconnectNonce", params.reconnect_nonce);
    let service_id = parse_service_id(&ws_url);
    Ok(EndpointInfo {
        url: ws_url,
        service_id,
        params: params.clamped(),
    })
}

/// WS 主循环：协商 → 连接 → 心跳/收发 → 断开重连
async fn ws_loop(inner: Arc<Inner>) {
    let mut attempts: i64 = 0;
    loop {
        if inner.stopped() {
            return;
        }
        let ep = match fetch_endpoint(&inner).await {
            Ok(ep) => ep,
            Err(EndpointError::Fatal(msg)) => {
                eprintln!("[feishu] {msg}，放弃重试");
                inner.set_status(ChannelStatus::Failed(msg));
                return;
            }
            Err(EndpointError::Retry(msg)) => {
                eprintln!("[feishu] {msg}");
                attempts += 1;
                let p = inner.current_params();
                if p.reconnect_count >= 0 && attempts > p.reconnect_count {
                    inner.set_status(ChannelStatus::Failed(format!(
                        "重连次数超限({})",
                        p.reconnect_count
                    )));
                    return;
                }
                inner.set_status(ChannelStatus::Reconnecting);
                if !inner.interruptible_sleep(p.reconnect_interval).await {
                    return;
                }
                continue;
            }
        };
        inner.update_params(ep.params);
        match connect_and_run(&inner, &ep).await {
            Ok(reason) => {
                eprintln!("[feishu] 连接断开：{reason}");
                attempts = 0; // 正常断开（心跳超时/close）不计失败
            }
            Err(e) => {
                eprintln!("[feishu] 连接异常：{e}");
                attempts += 1;
            }
        }
        if inner.stopped() {
            return;
        }
        let p = inner.current_params();
        if p.reconnect_count >= 0 && attempts > p.reconnect_count {
            inner.set_status(ChannelStatus::Failed(format!(
                "重连次数超限({})",
                p.reconnect_count
            )));
            return;
        }
        inner.set_status(ChannelStatus::Reconnecting);
        // 随机 0..ReconnectNonce 秒延迟后重新协商
        let delay = rand_below_inclusive(p.reconnect_nonce);
        if !inner.interruptible_sleep(delay).await {
            return;
        }
    }
}

/// 单次连接生命周期：收发 + 心跳，返回断开原因
async fn connect_and_run(inner: &Arc<Inner>, ep: &EndpointInfo) -> Result<String, String> {
    let (ws, _) = tokio_tungstenite::connect_async(&ep.url)
        .await
        .map_err(|e| format!("WS 握手失败: {e}"))?;
    let (mut sink, mut stream) = ws.split();
    inner.set_status(ChannelStatus::Connected);
    eprintln!("[feishu] 已连接 service_id={}", ep.service_id);

    let mut last_inbound = Instant::now();
    let mut frags = FragmentCache::new();
    let ping_secs = inner.current_params().ping_interval;
    let mut ping = Box::pin(tokio::time::sleep(Duration::from_secs(ping_secs)));

    loop {
        if inner.stopped() {
            return Ok("收到 stop 信号".to_string());
        }
        tokio::select! {
            _ = &mut ping => {
                let p = inner.current_params();
                // 心跳超时：超过 2×PingInterval 无入站帧 → 主动断开触发重连
                if last_inbound.elapsed() > Duration::from_secs(p.ping_interval * HEARTBEAT_TIMEOUT_FACTOR) {
                    return Ok("心跳超时".to_string());
                }
                let frame = pb::Frame {
                    seq_id: 0,
                    log_id: 0,
                    service: ep.service_id,
                    method: 0, // control
                    headers: vec![pb::Header {
                        key: "type".to_string(),
                        value: "ping".to_string(),
                    }],
                    ..Default::default()
                };
                if sink.send(Message::Binary(pb::encode_frame(&frame))).await.is_err() {
                    return Ok("发送心跳失败".to_string());
                }
                ping = Box::pin(tokio::time::sleep(Duration::from_secs(p.ping_interval)));
            }
            msg = stream.next() => {
                match msg {
                    Some(Ok(Message::Binary(bytes))) => {
                        last_inbound = Instant::now();
                        if let Err(e) = handle_frame(inner, ep.service_id, &bytes, &mut frags, &mut sink).await {
                            eprintln!("[feishu] 帧处理失败：{e}");
                        }
                    }
                    Some(Ok(Message::Close(_))) => return Ok("对端关闭".to_string()),
                    // 协议层 ping/pong/text 也算活性（node-sdk 同款：任何入站帧都
                    // clearLiveness——只认 Binary 会把健康连接误杀，3 分钟周期断连实锤）
                    Some(Ok(_)) => {
                        last_inbound = Instant::now();
                    }
                    Some(Err(e)) => return Err(format!("WS 读错误: {e}")),
                    None => return Ok("流结束".to_string()),
                }
            }
            _ = inner.notify.notified() => return Ok("收到 stop 信号".to_string()),
        }
    }
}

/// 处理一个入站二进制帧
async fn handle_frame(
    inner: &Arc<Inner>,
    service_id: i32,
    bytes: &[u8],
    frags: &mut FragmentCache,
    sink: &mut futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
        Message,
    >,
) -> Result<(), String> {
    let started = Instant::now();
    let frame = pb::decode_frame(bytes).ok_or_else(|| "帧解码失败".to_string())?;
    let ftype = header_get(&frame, "type").unwrap_or("").to_string();

    if frame.method == 0 {
        // control 帧：pong 携带新的心跳/重连参数
        if ftype == "pong" {
            if let Some(payload) = &frame.payload {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(payload) {
                    let mut p = inner.current_params();
                    let gu = |k: &str, old: u64| {
                        v.get(k)
                            .and_then(|x| x.as_i64())
                            .map(|x| if x < 0 { old } else { x as u64 })
                            .unwrap_or(old)
                    };
                    p.ping_interval = gu("PingInterval", p.ping_interval);
                    p.reconnect_count = v
                        .get("ReconnectCount")
                        .and_then(|x| x.as_i64())
                        .unwrap_or(p.reconnect_count);
                    p.reconnect_interval = gu("ReconnectInterval", p.reconnect_interval);
                    p.reconnect_nonce = gu("ReconnectNonce", p.reconnect_nonce);
                    inner.update_params(p);
                }
            }
        }
        return Ok(());
    }

    // data 帧（method=1）：可能分片，重组后 ACK
    let message_id = header_get(&frame, "message_id").unwrap_or("").to_string();
    let sum = header_get(&frame, "sum")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(1);
    let seq = header_get(&frame, "seq")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(1);
    let payload = frame.payload.clone().unwrap_or_default();

    if ftype == "event" {
        if let Some(full) = frags.insert(&message_id, sum, seq, payload) {
            // 去重：重投直接 ACK 不回调
            let dedup_key = if message_id.is_empty() {
                parse_event(&full)
                    .map(|dm| dm.message_id.clone())
                    .unwrap_or_default()
            } else {
                message_id.clone()
            };
            let fresh = inner.dedup.lock().unwrap().check_and_insert(&dedup_key);
            if fresh {
                if let Some(dm) = parse_event(&full) {
                    inner.emit_message(dm);
                }
            }
        }
    }

    // ACK：原帧回敬（同 SeqID/LogID/service/method/headers）+ biz_rt，payload 换 {"code":200}
    let mut headers = frame.headers.clone();
    headers.push(pb::Header {
        key: "biz_rt".to_string(),
        value: started.elapsed().as_millis().to_string(),
    });
    let ack = pb::Frame {
        seq_id: frame.seq_id,
        log_id: frame.log_id,
        service: if frame.service != 0 { frame.service } else { service_id },
        method: frame.method,
        headers,
        payload: Some(b"{\"code\":200}".to_vec()),
        ..Default::default()
    };
    sink.send(Message::Binary(pb::encode_frame(&ack)))
        .await
        .map_err(|e| format!("ACK 发送失败: {e}"))?;
    Ok(())
}

/// 取帧头 value
fn header_get<'a>(frame: &'a pb::Frame, key: &str) -> Option<&'a str> {
    frame
        .headers
        .iter()
        .find(|h| h.key == key)
        .map(|h| h.value.as_str())
}

/// 解析 im.message.receive_v1 事件 JSON → FeishuDm；非该事件或解析失败返回 None
fn parse_event(payload: &[u8]) -> Option<FeishuDm> {
    let v: serde_json::Value = serde_json::from_slice(payload).ok()?;
    let event_type = v
        .get("header")
        .and_then(|h| h.get("event_type"))
        .and_then(|t| t.as_str())?;
    if event_type != "im.message.receive_v1" {
        return None;
    }
    let event = v.get("event")?;
    let open_id = event
        .get("sender")
        .and_then(|s| s.get("sender_id"))
        .and_then(|id| id.get("open_id"))
        .and_then(|o| o.as_str())
        .unwrap_or("")
        .to_string();
    let msg = event.get("message")?;
    let message_id = msg
        .get("message_id")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let msg_type = msg
        .get("message_type")
        .and_then(|m| m.as_str())
        .unwrap_or("");
    // content 本身是转义的 JSON 字符串：{"text":"..."}
    let text = if msg_type == "text" {
        msg.get("content")
            .and_then(|c| c.as_str())
            .and_then(|c| serde_json::from_str::<serde_json::Value>(c).ok())
            .and_then(|c| {
                c.get("text")
                    .and_then(|t| t.as_str())
                    .map(|t| t.to_string())
            })
            .unwrap_or_default()
    } else {
        String::new()
    };
    Some(FeishuDm {
        open_id,
        message_id,
        text,
    })
}

/// 获取 tenant_access_token：缓存优先，过期前 300s 刷新，失败重试 3 次
async fn get_token(inner: &Arc<Inner>) -> Result<String, String> {
    {
        let cache = inner.token.lock().unwrap();
        if let Some(t) = cache.as_ref() {
            if t.expire_at > Instant::now() + Duration::from_secs(TOKEN_REFRESH_MARGIN_SECS) {
                return Ok(t.token.clone());
            }
        }
    }
    let mut last_err = String::from("未知错误");
    for i in 0..TOKEN_RETRY {
        match fetch_token(inner).await {
            Ok(t) => {
                let token = t.token.clone();
                *inner.token.lock().unwrap() = Some(t);
                return Ok(token);
            }
            Err(e) => {
                last_err = e;
                if i + 1 < TOKEN_RETRY {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }
    Err(last_err)
}

async fn fetch_token(inner: &Arc<Inner>) -> Result<CachedToken, String> {
    let url = format!(
        "{}/open-apis/auth/v3/tenant_access_token/internal",
        inner.cfg.domain
    );
    let body = serde_json::json!({
        "app_id": inner.cfg.app_id,
        "app_secret": inner.cfg.app_secret,
    });
    let resp = inner
        .http
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("ERROR: token 请求失败: {e}"))?;
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("ERROR: token 响应解析失败: {e}"))?;
    let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = v
            .get("msg")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown");
        return Err(format!("ERROR: token 获取失败 code={code} msg={msg}"));
    }
    let token = v
        .get("tenant_access_token")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    if token.is_empty() {
        return Err("ERROR: token 响应缺少 tenant_access_token".to_string());
    }
    let expire = v
        .get("expire")
        .and_then(|e| e.as_i64())
        .filter(|e| *e > 0)
        .unwrap_or(7200) as u64;
    Ok(CachedToken {
        token,
        expire_at: Instant::now() + Duration::from_secs(expire),
    })
}

/// Frame protobuf 最小编解码器（手写 varint + tag，不引 prost）
///
/// 只支持 Frame 用到的 wire type：0=varint（uint64/int32），2=length-delimited
/// （string/bytes/嵌套 Header）。decode 容忍未知字段（按 wire type 跳过）。
pub(crate) mod pb {
    /// message Header { string key = 1; string value = 2; }
    #[derive(Debug, Clone, Default, PartialEq)]
    pub struct Header {
        pub key: String,
        pub value: String,
    }

    /// message Frame（字段编号见模块文档）
    #[derive(Debug, Clone, Default, PartialEq)]
    pub struct Frame {
        pub seq_id: u64,          // 1
        pub log_id: u64,          // 2
        pub service: i32,         // 3
        pub method: i32,          // 4: control=0, data=1
        pub headers: Vec<Header>, // 5
        pub payload_encoding: Option<String>, // 6
        pub payload_type: Option<String>,     // 7
        pub payload: Option<Vec<u8>>,         // 8
        pub log_id_new: Option<String>,       // 9
    }

    fn put_varint(buf: &mut Vec<u8>, mut v: u64) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                buf.push(b);
                return;
            }
            buf.push(b | 0x80);
        }
    }

    fn get_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
        let mut v: u64 = 0;
        let mut shift = 0u32;
        loop {
            if *pos >= buf.len() {
                return None;
            }
            let b = buf[*pos];
            *pos += 1;
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
            shift += 7;
            if shift >= 64 {
                return None; // varint 超长，数据损坏
            }
        }
    }

    fn put_tag(buf: &mut Vec<u8>, field: u32, wire: u32) {
        put_varint(buf, ((field as u64) << 3) | wire as u64);
    }

    fn put_len_delim(buf: &mut Vec<u8>, field: u32, data: &[u8]) {
        put_tag(buf, field, 2);
        put_varint(buf, data.len() as u64);
        buf.extend_from_slice(data);
    }

    fn encode_header(h: &Header) -> Vec<u8> {
        let mut buf = Vec::new();
        put_len_delim(&mut buf, 1, h.key.as_bytes());
        put_len_delim(&mut buf, 2, h.value.as_bytes());
        buf
    }

    /// 编码 Frame（proto3 语义：零值字段省略）
    pub fn encode_frame(f: &Frame) -> Vec<u8> {
        let mut buf = Vec::new();
        if f.seq_id != 0 {
            put_tag(&mut buf, 1, 0);
            put_varint(&mut buf, f.seq_id);
        }
        if f.log_id != 0 {
            put_tag(&mut buf, 2, 0);
            put_varint(&mut buf, f.log_id);
        }
        if f.service != 0 {
            put_tag(&mut buf, 3, 0);
            // int32 负数按 10 字节 varint（64 位符号扩展）编码
            put_varint(&mut buf, f.service as i64 as u64);
        }
        if f.method != 0 {
            put_tag(&mut buf, 4, 0);
            put_varint(&mut buf, f.method as i64 as u64);
        }
        for h in &f.headers {
            put_len_delim(&mut buf, 5, &encode_header(h));
        }
        if let Some(s) = &f.payload_encoding {
            put_len_delim(&mut buf, 6, s.as_bytes());
        }
        if let Some(s) = &f.payload_type {
            put_len_delim(&mut buf, 7, s.as_bytes());
        }
        if let Some(p) = &f.payload {
            put_len_delim(&mut buf, 8, p);
        }
        if let Some(s) = &f.log_id_new {
            put_len_delim(&mut buf, 9, s.as_bytes());
        }
        buf
    }

    /// 按 wire type 跳过未知字段：0=varint 1=64bit 2=len 5=32bit
    fn skip_field(buf: &[u8], pos: &mut usize, wire: u64) -> Option<()> {
        match wire {
            0 => {
                get_varint(buf, pos)?;
            }
            1 => {
                *pos = pos.checked_add(8)?;
                if *pos > buf.len() {
                    return None;
                }
            }
            2 => {
                let len = get_varint(buf, pos)? as usize;
                *pos = pos.checked_add(len)?;
                if *pos > buf.len() {
                    return None;
                }
            }
            5 => {
                *pos = pos.checked_add(4)?;
                if *pos > buf.len() {
                    return None;
                }
            }
            _ => return None, // 3/4(组) 已废弃，视为损坏
        }
        Some(())
    }

    fn get_len_delim<'a>(buf: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
        let len = get_varint(buf, pos)? as usize;
        let end = pos.checked_add(len)?;
        if end > buf.len() {
            return None;
        }
        let out = &buf[*pos..end];
        *pos = end;
        Some(out)
    }

    fn decode_header(buf: &[u8]) -> Option<Header> {
        let mut h = Header::default();
        let mut pos = 0usize;
        while pos < buf.len() {
            let tag = get_varint(buf, &mut pos)?;
            let field = tag >> 3;
            let wire = tag & 7;
            match (field, wire) {
                (1, 2) => {
                    let b = get_len_delim(buf, &mut pos)?;
                    h.key = String::from_utf8_lossy(b).into_owned();
                }
                (2, 2) => {
                    let b = get_len_delim(buf, &mut pos)?;
                    h.value = String::from_utf8_lossy(b).into_owned();
                }
                _ => skip_field(buf, &mut pos, wire)?,
            }
        }
        Some(h)
    }

    /// 解码 Frame；数据损坏返回 None，未知字段跳过
    pub fn decode_frame(buf: &[u8]) -> Option<Frame> {
        let mut f = Frame::default();
        let mut pos = 0usize;
        while pos < buf.len() {
            let tag = get_varint(buf, &mut pos)?;
            let field = tag >> 3;
            let wire = tag & 7;
            match (field, wire) {
                (1, 0) => f.seq_id = get_varint(buf, &mut pos)?,
                (2, 0) => f.log_id = get_varint(buf, &mut pos)?,
                (3, 0) => f.service = get_varint(buf, &mut pos)? as i64 as i32,
                (4, 0) => f.method = get_varint(buf, &mut pos)? as i64 as i32,
                (5, 2) => {
                    let b = get_len_delim(buf, &mut pos)?;
                    f.headers.push(decode_header(b)?);
                }
                (6, 2) => {
                    let b = get_len_delim(buf, &mut pos)?;
                    f.payload_encoding = Some(String::from_utf8_lossy(b).into_owned());
                }
                (7, 2) => {
                    let b = get_len_delim(buf, &mut pos)?;
                    f.payload_type = Some(String::from_utf8_lossy(b).into_owned());
                }
                (8, 2) => {
                    let b = get_len_delim(buf, &mut pos)?;
                    f.payload = Some(b.to_vec());
                }
                (9, 2) => {
                    let b = get_len_delim(buf, &mut pos)?;
                    f.log_id_new = Some(String::from_utf8_lossy(b).into_owned());
                }
                _ => skip_field(buf, &mut pos, wire)?,
            }
        }
        Some(f)
    }
}

#[cfg(test)]
mod tests {
    use super::pb::{self, Frame, Header};
    use super::*;

    fn sample_frame() -> Frame {
        Frame {
            seq_id: u64::MAX - 1, // 大 varint：10 字节编码
            log_id: 123_456_789_012_345,
            service: -7, // 负 int32：10 字节符号扩展 varint
            method: 1,
            headers: vec![
                Header {
                    key: "type".to_string(),
                    value: "event".to_string(),
                },
                Header {
                    key: "message_id".to_string(),
                    value: "msg_abc".to_string(),
                },
                Header {
                    key: "sum".to_string(),
                    value: "1".to_string(),
                },
            ],
            payload_encoding: Some("utf-8".to_string()),
            payload_type: Some("json".to_string()),
            payload: Some(vec![0, 1, 2, 0xff, 0xfe, 0x80]),
            log_id_new: Some("trace-xyz".to_string()),
        }
    }

    #[test]
    fn pb_roundtrip() {
        let f = sample_frame();
        let enc = pb::encode_frame(&f);
        let dec = pb::decode_frame(&enc).expect("解码失败");
        assert_eq!(f, dec);
    }

    #[test]
    fn pb_decode_tolerates_unknown_fields() {
        let f = sample_frame();
        let mut enc = pb::encode_frame(&f);
        // 手工拼未知字段：field15 varint=99、field14 len-delim "abc"、
        // field13 fixed32、field12 fixed64
        enc.push(15 << 3); // tag: field15, wire0
        enc.push(99);
        enc.push(14 << 3 | 2); // tag: field14, wire2
        enc.push(3);
        enc.extend_from_slice(b"abc");
        enc.push(13 << 3 | 5); // tag: field13, wire5
        enc.extend_from_slice(&[1, 2, 3, 4]);
        enc.push(12 << 3 | 1); // tag: field12, wire1
        enc.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let dec = pb::decode_frame(&enc).expect("含未知字段应仍可解码");
        assert_eq!(f, dec);
    }

    #[test]
    fn fragment_reassembly_out_of_order() {
        let mut cache = FragmentCache::new();
        // sum=3，seq=2,0,1 乱序到达
        assert!(cache.insert("m1", 3, 2, b"c".to_vec()).is_none());
        assert!(cache.insert("m1", 3, 0, b"a".to_vec()).is_none());
        let full = cache.insert("m1", 3, 1, b"b".to_vec()).expect("应重组完成");
        assert_eq!(full, b"abc");
        // 重组完成后缓存应清空，同 id 可复用
        assert!(cache.map.is_empty());
    }

    #[test]
    fn parse_event_json() {
        let payload = r#"{"header":{"event_type":"im.message.receive_v1","tenant_key":"t1","app_id":"cli_xxx"},"event":{"sender":{"sender_id":{"open_id":"ou_xxx","union_id":"on_yyy"},"sender_type":"user"},"message":{"message_id":"om_xxx","chat_id":"oc_xxx","chat_type":"p2p","message_type":"text","content":"{\"text\":\"用户消息\"}","create_time":"1700000000"}}}"#.as_bytes();
        let dm = parse_event(payload).expect("事件解析失败");
        assert_eq!(dm.open_id, "ou_xxx");
        assert_eq!(dm.message_id, "om_xxx");
        assert_eq!(dm.text, "用户消息");
        // 非文本类型 text 为空串
        let payload2 = br#"{"header":{"event_type":"im.message.receive_v1"},"event":{"sender":{"sender_id":{"open_id":"ou_1"}},"message":{"message_id":"om_2","message_type":"image","content":"{\"image_key\":\"k\"}"}}}"#;
        let dm2 = parse_event(payload2).expect("图片事件应解析");
        assert_eq!(dm2.text, "");
        // 非消息事件返回 None
        let payload3 = br#"{"header":{"event_type":"im.chat.updated"},"event":{}}"#;
        assert!(parse_event(payload3).is_none());
    }

    #[test]
    fn dedup_lru() {
        let mut lru = DedupLru::new(512);
        assert!(lru.check_and_insert("om_1")); // 首次：应处理
        assert!(!lru.check_and_insert("om_1")); // 重投：跳过
        assert!(lru.check_and_insert("om_2"));
        // 空 id 不参与去重
        assert!(lru.check_and_insert(""));
        assert!(lru.check_and_insert(""));
        // 容量淘汰：cap=2 时最老条目被挤出后可重新插入
        let mut small = DedupLru::new(2);
        assert!(small.check_and_insert("a"));
        assert!(small.check_and_insert("b"));
        assert!(small.check_and_insert("c")); // 挤出 "a"
        assert!(small.check_and_insert("a")); // "a" 已被淘汰，视为新
    }

    #[test]
    fn parse_service_id_from_url() {
        assert_eq!(
            parse_service_id("wss://open.feishu.cn/ws?device_id=abc&service_id=123"),
            123
        );
        assert_eq!(parse_service_id("wss://x/ws?foo=1"), 0);
        assert_eq!(parse_service_id("wss://x/ws"), 0);
    }
}
