//! L3 跨会话记忆索引（自传体记忆）
//!
//! 设计目标：Agent 能记住【自己经历过的事】——上个会话里用户说过的关键信息，
//! 在新会话中被相关问题唤起。这不是 RAG（检索外部知识），是检索 Agent 自身经历。
//!
//! v0.25 架构：
//! - EmbeddingProvider 抽象：文本 → 向量。两个实现：
//!   - HashEmbedding：本地字符 n-gram 哈希（256 维，零依赖，默认）
//!   - ApiEmbedding：OpenAI 兼容 embedding API（如智谱 embedding-3）
//! - 存储层维度自适应：每条向量记录 emb_dim + embed_id（后端签名），
//!   不同后端/维度不混检——签名不一致时诚实报错并提示 `r2 memory migrate`。
//! - 记忆生命周期：检索按时间衰减排序；同主题新记忆覆盖旧记忆（superseded 标记）。
//! - 记忆条目：每轮 Q&A 一条（用户输入 + assistant 最终回答），双向量存储
//!   （query/answer 各一行，检索取两边最高分）。

use rusqlite::{params, Connection};

/// HashEmbedding 产出维度
pub const EMBED_DIM: usize = 256;

/// 单条记忆回答的最大存储字符数（防膨胀）
const MAX_ANSWER_CHARS: usize = 2000;

/// 嵌入后端抽象：文本 → 向量
#[async_trait::async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// 后端签名，如 "hash" / "openai_compat:embedding-3"
    fn id(&self) -> &str;
    /// 该后端产出维度（ApiEmbedding 首次调用前未知，返回 0）
    fn dim(&self) -> usize;
    async fn embed(&self, text: &str) -> Result<Vec<f32>, String>;
}

/// 本地 hash n-gram 嵌入（dim=256，同步计算即返回，永不失败）
pub struct HashEmbedding;

#[async_trait::async_trait]
impl EmbeddingProvider for HashEmbedding {
    fn id(&self) -> &str {
        "hash"
    }

    fn dim(&self) -> usize {
        EMBED_DIM
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, String> {
        Ok(embed(text))
    }
}

/// OpenAI 兼容 embedding API 后端
pub struct ApiEmbedding {
    client: reqwest::Client,
    /// API 基础地址（不含尾部斜杠），如 https://open.bigmodel.cn/api/paas/v4
    base_url: String,
    api_key: String,
    /// 嵌入模型名，如 embedding-3
    model: String,
    /// 后端签名缓存（id() 返回 &str 需要持有）
    id: String,
    /// 产出维度缓存：首次成功调用后写入
    dim_cache: std::sync::Mutex<Option<usize>>,
}

impl ApiEmbedding {
    pub fn new(base_url: &str, api_key: &str, model: &str) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            id: format!("openai_compat:{model}"),
            dim_cache: std::sync::Mutex::new(None),
        }
    }
}

/// 解析 OpenAI 兼容 embedding 响应体 → 向量（纯函数，便于单测，不发网络）
fn parse_embedding_response(body: &str) -> Result<Vec<f32>, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("embedding 响应不是合法 JSON：{e}"))?;
    let arr = v
        .get("data")
        .and_then(|d| d.as_array())
        .and_then(|d| d.first())
        .and_then(|d| d.get("embedding"))
        .and_then(|e| e.as_array())
        .ok_or_else(|| "embedding 响应缺少 data[0].embedding 字段".to_string())?;
    arr.iter()
        .map(|x| {
            x.as_f64()
                .map(|f| f as f32)
                .ok_or_else(|| "embedding 向量含非数值元素".to_string())
        })
        .collect()
}

#[async_trait::async_trait]
impl EmbeddingProvider for ApiEmbedding {
    fn id(&self) -> &str {
        &self.id
    }

    fn dim(&self) -> usize {
        self.dim_cache
            .lock()
            .map(|g| g.unwrap_or(0))
            .unwrap_or(0)
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, String> {
        if text.trim().is_empty() {
            return Err("空文本无法嵌入".to_string());
        }
        let url = format!("{}/embeddings", self.base_url);
        let body = serde_json::json!({"model": self.model, "input": text});
        let mut last_err = String::from("embedding 请求失败");
        // 最多 3 次尝试：首次 + 429/5xx/网络错误重试 2 次（退避 1s/2s）
        for attempt in 0..=2u32 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(u64::from(attempt))).await;
            }
            let resp = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await;
            match resp {
                Ok(r) if r.status().is_success() => {
                    let text_body = r
                        .text()
                        .await
                        .map_err(|e| format!("读取 embedding 响应失败：{e}"))?;
                    let v = parse_embedding_response(&text_body)?;
                    if let Ok(mut g) = self.dim_cache.lock() {
                        *g = Some(v.len());
                    }
                    return Ok(v);
                }
                Ok(r) => {
                    let status = r.status();
                    last_err = format!("embedding API 返回错误（HTTP {status}）");
                    // 只有 429/5xx 值得重试；其它 4xx 是请求本身的问题
                    if status.as_u16() != 429 && !status.is_server_error() {
                        return Err(last_err);
                    }
                }
                Err(e) => {
                    last_err = format!("embedding 请求失败：{e}");
                }
            }
        }
        Err(last_err)
    }
}

/// 按配置构造嵌入后端（hash 默认；api → OpenAI 兼容；未知值告警并回退 hash）
pub fn build_embedding_provider(config: &crate::config::Config) -> Box<dyn EmbeddingProvider> {
    match config.context.l3_embedding.as_str() {
        "api" => Box::new(ApiEmbedding::new(
            &config.context.embedding.base_url,
            &config.context.embedding.api_key,
            &config.context.embedding.model,
        )),
        "hash" => Box::new(HashEmbedding),
        other => {
            tracing::warn!("未知 l3_embedding 后端 \"{other}\"，回退到 hash");
            Box::new(HashEmbedding)
        }
    }
}

