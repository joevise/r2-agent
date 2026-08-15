//! RPC 协议层集成测试（不依赖网络）
//!
//! 覆盖：initialize/reset/resume/branch 的真实会话路径（临时目录）、
//! 错误码路由、shutdown。prompt 全流程由 r2-core 内联测试用 MockProvider
//! 覆盖（集成测试无法注入 Mock），真实模型 E2E 另测。

use r2_core::config::Config;
use r2_core::rpc::{RpcOutcome, RpcServer};
use serde_json::{json, Value};

/// 构造配置指向临时目录的 RpcServer（不读真实 ~/.r2）
fn test_server(tmp: &tempfile::TempDir) -> RpcServer {
    let dir = tmp.path().to_string_lossy().to_string();
    RpcServer::new().with_config_loader(move |_| {
        let mut config = Config::default_config();
        config.session.dir = dir.clone();
        Ok(config)
    })
}

fn line_of(outcome: RpcOutcome) -> String {
    match outcome {
        RpcOutcome::Line(l) => l,
        other => panic!("期望 Line，实际：{}", std::any::type_name_of_val(&other)),
    }
}

fn parse(line: &str) -> Value {
    serde_json::from_str(line).expect("输出必须是合法 JSON")
}

#[test]
fn test_initialize_creates_session_in_tempdir() {
    let tmp = tempfile::tempdir().unwrap();
    let mut server = test_server(&tmp);
    let resp = parse(&line_of(
        server.handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#),
    ));
    let session_id = resp["result"]["session_id"].as_str().unwrap().to_string();
    // 会话文件确实落在临时目录里
    assert!(tmp.path().join(format!("{session_id}.jsonl")).exists());
    assert_eq!(resp["result"]["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn test_initialize_with_config_path_param() {
    // 用默认加载器（真实路径检查）；config_path 指向不存在的文件 → 友好错误而非 panic
    let mut server = RpcServer::new();
    let req = json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{"config_path":"/nonexistent/x.toml"}});
    let resp = parse(&line_of(server.handle_line(&req.to_string())));
    assert_eq!(resp["error"]["code"], -32001);
    assert!(resp["error"]["message"]
        .as_str()
        .unwrap()
        .contains("不存在"));
}

#[test]
fn test_reset_returns_new_session_id() {
    let tmp = tempfile::tempdir().unwrap();
    let mut server = test_server(&tmp);
    let first = parse(&line_of(
        server.handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#),
    ))["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let second = parse(&line_of(
        server.handle_line(r#"{"jsonrpc":"2.0","id":2,"method":"reset"}"#),
    ))["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(first, second, "reset 应开新会话");
    assert!(tmp.path().join(format!("{second}.jsonl")).exists());
}

#[test]
fn test_resume_and_branch_missing_session() {
    let tmp = tempfile::tempdir().unwrap();
    let mut server = test_server(&tmp);
    let resp = parse(&line_of(server.handle_line(
        r#"{"jsonrpc":"2.0","id":1,"method":"resume","params":{"session_id":"nope"}}"#,
    )));
    assert_eq!(resp["error"]["code"], -32001);
    let resp = parse(&line_of(server.handle_line(
        r#"{"jsonrpc":"2.0","id":2,"method":"branch","params":{"parent_id":"nope"}}"#,
    )));
    assert_eq!(resp["error"]["code"], -32001);
}

#[test]
fn test_branch_from_existing_session() {
    let tmp = tempfile::tempdir().unwrap();
    let mut server = test_server(&tmp);
    let parent = parse(&line_of(
        server.handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#),
    ))["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let req = json!({"jsonrpc":"2.0","id":2,"method":"branch","params":{"parent_id":parent}});
    let resp = parse(&line_of(server.handle_line(&req.to_string())));
    let child = resp["result"]["session_id"].as_str().unwrap();
    assert_ne!(child, parent, "branch 应产生新会话 id");
    assert_eq!(resp["result"]["inherited_count"], 0); // 父会话还没有消息
    assert!(tmp.path().join(format!("{child}.jsonl")).exists());
}

#[test]
fn test_invalid_params_and_request() {
    let tmp = tempfile::tempdir().unwrap();
    let mut server = test_server(&tmp);
    // prompt 缺 input → -32602
    let resp = parse(&line_of(
        server.handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"prompt"}"#),
    ));
    assert_eq!(resp["error"]["code"], -32602);
    // 字符串 id → -32600
    let resp = parse(&line_of(
        server.handle_line(r#"{"jsonrpc":"2.0","id":"abc","method":"reset"}"#),
    ));
    assert_eq!(resp["error"]["code"], -32600);
    // 无 method → -32600
    let resp = parse(&line_of(server.handle_line(r#"{"jsonrpc":"2.0","id":5}"#)));
    assert_eq!(resp["error"]["code"], -32600);
    // 宿主通知（无 id）→ 静默忽略
    assert!(matches!(
        server.handle_line(r#"{"jsonrpc":"2.0","method":"ping"}"#),
        RpcOutcome::None
    ));
}

#[test]
fn test_shutdown_outcome_carries_response() {
    let mut server = RpcServer::new();
    match server.handle_line(r#"{"jsonrpc":"2.0","id":42,"method":"shutdown"}"#) {
        RpcOutcome::Shutdown(line) => {
            let resp = parse(&line);
            assert_eq!(resp["id"], 42);
            assert_eq!(resp["result"], json!({}));
        }
        _ => panic!("shutdown 应返回 Shutdown"),
    }
}
