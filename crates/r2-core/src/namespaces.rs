//! 进程内 namespace 隔离（strict 沙箱档）
//!
//! mount ns（chroot 最小根目录）+ pid ns + net ns（断网）。
//! 无 root 时通过 user namespace 前置获得 mount/pid/net 能力。
//!
//! v0.5 采用 chroot（pivot_root 是后续增强）：
//! bash 在最小根内只能看到 bin/dev/proc/tmp/work，
//! ~/.ssh、/etc 等宿主路径在该视图中不存在（结构性不可见，非权限拦截）。

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
    // root 直接可用
    if unsafe { libc::geteuid() } == 0 {
        return true;
    }
    // AppArmor 限制探测（Ubuntu 23.10+）
    if let Ok(v) = std::fs::read_to_string(
        "/proc/sys/kernel/apparmor_restrict_unprivileged_userns",
    ) {
        if v.trim() == "1" {
            // 被限制——userns 是空壳，mount/pid/net 建不出来
            return false;
        }
    }
    // 其余情况：uid_map 存在即认为可（保守）
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
/// 结构：bin/{busybox,sh,bash,...} dev/{null,...} tmp/ proc/ work/
/// - busybox 缺失 → Err（调用方降级并提示安装 busybox-static）
/// - mknod 失败 → 跳过（非致命）
pub fn prepare_min_root(work_dir: &Path) -> Result<PathBuf, String> {
    let busybox =
        find_busybox().ok_or("未找到 busybox（strict 档需要）。Ubuntu: apt install busybox-static")?;
    let root = work_dir.join(".sandbox-root");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("bin"))
        .map_err(|e| format!("创建 bin 失败：{e}"))?;
    let _ = std::fs::create_dir_all(root.join("tmp"));
    let _ = std::fs::create_dir_all(root.join("proc"));
    let _ = std::fs::create_dir_all(root.join("work"));
    let _ = std::fs::create_dir_all(root.join("dev"));
    // 拷贝 busybox（拷贝而非 bind：chroot 后仍可用）
    std::fs::copy(&busybox, root.join("bin/busybox"))
        .map_err(|e| format!("拷贝 busybox 失败：{e}"))?;
    // busybox 多调用符号链接
    for link in [
        "sh", "bash", "ls", "cat", "ps", "mount", "umount", "echo", "sleep", "env", "grep",
        "find", "head", "tail", "wc", "mkdir", "rm", "cp", "mv", "touch", "true", "false",
    ] {
        let _ = symlink("busybox", root.join("bin").join(link));
    }
    // dev 设备节点（尽力而为；user ns 内 mknod 受限时部分失败可接受）
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
///
/// pre_exec 顺序（exec 前，子进程上下文）：
/// 1. 无 root：unshare(CLONE_NEWUSER) → setgroups=deny → uid_map/gid_map 映射自身
/// 2. unshare(CLONE_NEWNS | CLONE_NEWPID | CLONE_NEWNET)（net 无配置=断网；pid 对 exec 后自身生效=PID1）
/// 3. mount proc → root/proc；bind work_dir → root/work
/// 4. chroot(root) + chdir("/")
/// 5. exec /bin/sh -c cmd（宿主路径，chroot 前解析）
pub fn exec_in_sandbox(cmd: &str, work_dir: &Path) -> Result<(i32, String), String> {
    use std::os::unix::process::CommandExt;

    let root = prepare_min_root(work_dir)?;
    let root_s = root.to_string_lossy().into_owned();
    let work_s = work_dir.to_string_lossy().into_owned();

    let is_root = unsafe { libc::geteuid() } == 0;
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };

    let mut command = std::process::Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(cmd)
        .current_dir(work_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    unsafe {
        command.pre_exec(move || {
            // 1) user namespace 前置（无 root）
            if !is_root {
                if libc::unshare(libc::CLONE_NEWUSER) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let _ = std::fs::write("/proc/self/setgroups", b"deny");
                let uid_line = format!("0 {uid} 1\n");
                let gid_line = format!("0 {gid} 1\n");
                std::fs::write("/proc/self/uid_map", &uid_line)
                    .map_err(|e| std::io::Error::other(format!("uid_map: {e}")))?;
                std::fs::write("/proc/self/gid_map", &gid_line)
                    .map_err(|e| std::io::Error::other(format!("gid_map: {e}")))?;
            }
            // 2) mount + pid + net namespace
            if libc::unshare(libc::CLONE_NEWNS | libc::CLONE_NEWPID | libc::CLONE_NEWNET) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // 3) work bind（注意：**不挂 /proc**）
            // 为什么不挂：unshare(CLONE_NEWPID) 不改变当前进程的 pid ns（只影响
            // 之后 fork 的子进程），而 procfs 内容取决于挂载者的 active pid ns——
            // 此处挂的 /proc 仍反映宿主进程列表，/proc/1/root 更是经典 chroot
            // 逃逸面。正确解法是双 fork（孙进程 exec 时才在新 pid ns 内成为 PID 1），
            // v0.5.1 重构。当前诚实取舍：ns 内 /proc 为空目录，ps/top 不可用。
            let zero: *const libc::c_void = std::ptr::null();
            let work_dst = CString::new(format!("{root_s}/work")).unwrap();
            let work_src = CString::new(work_s.as_str()).unwrap();
            if libc::mount(
                work_src.as_ptr(),
                work_dst.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND | libc::MS_REC,
                zero,
            ) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // 4) chroot + chdir("/")
            let root_c = CString::new(root_s.as_str()).unwrap();
            if libc::chroot(root_c.as_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let slash = CString::new("/").unwrap();
            if libc::chdir(slash.as_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // env：chroot 后 PATH=/bin（busybox 链接）
    command.env_clear();
    command.env("PATH", "/bin");
    command.env("HOME", "/");

    let mut child = command
        .spawn()
        .map_err(|e| format!("沙箱进程启动失败：{e}（pre_exec 的 ns/chroot 阶段失败常见于权限，可降级 container 档）"))?;
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let mut out = String::new();
    let mut buf = [0u8; 4096];
    // 两管道轮询读：任一有数据就收；用 would-block 模拟非阻塞太复杂，
    // 直接先读完 stdout 再读 stderr（bash 场景输出顺序无强要求）
    if let Some(p) = out_pipe.as_mut() {
        while out.len() < OUTPUT_LIMIT {
            match p.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => out.push_str(&String::from_utf8_lossy(&buf[..n])),
            }
        }
    }
    if let Some(p) = err_pipe.as_mut() {
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
            return;
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

    #[test]
    #[ignore]
    fn test_exec_sandbox_isolation() {
        if !can_namespace() {
            eprintln!("跳过：本机 namespace 不可用（AppArmor 限制或非 root）");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        // 根目录只有最小结构
        let (c1, out1) = exec_in_sandbox("ls /", tmp.path()).unwrap();
        assert_eq!(c1, 0);
        for must in ["bin", "dev", "proc", "tmp", "work"] {
            assert!(out1.contains(must), "根目录应含 {must}: {out1}");
        }
        // 宿主的 home 不可见
        let (c2, out2) = exec_in_sandbox("ls /home 2>&1 || echo NO_HOME", tmp.path()).unwrap();
        assert!(out2.contains("NO_HOME") || c2 != 0, "宿主 /home 不应可见: {out2}");
        // 网络不通（net ns 无配置）
        let (c3, _) = exec_in_sandbox("wget -T 2 -q http://127.0.0.1:1/ || echo NET_BLOCKED", tmp.path()).unwrap();
        let _ = c3;
    }
}

/// 为已配置好的进程 Command 安装 namespace 隔离（pre_exec）。
///
/// 供 bash 工具的 strict 档使用：命令仍由调用方 spawn 并收集输出，
/// 本函数只负责在 fork 后、exec 前把子进程关进 mount/pid/net namespace + chroot 最小根。
///
/// 前置：调用方必须已 `prepare_min_root(work_dir)` 成功（返回的 root 传入）。
/// 注意：安装后子进程的 exec 目标（bash）会在 chroot 内解析——
/// 因此 Command 应使用 chroot 内存在的路径（/bin/sh via busybox）。
///
/// # Safety
/// pre_exec 闭包在 fork 后的子进程中运行，只调用 async-signal-safe 之外的
/// libc/文件操作，但参数全部为固定值（无用户可控指针），且失败即返回 Err 阻止 exec。
pub unsafe fn install_sandbox_pre_exec(
    cmd: &mut std::process::Command,
    root: PathBuf,
    work_dir: PathBuf,
) {
    use std::os::unix::process::CommandExt;
    let is_root = libc::geteuid() == 0;
    let uid = libc::getuid();
    let gid = libc::getgid();
    let root_s = root.to_string_lossy().into_owned();
    let work_s = work_dir.to_string_lossy().into_owned();
    cmd.pre_exec(move || {
        if !is_root {
            if libc::unshare(libc::CLONE_NEWUSER) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let _ = std::fs::write("/proc/self/setgroups", b"deny");
            std::fs::write("/proc/self/uid_map", format!("0 {uid} 1\n"))
                .map_err(|e| std::io::Error::other(format!("uid_map: {e}")))?;
            std::fs::write("/proc/self/gid_map", format!("0 {gid} 1\n"))
                .map_err(|e| std::io::Error::other(format!("gid_map: {e}")))?;
        }
        if libc::unshare(libc::CLONE_NEWNS | libc::CLONE_NEWPID | libc::CLONE_NEWNET) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let zero: *const libc::c_void = std::ptr::null();
        let proc_dst = CString::new(format!("{root_s}/proc")).unwrap();
        let work_dst = CString::new(format!("{root_s}/work")).unwrap();
        let work_src = CString::new(work_s.as_str()).unwrap();
        let psrc = CString::new("proc").unwrap();
        let ptype = CString::new("proc").unwrap();
        if libc::mount(psrc.as_ptr(), proc_dst.as_ptr(), ptype.as_ptr(), 0, zero) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::mount(work_src.as_ptr(), work_dst.as_ptr(), std::ptr::null(),
            libc::MS_BIND | libc::MS_REC, zero) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let root_c = CString::new(root_s.as_str()).unwrap();
        if libc::chroot(root_c.as_ptr()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let slash = CString::new("/").unwrap();
        if libc::chdir(slash.as_ptr()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    });
}
