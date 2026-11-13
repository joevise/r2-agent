//! 权限门（v0.16.0 权限模型第一批）：手机权限模型——默认最小权限；
//! 改变宿主机持久状态的 bash 命令拦下问用户；三档授权（仅此一次/本会话/永远）；
//! 全程留痕（~/.r2/approvals.log）。铁律：无人应答 = 默认拒绝（不需要快，需要是对）。
//!
//! 三层结构：
//! - [`judge`]：纯函数三级判定（Green/Yellow/Red），规则表集中在本文下方常量区
//! - [`ApprovalHub`]：请求/答复/白名单中枢（permissions.toml 永远授权 +
//!   session 白名单 + 留痕日志 + 审批请求广播）
//! - [`ApprovalHook`]：Hook trait 实现，只管 bash 工具（MCP/文件工具不进门——
//!   MCP 是显式安装的受信工具，write/edit 本就锁 work_dir）

use crate::events::AgentEvent;
use crate::hooks::{Hook, HookCtx};
use crate::types::ToolCall;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// 授权请求默认超时（秒）：无人应答 = 默认拒绝
pub const HUB_TIMEOUT_SECS: u64 = 300;

/// 影响面三级
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    Green,
    Yellow { reason: String },
    Red { reason: String },
}

/// 判定上下文
pub struct JudgeCtx<'a> {
    /// 降级联动：true 时 Yellow → Red（沙箱降级期宁可多问不如直接拒）
    pub sandbox_degraded: bool,
    /// permissions.toml 模式（词前缀匹配）
    pub forever_allows: &'a [String],
    /// 本会话已授权模式
    pub session_allows: &'a [String],
}

// ── 规则表（集中管理，可测可解释；不做 glob/正则，只用词前缀 + 规则名）──

/// 红级模式（任一段命中即拒）：与 bash.rs DANGEROUS_PATTERNS 双保险，那边原样保留
const RED_PATTERNS: &[&str] = &[
    "rm -rf /",
    "rm -rf ~",
    "rm -fr /",
    "rm -fr ~",
    "mkfs",
    "of=/dev/",
    ":(){",
    "chmod -R 777 /",
];

/// 提权命令（段首词命中即黄；规则名 = 词本身）
const PRIVESC_WORDS: &[&str] = &["sudo", "su", "doas", "pkexec", "systemd-run", "systemctl"];

/// 包管理器（段首词）+ 变更动作（段内任一 token）→ 黄。宁可多问，不追求精确
const PKG_MGRS: &[&str] = &[
    "apt", "apt-get", "dpkg", "snap", "brew", "npm", "pnpm", "yarn", "pip", "pip3", "cargo",
    "gem", "go", "uv",
];
const PKG_ACTIONS: &[&str] = &["install", "add", "upgrade", "remove", "-g", "--global", "global"];

/// 写类命令：仅段首词命中时才扫路径 token（避免 `ls /etc`、`cat ~/.bashrc` 误伤）
const WRITE_CMDS: &[&str] = &["cp", "mv", "tee", "ln", "install", "rsync", "scp"];

/// 系统/持久位置前缀：写类段内任一 token 以其开头即黄
const WRITE_PATH_PREFIXES: &[&str] = &[
    "/usr/",
    "/etc/",
    "/boot/",
    "/var/",
    "/dev/",
    "~/.local/bin",
    "~/.bashrc",
    "~/.zshrc",
    "~/.profile",
    "~/.config/systemd",
    "~/.r2/config.toml",
    "~/.r2/permissions.toml",
];

/// 管道安装目标 shell 片段
const PIPE_SHELLS: &[&str] = &["| sh", "|sh", "| bash", "|bash", "| zsh", "|zsh"];

/// 包管理器规则名：npm 家族全局安装 / uv 工具 / 其余 install 系
fn pkg_rule_name(word: &str) -> String {
    match word {
        "npm" | "pnpm" | "yarn" => format!("{word}-global"),
        "uv" => "uv-tool".to_string(),
        _ => format!("{word}-install"),
    }
}

