//! 会话持久化集成测试：JSONL 往返、崩溃恢复、坏行容忍
//!
//! 本 crate 仅产出二进制，测试通过 #[path] 直接引用源码模块。

#[path = "../src/types.rs"]
mod types;
#[path = "../src/session.rs"]
mod session;

use session::{Session, SessionEntry};
use tempfile::TempDir;
use types::{Role, ToolCall};

/// 往会话里写 3 条标准记录（user / assistant 带工具调用 / tool_result）
fn write_three(session: &mut Session) -> ToolCall {
    let tc = ToolCall {
        id: "call_001".to_string(),
        name: "bash".to_string(),
        arguments: r#"{"command":"ls"}"#.to_string(),
    };
    session
        .append(&SessionEntry::message(Role::User, "帮我创建文件"))
        .unwrap();
    session
        .append(&SessionEntry::assistant("好的，我来执行", vec![tc.clone()]))
        .unwrap();
    session
        .append(&SessionEntry::tool_result("call_001", "file1.rs\nfile2.rs"))
        .unwrap();
    tc
}

#[test]
fn test_recover_roundtrip() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_str().unwrap();

    let id = {
        let mut session = Session::create(dir_path).unwrap();
        write_three(&mut session);
        session
            .append(&SessionEntry::checkpoint(1))
            .unwrap();
        let id = session.id().to_string();
        drop(session); // 关闭句柄，模拟进程退出
        id
    };

    let (_session, messages) = Session::recover(dir_path, &id).unwrap();
    assert_eq!(messages.len(), 3);

    assert_eq!(messages[0].role, Role::User);
    assert_eq!(messages[0].content, "帮我创建文件");
    assert!(messages[0].tool_calls.is_none());

    assert_eq!(messages[1].role, Role::Assistant);
    assert_eq!(messages[1].content, "好的，我来执行");
    let calls = messages[1].tool_calls.as_ref().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "call_001");
    assert_eq!(calls[0].name, "bash");
    assert_eq!(calls[0].arguments, r#"{"command":"ls"}"#);

    assert_eq!(messages[2].role, Role::Tool);
    assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_001"));
    assert_eq!(messages[2].content, "file1.rs\nfile2.rs");
}

/// 崩溃模拟：3 条完整记录 + 末尾半行残 JSON → 前 3 条完整恢复，残行丢弃
#[test]
fn test_recover_drops_incomplete_last_line() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_str().unwrap();

    let id = {
        let mut session = Session::create(dir_path).unwrap();
        write_three(&mut session);
        session.id().to_string()
    };

    // 手动 append 半行 JSON（无换行的残行）
    let path = dir.path().join(format!("{id}.jsonl"));
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    write!(f, r#"{{"type":"message","role":"user","content":"写到一半"#).unwrap();
    drop(f);

    let (_session, messages) = Session::recover(dir_path, &id).unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0].content, "帮我创建文件");
    assert_eq!(messages[2].content, "file1.rs\nfile2.rs");
}

/// 中间坏行：第 2 行是垃圾字符串 → 第 1、3 条恢复，第 2 条跳过
#[test]
fn test_recover_skips_bad_middle_line() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_str().unwrap();

    let path = dir.path().join("bad-middle.jsonl");
    std::fs::write(
        &path,
        concat!(
            r#"{"type":"message","role":"user","content":"第一条","ts":1}"#,
            "\n",
            "这不是JSON垃圾行",
            "\n",
            r#"{"type":"message","role":"assistant","content":"第三条","ts":3}"#,
            "\n",
        ),
    )
    .unwrap();

    let (_session, messages) = Session::recover(dir_path, "bad-middle").unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].content, "第一条");
    assert_eq!(messages[1].content, "第三条");
}

/// 未知 role 的消息行被跳过
#[test]
fn test_recover_skips_unknown_role() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_str().unwrap();

    let path = dir.path().join("unknown-role.jsonl");
    std::fs::write(
        &path,
        concat!(
            r#"{"type":"message","role":"alien","content":"???","ts":1}"#,
            "\n",
            r#"{"type":"message","role":"user","content":"正常消息","ts":2}"#,
            "\n",
        ),
    )
    .unwrap();

    let (_session, messages) = Session::recover(dir_path, "unknown-role").unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, "正常消息");
}

/// 空文件 recover → 0 条消息不报错
#[test]
fn test_recover_empty_file() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_str().unwrap();

    let path = dir.path().join("empty.jsonl");
    std::fs::write(&path, "").unwrap();

    let (_session, messages) = Session::recover(dir_path, "empty").unwrap();
    assert!(messages.is_empty());
}

/// 恢复不存在的会话 → 报错
#[test]
fn test_recover_missing_session() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_str().unwrap();
    let result = Session::recover(dir_path, "no-such-session");
    assert!(result.is_err());
    let err = match result {
        Ok(_) => panic!("应报错"),
        Err(e) => e,
    };
    assert!(err.contains("会话不存在"));
}
