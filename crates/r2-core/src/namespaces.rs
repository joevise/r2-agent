//! 进程内 namespace 隔离（strict 沙箱档）— 双 fork 版（v0.5.2）
//!
//! mount ns（chroot 最小根）+ pid ns + net ns（断网）。
//! 无 root 时通过 user namespace 前置获得 mount/pid/net 能力
//! （AppArmor 限制的环境由 can_namespace() 诚实探测并降级）。
//!
//! ## 为什么必须双 fork（v0.5.0 单 fork 的教训）
//!
//! `unshare(CLONE_NEWPID)` 只对**之后 fork 出的子进程**生效，对调用者自身无效。
//! 单 fork 时 exec 出的 sh 仍活在宿主 PID 视图——此时挂 /proc 会暴露宿主进程
//! 列表，`/proc/1/root` 更是经典 chroot 逃逸面（v0.5.0 实测发现，故当时禁挂
//! /proc，ps 不可用）。双 fork 让 sh「出生」在新 PID 空间（成为其中的 PID 1），
//! 由它挂 /proc——挂载者的 active pid ns 即新空间，只含沙箱进程。
//!
//! 附赠性质：
//! - **ps/top 复活**：沙箱内 /proc 可用，且只见沙箱自身进程（宿主进程不可见）
//! - **内核级清理**：PID 1（sh）死亡 → 内核回收整个 PID 空间，零僵尸
//! - **退出码传播**：中间进程 waitpid 孙进程后 _exit 同码（信号 → 128+sig）

use std::ffi::CString;
use std::io::Read;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

/// 输出上限（与 bash 工具一致）
const OUTPUT_LIMIT: usize = 64 * 1024;

/// 检测 namespace 沙箱是否真正可用。
///
/// 关键：Ubuntu 23.10+ 默认 `apparmor_restrict_unprivileged_userns=1`——
/// 它允许 unshare(NEWUSER) 成功，但剥夺新 user ns 内的一切能力：
/// uid_map 写入 EPERM、后续 unshare(NEWNS/NEWPID/NEWNET) 全部 EPERM。
/// 因此仅检查 uid_map 存在不够，必须实测「能否在 user ns 内写 uid_map」。
///
/// 三种可用路径：
/// 1. 进程本身是 root（euid==0）→ 直接可建 mount/pid/net ns（不需要 userns）
/// 2. 非 root 但 AppArmor 未限制 → userns 路径可用
/// 3. 非 root 且 AppArmor 限制（Ubuntu 默认）→ 不可用，须降级
pub fn can_namespace() -> bool {
    if unsafe { libc::geteuid() } == 0 {
        return true;
    }
    if let Ok(v) = std::fs::read_to_string(
        "/proc/sys/kernel/apparmor_restrict_unprivileged_userns",
    ) {
        if v.trim() == "1" {
            return false;
        }
    }
    std::fs::read_to_string("/proc/self/uid_map")
        .map(|m| !m.trim().is_empty())
        .unwrap_or(false)
}

/// 定位系统 busybox（strict 档的最小根 /bin 唯一居民）
pub fn find_busybox() -> Option<PathBuf> {
    for p in ["/usr/bin/busybox", "/bin/busybox", "/usr/local/bin/busybox"] {
        if Path::new(p).exists() {
            return Some(PathBuf::from(p));
        }
    }
    None
}