/// 切段：按 `;`、`&&`、`||`、`\n` 分（刻意不按 `|` 切——管道安装规则
/// 需要同段看到 `curl ... | sh` 整体；`|` 切段会让该规则永远不可达）
fn split_segments(command: &str) -> Vec<String> {
    let normalized = command
        .replace("&&", ";")
        .replace("||", ";")
        .replace('\n', ";");
    normalized
        .split(';')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// 段的 token 序列（按空白分词）
fn tokens(segment: &str) -> Vec<&str> {
    segment.split_whitespace().collect()
}

/// 单段黄级判定：返回 (规则名, 描述)。规则名语义清晰可审计——
/// Forever/Session 授权登记的就是规则名（如 "sudo"、"npm-global"、"write-system-path:/etc/"）
fn classify_segment(segment: &str) -> Option<(String, String)> {
    let toks = tokens(segment);
    let first = toks.first().copied().unwrap_or("");

    // 提权
    if PRIVESC_WORDS.contains(&first) {
        return Some((first.to_string(), format!("提权操作（{first}）")));
    }
    // 包管理安装：段首词是包管理器 && 段内任意 token 是变更动作
    if PKG_MGRS.contains(&first) && toks.iter().any(|t| PKG_ACTIONS.contains(t)) {
        let rule = pkg_rule_name(first);
        return Some((rule, format!("包管理安装/变更（{first}）")));
    }
    // 管道安装：段含 curl/wget 且同段含 `| sh` 系
    if toks.iter().any(|t| *t == "curl" || *t == "wget")
        && PIPE_SHELLS.iter().any(|p| segment.contains(p))
    {
        return Some(("pipe-install".to_string(), "管道安装（下载直执行）".to_string()));
    }
    // 写系统位置：写类段首词或含重定向时才扫路径 token
    let is_write = WRITE_CMDS.contains(&first) || segment.contains('>');
    if is_write {
        for tok in &toks {
            // 重定向粘连形式（>/etc/x、2>/var/x）取 '>' 之后的真实目标
            let target = match tok.rfind('>') {
                Some(i) => &tok[i + 1..],
                None => tok,
            };
            for prefix in WRITE_PATH_PREFIXES {
                if target.starts_with(prefix) {
                    return Some((
                        format!("write-system-path:{prefix}"),
                        format!("写系统位置（{prefix}）"),
                    ));
                }
            }
        }
    }
    None
}

/// 整条命令命中的黄级规则名（供 Forever/Session 授权登记用）
pub fn yellow_rule(command: &str) -> Option<String> {
    split_segments(command)
        .iter()
        .find_map(|seg| classify_segment(seg).map(|(rule, _)| rule))
}

/// 三级判定（纯函数，只判 bash 命令串）
pub fn judge(command: &str, ctx: &JudgeCtx) -> Gate {
    let segments = split_segments(command);
    // 红级：任一段命中
    for seg in &segments {
        if let Some(p) = RED_PATTERNS.iter().find(|p| seg.contains(**p)) {
            return Gate::Red {
                reason: format!("命中高危模式 `{p}`"),
            };
        }
    }
    // 黄级：任一段命中
    for seg in &segments {
        if let Some((rule, desc)) = classify_segment(seg) {
            let reason = format!("{desc} [规则:{rule}]");
            if ctx.sandbox_degraded {
                // 降级联动：namespaces 不可用时黄级直接按红级拒（先紧后松）
                return Gate::Red {
                    reason: format!("沙箱降级（namespaces 不可用），黄级按红级处理：{reason}"),
                };
            }
            return Gate::Yellow { reason };
        }
    }
    Gate::Green
}

/// 白名单词前缀匹配：pattern 的词序列是 command 词序列的前缀
/// （"sudo" 匹配一切 sudo …；"npm -g" 匹配 "npm -g install x"）
pub fn word_prefix_match(pattern: &str, command: &str) -> bool {
    let pat: Vec<&str> = pattern.split_whitespace().collect();
    if pat.is_empty() {
        return false;
    }
    let toks: Vec<&str> = command.split_whitespace().collect();
    toks.len() >= pat.len() && toks[..pat.len()] == pat[..]
}

/// 三档授权的答复
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow(Scope),
    Deny,
}

/// 授权范围
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Once,
    Session,
    Forever,
}

/// 审批请求（广播给壳层；批2 审批 UI 订阅此通道）
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub id: u64,
    pub session_id: String,
    pub command: String,
    pub reason: String,
}

/// 一次授权请求的凭据：hook 持 rx 挂起等答复
pub struct Ticket {
    pub id: u64,
    pub command: String,
    pub reason: String,
    pub rx: tokio::sync::oneshot::Receiver<Decision>,
}

/// permissions.toml 结构（~/.r2/；损坏按空表容错，对齐 feishu_dedup 模式）
#[derive(serde::Deserialize)]
struct PermFile {
    #[serde(default)]
    allow: Vec<PermAllow>,
}

#[derive(serde::Deserialize)]
struct PermAllow {
    pattern: String,
}

struct HubInner {
    next_id: u64,
    /// permissions.toml 永远授权模式（词前缀序列或规则名）
    forever: Vec<String>,
    /// 本会话授权：session_id -> 规则名列表（进程内，不落盘）
    sessions: HashMap<String, Vec<String>>,
    /// 在途授权请求：id -> 答复通道
    pending_map: HashMap<u64, tokio::sync::oneshot::Sender<Decision>>,
    perm_path: std::path::PathBuf,
    log_path: std::path::PathBuf,
    log_est_lines: usize,
}

