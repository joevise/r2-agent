//! `r2 sandbox run` — 自孵化 supervisor（v0.5 模块B）
//!
//! 每个会话 = 一个隔离子 r2 进程：
//! - 会话 cgroup（pids.max + 可选 memory.max，整棵子树核算）
//! - env 清洗（宿主环境零泄漏，密钥经 0600 配置文件传递）
//! - 子 r2 的 bash 走 strict 档（模块A namespace：假根/pid隔离/断网）
//! - 进程退出 → namespace 内核回收 + cgroup systemd 回收，零残留
//!
//! 时序关键：组名用 **supervisor 的 pid**（spawn 前可知）→ R2_CGROUP_JOIN
//! 在 spawn 时注入子进程 env → 子 r2 的 bash 树直接入会话组。
//!
//! 设计原则：Docker 的隔离本体就是 namespace+cgroup+seccomp——R2 已全部自实现，
//! 不再雇 200MB 的管家。详见 docs/v05-cloud-sandbox-plan.md。

use r2_core::config::Config;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

pub struct SandboxRunArgs {
    pub prompt: String,
    pub memory_mb: u64,
    pub pids: u32,
    pub ephemeral: bool,
    pub timeout_secs: u64,
}

/// 会话目录：{cwd}/r2-sessions/sess-{时间戳}-{supervisor_pid}
fn session_dir() -> PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("r2-sessions")
        .join(format!("sess-{ts}-{}", std::process::id()))
}

