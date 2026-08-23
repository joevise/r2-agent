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
use std::path::{Path, PathBuf};

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
    // supervisor 注入的会话 cgroup 路径：子 r2 的 bash 树直接入会话组，
    // 使会话级 pids/memory 限额覆盖整棵子树（层级计数是内核语义）
    "R2_CGROUP_JOIN",
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
    /// cgroup v2 pids 限制开关（失败自动降级 rlimits，不影响执行）
    pub cgroup: bool,
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
            cgroup: cfg.cgroup,
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
            // 措辞准确性（8/23）：ns 隔离（假根/pid/net）与 cgroup 由 namespaces.rs 提供、
            // 不依赖此 feature；未编译仅意味着系统调用白名单缺失
            Some(
                "WARN: seccomp 系统调用白名单未编译（需 sandbox-strict feature）；namespace 隔离仍生效".to_string(),
            )
        } else {
            None
        };
        Ok(warn)
    }
}

/// cgroup v2 unified 挂载点
const CGROUP_MOUNT: &str = "/sys/fs/cgroup";

/// r2 专属 cgroup 组名（建在进程当前 cgroup 之下——用户级 systemd 委派场景
/// 不能在挂载点根目录建组，必须在自己所在的组内建子组）
const CGROUP_AGENT_DIR: &str = "r2-agent";

/// 找到"可以建组"的最近 cgroup 层：从当前进程所在组逐层向上，
/// 在每层试探建组+写 pids.max（立即清理），首个成功者即返回。
/// 为什么要向上：systemd nsdelegate 严格模式下，进程所在的深层 service/scope
/// 不允许内部建组，但其父层（如 user@1000.service）允许建"兄弟组"——
/// 跨层把 bash 进程写入父层新建组完全合法。
/// 全部失败返回 None（降级 rlimits）。
fn current_cgroup_dir() -> Option<PathBuf> {
    let Ok(content) = std::fs::read_to_string("/proc/self/cgroup") else {
        return None;
    };
    let cur = content.lines().find_map(|l| l.strip_prefix("0::"))?;
    let cur = cur.trim_start_matches('/');
    if cur.is_empty() {
        return None;
    }
    let mut node = PathBuf::from(CGROUP_MOUNT).join(cur);
    let mount = PathBuf::from(CGROUP_MOUNT);
    for _ in 0..12 {
        if is_cgroup_v2(&node) {
            let probe = node.join("r2-probe");
            if std::fs::create_dir(&probe).is_ok() {
                // domain threaded 层下子组 type 为 "domain invalid"/"threaded"——
                // 不接受进程迁移（写 cgroup.procs 报 EOPNOTSUPP），必须跳过
                let ty = std::fs::read_to_string(probe.join("cgroup.type"))
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let type_ok = ty == "domain";
                let write_ok = type_ok && std::fs::write(probe.join("pids.max"), "2").is_ok();
                std::fs::remove_dir(&probe).ok();
                if write_ok {
                    return Some(node);
                }
            }
        }
        match node.parent() {
            Some(p) if *p != mount => node = p.to_path_buf(),
            _ => break,
        }
    }
    None
}

/// 检测 root 是否为 cgroup v2 unified 挂载点（存在 cgroup.controllers）
fn is_cgroup_v2(root: &Path) -> bool {
    root.join("cgroup.controllers").exists()
}

/// pids.max 的写入值：0 = 不限（写 "max"）
fn pids_max_value(max_processes: u32) -> String {
    if max_processes == 0 {
        "max".to_string()
    } else {
        max_processes.to_string()
    }
}

/// 把 pid 挂入 r2 专属 cgroup 并写入 pids.max（root 参数便于测试注入 mock fs）。
/// 任一步失败返回 Err，调用方降级为 rlimits，不影响命令执行。
fn attach_to_cgroup_at(root: &Path, max_processes: u32, pid: u32) -> Result<(), String> {
    if !is_cgroup_v2(root) {
        return Err(format!("{} 非 cgroup v2 unified 挂载", root.display()));
    }
    // 单层结构：直接在最近可用层下建 r2-agent-{pid} 组。
    // 为什么不做 r2-agent/{pid} 父子两层：子组要生效 pids 控制器需要父组先开
    // subtree_control，而"内部已有进程的组不能再改 subtree_control"（内核规则），
    // 在 systemd 委派场景下父层往往已含进程导致静默失败。单层组直接继承所在层
    // 的控制器可用性，实测稳定。
    // 组名带 pid：每次 bash 调用一个组，进程退出后 systemd 回收空组。
    let group = root.join(format!("{CGROUP_AGENT_DIR}-{pid}"));
    if !group.exists() {
        std::fs::create_dir(&group).map_err(|e| format!("创建 {} 失败：{e}", group.display()))?;
    }
    std::fs::write(group.join("pids.max"), pids_max_value(max_processes))
        .map_err(|e| format!("写 pids.max 失败：{e}"))?;
    // 子进程一旦进组，其全部后代继承该组，fork 炸弹被 pids.max 掐死
    std::fs::write(group.join("cgroup.procs"), pid.to_string())
        .map_err(|e| format!("写 cgroup.procs 失败：{e}"))?;
    Ok(())
}