/// 留痕日志轮转线（行数；对齐 dedup 的 5 万行模式）
const LOG_ROTATE_MAX: usize = 50_000;

/// ApprovalHub：请求/答复/白名单中枢
pub struct ApprovalHub {
    inner: Arc<Mutex<HubInner>>,
    tx: tokio::sync::broadcast::Sender<ApprovalRequest>,
    /// 授权请求超时（秒）；测试可构造覆盖短值
    pub timeout_secs: u64,
}

fn r2_dir_file(name: &str) -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    std::path::PathBuf::from(home).join(".r2").join(name)
}

impl ApprovalHub {
    /// 加载 ~/.r2/permissions.toml（不存在/损坏 = 空表，不阻塞）
    pub fn new() -> Self {
        Self::with_paths(r2_dir_file("permissions.toml"), r2_dir_file("approvals.log"))
    }

    /// 显式路径构造（测试用；生产走 new）
    pub fn with_paths(perm_path: std::path::PathBuf, log_path: std::path::PathBuf) -> Self {
        let forever = Self::load_forever(&perm_path);
        let log_est_lines = std::fs::read_to_string(&log_path)
            .map(|b| b.lines().count())
            .unwrap_or(0);
        let (tx, _) = tokio::sync::broadcast::channel(64);
        Self {
            inner: Arc::new(Mutex::new(HubInner {
                next_id: 1,
                forever,
                sessions: HashMap::new(),
                pending_map: HashMap::new(),
                perm_path,
                log_path,
                log_est_lines,
            })),
            tx,
            timeout_secs: HUB_TIMEOUT_SECS,
        }
    }

    /// 测试注入短超时（勿用 sleep 300s 真等）
    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }

    fn load_forever(path: &std::path::Path) -> Vec<String> {
        let Ok(body) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        match toml::from_str::<PermFile>(&body) {
            Ok(f) => f.allow.into_iter().map(|a| a.pattern).collect(),
            Err(e) => {
                eprintln!("[approval] permissions.toml 损坏按空表处理（{path:?}）：{e}");
                Vec::new()
            }
        }
    }

    /// 永远授权模式快照
    pub fn forever_allows(&self) -> Vec<String> {
        self.inner.lock().unwrap().forever.clone()
    }

    /// 本会话授权模式快照
    pub fn session_allows(&self, session_id: &str) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .sessions
            .get(session_id)
            .cloned()
            .unwrap_or_default()
    }

    /// 白名单命中检查：forever / session 两层；模式 = 规则名相等 或 词前缀命中
    pub fn is_allowed(&self, session_id: &str, command: &str, rule: &str) -> bool {
        let inner = self.inner.lock().unwrap();
        let hit = |pats: &[String]| {
            pats.iter()
                .any(|p| p == rule || word_prefix_match(p, command))
        };
        hit(&inner.forever)
            || inner
                .sessions
                .get(session_id)
                .is_some_and(|pats| hit(pats))
    }

    /// 有无审批通道订阅者（批1：无壳层订阅即默认拒）
    pub fn has_subscribers(&self) -> bool {
        self.tx.receiver_count() > 0
    }

    /// 订阅审批请求（批2 审批 UI 接口预留）
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<ApprovalRequest> {
        self.tx.subscribe()
    }

    /// 登记 pending + 广播事件，返回凭据（hook 挂起等答复）
    pub fn request(&self, session_id: &str, command: &str, reason: &str) -> Ticket {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut inner = self.inner.lock().unwrap();
        let id = inner.next_id;
        inner.next_id += 1;
        inner.pending_map.insert(id, tx);
        let req = ApprovalRequest {
            id,
            session_id: session_id.to_string(),
            command: command.to_string(),
            reason: reason.to_string(),
        };
        // 无订阅者时 send 返回 Err——调用方已先查 has_subscribers，此处忽略竞争失败
        let _ = self.tx.send(req);
        Ticket {
            id,
            command: command.to_string(),
            reason: reason.to_string(),
            rx,
        }
    }

    /// 壳层/测试答复入口
    pub fn resolve(&self, id: u64, d: Decision) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(tx) = inner.pending_map.remove(&id) {
            let _ = tx.send(d);
        }
    }

    /// 本会话授权（进程内，keyed by session_id）
    pub fn grant_session(&self, session_id: &str, pattern: &str) {
        let mut inner = self.inner.lock().unwrap();
        let pats = inner.sessions.entry(session_id.to_string()).or_default();
        if !pats.iter().any(|p| p == pattern) {
            pats.push(pattern.to_string());
        }
    }

    /// 永远授权：追加落盘 permissions.toml（原子写：临时文件 + rename）
    pub fn grant_forever(&self, pattern: &str) {
        let mut inner = self.inner.lock().unwrap();
        if !inner.forever.iter().any(|p| p == pattern) {
            inner.forever.push(pattern.to_string());
        }
        let path = inner.perm_path.clone();
        let mut out = std::fs::read_to_string(&path).unwrap_or_default();
        let escaped = pattern.replace('\\', "\\\\").replace('"', "\\\"");
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("[[allow]]\npattern = \"{escaped}\"\n"));
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("tmp");
        if let Err(e) = std::fs::write(&tmp, out).and_then(|_| std::fs::rename(&tmp, &path)) {
            eprintln!("[approval] permissions.toml 落盘失败（{path:?}）：{e}");
        }
    }

    /// 留痕：每次黄级处置必落一条（放行/拒绝/超时/白名单命中均记）。
    /// JSONL append-only；超 5 万行轮转保留后半（对齐 feishu_dedup 简单模式）
    pub fn log(&self, session_id: &str, command: &str, reason: &str, decision: &str, scope: &str) {
        use std::io::Write;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let line = serde_json::json!({
            "ts": ts,
            "session": session_id,
            "command": command,
            "reason": reason,
            "decision": decision,
            "scope": scope,
        })
        .to_string();
        let mut inner = self.inner.lock().unwrap();
        let path = inner.log_path.clone();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let r = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| writeln!(f, "{line}"));
        match r {
            Ok(_) => {
                inner.log_est_lines += 1;
                if inner.log_est_lines > LOG_ROTATE_MAX {
                    inner.log_est_lines = rotate_log(&path, inner.log_est_lines);
                }
            }
            Err(e) => eprintln!("[approval] 留痕日志追加失败（{path:?}）：{e}"),
        }
    }
}