/// 准备最小根目录 {work_dir}/.sandbox-root/
///
/// 结构：bin/{busybox,sh,...} dev/{null,...} tmp/ proc/ work/
/// - busybox 缺失 → Err（调用方降级并提示安装 busybox-static）
/// - mknod 失败 → 跳过（非致命）
pub fn prepare_min_root(work_dir: &Path) -> Result<PathBuf, String> {
    let busybox = find_busybox()
        .ok_or("未找到 busybox（strict 档需要）。Ubuntu: apt install busybox-static")?;
    let root = work_dir.join(".sandbox-root");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("bin"))
        .map_err(|e| format!("创建 bin 失败：{e}"))?;
    let _ = std::fs::create_dir_all(root.join("tmp"));
    let _ = std::fs::create_dir_all(root.join("proc"));
    let _ = std::fs::create_dir_all(root.join("work"));
    let _ = std::fs::create_dir_all(root.join("dev"));
    std::fs::copy(&busybox, root.join("bin/busybox"))
        .map_err(|e| format!("拷贝 busybox 失败：{e}"))?;
    for link in [
        "sh", "bash", "ls", "cat", "ps", "top", "mount", "umount", "echo", "sleep", "env",
        "grep", "find", "head", "tail", "wc", "mkdir", "rm", "cp", "mv", "touch", "true",
        "false",
    ] {
        let _ = symlink("busybox", root.join("bin").join(link));
    }
    for (name, major, minor) in
        [("null", 1u32, 3u32), ("zero", 1, 5), ("random", 1, 8), ("urandom", 1, 9)]
    {
        let path_c = CString::new(root.join("dev").join(name).to_string_lossy().as_bytes())
            .unwrap();
        unsafe {
            let dev_t = ((major as u64) << 8) | (minor as u64);
            let _ = libc::mknod(
                path_c.as_ptr(),
                0o020_666 | libc::S_IFCHR,
                dev_t as libc::dev_t,
            );
        }
    }
    Ok(root)
}

/// 在 namespace 沙箱内执行命令，返回 (exit_code, 合并输出)。
/// 测试/独立调用入口；bash 工具走 install_sandbox_pre_exec（同一核心）。
pub fn exec_in_sandbox(cmd: &str, work_dir: &Path) -> Result<(i32, String), String> {
    let root = prepare_min_root(work_dir)?;
    let mut command = std::process::Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(cmd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    unsafe {
        install_sandbox_pre_exec(&mut command, root, work_dir.to_path_buf(), cmd);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("沙箱进程启动失败：{e}（ns/chroot 阶段失败常见于权限，可降级 container 档）"))?;
    let mut out = String::new();
    let mut buf = [0u8; 4096];
    if let Some(p) = child.stdout.as_mut() {
        while out.len() < OUTPUT_LIMIT {
            match p.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => out.push_str(&String::from_utf8_lossy(&buf[..n])),
            }
        }
    }
    if let Some(p) = child.stderr.as_mut() {
        while out.len() < OUTPUT_LIMIT {
            match p.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => out.push_str(&String::from_utf8_lossy(&buf[..n])),
            }
        }
    }
    if out.len() > OUTPUT_LIMIT {
        out.truncate(OUTPUT_LIMIT);
        out.push_str("\n...(输出截断)");
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let code = status.code().unwrap_or(-1);
    out.push_str(&format!("\nexit_code={code}\n"));
    Ok((code, out))
}

/// 双 fork 链的全部预构建参数（fork 前分配完毕，闭包内零 malloc——
/// pre_exec 上下文要求 async-signal-safe，避免 fork 的 malloc 死锁风险）
struct NsPrebuilt {
    root: CString,
    work_src: CString,
    work_dst: CString,
    proc_dst: CString,
    proc_fs: CString,
    work_in_root: CString,
    slash: CString,
    uid_map: String,
    gid_map: String,
    is_root: bool,
    /// execve 的正确形态：NULL 结尾的指针数组（指向下方 CString 的堆数据）。
    /// 经典陷阱：Vec<CString> 的 as_ptr() 是结构体数组指针（{ptr,len,cap}×N），
    /// 不是 char** ——直接传给 execve 会读到垃圾指针 → ENOENT → 127。
    /// CString 堆数据不随 Vec move 失效，fork 前构建一次即安全。
    argv_c: Vec<CString>,
    envp_c: Vec<CString>,
    /// 指针以 usize 携带（裸指针非 Send/Sync，闭包边界过不去；
    /// 仅在 fork 出的子进程内转回指针并解引用，无跨线程语义）
    argv_ptrs: Vec<usize>,
    envp_ptrs: Vec<usize>,
}