/// 记忆库文件路径（会话目录下的 memory.db）
pub fn memory_db_path(config: &crate::config::Config) -> String {
    format!(
        "{}/memory.db",
        crate::config::expand_tilde(&config.session.dir)
    )
}

/// FNV-1a 哈希（64 位），不引外部依赖
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// 字符 n-gram（1~3）哈希嵌入：文本 → 256 维 L2 归一化向量
///
/// 对中文按字符 n-gram（捕获字共现），对英文同样按字符（跨语言不需要分词）。
/// 纯三元组对短文本语序太敏感（"我最喜欢的颜色" 与 "我喜欢蓝色" 零重叠），
/// 所以叠加一元/二元提升召回——这是 v0.1 的关键决策。
/// 空文本返回零向量（检索时分数自然为 0，会被阈值过滤）。
pub fn embed(text: &str) -> Vec<f32> {
    let chars: Vec<char> = text.chars().collect();
    let mut v = vec![0.0f32; EMBED_DIM];
    for n in 1..=3 {
        if chars.len() < n {
            break;
        }
        for w in chars.windows(n) {
            // 把 n-gram 的字符拼成 UTF-8 字节做哈希
            let mut buf = [0u8; 12];
            let mut len = 0;
            for c in w {
                let mut tmp = [0u8; 4];
                let s = c.encode_utf8(&mut tmp);
                buf[len..len + s.len()].copy_from_slice(s.as_bytes());
                len += s.len();
            }
            let bucket = (fnv1a(&buf[..len]) % EMBED_DIM as u64) as usize;
            v[bucket] += 1.0;
        }
    }
    // L2 归一化；全零向量保持全零
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

/// 余弦相似度：两边都是 L2 归一化向量时直接点积
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// 向量 → BLOB：f32 按 to_bits 小端打包
fn encode_emb(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_bits().to_le_bytes());
    }
    out
}

/// BLOB → 向量（反解 encode_emb）
fn decode_emb(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|c| f32::from_bits(u32::from_le_bytes([c[0], c[1], c[2], c[3]])))
        .collect()
}

/// 时间衰减因子：30 天内 1.0；30~90 天 0.9；90 天+ 0.75
/// （简单三档，不做连续函数——够用且可解释）
fn decay_factor(created_at: i64, now: i64) -> f64 {
    let age_days = (now - created_at).max(0) / 86_400;
    if age_days <= 30 {
        1.0
    } else if age_days <= 90 {
        0.9
    } else {
        0.75
    }
}

/// 当前 unix 秒
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 一条被唤起的跨会话记忆
#[derive(Debug)]
pub struct MemoryHit {
    /// fragments 行 id（同一 Q&A 双向量两行中取代表行）
    pub id: i64,
    /// 当时的用户输入
    pub query: String,
    /// 当时的回答（可截断存储）
    pub answer: String,
    /// 衰减调整后的分数
    pub score: f32,
    pub session_id: String,
    pub created_at: i64,
}

/// 记忆列表条目（memory list 用）
pub struct MemoryEntry {
    pub id: i64,
    pub query: String,
    pub answer: String,
    pub session_id: String,
    pub embed_id: String,
    pub created_at: i64,
    pub superseded: bool,
}

/// 记忆库统计（memory stats 用）
pub struct MemoryStats {
    /// 记忆总条数（按 Q&A 去重，双向量两行算一条）
    pub total: usize,
    /// 各嵌入后端的记忆条数分布
    pub by_embed_id: Vec<(String, usize)>,
    /// 最早/最晚记忆时间（空库为 None）
    pub oldest: Option<i64>,
    pub newest: Option<i64>,
}

/// migrate 结果汇报
pub struct MigrateReport {
    pub total: usize,
    pub migrated: usize,
    pub failed: usize,
}

/// 记忆存储：SQLite 单表（fragments）
///
/// 表结构含 emb_dim/embed_id/superseded 三列（v0.25 新增）；
/// 打开旧库时自动 ALTER TABLE 补列，旧行默认 embed_id="hash"、emb_dim=256。
pub struct MemoryStore {
    /// Mutex 使 MemoryStore 可跨 await 共享（Connection 本身 !Sync）；
    /// 所有操作都是短临界区，无竞争瓶颈
    conn: std::sync::Mutex<Connection>,
    /// 当前后端签名
    embed_id: String,
    /// 库里残留其它后端向量时的报错信息（检索/写入拒绝混检，migrate 不受限）
    mismatch: Option<String>,
}

impl MemoryStore {
    /// 打开（不存在则建表；旧库自动补列）
    ///
    /// embed_id 为当前嵌入后端签名。库里若存在其它签名的向量，
    /// search/store 会返回 Err 提示运行 `r2 memory migrate`——诚实报错不混检。
    pub fn open(path: &str, embed_id: &str) -> Result<Self, String> {
        let conn =
            Connection::open(path).map_err(|e| format!("打开记忆库失败（{path}）：{e}"))?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS fragments (
                id INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL,
                query TEXT NOT NULL,
                answer TEXT NOT NULL,
                emb BLOB NOT NULL,
                emb_dim INTEGER NOT NULL DEFAULT 256,
                embed_id TEXT NOT NULL DEFAULT 'hash',
                superseded INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL
            )",
            [],
        )
        .map_err(|e| format!("创建记忆表失败：{e}"))?;
        // 旧库（v0.1 schema，无新列）自动补列；已存在则忽略错误。
        // ADD COLUMN 带 DEFAULT 会用默认值回填旧行——旧向量正是 hash/256。
        for sql in [
            "ALTER TABLE fragments ADD COLUMN emb_dim INTEGER NOT NULL DEFAULT 256",
            "ALTER TABLE fragments ADD COLUMN embed_id TEXT NOT NULL DEFAULT 'hash'",
            "ALTER TABLE fragments ADD COLUMN superseded INTEGER NOT NULL DEFAULT 0",
        ] {
            match conn.execute(sql, []) {
                Ok(_) => {}
                Err(e) if e.to_string().contains("duplicate column") => {}
                Err(e) => return Err(format!("记忆表结构迁移失败：{e}")),
            }
        }