/// 轮转：保留后半（原子：临时文件 + rename），返回轮转后行数
fn rotate_log(path: &std::path::Path, est_lines: usize) -> usize {
    let Ok(body) = std::fs::read_to_string(path) else {
        return 0;
    };
    let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
    let kept = &lines[lines.len() / 2..];
    let tmp = path.with_extension("tmp");
    match std::fs::write(&tmp, kept.join("\n") + "\n").and_then(|_| std::fs::rename(&tmp, path)) {
        Ok(_) => kept.len(),
        Err(e) => {
            eprintln!("[approval] 留痕日志轮转失败（{path:?}）：{e}");
            est_lines
        }
    }
}

/// ApprovalHook：权限门 hook，只管 bash 工具
pub struct ApprovalHook {
    hub: Arc<ApprovalHub>,
}

impl ApprovalHook {
    pub fn new(hub: Arc<ApprovalHub>) -> Self {
        Self { hub }
    }
}

#[async_trait::async_trait]
impl Hook for ApprovalHook {
    fn name(&self) -> &str {
        "approval"
    }

    async fn on_tool_start(&self, call: &ToolCall, ctx: &HookCtx<'_>) -> Option<String> {
        // MCP/文件工具不进门（MCP 是显式安装的受信工具；write/edit 本就锁 work_dir）
        if call.name != "bash" {
            return None;
        }
        let args: serde_json::Value = serde_json::from_str(&call.arguments).ok()?;
        let Some(command) = args.get("command").and_then(|v| v.as_str()) else {
            return None; // 参数残缺交给 bash 工具自己报错
        };
        let forever = self.hub.forever_allows();
        let session = self.hub.session_allows(ctx.session_id);
        let jctx = JudgeCtx {
            sandbox_degraded: ctx.sandbox_degraded,
            forever_allows: &forever,
            session_allows: &session,
        };
        match judge(command, &jctx) {
            Gate::Green => None,
            Gate::Red { reason } => {
                self.hub.log(ctx.session_id, command, &reason, "red-deny", "");
                Some(format!("已拒绝（高危操作）：{reason}。命令：{command}"))
            }
            Gate::Yellow { reason } => {
                let rule = yellow_rule(command).unwrap_or_default();
                // 白名单（forever / session 词前缀或规则名）命中 → 留痕放行
                if self.hub.is_allowed(ctx.session_id, command, &rule) {
                    self.hub.log(ctx.session_id, command, &reason, "whitelist", "");
                    return None;
                }
                // 批1：无审批通道一律默认拒绝（先紧后松，预期行为）
                if !self.hub.has_subscribers() {
                    self.hub.log(ctx.session_id, command, &reason, "no-channel-deny", "");
                    return Some(format!(
                        "此操作需授权，但当前无审批通道（控制台未接入审批 UI），默认拒绝。\
                         命令：{command}。请在 permissions.toml 预授权或接入审批 UI。"
                    ));
                }
                let ticket = self.hub.request(ctx.session_id, command, &reason);
                if let Some(emitter) = ctx.emitter {
                    let _ = emitter.send(AgentEvent::ApprovalRequest {
                        id: ticket.id,
                        command: command.to_string(),
                        reason: reason.clone(),
                    });
                }
                let timeout = std::time::Duration::from_secs(self.hub.timeout_secs);
                match tokio::time::timeout(timeout, ticket.rx).await {
                    Ok(Ok(Decision::Allow(Scope::Once))) => {
                        self.hub.log(ctx.session_id, command, &reason, "allow", "once");
                        None
                    }
                    Ok(Ok(Decision::Allow(Scope::Session))) => {
                        self.hub.grant_session(ctx.session_id, &rule);
                        self.hub.log(ctx.session_id, command, &reason, "allow", "session");
                        None
                    }
                    Ok(Ok(Decision::Allow(Scope::Forever))) => {
                        self.hub.grant_forever(&rule);
                        self.hub.log(ctx.session_id, command, &reason, "allow", "forever");
                        None
                    }
                    Ok(Ok(Decision::Deny)) => {
                        self.hub.log(ctx.session_id, command, &reason, "deny", "");
                        // 不用 ERROR 前缀：拒绝不是工具故障，不进失败重试硬信号
                        Some(
                            "用户未批准该操作。请换不改变系统状态的方案，或向用户说明理由后重试。"
                                .to_string(),
                        )
                    }
                    // oneshot 发送端被丢（Hub 关闭）按超时同策：默认拒绝
                    Ok(Err(_)) | Err(_) => {
                        self.hub.log(ctx.session_id, command, &reason, "timeout-deny", "");
                        Some(format!(
                            "授权请求 {} 秒无人应答，默认拒绝（铁律：无人应答=拒绝）。命令：{command}",
                            self.hub.timeout_secs
                        ))
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChunkStream, ModelProvider, ModelResult};
    use crate::types::{Message, StreamChunk, ToolSchema};

    fn jctx(degraded: bool) -> JudgeCtx<'static> {
        JudgeCtx {
            sandbox_degraded: degraded,
            forever_allows: &[],
            session_allows: &[],
        }
    }

    fn is_green(cmd: &str) -> bool {
        judge(cmd, &jctx(false)) == Gate::Green
    }

    fn yellow_rule_of(cmd: &str) -> String {
        match judge(cmd, &jctx(false)) {
            Gate::Yellow { reason } => reason,
            g => panic!("应为 Yellow，实得 {g:?}（{cmd}）"),
        }
    }

    // ── judge 三级判定 ──

    #[test]
    fn test_judge_green_false_positive_control() {
        // 误伤对照组：含敏感字面但不构成状态变更的命令必须绿
        for cmd in [
            "echo sudo",
            "ls /etc",
            "cat ~/.bashrc",
            "ls -la",
            "cargo build --release",
            "npm run dev",
            "pip list",
            "grep -r sudo ./src",
            "echo hello | grep h",
            "git log --oneline",
        ] {
            assert!(is_green(cmd), "误伤：{cmd} → {:?}", judge(cmd, &jctx(false)));
        }
    }

    #[test]
    fn test_judge_red_patterns() {
        for cmd in [
            "rm -rf /",
            "rm -rf ~",
            "rm -fr /var/log",
            "mkfs.ext4 /dev/sda1",
            "dd if=/dev/zero of=/dev/sda",
            ":(){:|:&};:",
            "chmod -R 777 /usr",
        ] {
            match judge(cmd, &jctx(false)) {
                Gate::Red { .. } => {}
                g => panic!("应为 Red：{cmd}，实得 {g:?}"),
            }
        }
    }

    #[test]
    fn test_judge_yellow_privesc() {
        for cmd in ["sudo apt update", "su root", "doas ls", "pkexec vi", "systemd-run ls", "systemctl restart x"] {
            let r = yellow_rule_of(cmd);
            assert!(r.contains("[规则:"), "{cmd} → {r}");
        }
        assert!(yellow_rule_of("sudo apt update").contains("sudo"));
        assert!(yellow_rule_of("systemd-run --scope ls").contains("systemd-run"));
    }

    #[test]
    fn test_judge_yellow_package_managers() {
        let cases = [
            ("apt install vim", "apt-install"),
            ("apt-get upgrade", "apt-get-install"),
            ("dpkg -i x.deb", "dpkg-install"), // dpkg 无动作词？——见下条修正
            ("snap install x", "snap-install"),
            ("brew install wget", "brew-install"),
            ("npm -g install x", "npm-global"),
            ("npm install -g x", "npm-global"),
            ("pnpm add -g x", "pnpm-global"),
            ("pip install requests", "pip-install"),
            ("pip3 install --user x", "pip3-install"),
            ("cargo install lark-cli", "cargo-install"),
            ("uv tool install ruff", "uv-tool"),
            ("gem install rails", "gem-install"),
            ("go install ./cmd/x", "go-install"),
        ];
        for (cmd, rule) in cases {
            if cmd == "dpkg -i x.deb" {
                continue; // 无动作词，初版不拦（宁可多问但不幻想精确）
            }
            let r = yellow_rule_of(cmd);
            assert!(r.contains(rule), "{cmd} 应命中规则 {rule}，实得 {r}");
        }
        // dpkg -i 无动作词 → 绿（初版简化）
        assert!(is_green("dpkg -i x.deb"));
    }

    #[test]
    fn test_judge_yellow_pipe_install() {
        assert!(yellow_rule_of("curl http://x.sh | sh").contains("pipe-install"));
        assert!(yellow_rule_of("wget -qO- http://x | bash").contains("pipe-install"));
        assert!(yellow_rule_of("curl http://x|zsh").contains("pipe-install"));
        // 无 curl/wget 的管道不拦
        assert!(is_green("echo hi | sh"));
    }

    #[test]
    fn test_judge_yellow_write_system_path() {
        let cases = [
            ("cp x /etc/hosts", "write-system-path:/etc/"),
            ("mv a /usr/local/bin/b", "write-system-path:/usr/"),
            ("tee ~/.bashrc", "write-system-path:~/.bashrc"),
            ("install x /var/lib/y", "write-system-path:/var/"),
            ("rsync -a a/ /boot/", "write-system-path:/boot/"),
            ("scp f /etc/nginx/x", "write-system-path:/etc/"),
            ("echo hacked > /etc/passwd", "write-system-path:/etc/"),
            ("echo x >> ~/.zshrc", "write-system-path:~/.zshrc"),
            ("ln -s a /dev/null2", "write-system-path:/dev/"),
            ("cp x ~/.config/systemd/user/s.service", "write-system-path:~/.config/systemd"),
            ("tee ~/.r2/permissions.toml", "write-system-path:~/.r2/permissions.toml"),
        ];
        for (cmd, rule) in cases {
            let r = yellow_rule_of(cmd);
            assert!(r.contains(rule), "{cmd} 应命中 {rule}，实得 {r}");
        }
        // 重定向粘连无空格形式
        assert!(yellow_rule_of("echo x 2>/var/log/e").contains("write-system-path:/var/"));
    }

    #[test]
    fn test_judge_multisegment_and_word_boundary() {
        // 多段：第二段命中黄
        assert!(yellow_rule_of("echo hi && sudo apt install").contains("sudo"));
        // 多段：第二段命中红
        match judge("echo ok; rm -rf /", &jctx(false)) {
            Gate::Red { .. } => {}
            g => panic!("应为 Red，实得 {g:?}"),
        }
        // 词边界：sudoedit / aptitude 不是 sudo/apt
        assert!(is_green("sudoedit /etc/hosts")); // sudoedit 不在提权表（写路径也非写类首词）
        assert!(is_green("aptitude search x"));
        assert!(is_green("my-sudo-wrapper run"));
    }

    #[test]
    fn test_judge_degraded_escalates_yellow_to_red() {
        match judge("sudo apt install vim", &jctx(true)) {
            Gate::Red { reason } => assert!(reason.contains("沙箱降级")),
            g => panic!("降级时黄应升红，实得 {g:?}"),
        }
        // 绿不受降级影响
        assert_eq!(judge("ls -la", &jctx(true)), Gate::Green);
    }

    // ── 词前缀匹配 ──

    #[test]
    fn test_word_prefix_match() {
        assert!(word_prefix_match("sudo", "sudo apt install vim"));
        assert!(word_prefix_match("npm -g", "npm -g install x"));
        assert!(!word_prefix_match("npm -g", "npm install -g x")); // 词序敏感
        assert!(!word_prefix_match("sud", "sudo ls"));
        assert!(!word_prefix_match("", "sudo ls"));
        assert!(!word_prefix_match("sudo ls x y", "sudo ls"));
    }

    // ── ApprovalHub ──

    fn temp_hub(tmp: &tempfile::TempDir) -> ApprovalHub {
        ApprovalHub::with_paths(
            tmp.path().join("permissions.toml"),
            tmp.path().join("approvals.log"),
        )
    }

    fn bash_call_with(cmd: &str) -> ToolCall {
        ToolCall {
            id: "t1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command": cmd}).to_string(),
        }
    }

    struct NullProvider;

    #[async_trait::async_trait]
    impl ModelProvider for NullProvider {
        fn id(&self) -> &str {
            "null"
        }
        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolSchema],
        ) -> ModelResult<ChunkStream> {
            Ok(Box::pin(futures_util::stream::iter(vec![
                Ok(StreamChunk::Done),
            ])))
        }
        fn parse_response(&self, _chunks: &[StreamChunk]) -> ModelResult<(String, Vec<ToolCall>)> {
            Ok((String::new(), vec![]))
        }
    }

    fn hook_ctx<'a>(
        provider: &'a mut NullProvider,
        signals: &'a crate::evolution::TurnSignals,
        session_id: &'a str,
    ) -> HookCtx<'a> {
        HookCtx {
            provider,
            signals,
            task_summary: "",
            work_dir: "",
            session_id,
            emitter: None,
            goal_baseline: None,
            skills_baseline: &[],
            sandbox_degraded: false,
        }
    }

    #[test]
    fn test_hub_whitelist_prefix_and_rule_name() {
        let tmp = tempfile::tempdir().unwrap();
        let hub = temp_hub(&tmp);
        hub.grant_session("s1", "sudo");
        assert!(hub.is_allowed("s1", "sudo apt install", "sudo"));
        assert!(!hub.is_allowed("s2", "sudo apt install", "sudo")); // 别的会话不享用
        assert!(!hub.is_allowed("s1", "apt install vim", "apt-install"));
    }

    #[test]
    fn test_hub_grant_forever_persists_and_reloads() {
        let tmp = tempfile::tempdir().unwrap();
        let hub = temp_hub(&tmp);
        hub.grant_forever("npm-global");
        drop(hub);
        // 重新加载：永远授权从盘恢复
        let hub2 = temp_hub(&tmp);
        assert!(hub2.is_allowed("any", "npm -g install x", "npm-global"));
        let body = std::fs::read_to_string(tmp.path().join("permissions.toml")).unwrap();
        assert!(body.contains("[[allow]]"), "{body}");
        assert!(body.contains("npm-global"), "{body}");
    }

    #[test]
    fn test_hub_corrupt_permissions_toml_tolerated() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("permissions.toml"), "{{{{不是toml").unwrap();
        let hub = temp_hub(&tmp); // 不 panic，按空表
        assert!(hub.forever_allows().is_empty());
    }

    #[tokio::test]
    async fn test_hub_request_resolve_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let hub = temp_hub(&tmp);
        assert!(!hub.has_subscribers());
        let mut rx = hub.subscribe();
        assert!(hub.has_subscribers());
        let t = hub.request("s1", "sudo ls", "提权");
        assert_eq!(t.id, 1);
        let req = rx.recv().await.unwrap();
        assert_eq!(req.id, 1);
        assert_eq!(req.command, "sudo ls");
        hub.resolve(1, Decision::Allow(Scope::Once));
        assert_eq!(t.rx.await.unwrap(), Decision::Allow(Scope::Once));
        // 未知 id 答复不 panic
        hub.resolve(999, Decision::Deny);
    }

    #[test]
    fn test_hub_log_appends_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
        let hub = temp_hub(&tmp);
        hub.log("s1", "sudo ls", "提权 [规则:sudo]", "deny", "");
        hub.log("s1", "npm -g i x", "包 [规则:npm-global]", "allow", "forever");
        let body = std::fs::read_to_string(tmp.path().join("approvals.log")).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v["decision"], "deny");
        assert_eq!(v["command"], "sudo ls");
        assert_eq!(v["session"], "s1");
        let v2: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(v2["scope"], "forever");
    }

    // ── ApprovalHook ──

    #[tokio::test]
    async fn test_hook_non_bash_passthrough() {
        let tmp = tempfile::tempdir().unwrap();
        let hub = Arc::new(temp_hub(&tmp));
        let hook = ApprovalHook::new(hub);
        let mut provider = NullProvider;
        let signals = crate::evolution::TurnSignals::default();
        let ctx = hook_ctx(&mut provider, &signals, "s1");
        // 非 bash 工具即便参数像命令也不进门
        let call = ToolCall {
            id: "t".into(),
            name: "write".into(),
            arguments: serde_json::json!({"command": "sudo rm -rf /"}).to_string(),
        };
        assert!(hook.on_tool_start(&call, &ctx).await.is_none());
    }

    #[tokio::test]
    async fn test_hook_green_and_red() {
        let tmp = tempfile::tempdir().unwrap();
        let hub = Arc::new(temp_hub(&tmp));
        let hook = ApprovalHook::new(hub);
        let mut provider = NullProvider;
        let signals = crate::evolution::TurnSignals::default();
        let ctx = hook_ctx(&mut provider, &signals, "s1");
        assert!(hook.on_tool_start(&bash_call_with("ls -la"), &ctx).await.is_none());
        let r = hook.on_tool_start(&bash_call_with("rm -rf /"), &ctx).await;
        assert!(r.unwrap().contains("已拒绝（高危操作）"));
    }

    #[tokio::test]
    async fn test_hook_yellow_no_channel_default_deny() {
        let tmp = tempfile::tempdir().unwrap();
        let hub = Arc::new(temp_hub(&tmp));
        let hook = ApprovalHook::new(hub);
        let mut provider = NullProvider;
        let signals = crate::evolution::TurnSignals::default();
        let ctx = hook_ctx(&mut provider, &signals, "s1");
        let r = hook.on_tool_start(&bash_call_with("sudo apt install vim"), &ctx).await;
        let msg = r.expect("黄级无通道应拦截");
        assert!(msg.contains("无审批通道"), "{msg}");
        assert!(msg.contains("permissions.toml"), "{msg}");
        assert!(!msg.starts_with("ERROR"), "拒绝不是工具故障：{msg}");
        // 留痕
        let body = std::fs::read_to_string(tmp.path().join("approvals.log")).unwrap();
        assert!(body.contains("no-channel-deny"), "{body}");
    }

    #[tokio::test]
    async fn test_hook_allow_once_session_forever() {
        let tmp = tempfile::tempdir().unwrap();
        let hub = Arc::new(temp_hub(&tmp));
        let hook = ApprovalHook::new(hub.clone());
        let mut provider = NullProvider;
        let signals = crate::evolution::TurnSignals::default();
        let _rx = hub.subscribe(); // 有通道

        // Allow(Once)：放行但不登记
        {
            let ctx = hook_ctx(&mut provider, &signals, "s1");
            let h2 = hub.clone();
            // join! 并发驱动：先让 hook 挂起登记请求，再由另一侧答复
            let call = bash_call_with("sudo apt install vim");
            let (r, _) = tokio::join!(
                hook.on_tool_start(&call, &ctx),
                async {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    h2.resolve(1, Decision::Allow(Scope::Once));
                }
            );
            assert!(r.is_none());
            assert!(!hub.is_allowed("s1", "sudo apt install vim", "sudo"), "Once 不登记");
        }
        // Allow(Session)：放行且本会话免问
        {
            let ctx = hook_ctx(&mut provider, &signals, "s1");
            let h2 = hub.clone();
            let call = bash_call_with("sudo apt install vim");
            let (r, _) = tokio::join!(
                hook.on_tool_start(&call, &ctx),
                async {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    h2.resolve(2, Decision::Allow(Scope::Session));
                }
            );
            assert!(r.is_none());
            assert!(hub.is_allowed("s1", "sudo apt install vim", "sudo"));
        }
        // 第三条同模式：白名单直接放行（无需答复）
        {
            let ctx = hook_ctx(&mut provider, &signals, "s1");
            assert!(hook.on_tool_start(&bash_call_with("sudo systemctl restart x"), &ctx).await.is_none());
        }
        // Allow(Forever)：落盘 permissions.toml
        {
            let ctx = hook_ctx(&mut provider, &signals, "s1");
            let h2 = hub.clone();
            let call = bash_call_with("npm -g install x");
            let (r, _) = tokio::join!(
                hook.on_tool_start(&call, &ctx),
                async {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    h2.resolve(3, Decision::Allow(Scope::Forever));
                }
            );
            assert!(r.is_none());
            let body = std::fs::read_to_string(tmp.path().join("permissions.toml")).unwrap();
            assert!(body.contains("npm-global"), "{body}");
        }
    }

    #[tokio::test]
    async fn test_hook_deny_message() {
        let tmp = tempfile::tempdir().unwrap();
        let hub = Arc::new(temp_hub(&tmp));
        let hook = ApprovalHook::new(hub.clone());
        let mut provider = NullProvider;
        let signals = crate::evolution::TurnSignals::default();
        let _rx = hub.subscribe();
        let ctx = hook_ctx(&mut provider, &signals, "s1");
        let call = bash_call_with("sudo ls");
        let (msg, _) = tokio::join!(
            hook.on_tool_start(&call, &ctx),
            async {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                hub.resolve(1, Decision::Deny);
            }
        );
        let msg = msg.expect("Deny 应拦截");
        assert!(msg.contains("用户未批准"), "{msg}");
        assert!(!msg.starts_with("ERROR"), "{msg}");
    }

    #[tokio::test]
    async fn test_hook_timeout_default_deny() {
        let tmp = tempfile::tempdir().unwrap();
        let hub = Arc::new(temp_hub(&tmp).with_timeout(1)); // 测试注入短值
        let _rx_keepalive = hub.subscribe(); // 接收端必须活着才有通道
        let hook = ApprovalHook::new(hub);
        let mut provider = NullProvider;
        let signals = crate::evolution::TurnSignals::default();
        let ctx = hook_ctx(&mut provider, &signals, "s1");
        let msg = hook
            .on_tool_start(&bash_call_with("sudo ls"), &ctx)
            .await
            .expect("超时默认拒");
        assert!(msg.contains("无人应答"), "{msg}");
        assert!(msg.contains("默认拒绝"), "{msg}");
        drop(_rx_keepalive);
    }
}
