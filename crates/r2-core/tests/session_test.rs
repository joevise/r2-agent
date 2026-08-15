//! 会话持久化集成测试：JSONL 往返、崩溃恢复、坏行容忍

use r2_core::session::{Session, SessionEntry};
use r2_core::types::{Role, ToolCall};
use tempfile::TempDir;

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

/// 基础分叉：父写 3 条 → branch(None) → 新会话 append 1 条 →
/// recover 新会话得 4 条（3 继承 + 1 新），父会话仍 3 条（不可变）
#[test]
fn test_branch_basic() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_str().unwrap();

    let parent_id = {
        let mut parent = Session::create(dir_path).unwrap();
        write_three(&mut parent);
        parent.id().to_string()
    };

    let (mut child, inherited) = Session::branch(dir_path, &parent_id, None).unwrap();
    assert_eq!(inherited.len(), 3);
    assert_eq!(inherited[0].content, "帮我创建文件");
    let child_id = child.id().to_string();
    child
        .append(&SessionEntry::message(Role::User, "分支后的新问题"))
        .unwrap();
    drop(child);

    let (_s, messages) = Session::recover(dir_path, &child_id).unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[3].content, "分支后的新问题");

    // 父会话不可变
    let (_p, parent_msgs) = Session::recover(dir_path, &parent_id).unwrap();
    assert_eq!(parent_msgs.len(), 3);
}

/// 定点分叉：父写 5 条 → branch(Some(2)) → recover 只继承前 2 条 + 自己的后续
#[test]
fn test_branch_at() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_str().unwrap();

    let parent_id = {
        let mut parent = Session::create(dir_path).unwrap();
        for i in 1..=5 {
            parent
                .append(&SessionEntry::message(Role::User, &format!("第{i}条")))
                .unwrap();
        }
        parent.id().to_string()
    };

    let (mut child, inherited) = Session::branch(dir_path, &parent_id, Some(2)).unwrap();
    assert_eq!(inherited.len(), 2);
    assert_eq!(inherited[1].content, "第2条");
    let child_id = child.id().to_string();
    child
        .append(&SessionEntry::message(Role::User, "分叉点之后"))
        .unwrap();
    drop(child);

    let (_s, messages) = Session::recover(dir_path, &child_id).unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0].content, "第1条");
    assert_eq!(messages[1].content, "第2条");
    assert_eq!(messages[2].content, "分叉点之后");

    // 超界 upto → 取全部
    let (_c2, inherited_all) = Session::branch(dir_path, &parent_id, Some(999)).unwrap();
    assert_eq!(inherited_all.len(), 5);
}

/// 链式分支：A 分出 B，B 分出 C → recover C 得 A 前缀 + B 段 + 自己
#[test]
fn test_branch_chain() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_str().unwrap();

    let id_a = {
        let mut a = Session::create(dir_path).unwrap();
        a.append(&SessionEntry::message(Role::User, "A的消息")).unwrap();
        a.id().to_string()
    };
    let id_b = {
        let (mut b, _) = Session::branch(dir_path, &id_a, None).unwrap();
        b.append(&SessionEntry::message(Role::User, "B的消息")).unwrap();
        b.id().to_string()
    };
    let id_c = {
        let (mut c, _) = Session::branch(dir_path, &id_b, None).unwrap();
        c.append(&SessionEntry::message(Role::User, "C的消息")).unwrap();
        c.id().to_string()
    };

    let (_s, messages) = Session::recover(dir_path, &id_c).unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0].content, "A的消息");
    assert_eq!(messages[1].content, "B的消息");
    assert_eq!(messages[2].content, "C的消息");
}

/// 断链降级：branch 后删除父文件 → recover 不 panic，只返回自己的消息
#[test]
fn test_branch_broken_parent() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_str().unwrap();

    let parent_id = {
        let mut parent = Session::create(dir_path).unwrap();
        write_three(&mut parent);
        parent.id().to_string()
    };
    let child_id = {
        let (mut child, _) = Session::branch(dir_path, &parent_id, None).unwrap();
        child
            .append(&SessionEntry::message(Role::User, "孤儿消息"))
            .unwrap();
        child.id().to_string()
    };

    std::fs::remove_file(dir.path().join(format!("{parent_id}.jsonl"))).unwrap();

    let (_s, messages) = Session::recover(dir_path, &child_id).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, "孤儿消息");
}

/// Branch entry 序列化往返
#[test]
fn test_entry_branch_serde() {
    let entry = SessionEntry::branch_marker("parent-123", 6);
    let line = serde_json::to_string(&entry).unwrap();
    assert!(line.contains(r#""type":"branch""#));
    assert!(line.contains(r#""parent_session":"parent-123""#));
    let back: SessionEntry = serde_json::from_str(&line).unwrap();
    match back {
        SessionEntry::Branch {
            parent_session,
            parent_upto,
            ..
        } => {
            assert_eq!(parent_session, "parent-123");
            assert_eq!(parent_upto, 6);
        }
        _ => panic!("应为 branch 记录"),
    }
}