        // 模型签名检查：库里 distinct embed_id 含非当前后端 → 拒绝混检
        let ids: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT DISTINCT embed_id FROM fragments")
                .map_err(|e| format!("读取记忆签名失败：{e}"))?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|e| format!("读取记忆签名失败：{e}"))?;
            rows.filter_map(|r| r.ok()).collect()
        };
        let others: Vec<&String> = ids.iter().filter(|id| id.as_str() != embed_id).collect();
        let mismatch = if others.is_empty() {
            None
        } else {
            Some(format!(
                "嵌入模型已变更（库中残留 {} 的向量，当前后端为 {embed_id}），运行 r2 memory migrate 重建",
                others
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        };
        Ok(Self {
            conn: std::sync::Mutex::new(conn),
            embed_id: embed_id.to_string(),
            mismatch,
        })
    }

    /// 取连接锁（中毒时不 panic，取回内部数据继续用——SQLite 操作本身可重入）
    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 当前后端签名
    pub fn embed_id(&self) -> &str {
        &self.embed_id
    }

    /// 模型签名是否不匹配（不匹配时 search/store 报 Err；list/stats/delete/migrate 仍可用）
    pub fn mismatch(&self) -> Option<&str> {
        self.mismatch.as_deref()
    }

    /// 存一轮 Q&A（向量由调用方预先算好；answer 截断到 MAX_ANSWER_CHARS 防膨胀）
    ///
    /// 入库前做同主题覆盖检查：同 embed_id 下与新 query 相似度超过
    /// supersede_threshold 的旧条目标记 superseded=1（不删除，审计可查）。
    pub async fn store(
        &self,
        session_id: &str,
        query: &str,
        answer: &str,
        q_emb: &[f32],
        a_emb: &[f32],
        supersede_threshold: f64,
    ) -> Result<(), String> {
        if let Some(msg) = &self.mismatch {
            return Err(msg.clone());
        }
        let answer: String = answer.chars().take(MAX_ANSWER_CHARS).collect();

        // 同主题覆盖：hash 后端 0.92 几乎只在字面全同时命中，语义后端才会真正触发
        if supersede_threshold < 1.0 && q_emb.iter().any(|&x| x != 0.0) {
            let conn = self.conn();
            let mut stmt = conn
                .prepare(
                    "SELECT id, session_id, query, answer, emb, emb_dim FROM fragments
                     WHERE embed_id = ?1 AND superseded = 0",
                )
                .map_err(|e| format!("覆盖检查失败：{e}"))?;
            let rows = stmt
                .query_map(params![self.embed_id], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Vec<u8>>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                })
                .map_err(|e| format!("覆盖检查失败：{e}"))?;
            // 已覆盖的 Q&A 组去重（双向量两行可能都命中，只标记一次）
            let mut seen: std::collections::HashSet<(String, String, String)> =
                std::collections::HashSet::new();
            for row in rows {
                let (id, sid, q, a, blob, dim) =
                    row.map_err(|e| format!("覆盖检查失败：{e}"))?;
                if dim as usize != q_emb.len() {
                    continue;
                }
                if f64::from(cosine(q_emb, &decode_emb(&blob))) > supersede_threshold
                    && seen.insert((sid.clone(), q.clone(), a.clone()))
                {
                    // 整条记忆（同一 Q&A 的双向量两行）一起标记覆盖
                    conn.execute(
                        "UPDATE fragments SET superseded = 1
                         WHERE session_id = ?1 AND query = ?2 AND answer = ?3",
                        params![sid, q, a],
                    )
                    .map_err(|e| format!("标记覆盖失败：{e}"))?;
                    eprintln!("[memory] 旧记忆被新信息覆盖（id={id}）");
                }
            }
        }

        self.insert_pair(session_id, query, &answer, q_emb, a_emb, now_secs())
    }

    /// 双向量插入：query 和 answer 各存一条独立 fragments 记录（同 session/q/a）。
    /// - 拼接嵌入会让两边互相稀释（长文本埋藏检索变弱），
    ///   还会抬高无关查询的基础相似度（长 answer 常用字 n-gram 重叠 → 误召回）。
    /// - 只嵌 query 会丢掉语义桥接（"宠物"查询匹配不上"橘猫"输入，
    ///   但能匹配回答里的"宠物猫"）。
    /// - 检索时天然取 max(query分, answer分)，两边哪边命中都算。
    fn insert_pair(
        &self,
        session_id: &str,
        query: &str,
        answer: &str,
        q_emb: &[f32],
        a_emb: &[f32],
        created_at: i64,
    ) -> Result<(), String> {
        let conn = self.conn();
        for v in [q_emb, a_emb] {
            conn.execute(
                "INSERT INTO fragments (session_id, query, answer, emb, emb_dim, embed_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    session_id,
                    query,
                    answer,
                    encode_emb(v),
                    v.len() as i64,
                    self.embed_id,
                    created_at
                ],
            )
            .map_err(|e| format!("写入记忆失败：{e}"))?;
        }
        Ok(())
    }

    /// 检索 top-k：排除当前会话（已在上下文里），只保留原始分 >= threshold，
    /// 按衰减调整后分数排序。
    ///
    /// 暴力扫描 + 点积（几千条毫秒级，够用）。维度不一致的行不参与比对。
    pub async fn search(
        &self,
        query_emb: &[f32],
        k: usize,
        threshold: f32,
        exclude_session: &str,
    ) -> Result<Vec<MemoryHit>, String> {
        if let Some(msg) = &self.mismatch {
            return Err(msg.clone());
        }
        // 空查询 → 零向量，全库分数都是 0，直接返回空
        if query_emb.is_empty() || query_emb.iter().all(|&x| x == 0.0) {
            return Ok(Vec::new());
        }
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT id, session_id, query, answer, emb, emb_dim, created_at
                 FROM fragments WHERE session_id != ?1 AND superseded = 0",
            )
            .map_err(|e| format!("检索记忆失败：{e}"))?;
        let rows = stmt
            .query_map(params![exclude_session], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })
            .map_err(|e| format!("检索记忆失败：{e}"))?;

        let now = now_secs();
        // 双向量存储下同一 Q&A 有两条记录（query 向量 + answer 向量），
        // 按 (session_id, query) 去重、保留最高分，对上层透明
        use std::collections::HashMap;
        let mut best: HashMap<(String, String), MemoryHit> = HashMap::new();
        for row in rows {
            let (id, session_id, q, a, blob, dim, created_at) =
                row.map_err(|e| format!("读取记忆失败：{e}"))?;
            // 维度不一致不参与比对（防御性校验；签名检查已挡住跨后端混检）
            if dim as usize != query_emb.len() {
                continue;
            }
            let score = cosine(query_emb, &decode_emb(&blob));
            if score < threshold {
                continue;
            }
            let adjusted = (f64::from(score) * decay_factor(created_at, now)) as f32;
            let key = (session_id.clone(), q.clone());
            match best.get(&key) {
                Some(prev) if prev.score >= adjusted => {}
                _ => {
                    best.insert(
                        key,
                        MemoryHit {
                            id,
                            query: q,
                            answer: a,
                            score: adjusted,
                            session_id,
                            created_at,
                        },
                    );
                }
            }
        }
        let mut hits: Vec<MemoryHit> = best.into_values().collect();
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(k);
        Ok(hits)
    }

    /// 列出记忆（按 Q&A 去重，新的在前）
    pub fn list(&self, limit: usize) -> Result<Vec<MemoryEntry>, String> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT MIN(id), session_id, query, answer, embed_id, created_at, superseded
                 FROM fragments
                 GROUP BY session_id, query, answer
                 ORDER BY MIN(id) DESC LIMIT ?1",
            )
            .map_err(|e| format!("列出记忆失败：{e}"))?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                Ok(MemoryEntry {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    query: row.get(2)?,
                    answer: row.get(3)?,
                    embed_id: row.get(4)?,
                    created_at: row.get(5)?,
                    superseded: row.get::<_, i64>(6)? != 0,
                })
            })
            .map_err(|e| format!("列出记忆失败：{e}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("列出记忆失败：{e}"))
    }

    /// 删除指定记忆（同一 Q&A 的双向量两行一起删）；返回是否命中
    pub fn delete(&self, id: i64) -> Result<bool, String> {
        let conn = self.conn();
        let key: Option<(String, String, String)> = conn
            .query_row(
                "SELECT session_id, query, answer FROM fragments WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .ok();
        let Some((sid, q, a)) = key else {
            return Ok(false);
        };
        let n = conn
            .execute(
                "DELETE FROM fragments WHERE session_id = ?1 AND query = ?2 AND answer = ?3",
                params![sid, q, a],
            )
            .map_err(|e| format!("删除记忆失败：{e}"))?;
        Ok(n > 0)
    }

    /// 统计：总条数（按 Q&A 去重）/ 各后端分布 / 时间跨度
    pub fn stats(&self) -> Result<MemoryStats, String> {
        let conn = self.conn();
        let total: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM (SELECT 1 FROM fragments GROUP BY session_id, query, answer)",
                [],
                |row| row.get(0),
            )
            .map_err(|e| format!("统计记忆失败：{e}"))?;
        let mut stmt = conn
            .prepare(
                "SELECT embed_id, COUNT(*) FROM (
                     SELECT session_id, query, answer, embed_id
                     FROM fragments GROUP BY session_id, query, answer
                 ) GROUP BY embed_id ORDER BY COUNT(*) DESC",
            )
            .map_err(|e| format!("统计记忆失败：{e}"))?;
        let by_embed_id: Vec<(String, usize)> = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize))
            })
            .map_err(|e| format!("统计记忆失败：{e}"))?
            .filter_map(|r| r.ok())
            .collect();
        let (oldest, newest): (Option<i64>, Option<i64>) = conn
            .query_row("SELECT MIN(created_at), MAX(created_at) FROM fragments", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .map_err(|e| format!("统计记忆失败：{e}"))?;
        Ok(MemoryStats {
            total: total as usize,
            by_embed_id,
            oldest,
            newest,
        })
    }

    /// 用当前后端重建全部记忆向量（嵌入模型变更后调用）
    ///
    /// 按 (session_id, query, answer, created_at) 分组还原 Q&A 对
    /// （store 先插 query 向量行再插 answer 向量行，组内按 id 排序第一行为 query 向量），
    /// 每组重新 embed 一次并 UPDATE。失败条目跳过并计数。
    /// progress 回调每 50 条及结束时触发（参数：已处理, 总数）。
    pub async fn migrate(
        &self,
        provider: &dyn EmbeddingProvider,
        progress: &(dyn Fn(usize, usize) + Sync),
    ) -> Result<MigrateReport, String> {
        // 组 key → 组内行 id 列表（按 id 升序，保持插入序）
        use std::collections::BTreeMap;
        let rows: Vec<(i64, String, String, String, i64)> = {
            let conn = self.conn();
            let mut stmt = conn
                .prepare(
                    "SELECT id, session_id, query, answer, created_at FROM fragments ORDER BY id",
                )
                .map_err(|e| format!("读取记忆失败：{e}"))?;
            let mapped = stmt
                .query_map([], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                })
                .map_err(|e| format!("读取记忆失败：{e}"))?;
            mapped.filter_map(|r| r.ok()).collect()
        };

        let mut groups: BTreeMap<(String, String, String, i64), Vec<i64>> = BTreeMap::new();
        for (id, sid, q, a, ts) in rows {
            groups.entry((sid, q, a, ts)).or_default().push(id);
        }

        let total = groups.len();
        let mut migrated = 0usize;
        let mut failed = 0usize;
        let new_id = provider.id().to_string();
        for (i, ((_sid, q, a, _ts), ids)) in groups.into_iter().enumerate() {
            // query / answer 各嵌入一次；任一失败则跳过该组
            let result = async {
                let qv = provider.embed(&q).await?;
                let av = provider.embed(&a).await?;
                Ok::<(Vec<f32>, Vec<f32>), String>((qv, av))
            }
            .await;
            match result {
                Ok((qv, av)) => {
                    for (j, row_id) in ids.iter().enumerate() {
                        // 第一行是 query 向量，其余是 answer 向量（插入序保证）
                        let v = if j == 0 { &qv } else { &av };
                        // 每组现取现放锁：不跨 await 持有
                        self.conn()
                            .execute(
                                "UPDATE fragments SET emb = ?1, emb_dim = ?2, embed_id = ?3 WHERE id = ?4",
                                params![encode_emb(v), v.len() as i64, new_id, row_id],
                            )
                            .map_err(|e| format!("迁移写入失败：{e}"))?;
                    }
                    migrated += 1;
                }
                Err(e) => {
                    failed += 1;
                    tracing::warn!("记忆迁移跳过（嵌入失败）：{e}");
                }
            }
            let done = i + 1;
            if done % 50 == 0 || done == total {
                progress(done, total);
            }
        }
        Ok(MigrateReport {
            total,
            migrated,
            failed,
        })
    }

    /// 测试用：以指定 created_at 造数据（不做覆盖检查）
    #[cfg(test)]
    async fn store_at(
        &self,
        session_id: &str,
        query: &str,
        answer: &str,
        q_emb: &[f32],
        a_emb: &[f32],
        created_at: i64,
    ) -> Result<(), String> {
        self.insert_pair(session_id, query, answer, q_emb, a_emb, created_at)
    }
}