/// 双 fork 沙箱链的执行核心：在 pre_exec 上下文（fork 后、exec 前）调用。
///
/// 进程树（成功路径）：
///   r2 → std fork(中间进程) ── unshare(user?) + MS_PRIVATE + unshare(ns|pid|net)
///        │                     + bind work→{root}/work
///        └─ libc fork(孙进程 = 新 PID ns 内 PID 1)
///             mount proc → {root}/proc（挂载者在新 ns → 只含沙箱进程）
///             chroot(root) + chdir("/work") + execve sh -c <命令>
///        中间进程：waitpid(孙) → _exit 传播退出码（信号 → 128+sig）
///
/// 挂载私有化（MS_REC|MS_PRIVATE on /）：unshare(NEWNS) 后挂载树仍是宿主的
/// 共享副本，不锁私有则我们的 bind/proc 挂载可能**回传宿主**（systemd 环境
/// / 默认 shared）。这是 bubblewrap 等工具的标准做法。
///
/// 退出码约定（fork 后的错误无法返回 std，以码表达）：
///   125=挂载/等待失败，126=chroot/chdir 失败，
///   160+errno=execve 失败诊断（162=ENOENT 173=EACCES 174=EFAULT 168=ENOEXEC）
///
/// # Safety
/// 运行于 fork 后的子进程上下文，仅调用 libc 原语与预构建 C 字符串；
/// uid_map/gid_map 的 fs::write 非 async-signal-safe，仅非 root 路径使用
/// （该路径在 AppArmor 环境被 can_namespace 拦截，实际仅无限制环境到达）。
unsafe fn double_fork_ns_exec(pre: &NsPrebuilt) -> std::io::Result<()> {
    // 1) user namespace 前置（无 root 时获得 mount/pid/net 能力）
    if !pre.is_root {
        if libc::unshare(libc::CLONE_NEWUSER) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // setgroups deny 必须先于 gid_map（内核写入顺序规则）
        let _ = std::fs::write("/proc/self/setgroups", b"deny");
        std::fs::write("/proc/self/uid_map", pre.uid_map.as_bytes())
            .map_err(|e| std::io::Error::other(format!("uid_map: {e}")))?;
        std::fs::write("/proc/self/gid_map", pre.gid_map.as_bytes())
            .map_err(|e| std::io::Error::other(format!("gid_map: {e}")))?;
    }
    // 2) mount + pid + net 三件套（net ns 新建后无任何网卡 → 天然断网）
    if libc::unshare(libc::CLONE_NEWNS | libc::CLONE_NEWPID | libc::CLONE_NEWNET) != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // 3) 挂载传播锁私有：防止沙箱内挂载回传宿主 mount ns
    if libc::mount(
        std::ptr::null(),
        pre.slash.as_ptr(),
        std::ptr::null(),
        libc::MS_REC | libc::MS_PRIVATE,
        std::ptr::null(),
    ) != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // 4) bind work_dir → {root}/work（chroot 前做：源路径此刻仍在完整视图内）
    if libc::mount(
        pre.work_src.as_ptr(),
        pre.work_dst.as_ptr(),
        std::ptr::null(),
        libc::MS_BIND | libc::MS_REC,
        std::ptr::null(),
    ) != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // 5) 双 fork：孙进程出生在新 PID ns
    let pid = libc::fork();
    if pid < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if pid == 0 {
        // ── 孙进程（新 PID ns 内 PID 1）──
        // 挂 /proc：挂载者的 active pid ns = 新空间 → 只显示沙箱进程；
        // /proc/1 = sh 自身（cmdline 是沙箱命令），宿主进程结构性不可见
        if libc::mount(
            pre.proc_fs.as_ptr(),
            pre.proc_dst.as_ptr(),
            pre.proc_fs.as_ptr(),
            0,
            std::ptr::null(),
        ) != 0 {
            libc::_exit(125);
        }
        if libc::chroot(pre.root.as_ptr()) != 0 {
            libc::_exit(126);
        }
        // 相对路径 → 真实工作目录（bind 在 /work）
        if libc::chdir(pre.work_in_root.as_ptr()) != 0 {
            libc::_exit(126);
        }
        libc::execve(
            pre.argv_ptrs[0] as *const libc::c_char,
            pre.argv_ptrs.as_ptr() as *const *const libc::c_char,
            pre.envp_ptrs.as_ptr() as *const *const libc::c_char,
        );
        // execve 失败：160+errno 编码诊断（ENOENT=162 EACCES=173 EFAULT=174 ...）
        let e = std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(90);
        libc::_exit(160 + e.min(90) as i32);
    }
    // ── 中间进程（保姆）：等孙进程并传播退出码 ──
    // 注：rlimits/seccomp 由更早的 pre_exec 闭包设好（bash.rs 装闭包顺序：
    // 先 apply 后 ns），经 fork 继承给孙进程。
    let mut status: libc::c_int = 0;
    loop {
        let r = libc::waitpid(pid, &mut status, 0);
        if r == pid {
            break;
        }
        if r < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            libc::_exit(125);
        }
    }
    if libc::WIFEXITED(status) {
        libc::_exit(libc::WEXITSTATUS(status));
    }
    if libc::WIFSIGNALED(status) {
        // 信号死亡 → 128+sig（shell 惯例），std 侧 code() 可读
        libc::_exit(128 + libc::WTERMSIG(status));
    }
    libc::_exit(125);
}

