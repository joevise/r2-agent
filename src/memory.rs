//! L3 跨会话记忆索引（自传体记忆）
//!
//! 设计目标：Agent 能记住【自己经历过的事】——上个会话里用户说过的关键信息，
//! 在新会话中被相关问题唤起。这不是 RAG（检索外部知识），是检索 Agent 自身经历。
//!
//! v0.1 务实方案（重要决策）：
//! - Embedding：本地字符三元组哈希嵌入（256 维，零外部依赖），不用外部 embedding API。
//!   理由：极简哲学 + 离线可用 + "还记得上次说过什么"场景够用。
//!   DashScope/OpenAI embedding 是未来升级点（config 预留）。
//! - 记忆条目：每轮 Q&A 一条（用户输入 + assistant 最终回答），
//!   不做"会话结束提取关键片段"。理由：不需要额外 LLM 调用，立即有价值；
//!   提取式精炼是后续优化。
//! - 检索：暴力扫描 + 余弦点积（几千条毫秒级，够用）。

use rusqlite::{params, Connection};

/// 嵌入向量维度
pub const EMBED_DIM: usize = 256;

/// 单条记忆回答的最大存储字符数（防膨胀）
const MAX_ANSWER_CHARS: usize = 2000;

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

/// 向量 → BLOB：256 个 f32 按 to_bits 小端打包
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

/// 一条被唤起的跨会话记忆
pub struct MemoryHit {
    /// 当时的用户输入
    pub query: String,
    /// 当时的回答（可截断存储）
    pub answer: String,
    pub score: f32,
    pub session_id: String,
    pub created_at: i64,
}

/// 记忆存储：SQLite 单表
pub struct MemoryStore {
    conn: Connection,
}

