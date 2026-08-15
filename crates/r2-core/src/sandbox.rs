//! 沙箱隔离模块：bash 工具执行命令前的三级隔离（off / container / strict）
//!
//! v0.1 边界（务实版）：
//! - Off：什么都不做
//! - Container：rlimits（NPROC/AS/CPU/FSIZE）+ 环境变量清洗 + cwd 锁定（cwd 由 bash 工具保证）
//! - Strict：Container + seccomp 白名单（需 `sandbox-strict` feature，否则降级为 Container + warn）
//!
//! 说明：真正的 chroot / namespace 隔离是 v0.2 的活，等真实部署需要时再加
//!（`apply` 的 `work_dir` 参数即为 v0.2 预留）。

use crate::config::SandboxConfig;
use std::path::Path;

/// 沙箱级别
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxLevel {
    Off,
    Container,
    Strict,
}

impl SandboxLevel {
    /// 解析配置字符串："off" | "container" | "strict"
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "off" => Ok(Self::Off),
            "container" => Ok(Self::Container),
            "strict" => Ok(Self::Strict),
            other => Err(format!(
                "非法沙箱级别: \"{other}\"，仅支持 \"off\" | \"container\" | \"strict\""
            )),
        }
    }
}

/// 环境变量白名单：其余（含 API key 类）全部剔除
const ENV_WHITELIST: &[&str] = &[
    "PATH", "LANG", "LC_ALL", "LC_CTYPE", "HOME", "TERM", "TMPDIR", "PWD",
];

/// 清洗后的 PATH：防止用户 PATH 注入
const SANDBOX_PATH: &str = "/usr/bin:/bin";

/// 沙箱配置快照（从 Config 提取，bash 工具持有）
pub struct Sandbox {
    pub level: SandboxLevel,
    /// RLIMIT_NPROC
    pub max_processes: u32,
    /// RLIMIT_AS（MB）
    pub max_memory_mb: u32,
    /// RLIMIT_CPU（秒）
    pub cpu_time_secs: u32,
    /// RLIMIT_FSIZE（MB，防写爆磁盘）
    pub max_file_size_mb: u32,
    /// level == Strict
    pub strict: bool,
}

impl Sandbox {
    /// 从配置构造；level 非法时返回错误（正常路径下 config 加载阶段已校验）
    pub fn from_config(cfg: &SandboxConfig) -> Result<Self, String> {
        let level = SandboxLevel::parse(&cfg.level)?;
        Ok(Self {
            level,
            max_processes: cfg.max_processes as u32,
            max_memory_mb: cfg.max_memory_mb as u32,
            cpu_time_secs: cfg.cpu_time_secs,
            max_file_size_mb: cfg.max_file_size_mb,
            strict: level == SandboxLevel::Strict,
        })
    }

    /// 应用沙箱到 Command。返回 Ok(Some(warn)) 表示发生了降级。
    pub fn apply(
        &self,
        cmd: &mut tokio::process::Command,
        work_dir: &Path,
    ) -> Result<Option<String>, String> {
        self.apply_inner(cmd, work_dir, true)
    }

    /// 不带 seccomp 的应用（仅供 strict 降级重试使用）
    pub(crate) fn apply_without_seccomp(
        &self,
        cmd: &mut tokio::process::Command,
        work_dir: &Path,
    ) -> Result<Option<String>, String> {
        self.apply_inner(cmd, work_dir, false)
    }

    fn apply_inner(
        &self,
        cmd: &mut tokio::process::Command,
        _work_dir: &Path,
        with_seccomp: bool,
    ) -> Result<Option<String>, String> {
        if self.level == SandboxLevel::Off {
            return Ok(None);
        }
        // container / strict 都应用：环境清洗 + rlimits
        clean_env(cmd);
        let use_seccomp = self.strict && with_seccomp && cfg!(feature = "sandbox-strict");
        install_pre_exec(cmd, self, use_seccomp);

        let warn = if self.strict && !cfg!(feature = "sandbox-strict") {
            Some(
                "WARN: seccomp 未编译（需要 sandbox-strict feature），strict 降级为 container 行为"
                    .to_string(),
            )
        } else {
            None
        };
        Ok(warn)
    }
}

/// 环境变量清洗：只保留白名单，PATH 重设为固定值
fn clean_env(cmd: &mut tokio::process::Command) {
    let kept: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| ENV_WHITELIST.contains(&k.as_str()) && k != "PATH")
        .collect();
    cmd.env_clear();
    for (k, v) in kept {
        cmd.env(k, v);
    }
    cmd.env("PATH", SANDBOX_PATH);
}