/// bash 子进程 spawn 后立即调用：挂入 cgroup 限 pids。
/// 返回 Some(warn) 表示降级（非 root / 非 v2 / 只读等），None 表示成功。
/// 清理说明：临时组不主动删除——systemd 会回收空组，主动删与进程退出有竞态。
pub fn attach_child_to_cgroup(max_processes: u32, pid: u32) -> Option<String> {
    // supervisor 场景：直接入会话组（会话级 pids/memory 统一核算整棵子树）
    if let Ok(group) = std::env::var("R2_CGROUP_JOIN") {
        let path = std::path::PathBuf::from(&group);
        if path.join("cgroup.procs").exists() {
            return match std::fs::write(path.join("cgroup.procs"), pid.to_string()) {
                Ok(()) => None,
                Err(e) => Some(format!("[sandbox] 加入会话组失败（{e}），独立核算")),
            };
        }
    }
    let Some(dir) = current_cgroup_dir() else {
        return Some("[sandbox] cgroup 无可用层级（systemd 锁定），降级 rlimits".to_string());
    };
    match attach_to_cgroup_at(&dir, max_processes, pid) {
        Ok(()) => None,
        Err(e) => Some(format!("[sandbox] cgroup 不可用（{e}），降级 rlimits")),
    }
}

/// supervisor 专用：创建会话 cgroup 组（pids.max + 可选 memory.max）并放入子进程。
/// 返回 (warn, 组路径)——路径用于注入 R2_CGROUP_JOIN 环境变量给子 r2，
/// 使其后代 bash 直接入组，实现会话级资源核算。
/// 内存说明：memory.max 对整棵子树生效（含 bash 后代）；OOM 时内核直接 SIGKILL，
/// 因此默认不限（0），由调用方按需设置（云场景建议 512M+）。
pub fn create_session_cgroup(
    name_pid: u32,
    max_processes: u32,
    memory_limit_mb: u64,
) -> (Option<String>, Option<std::path::PathBuf>) {
    let Some(dir) = current_cgroup_dir() else {
        return (
            Some("[sandbox] cgroup 无可用层级，会话资源限额降级 rlimits".to_string()),
            None,
        );
    };
    // 组名用 supervisor 的 pid（spawn 前可知）→ 可先注入子进程 env（R2_CGROUP_JOIN）
    let group = dir.join(format!("{CGROUP_AGENT_DIR}-sess-{name_pid}"));
    if !group.exists() {
        if let Err(e) = std::fs::create_dir(&group) {
            return (
                Some(format!("[sandbox] 会话组创建失败（{e}），降级 rlimits")),
                None,
            );
        }
    }
    if let Err(e) = std::fs::write(group.join("pids.max"), pids_max_value(max_processes)) {
        return (Some(format!("[sandbox] 会话 pids.max 写入失败（{e}）")), None);
    }
    let mem_warn = if memory_limit_mb > 0 {
        let bytes = memory_limit_mb * 1024 * 1024;
        std::fs::write(group.join("memory.max"), bytes.to_string())
            .err()
            .map(|e| format!("[sandbox] 会话 memory.max 写入失败（{e}），内存仅 rlimits"))
    } else {
        None
    };
    // 注意：不写 cgroup.procs——子进程 pid 在 spawn 后才知，由 supervisor 写入
    (mem_warn, Some(group))
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
            ..Default::default()
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
        // 与 test_container_cleans_env 用不同变量名：彻底避免进程级环境变量的并行竞态
        std::env::set_var("R2_TEST_SECRET_KEEP", "leak");
        let sbx = Sandbox::from_config(&cfg_with("off")).unwrap();
        let out = run_sandboxed(&sbx, "env | grep R2_TEST").await;
        std::env::remove_var("R2_TEST_SECRET_KEEP");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains("R2_TEST_SECRET_KEEP=leak"),
            "off 级别应保留环境：{text}"
        );
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

    #[test]
    fn test_cgroup_v2_detection() {
        let tmp = tempfile::tempdir().unwrap();
        // 无 cgroup.controllers：非 v2
        assert!(!is_cgroup_v2(tmp.path()));
        // 模拟 v2 挂载点
        std::fs::write(tmp.path().join("cgroup.controllers"), "cpu io pids").unwrap();
        assert!(is_cgroup_v2(tmp.path()));
    }

    #[test]
    fn test_pids_max_value() {
        assert_eq!(pids_max_value(0), "max");
        assert_eq!(pids_max_value(64), "64");
        assert_eq!(pids_max_value(256), "256");
    }

    #[test]
    fn test_attach_mock_v2_success() {
        // mock v2 挂载点：建组 + pids.max + cgroup.procs 全链路成功
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("cgroup.controllers"), "pids").unwrap();
        attach_to_cgroup_at(tmp.path(), 64, 12345).unwrap();
        // v0.4.1 起单层组结构：r2-agent-{pid}（两层结构有 subtree_control 竞态）
        let group = tmp.path().join(format!("{CGROUP_AGENT_DIR}-12345"));
        assert_eq!(std::fs::read_to_string(group.join("pids.max")).unwrap(), "64");
        assert_eq!(
            std::fs::read_to_string(group.join("cgroup.procs")).unwrap(),
            "12345"
        );
        // 幂等：同 pid 再挂一次复用已有组
        attach_to_cgroup_at(tmp.path(), 0, 12345).unwrap();
        assert_eq!(std::fs::read_to_string(group.join("pids.max")).unwrap(), "max");
    }

    #[test]
    fn test_attach_non_v2_errors_not_panics() {
        // 非 v2 根（空目录）→ Err 降级，不 panic
        let tmp = tempfile::tempdir().unwrap();
        let err = attach_to_cgroup_at(tmp.path(), 64, 1).unwrap_err();
        assert!(err.contains("非 cgroup v2"), "got: {err}");
    }

    #[test]
    fn test_attach_readonly_fs_degrades() {
        // mock 只读 fs：v2 文件存在但目录不可写 → Err 降级（root 下权限位失效，跳过）
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("cgroup.controllers"), "pids").unwrap();
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let result = attach_to_cgroup_at(tmp.path(), 64, 1);
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err(), "只读 fs 应降级：{result:?}");
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