impl MemoryStore {
    /// 打开（不存在则建表）
    pub fn open(path: &str) -> Result<Self, String> {
        let conn =
            Connection::open(path).map_err(|e| format!("打开记忆库失败（{path}）：{e}"))?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS fragments (
                id INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL,
                query TEXT NOT NULL,
                answer TEXT NOT NULL,
                emb BLOB NOT NULL,
                created_at INTEGER NOT NULL
            )",
            [],
        )
        .map_err(|e| format!("创建记忆表失败：{e}"))?;
        Ok(Self { conn })
    }

    /// 存一轮 Q&A（answer 截断到 MAX_ANSWER_CHARS 防膨胀）
    pub fn store(&self, session_id: &str, query: &str, answer: &str) -> Result<(), String> {
        let answer: String = answer.chars().take(MAX_ANSWER_CHARS).collect();
        // 双向量策略：query 和 answer 各存一条独立 fragments 记录（同 session/q/a）。
        // - 拼接嵌入会让两边互相稀释（T7 长文本埋藏检索变弱），
        //   还会抬高无关查询的基础相似度（T4 误召回：长 answer 常用字 n-gram 重叠 → 0.385）。
        // - 只嵌 query 会丢掉语义桥接（T2："宠物"查询匹配不上"橘猫"输入，
        //   但能匹配 GLM 回答里的"宠物猫"）。
        // - 检索时天然取 max(query分, answer分)，两边哪边命中都算。
        let emb_q = encode_emb(&embed(query));
        let emb_a = encode_emb(&embed(&answer));
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.conn
            .execute(
                "INSERT INTO fragments (session_id, query, answer, emb, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![session_id, query, answer, emb_q, created_at],
            )
            .map_err(|e| format!("写入记忆失败：{e}"))?;
        self.conn
            .execute(
                "INSERT INTO fragments (session_id, query, answer, emb, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![session_id, query, answer, emb_a, created_at],
            )
            .map_err(|e| format!("写入记忆失败：{e}"))?;
        Ok(())
    }

    /// 检索 top-k：排除当前会话（已在上下文里），只保留 score >= threshold
    ///
    /// 暴力扫描 + 点积（几千条毫秒级，够用）。
    pub fn search(
        &self,
        query: &str,
        k: usize,
        threshold: f32,
        exclude_session: &str,
    ) -> Result<Vec<MemoryHit>, String> {
        let qv = embed(query);
        // 空查询 → 零向量，全库分数都是 0，直接返回空
        if qv.iter().all(|&x| x == 0.0) {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .conn
            .prepare(
                "SELECT session_id, query, answer, emb, created_at
                 FROM fragments WHERE session_id != ?1",
            )
            .map_err(|e| format!("检索记忆失败：{e}"))?;
        let rows = stmt
            .query_map(params![exclude_session], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })
            .map_err(|e| format!("检索记忆失败：{e}"))?;

        // 双向量存储下同一 Q&A 有两条记录（query 向量 + answer 向量），
        // 按 (session_id, query) 去重、保留最高分，对上层透明
        use std::collections::HashMap;
        let mut best: HashMap<(String, String), MemoryHit> = HashMap::new();
        for row in rows {
            let (session_id, q, a, emb, created_at) =
                row.map_err(|e| format!("读取记忆失败：{e}"))?;
            let score = cosine(&qv, &decode_emb(&emb));
            if score < threshold {
                continue;
            }
            let key = (session_id.clone(), q.clone());
            match best.get(&key) {
                Some(prev) if prev.score >= score => {}
                _ => {
                    best.insert(
                        key,
                        MemoryHit { query: q, answer: a, score, session_id, created_at },
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
}

#[cfg(all(test, feature = "l3-memory"))]
mod tests {
    use super::*;

    fn norm(v: &[f32]) -> f32 {
        v.iter().map(|x| x * x).sum::<f32>().sqrt()
    }

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
    fn test_store_and_search() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mem.db").to_string_lossy().to_string();
        let store = MemoryStore::open(&path).unwrap();
        store
            .store("s1", "我喜欢蓝色", "好的，我记住了你喜欢蓝色")
            .unwrap();
        store
            .store("s1", "今晚吃什么", "推荐你去吃火锅")
            .unwrap();
        store
            .store("s1", "Rust 泛型怎么写", "用 <T: Trait> 约束即可")
            .unwrap();

        let hits = store.search("我最喜欢的颜色", 3, 0.30, "other").unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].query.contains("蓝色"));
        // 阈值过滤生效：超高阈值搜不到
        let none = store.search("我最喜欢的颜色", 3, 0.999, "other").unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn test_exclude_current_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mem.db").to_string_lossy().to_string();
        let store = MemoryStore::open(&path).unwrap();
        store
            .store("s1", "我喜欢蓝色", "好的，我记住了你喜欢蓝色")
            .unwrap();
        // 同 session 的条目被排除
        let hits = store.search("我喜欢蓝色", 3, 0.0, "s1").unwrap();
        assert!(hits.is_empty());
        // 换 session 能搜到
        let hits = store.search("我喜欢蓝色", 3, 0.0, "s2").unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn test_persistence_across_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mem.db").to_string_lossy().to_string();
        {
            let store = MemoryStore::open(&path).unwrap();
            store
                .store("s1", "my favorite color is blue", "noted, you like blue")
                .unwrap();
        }
        // drop 后重新 open，还能搜到
        let store = MemoryStore::open(&path).unwrap();
        let hits = store.search("what color do I like", 3, 0.1, "s2").unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].answer.contains("blue"));
    }

    #[test]
    fn test_mixed_language_search() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mem.db").to_string_lossy().to_string();
        let store = MemoryStore::open(&path).unwrap();
        store
            .store("s1", "介绍一下 Rust 的 ownership", "ownership 是所有权机制")
            .unwrap();
        store
            .store("s1", "今天天气怎么样", "我不知道实时天气")
            .unwrap();
        // 中英混合查询能命中中文条目
        let hits = store.search("what is ownership in Rust", 3, 0.1, "s2").unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].query.contains("ownership"));
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
    fn test_short_query() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mem.db").to_string_lossy().to_string();
        let store = MemoryStore::open(&path).unwrap();
        store
            .store("s1", "我喜欢蓝色", "好的，我记住了你喜欢蓝色")
            .unwrap();
        store
            .store("s1", "今晚吃什么", "推荐你去吃火锅")
            .unwrap();
        // 单字查询：一元 n-gram 应能命中含"蓝"的条目
        let hits = store.search("蓝", 3, 0.0, "s2").unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].query.contains("蓝"));
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

    #[test]
    #[ignore]
    fn test_scale_200() {
        let colors = ["红", "蓝", "绿", "黄", "紫"];
        let numbers = ["编号0", "编号1", "编号2", "编号3", "编号4", "编号5", "编号6"];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mem.db").to_string_lossy().to_string();
        let store = MemoryStore::open(&path).unwrap();
        for i in 0..200 {
            let content = format!(
                "用户测试条目 {i}：主题是{}{}",
                colors[i % colors.len()],
                numbers[i % numbers.len()]
            );
            store.store("s1", &content, "已记录").unwrap();
        }
        let hits = store
            .search("主题是红色编号5的条目", 3, 0.0, "s2")
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

    #[test]
    fn test_threshold_filter_effective() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mem.db").to_string_lossy().to_string();
        let store = MemoryStore::open(&path).unwrap();
        store
            .store("s1", "我喜欢蓝色", "好的，我记住了你喜欢蓝色")
            .unwrap();
        // 用改写查询（相似度介于 0~1）：高阈值过滤、零阈值命中
        let none = store.search("我超喜欢蓝色", 3, 0.99, "s2").unwrap();
        assert!(none.is_empty());
        let hits = store.search("我超喜欢蓝色", 3, 0.0, "s2").unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].score > 0.0);
    }
}