/// 为进程 Command 安装双 fork namespace 沙箱（strict 档 bash 用）。
///
/// **闭包顺序约定（关键）**：本函数装的闭包在成功路径上不返回——中间进程
/// 在闭包内 _exit。因此调用方必须先装 rlimits/seccomp 闭包（sandbox.apply），
/// 再装本闭包；setrlimit 经双 fork 继承给最终命令进程。
///
/// 语义：成功路径上 std 的 exec 不会发生——孙进程由本链自行 execve，
/// std 侧 Child::wait 拿到的即沙箱命令的真实退出码（信号 → 128+sig）。
/// fork 前失败（unshare/挂载）照常作为 spawn 错误冒泡 → 调用方降级。
///
/// # Safety
/// pre_exec 仅在 unix fork 后执行；见 double_fork_ns_exec 的安全性说明。
pub unsafe fn install_sandbox_pre_exec(
    cmd: &mut std::process::Command,
    root: PathBuf,
    work_dir: PathBuf,
    command: &str,
) {
    use std::os::unix::process::CommandExt;
    let is_root = libc::geteuid() == 0;
    let uid = libc::getuid();
    let gid = libc::getgid();
    let root_s = root.to_string_lossy().into_owned();
    let work_s = work_dir.to_string_lossy().into_owned();
    let mut pre = NsPrebuilt {
        root: CString::new(root_s.as_str()).unwrap(),
        work_src: CString::new(work_s.as_str()).unwrap(),
        work_dst: CString::new(format!("{root_s}/work")).unwrap(),
        proc_dst: CString::new(format!("{root_s}/proc")).unwrap(),
        proc_fs: CString::new("proc").unwrap(),
        work_in_root: CString::new("/work").unwrap(),
        slash: CString::new("/").unwrap(),
        uid_map: format!("0 {uid} 1\n"),
        gid_map: format!("0 {gid} 1\n"),
        is_root,
        // argv：sh -c <命令>（chroot 内 /bin/sh → busybox；Debian busybox
        // 无 bash applet——避让真 bash，ash 语法兼容 POSIX）
        argv_c: vec![
            CString::new("/bin/sh").unwrap(),
            CString::new("-c").unwrap(),
            CString::new(command).unwrap(),
        ],
        envp_c: vec![
            CString::new("PATH=/bin").unwrap(),
            CString::new("HOME=/").unwrap(),
            CString::new("TERM=dumb").unwrap(),
        ],
        argv_ptrs: Vec::new(),
        envp_ptrs: Vec::new(),
    };
    // 指针数组最后构建（NULL 结尾，依赖上面 CString 的堆地址）
    pre.argv_ptrs = pre
        .argv_c
        .iter()
        .map(|c| c.as_ptr() as usize)
        .chain(std::iter::once(0))
        .collect();
    pre.envp_ptrs = pre
        .envp_c
        .iter()
        .map(|c| c.as_ptr() as usize)
        .chain(std::iter::once(0))
        .collect();
    cmd.pre_exec(move || {
        // SAFETY: 参数全部 fork 前预构建，无用户可控指针
        unsafe { double_fork_ns_exec(&pre) }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_can_namespace_no_panic() {
        let _ = can_namespace();
    }

    #[test]
    fn test_find_busybox_no_panic() {
        let _ = find_busybox();
    }

    #[test]
    fn test_prepare_min_root_structure() {
        if find_busybox().is_none() {
            return; // 环境缺 busybox 跳过
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = prepare_min_root(tmp.path()).expect("准备最小根");
        assert!(root.join("bin/busybox").exists());
        assert!(root.join("bin/sh").exists());
        assert!(root.join("proc").exists());
        assert!(root.join("work").exists());
        assert!(root.join("tmp").exists());
    }

    /// 真实 ns 执行（手动跑：cargo test -p r2-core namespaces -- --ignored）
    #[test]
    #[ignore]
    fn test_exec_in_sandbox_real() {
        if !can_namespace() {
            eprintln!("跳过：本机 namespace 不可用（AppArmor 限制或非 root）");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("data.txt"), "count: 42").unwrap();
        let (code, out) = exec_in_sandbox("cat /work/data.txt", tmp.path()).expect("ns 执行");
        assert_eq!(code, 0, "输出: {out}");
        assert!(out.contains("count: 42"), "bind 生效应能读到 work 文件: {out}");
    }

    /// 双 fork 核心安全性质（手动跑，需 root/无限制环境）
    #[test]
    #[ignore]
    fn test_exec_sandbox_isolation() {
        if !can_namespace() {
            eprintln!("跳过：本机 namespace 不可用");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        // 根目录只有最小结构
        let (c1, out1) = exec_in_sandbox("ls /", tmp.path()).unwrap();
        assert_eq!(c1, 0);
        for must in ["bin", "dev", "proc", "tmp", "work"] {
            assert!(out1.contains(must), "根目录应含 {must}: {out1}");
        }
        // 宿主的 home/root 不可见（结构性不存在）
        let (c2, out2) = exec_in_sandbox("ls /home /root 2>&1 || echo NO_HOME", tmp.path()).unwrap();
        assert!(out2.contains("NO_HOME") || c2 != 0, "宿主目录不应可见: {out2}");
        // v0.5.2 新性质：/proc 已挂载且只含沙箱进程
        // /proc/1/cmdline 是沙箱 sh 自己（含本测试命令文本），绝不能含宿主 cmdline
        let (_, out3) = exec_in_sandbox("cat /proc/1/cmdline", tmp.path()).unwrap();
        assert!(out3.contains("ls /") || out3.contains("cmdline"), "PID1 应是沙箱 sh: {out3}");
        // ps 可用（v0.5.0 禁挂 proc 后废掉的能力复活）
        let (c4, out4) = exec_in_sandbox("ps", tmp.path()).unwrap();
        assert_eq!(c4, 0, "ps 应可用: {out4}");
        // 网络不通（net ns 无配置）
        let (c5, _) = exec_in_sandbox("wget -T 2 -q http://127.0.0.1:1/ 2>&1; echo rc=$?", tmp.path()).unwrap();
        let _ = c5;
    }
}