/// 从父配置生成会话配置 TOML（写入 {sess}/.r2-session.toml，0600）。
/// 密钥安全：model 段（含 api_key）从源配置文件**整段复制**，不经过代码拼接/日志。
fn write_session_config(
    parent: &Config,
    source_config_path: Option<&str>,
    sess_dir: &std::path::Path,
    pids: u32,
) -> Result<PathBuf, String> {
    let mut toml = String::new();

    // model 段：优先从源配置文件原样复制（保真）
    let model_section = match source_config_path.and_then(|p| std::fs::read_to_string(p).ok()) {
        Some(raw) => raw
            .lines()
            .skip_while(|l| !l.trim_start().starts_with("[model]"))
            // 边界：下一个非 model 段头（[model.xxx] 子表保留，[agent] 等停止）
            .take_while(|l| {
                let t = l.trim_start();
                t.is_empty()
                    || t.starts_with('#')
                    || !t.starts_with('[')
                    || t.starts_with("[model")
            })
            .collect::<Vec<_>>()
            .join("\n"),
        None => {
            // 无源文件（默认配置路径不存在）：手工重建最小 model 段
            match parent.model.provider.as_str() {
                "openai_compat" => format!(
                    "[model]\nprovider = \"openai_compat\"\n[model.openai_compat]\nbase_url = \"{}\"\napi_key = \"{}\"\nmodel = \"{}\"\n",
                    parent.model.openai_compat.base_url,
                    parent.model.openai_compat.api_key,
                    parent.model.openai_compat.model,
                ),
                "anthropic" => format!(
                    "[model]\nprovider = \"anthropic\"\n[model.anthropic]\nbase_url = \"{}\"\napi_key = \"{}\"\nmodel = \"{}\"\n",
                    parent.model.anthropic.base_url,
                    parent.model.anthropic.api_key,
                    parent.model.anthropic.model,
                ),
                other => return Err(format!("未知 provider：{other}")),
            }
        }
    };
    // 密钥确实带过来了（只验存在，不打印内容）
    if !model_section.contains("api_key") {
        return Err("model 段缺少 api_key，中止会话生成".to_string());
    }
    toml.push_str(model_section.trim_end());
    toml.push('\n');

    // agent 段：work_dir 锁定会话目录
    toml.push_str(&format!("\n[agent]\nwork_dir = {:?}\n", sess_dir.display()));

    // sandbox 段：强制 strict。pids 仅用于会话 cgroup（pids.max 按子树计数，正确）；
    // 绝不写进 max_processes（→ RLIMIT_NPROC 按真实 uid 全局计线程，桌面机 uid 1000
    // 名下飞书/Cursor/浏览器等 2000+ 线程，256 限额 = 所有 fork EAGAIN 团灭，8/23 实测实锤）
    toml.push_str(&format!(
        "\n[sandbox]\nlevel = \"strict\"\nmax_processes = 0\nbash_timeout_secs = {}\n",
        parent.sandbox.bash_timeout_secs
    ));

    let path = sess_dir.join(".r2-session.toml");
    std::fs::write(&path, toml).map_err(|e| format!("写会话配置失败：{e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(path)
}

/// supervisor 主流程
pub fn run_sandbox(
    parent: &Config,
    source_config_path: Option<&str>,
    args: SandboxRunArgs,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let sess = session_dir();
    std::fs::create_dir_all(&sess).map_err(|e| format!("建会话目录失败：{e}"))?;

    let sess_config = write_session_config(parent, source_config_path, &sess, args.pids)
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;

    println!("═══ r2 自孵化沙箱会话 ═══");
    println!("  会话目录: {}", sess.display());

    // 1) 预建会话组（组名含 supervisor pid——spawn 前可知，可注入 env）
    let (cg_warn, group) = r2_core::sandbox::create_session_cgroup(
        std::process::id(),
        args.pids,
        args.memory_mb,
    );

    // 2) spawn 子 r2（自孵化：同二进制，全新进程），env 零泄漏
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(&exe);
    cmd.arg("--once")
        .arg(&args.prompt)
        .arg("--config")
        .arg(sess_config.display().to_string())
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", sess.display().to_string())
        .env("TERM", std::env::var("TERM").unwrap_or_else(|_| "dumb".into()));
    if let Some(g) = &group {
        cmd.env("R2_CGROUP_JOIN", g.display().to_string());
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("孵化子 r2 失败：{e}"))?;
    let child_pid = child.id();
    println!("  子进程:   pid {child_pid}");

    // 3) 子进程入组（bash 树经 R2_CGROUP_JOIN 自动汇入，会话限额覆盖整棵子树）
    match &group {
        Some(g) => {
            if let Err(e) = std::fs::write(g.join("cgroup.procs"), child_pid.to_string()) {
                println!("  ⚠ 子进程入组失败（{e}），会话限额降级 rlimits");
            } else {
                println!(
                    "  cgroup:   {} (pids≤{}{})",
                    g.display(),
                    args.pids,
                    if args.memory_mb > 0 {
                        format!(", mem≤{}M", args.memory_mb)
                    } else {
                        String::new()
                    }
                );
            }
        }
        None => {
            if let Some(w) = cg_warn {
                println!("  ⚠ {w}");
            }
        }
    }

    // 4) 等待子进程（输出实时透传；超时强杀整树）
    let deadline = if args.timeout_secs > 0 {
        Some(Instant::now() + Duration::from_secs(args.timeout_secs))
    } else {
        None
    };
    let status = loop {
        match child.try_wait()? {
            Some(s) => break s,
            None => {
                if let Some(d) = deadline {
                    if Instant::now() >= d {
                        println!("  ⚠ 超时（{}s），终止会话进程树", args.timeout_secs);
                        let _ = child.kill();
                        break child.wait()?;
                    }
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    };
    println!("═══ 会话结束: exit {} ═══", status.code().unwrap_or(-1));

    // 5) 清理：--ephemeral 连会话目录一起删（cgroup 空组由 systemd 回收）
    if args.ephemeral {
        let _ = std::fs::remove_dir_all(&sess);
        println!("  (--ephemeral) 会话目录已清理");
    } else {
        println!("  会话产物保留于 {}", sess.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_dir_naming() {
        let d = session_dir();
        assert!(d.to_string_lossy().contains("r2-sessions/sess-"));
    }

    #[test]
    fn test_write_session_config_strict_injected() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = Config::default_config();
        let path = write_session_config(&parent, None, tmp.path(), 128).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("level = \"strict\""), "必须强制 strict：{content}");
        // max_processes 必须为 0（不设 RLIMIT_NPROC）：pids 只走 cgroup。
        // RLIMIT_NPROC 按真实 uid 全局计线程，桌面机 uid 1000 名下 2000+ 线程，
        // 任何正数限额都会让所有 fork EAGAIN 团灭（2026-08-23 实测实锤）
        assert!(content.contains("max_processes = 0"), "绝不设 RLIMIT_NPROC：{content}");
        assert!(!content.contains("max_processes = 128"));
        assert!(content.contains("api_key"), "密钥段必须复制");
        assert!(content.contains(&format!("work_dir = {:?}", tmp.path().display())));
        // 0600 权限
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "会话配置必须 0600（含密钥）");
        }
    }

    #[test]
    fn test_write_session_config_from_source_file() {
        // 源配置文件的 model 段整段复制（含自定义字段）
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.toml");
        std::fs::write(
            &src,
            "# 注释\n[model]\nprovider = \"openai_compat\"\n[model.openai_compat]\nbase_url = \"https://x\"\napi_key = \"sk-test-123\"\nmodel = \"m1\"\n\n[agent]\nwork_dir = \"/should-be-overridden\"\n",
        )
        .unwrap();
        let parent = Config::default_config();
        let path = write_session_config(&parent, Some(src.to_str().unwrap()), tmp.path(), 64)
            .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("sk-test-123"), "密钥原样复制");
        assert!(content.contains("base_url = \"https://x\""), "自定义端点保留");
        // work_dir 覆盖为会话目录
        let occurrences = content.matches("work_dir").count();
        assert_eq!(occurrences, 1, "源文件的 agent 段不得重复出现：{content}");
        assert!(content.contains(&format!("work_dir = {:?}", tmp.path().display())));
    }
}
