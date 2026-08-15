#!/usr/bin/env python3
"""确定性 mock MCP server（stdio，行分隔 JSON-RPC 2.0）——仅供 r2 测试使用。

用法：
  mock_mcp_server.py           正常模式：提供 echo / fail 两个假工具
  mock_mcp_server.py garbage   坏响应模式：对任何输入输出非 JSON 垃圾行
"""
import json
import sys

GARBAGE = len(sys.argv) > 1 and sys.argv[1] == "garbage"

TOOLS = [
    {
        "name": "echo",
        "description": "回显参数",
        "inputSchema": {
            "type": "object",
            "properties": {"text": {"type": "string"}},
        },
    },
    {
        "name": "fail",
        "description": "总是返回 isError",
        "inputSchema": {"type": "object", "properties": {}},
    },
]


def reply(id_, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": id_, "result": result}) + "\n")
    sys.stdout.flush()


def handle(msg):
    method = msg.get("method")
    id_ = msg.get("id")
    if method == "initialize":
        reply(id_, {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "mock", "version": "0.1.0"},
        })
    elif method == "notifications/initialized":
        pass  # 通知无响应
    elif method == "tools/list":
        reply(id_, {"tools": TOOLS})
    elif method == "tools/call":
        params = msg.get("params", {})
        name = params.get("name")
        args = params.get("arguments", {})
        if name == "echo":
            reply(id_, {
                "content": [{"type": "text", "text": "echo: " + json.dumps(args, ensure_ascii=False)}],
                "isError": False,
            })
        else:
            # fail 及未知工具：isError
            reply(id_, {
                "content": [{"type": "text", "text": "模拟失败"}],
                "isError": True,
            })


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        if GARBAGE:
            sys.stdout.write("这不是 JSON 垃圾行\n")
            sys.stdout.flush()
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        handle(msg)


if __name__ == "__main__":
    main()