/// 设置单个 rlimit（pre_exec 闭包内调用，返回 io 错误会中止 exec）
fn set_rlimit(resource: libc::__rlimit_resource_t, value: u64) -> std::io::Result<()> {
    let lim = libc::rlimit {
        rlim_cur: value,
        rlim_max: value,
    };
    // SAFETY: setrlimit 是异步信号安全的，pre_exec 上下文（fork 后 exec 前）调用合法
    if unsafe { libc::setrlimit(resource, &lim) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// 在 pre_exec 里安装 rlimits（rlimit 继承给 exec 后的进程）；use_seccomp 时装 seccomp
fn install_pre_exec(cmd: &mut tokio::process::Command, sbx: &Sandbox, use_seccomp: bool) {
    let nproc = u64::from(sbx.max_processes);
    let mem = u64::from(sbx.max_memory_mb) * 1024 * 1024;
    let cpu = u64::from(sbx.cpu_time_secs);
    let fsize = u64::from(sbx.max_file_size_mb) * 1024 * 1024;
    // SAFETY: 闭包内只调用异步信号安全的 libc 函数与 seccomp 安装，符合 pre_exec 约束
    unsafe {
        cmd.pre_exec(move || {
            // RLIMIT_NPROC 按"真实 UID 名下全部线程"计数（man 2 setrlimit）——
            // 桌面/共享 UID 机器上 GUI 应用线程（飞书/Cursor 等）会把配额吃满，导致 fork 全部 EAGAIN。
            // max_processes=0 表示不设此限制；仅单用途容器（r2 独占 uid）建议设 64-256。
            if nproc > 0 { 
                set_rlimit(libc::RLIMIT_NPROC, nproc)?; 
            }
            set_rlimit(libc::RLIMIT_AS, mem)?;
            set_rlimit(libc::RLIMIT_CPU, cpu)?;
            set_rlimit(libc::RLIMIT_FSIZE, fsize)?;
            #[cfg(feature = "sandbox-strict")]
            if use_seccomp {
                install_seccomp().map_err(std::io::Error::other)?;
            }
            #[cfg(not(feature = "sandbox-strict"))]
            let _ = use_seccomp;
            Ok(())
        });
    }
}

/// seccomp 白名单 syscall（x86_64 为主；个别名字在某些架构不存在时静默跳过）
#[cfg(feature = "sandbox-strict")]
const SECCOMP_WHITELIST: &[&str] = &[
    "read",
    "write",
    "open",
    "openat",
    "close",
    "stat",
    "fstat",
    "lstat",
    "newfstatat",
    "mmap",
    "munmap",
    "mprotect",
    "brk",
    "rt_sigaction",
    "rt_sigprocmask",
    "ioctl",
    "access",
    "exit",
    "exit_group",
    "clone",
    "fork",
    "vfork",
    "execve",
    "execveat",
    "wait4",
    "waitid",
    "pipe",
    "pipe2",
    "dup",
    "dup2",
    "dup3",
    "fcntl",
    "futex",
    "getcwd",
    "chdir",
    "rename",
    "unlink",
    "mkdir",
    "rmdir",
    "readlink",
    "getdents64",
    "clock_gettime",
    "gettimeofday",
    "nanosleep",
    "uname",
    "readv",
    "writev",
    "pread64",
    "pwrite64",
    "lseek",
    "fsync",
    "fdatasync",
    "ftruncate",
    "getpid",
    "getppid",
    "getuid",
    "geteuid",
    "getgid",
    "getegid",
    "set_tid_address",
    "set_robust_list",
    "rseq",
    "prlimit64",
    "getrandom",
    "arch_prctl",
    "statfs",
    "fstatfs",
];

/// 安装 seccomp 过滤器：默认 KillProcess + 白名单 Allow
#[cfg(feature = "sandbox-strict")]
fn install_seccomp() -> Result<(), String> {
    use libseccomp::{ScmpAction, ScmpFilterContext, ScmpSyscall};
    let mut filter =
        ScmpFilterContext::new_filter(ScmpAction::KillProcess).map_err(|e| e.to_string())?;
    for name in SECCOMP_WHITELIST {
        // 名字在个别架构/内核版本可能不存在：跳过而非失败
        if let Ok(sc) = ScmpSyscall::from_name(name) {
            filter
                .add_rule(ScmpAction::Allow, sc)
                .map_err(|e| e.to_string())?;
        }
    }
    filter.load().map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    /// 构造指定 level 的配置（其余用默认；max_processes 调高避免 RLIMIT_NPROC
    /// 按真实 uid 计数导致测试机 fork 失败）
    fn cfg_with(level: &str) -> SandboxConfig {
        SandboxConfig {
            level: level.to_string(),
            max_processes: 100_000,
            ..Default::default()
        }
    }

    /// 用指定沙箱跑一段脚本，返回 Output
    async fn run_sandboxed(sbx: &Sandbox, script: &str) -> std::process::Output {
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        sbx.apply(&mut cmd, Path::new(".")).unwrap();
        cmd.spawn().unwrap().wait_with_output().await.unwrap()
    }

    #[test]
    fn test_parse_levels() {
        assert_eq!(SandboxLevel::parse("off").unwrap(), SandboxLevel::Off);
        assert_eq!(
            SandboxLevel::parse("container").unwrap(),
            SandboxLevel::Container
        );
        assert_eq!(SandboxLevel::parse("strict").unwrap(), SandboxLevel::Strict);
    }

    #[test]
    fn test_parse_invalid() {
        let err = SandboxLevel::parse("docker").unwrap_err();
        assert!(err.contains("docker"), "got: {err}");
    }

    #[test]
    fn test_from_config_mapping() {
        let cfg = SandboxConfig {
            level: "strict".to_string(),
            bash_timeout_secs: 30,
            max_processes: 20,
            max_memory_mb: 256,
            cpu_time_secs: 30,
            max_file_size_mb: 50,
        };
        let sbx = Sandbox::from_config(&cfg).unwrap();
        assert_eq!(sbx.level, SandboxLevel::Strict);
        assert!(sbx.strict);
        assert_eq!(sbx.max_processes, 20);
        assert_eq!(sbx.max_memory_mb, 256);
        assert_eq!(sbx.cpu_time_secs, 30);
        assert_eq!(sbx.max_file_size_mb, 50);
        assert!(Sandbox::from_config(&cfg_with("bad")).is_err());
    }

    #[tokio::test]
    async fn test_container_cleans_env() {
        std::env::set_var("R2_TEST_SECRET", "leak");
        let sbx = Sandbox::from_config(&cfg_with("container")).unwrap();
        let out = run_sandboxed(&sbx, "env").await;
        std::env::remove_var("R2_TEST_SECRET");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            !text.contains("R2_TEST_SECRET"),
            "secret 泄漏到沙箱环境：{text}"
        );
        assert!(
            text.contains(&format!("PATH={SANDBOX_PATH}")),
            "PATH 未被清洗：{text}"
        );
    }

    #[tokio::test]
    async fn test_off_keeps_env() {
        std::env::set_var("R2_TEST_SECRET", "leak");
        let sbx = Sandbox::from_config(&cfg_with("off")).unwrap();
        let out = run_sandboxed(&sbx, "env").await;
        std::env::remove_var("R2_TEST_SECRET");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("R2_TEST_SECRET=leak"), "off 级别应保留环境：{text}");
    }

    #[tokio::test]
    async fn test_rlimit_fsize_truncates_write() {
        let cfg = SandboxConfig {
            max_file_size_mb: 1,
            ..cfg_with("container")
        };
        let sbx = Sandbox::from_config(&cfg).unwrap();
        let path = "/tmp/r2-fsize-test";
        let _ = std::fs::remove_file(path);
        // 写 2MB 应触发 RLIMIT_FSIZE（1MB）：head 被 SIGXFSZ 终止，文件被截断
        let out = run_sandboxed(&sbx, &format!("head -c 2000000 /dev/zero > {path}")).await;
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let _ = std::fs::remove_file(path);
        assert!(
            size <= 1024 * 1024,
            "文件应被截断到 1MB，实际 {size}；输出：{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[tokio::test]
    #[ignore = "慢且受测试机进程数影响，手动验证"]
    async fn test_rlimit_nproc_blocks_fork_bomb() {
        let cfg = SandboxConfig {
            max_processes: 5,
            ..cfg_with("container")
        };
        let sbx = Sandbox::from_config(&cfg).unwrap();
        let out = run_sandboxed(&sbx, "for i in $(seq 1 50); do sleep 10 & done; wait").await;
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success() || stderr.contains("Resource temporarily unavailable"),
            "预期大量 fork 失败：code={:?} stderr={stderr}",
            out.status.code()
        );
    }

    #[cfg(feature = "sandbox-strict")]
    #[tokio::test]
    #[ignore = "手动验证：白名单下 echo 应正常"]
    async fn test_seccomp_echo_ok() {
        let sbx = Sandbox::from_config(&cfg_with("strict")).unwrap();
        let out = run_sandboxed(&sbx, "echo hello").await;
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success() && text.contains("hello"), "got: {text}");
    }
}