#[cfg(all(test, feature = "l3-memory"))]
mod tests {
    use super::*;

    /// 测试用假后端：固定维度、返回确定性向量（首维 = 文本长度归一，验证管道用）
    struct FakeEmbedding {
        dim: usize,
    }

    #[async_trait::async_trait]
    impl EmbeddingProvider for FakeEmbedding {
        fn id(&self) -> &str {
            "fake"
        }

        fn dim(&self) -> usize {
            self.dim
        }

        async fn embed(&self, text: &str) -> Result<Vec<f32>, String> {
            let mut v = vec![0.0f32; self.dim];
            if !v.is_empty() {
                v[0] = text.chars().count() as f32;
                if self.dim > 1 {
                    v[1] = 1.0;
                }
            }
            let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                for x in &mut v {
                    *x /= norm;
                }
            }
            Ok(v)
        }
    }

    fn norm(v: &[f32]) -> f32 {
        v.iter().map(|x| x * x).sum::<f32>().sqrt()
    }

    fn temp_db() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mem.db").to_string_lossy().to_string();
        (dir, path)
    }

    /// hash 后端快捷写入
    async fn store_text(store: &MemoryStore, sid: &str, q: &str, a: &str) {
        store
            .store(sid, q, a, &embed(q), &embed(a), 0.92)
            .await
            .unwrap();
    }

    // ---------- 嵌入基础 ----------

    #[test]
    fn test_embed_deterministic() {
        assert_eq!(embed("我喜欢蓝色"), embed("我喜欢蓝色"));
    }

    #[test]
    fn test_embed_normalized() {
        let v = embed("我喜欢蓝色，尤其是深蓝色");
        assert!((norm(&v) - 1.0).abs() < 1e-5);
        // 空文本返回零向量
        assert_eq!(norm(&embed("")), 0.0);
    }

    #[test]
    fn test_embed_similarity_order() {
        let a = embed("我喜欢蓝色");
        let similar = embed("喜欢蓝色的我");
        let unrelated = embed("数据库索引优化");
        assert!(cosine(&a, &similar) > cosine(&a, &unrelated));
    }

    #[test]
    fn test_score_identical_text() {
        let a = embed("我最喜欢的颜色是蓝色");
        let b = embed("我最喜欢的颜色是蓝色");
        assert!((cosine(&a, &b) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn test_unrelated_below_threshold() {
        let a = embed("数据库索引优化");
        let b = embed("我养了一只橘猫");
        assert!(cosine(&a, &b) < 0.30);
    }

    #[test]
    fn test_semantic_paraphrase_order() {
        let unrelated = embed("Rust所有权与生命周期");
        let pairs = [
            ("我养了一只橘猫叫咪咪", "我的宠物猫名字"),
            ("我的项目代号是凤凰计划", "凤凰计划代号"),
            ("我最喜欢的颜色是蓝色", "我喜欢的颜色"),
        ];
        for (a, b) in pairs {
            let sim_pair = cosine(&embed(a), &embed(b));
            let sim_unrelated = cosine(&embed(a), &unrelated);
            assert!(
                sim_pair > sim_unrelated,
                "改写对 ({a}, {b}) 相似度 {sim_pair} 应 > 无关对相似度 {sim_unrelated}"
            );
        }
    }

    // ---------- EmbeddingProvider ----------

    #[tokio::test]
    async fn test_hash_embedding_matches_embed_fn() {
        // hash 后端包装输出与原 embed() 完全一致
        let p = HashEmbedding;
        assert_eq!(p.id(), "hash");
        assert_eq!(p.dim(), EMBED_DIM);
        for text in ["我喜欢蓝色", "", "Rust ownership 与生命周期"] {
            assert_eq!(p.embed(text).await.unwrap(), embed(text));
        }
    }

    #[test]
    fn test_api_embedding_parse_response() {
        // 只测 serde 解析，不发网络
        let body = r#"{"object":"list","data":[{"object":"embedding","index":0,"embedding":[0.1,0.2,0.3]}],"model":"embedding-3","usage":{"prompt_tokens":5,"total_tokens":5}}"#;
        let v = parse_embedding_response(body).unwrap();
        assert_eq!(v, vec![0.1f32, 0.2, 0.3]);
        // 缺字段 → 报错
        assert!(parse_embedding_response(r#"{"data":[]}"#).is_err());
        assert!(parse_embedding_response("not json").is_err());
        assert!(parse_embedding_response(r#"{"data":[{"embedding":["x"]}]}"#).is_err());
    }

    #[test]
    fn test_api_embedding_id() {
        let p = ApiEmbedding::new("https://example.com/v4/", "k", "embedding-3");
        assert_eq!(p.id(), "openai_compat:embedding-3");
        assert_eq!(p.dim(), 0); // 首次调用前未知
    }

    // ---------- 存储 / 检索 ----------

    #[tokio::test]
    async fn test_store_and_search() {
        let (_dir, path) = temp_db();
        let store = MemoryStore::open(&path, "hash").unwrap();
        store_text(&store, "s1", "我喜欢蓝色", "好的，我记住了你喜欢蓝色").await;
        store_text(&store, "s1", "今晚吃什么", "推荐你去吃火锅").await;
        store_text(&store, "s1", "Rust 泛型怎么写", "用 <T: Trait> 约束即可").await;

        let hits = store
            .search(&embed("我最喜欢的颜色"), 3, 0.30, "other")
            .await
            .unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].query.contains("蓝色"));
        // 阈值过滤生效：超高阈值搜不到
        let none = store
            .search(&embed("我最喜欢的颜色"), 3, 0.999, "other")
            .await
            .unwrap();
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn test_exclude_current_session() {
        let (_dir, path) = temp_db();
        let store = MemoryStore::open(&path, "hash").unwrap();
        store_text(&store, "s1", "我喜欢蓝色", "好的，我记住了你喜欢蓝色").await;
        // 同 session 的条目被排除
        let hits = store.search(&embed("我喜欢蓝色"), 3, 0.0, "s1").await.unwrap();
        assert!(hits.is_empty());
        // 换 session 能搜到
        let hits = store.search(&embed("我喜欢蓝色"), 3, 0.0, "s2").await.unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[tokio::test]
    async fn test_persistence_across_open() {
        let (_dir, path) = temp_db();
        {
            let store = MemoryStore::open(&path, "hash").unwrap();
            store_text(&store, "s1", "my favorite color is blue", "noted, you like blue").await;
        }
        // drop 后重新 open，还能搜到
        let store = MemoryStore::open(&path, "hash").unwrap();
        let hits = store
            .search(&embed("what color do I like"), 3, 0.1, "s2")
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].answer.contains("blue"));
    }

    #[tokio::test]
    async fn test_mixed_language_search() {
        let (_dir, path) = temp_db();
        let store = MemoryStore::open(&path, "hash").unwrap();
        store_text(&store, "s1", "介绍一下 Rust 的 ownership", "ownership 是所有权机制").await;
        store_text(&store, "s1", "今天天气怎么样", "我不知道实时天气").await;
        // 中英混合查询能命中中文条目
        let hits = store
            .search(&embed("what is ownership in Rust"), 3, 0.1, "s2")
            .await
            .unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].query.contains("ownership"));
    }

    #[tokio::test]
    async fn test_short_query() {
        let (_dir, path) = temp_db();
        let store = MemoryStore::open(&path, "hash").unwrap();
        store_text(&store, "s1", "我喜欢蓝色", "好的，我记住了你喜欢蓝色").await;
        store_text(&store, "s1", "今晚吃什么", "推荐你去吃火锅").await;
        // 单字查询：一元 n-gram 应能命中含"蓝"的条目
        let hits = store.search(&embed("蓝"), 3, 0.0, "s2").await.unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].query.contains("蓝"));
    }

    #[tokio::test]
    async fn test_threshold_filter_effective() {
        let (_dir, path) = temp_db();
        let store = MemoryStore::open(&path, "hash").unwrap();
        store_text(&store, "s1", "我喜欢蓝色", "好的，我记住了你喜欢蓝色").await;
        // 用改写查询（相似度介于 0~1）：高阈值过滤、零阈值命中
        let none = store.search(&embed("我超喜欢蓝色"), 3, 0.99, "s2").await.unwrap();
        assert!(none.is_empty());
        let hits = store.search(&embed("我超喜欢蓝色"), 3, 0.0, "s2").await.unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].score > 0.0);
    }

    // ---------- 维度自适应 ----------

    #[tokio::test]
    async fn test_dimension_adaptive() {
        let (_dir, path) = temp_db();
        let store = MemoryStore::open(&path, "openai_compat:test-model").unwrap();
        // 手造 768 维向量：A 首维为 1，B 第二维为 1
        let mut va = vec![0.0f32; 768];
        va[0] = 1.0;
        let mut vb = vec![0.0f32; 768];
        vb[1] = 1.0;
        store
            .store("s1", "记忆A", "回答A", &va, &va, 0.92)
            .await
            .unwrap();
        store
            .store("s1", "记忆B", "回答B", &vb, &vb, 0.92)
            .await
            .unwrap();
        // 768 维查询正常命中正交方向正确的条目
        let hits = store.search(&va, 5, 0.5, "s2").await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].query, "记忆A");
        // 维度不一致的查询不回结果（全库跳过）
        let wrong_dim = vec![1.0f32; 256];
        let hits = store.search(&wrong_dim, 5, 0.0, "s2").await.unwrap();
        assert!(hits.is_empty());
    }

    // ---------- 模型签名 ----------

    #[tokio::test]
    async fn test_model_signature_mismatch() {
        let (_dir, path) = temp_db();
        {
            let store = MemoryStore::open(&path, "hash").unwrap();
            store_text(&store, "s1", "我喜欢蓝色", "好的").await;
        }
        // 换后端签名重新打开：search/store 报 Err 提示 migrate，管理操作不受影响
        let store = MemoryStore::open(&path, "openai_compat:embedding-3").unwrap();
        assert!(store.mismatch().is_some());
        let err = store.search(&embed("我喜欢蓝色"), 3, 0.0, "s2").await;
        assert!(err.is_err());
        let msg = err.unwrap_err();
        assert!(msg.contains("migrate"), "错误应提示 migrate：{msg}");
        let err = store
            .store("s1", "新记忆", "新回答", &embed("新记忆"), &embed("新回答"), 0.92)
            .await;
        assert!(err.is_err());
        assert_eq!(store.stats().unwrap().total, 1);
        // 原签名打开则一切正常
        let store = MemoryStore::open(&path, "hash").unwrap();
        assert!(store.mismatch().is_none());
        assert!(!store
            .search(&embed("我喜欢蓝色"), 3, 0.0, "s2")
            .await
            .unwrap()
            .is_empty());
    }

    // ---------- 旧库迁移（schema 自动补列） ----------

    #[tokio::test]
    async fn test_legacy_schema_auto_upgrade() {
        let (_dir, path) = temp_db();
        // 手工建 v0.1 旧 schema（无 emb_dim/embed_id/superseded 列）并插入旧数据
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute(
                "CREATE TABLE fragments (
                    id INTEGER PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    query TEXT NOT NULL,
                    answer TEXT NOT NULL,
                    emb BLOB NOT NULL,
                    created_at INTEGER NOT NULL
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO fragments (session_id, query, answer, emb, created_at)
                 VALUES ('s1', '我喜欢蓝色', '好的', ?1, 1000)",
                params![encode_emb(&embed("我喜欢蓝色"))],
            )
            .unwrap();
        }
        // open 自动补列不报错，旧行默认 hash/256，检索正常
        let store = MemoryStore::open(&path, "hash").unwrap();
        assert!(store.mismatch().is_none());
        let hits = store.search(&embed("我喜欢蓝色"), 3, 0.0, "s2").await.unwrap();
        assert_eq!(hits.len(), 1);
        let entries = store.list(10).unwrap();
        assert_eq!(entries[0].embed_id, "hash");
        assert!(!entries[0].superseded);
    }

    // ---------- 时间衰减 ----------

    #[tokio::test]
    async fn test_decay_ordering() {
        let (_dir, path) = temp_db();
        let store = MemoryStore::open(&path, "hash").unwrap();
        let now = now_secs();
        // 两条同文本记忆（同原始分）：一条 30 天内，一条 90 天+
        let qv = embed("我喜欢蓝色");
        let av = embed("好的");
        store
            .store_at("s_old", "我喜欢蓝色", "好的", &qv, &av, now - 100 * 86_400)
            .await
            .unwrap();
        store
            .store_at("s_new", "我喜欢蓝色", "好的", &qv, &av, now - 10 * 86_400)
            .await
            .unwrap();
        let hits = store.search(&qv, 5, 0.0, "other").await.unwrap();
        assert_eq!(hits.len(), 2);
        // 衰减后新记忆排前
        assert_eq!(hits[0].session_id, "s_new");
        assert_eq!(hits[1].session_id, "s_old");
        assert!((hits[0].score - hits[1].score).abs() > 1e-6);
        // 衰减档位：90 天+ 0.75
        assert!((hits[1].score - hits[0].score * 0.75).abs() < 1e-4);
    }

    #[test]
    fn test_decay_factor_tiers() {
        let now = 1_000_000_000i64;
        assert_eq!(decay_factor(now, now), 1.0);
        assert_eq!(decay_factor(now - 30 * 86_400, now), 1.0);
        assert_eq!(decay_factor(now - 31 * 86_400, now), 0.9);
        assert_eq!(decay_factor(now - 90 * 86_400, now), 0.9);
        assert_eq!(decay_factor(now - 91 * 86_400, now), 0.75);
    }

    // ---------- 同主题覆盖 ----------

    #[tokio::test]
    async fn test_supersede_on_near_duplicate() {
        let (_dir, path) = temp_db();
        let store = MemoryStore::open(&path, "hash").unwrap();
        // 长文本微改：hash 后端也能达到 >0.92 的相似度
        let q1 = "我的项目代号是凤凰计划，团队目前有五个人，预计下个月中旬正式启动第一阶段开发工作";
        let q2 = "我的项目代号是凤凰计划，团队目前有六个人，预计下个月中旬正式启动第一阶段开发工作";
        assert!(cosine(&embed(q1), &embed(q2)) > 0.92, "测试数据本身应超过覆盖阈值");
        store_text(&store, "s1", q1, "好的，已记住凤凰计划").await;
        store_text(&store, "s1", q2, "好的，已记住凤凰计划最新进展").await;
        // 第一条被标记覆盖
        let entries = store.list(10).unwrap();
        assert_eq!(entries.len(), 2);
        let first = entries.iter().find(|e| e.query == q1).unwrap();
        let second = entries.iter().find(|e| e.query == q2).unwrap();
        assert!(first.superseded);
        assert!(!second.superseded);
        // 检索只回第二条
        let hits = store.search(&embed(q2), 5, 0.0, "s2").await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].query, q2);
    }

    #[tokio::test]
    async fn test_no_supersede_when_distinct() {
        let (_dir, path) = temp_db();
        let store = MemoryStore::open(&path, "hash").unwrap();
        store_text(&store, "s1", "我喜欢蓝色", "好的").await;
        store_text(&store, "s1", "数据库索引优化", "用 B+ 树").await;
        // 相似度低的条目互不覆盖
        let entries = store.list(10).unwrap();
        assert!(entries.iter().all(|e| !e.superseded));
    }

    // ---------- migrate ----------

    #[tokio::test]
    async fn test_migrate_to_new_backend() {
        let (_dir, path) = temp_db();
        {
            let store = MemoryStore::open(&path, "hash").unwrap();
            store_text(&store, "s1", "我喜欢蓝色", "好的，记住了").await;
            store_text(&store, "s1", "今晚吃什么", "推荐火锅").await;
            store_text(&store, "s1", "Rust 生命周期", "借用检查器来保证").await;
        }
        // 换后端打开（签名不匹配，但 migrate 允许）
        let fake = FakeEmbedding { dim: 64 };
        let store = MemoryStore::open(&path, fake.id()).unwrap();
        assert!(store.mismatch().is_some());
        let progress = std::sync::Mutex::new(Vec::new());
        let report = store
            .migrate(&fake, &|done, total| {
                if let Ok(mut g) = progress.lock() {
                    g.push((done, total));
                }
            })
            .await
            .unwrap();
        assert_eq!(report.total, 3);
        assert_eq!(report.migrated, 3);
        assert_eq!(report.failed, 0);
        // 结束时触发了一次进度回调
        let g = progress.lock().unwrap();
        assert!(g.contains(&(3, 3)));
        drop(g);
        // embed_id 全部更新，新签名检索正常
        assert_eq!(store.stats().unwrap().by_embed_id, vec![("fake".to_string(), 3)]);
        let store = MemoryStore::open(&path, "fake").unwrap();
        assert!(store.mismatch().is_none());
        let qv = fake.embed("我喜欢蓝色").await.unwrap();
        let hits = store.search(&qv, 5, 0.0, "s2").await.unwrap();
        assert!(!hits.is_empty());
        // 维度也更新了
        let entries = store.list(10).unwrap();
        assert_eq!(entries.len(), 3);
    }

    // ---------- 管理命令 ----------

    #[tokio::test]
    async fn test_list_limit_and_order() {
        let (_dir, path) = temp_db();
        let store = MemoryStore::open(&path, "hash").unwrap();
        store_text(&store, "s1", "第一条记忆", "回答一").await;
        store_text(&store, "s1", "第二条记忆", "回答二").await;
        store_text(&store, "s1", "第三条记忆", "回答三").await;
        let all = store.list(10).unwrap();
        assert_eq!(all.len(), 3);
        // 新的在前
        assert_eq!(all[0].query, "第三条记忆");
        let limited = store.list(2).unwrap();
        assert_eq!(limited.len(), 2);
    }

    #[tokio::test]
    async fn test_delete() {
        let (_dir, path) = temp_db();
        let store = MemoryStore::open(&path, "hash").unwrap();
        store_text(&store, "s1", "我喜欢蓝色", "好的").await;
        store_text(&store, "s1", "今晚吃什么", "火锅").await;
        let entries = store.list(10).unwrap();
        let target = entries.iter().find(|e| e.query == "我喜欢蓝色").unwrap();
        assert!(store.delete(target.id).unwrap());
        // 双向量两行一起删掉
        assert_eq!(store.stats().unwrap().total, 1);
        let hits = store.search(&embed("我喜欢蓝色"), 3, 0.0, "s2").await.unwrap();
        assert!(hits.iter().all(|h| h.query != "我喜欢蓝色"));
        // 不存在的 id 返回 false 而不是报错
        assert!(!store.delete(99999).unwrap());
    }

    #[tokio::test]
    async fn test_stats() {
        let (_dir, path) = temp_db();
        let store = MemoryStore::open(&path, "hash").unwrap();
        // 空库
        let s = store.stats().unwrap();
        assert_eq!(s.total, 0);
        assert!(s.by_embed_id.is_empty());
        assert_eq!(s.oldest, None);
        // 造两条不同时间的记忆
        let now = now_secs();
        store
            .store_at("s1", "记忆一", "回答一", &embed("记忆一"), &embed("回答一"), now - 86_400)
            .await
            .unwrap();
        store_text(&store, "s1", "记忆二", "回答二").await;
        let s = store.stats().unwrap();
        assert_eq!(s.total, 2);
        assert_eq!(s.by_embed_id, vec![("hash".to_string(), 2)]);
        assert_eq!(s.oldest, Some(now - 86_400));
        assert!(s.newest.unwrap() >= now);
    }

    // ---------- 规模（手动跑） ----------

    #[tokio::test]
    #[ignore]
    async fn test_scale_200() {
        let colors = ["红", "蓝", "绿", "黄", "紫"];
        let numbers = ["编号0", "编号1", "编号2", "编号3", "编号4", "编号5", "编号6"];
        let (_dir, path) = temp_db();
        let store = MemoryStore::open(&path, "hash").unwrap();
        for i in 0..200 {
            let content = format!(
                "用户测试条目 {i}：主题是{}{}",
                colors[i % colors.len()],
                numbers[i % numbers.len()]
            );
            store_text(&store, "s1", &content, "已记录").await;
        }
        let hits = store
            .search(&embed("主题是红色编号5的条目"), 3, 0.0, "s2")
            .await
            .unwrap();
        assert!(!hits.is_empty());
        let top = &hits[0];
        assert!(top.query.contains('红'), "top-1 应含\"红\"：{}", top.query);
        assert!(
            top.query.contains("编号5"),
            "top-1 应含\"编号5\"：{}",
            top.query
        );
        // i % 5 == 0 且 i % 7 == 5 → i ∈ {5, 40, 75, 110, 145, 180}（同分并列，任一即可）
        let expected: Vec<String> = [5, 40, 75, 110, 145, 180]
            .iter()
            .map(|i| format!("用户测试条目 {i}：主题是红编号5"))
            .collect();
        assert!(
            expected.contains(&top.query),
            "top-1 应为候选之一：{}",
            top.query
        );
    }
}
