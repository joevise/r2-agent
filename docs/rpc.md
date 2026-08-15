# R2 JSON-RPC 协议（`r2 serve`）

`r2 serve` 启动一个长驻进程，在 stdin/stdout 上说 JSON-RPC 2.0，供任何语言
（Node/Python/Go…）以子进程方式嵌入 R2 Agent。

## 传输

- **帧格式**：行分隔 JSON（JSONL），每行一个完整的请求/响应/通知，UTF-8，`\n` 结尾
- **stdin**：宿主 → r2 的请求
- **stdout**：r2 → 宿主的响应与通知（协议唯一出口，绝不混入日志）
- **stderr**：日志/警告，不进协议
- **EOF**：stdin 关闭后，r2 等当前在途 prompt 结束后以 0 退出

## 请求（宿主 → r2）

```json
{"jsonrpc":"2.0","id":1,"method":"prompt","params":{"input":"帮我看看这个项目"}}
```

`id` 目前仅支持非负整数。

### 方法表

| 方法 | 参数 | 结果 | 说明 |
|---|---|---|---|
| `initialize` | `{config_path?: string}` | `{session_id, version}` | 可选；不调用则首个 `prompt` 前自动用默认配置初始化 |
| `prompt` | `{input: string}` | `{final_text}` | 运行期间事件以通知推送；同一时刻只允许一个在途 prompt |
| `steer` | `{instruction: string}` | `{}` | 运行中随时可调（在途期间唯一不被拦的方法） |
| `reset` | `{}` | `{session_id}` | 清空上下文开新会话（等价 CLI `/clear`） |
| `branch` | `{parent_id: string, upto?: number}` | `{session_id, inherited_count}` | 从父会话分叉 |
| `resume` | `{session_id: string}` | `{session_id, message_count}` | 恢复历史会话 |
| `shutdown` | `{}` | `{}` | 响应后进程以 0 退出 |

**并发规则**：prompt 在途期间，除 `steer` 外的任何请求立即返回错误
`-32002 "prompt in flight"`。

## 响应（r2 → 宿主）

成功：

```json
{"jsonrpc":"2.0","id":1,"result":{"final_text":"已查看，这个项目是……"}}
```

错误：

```json
{"jsonrpc":"2.0","id":1,"error":{"code":-32001,"message":"模型请求失败（第 1 轮）：……"}}
```

### 错误码

| 码 | 含义 |
|---|---|
| `-32700` | 解析错误（垃圾 JSON，响应用 `"id":null`） |
| `-32600` | 无效请求（缺 method、id 非数字等） |
| `-32601` | 未知方法 |
| `-32602` | 参数无效 |
| `-32001` | 会话层错误（初始化/恢复/运行失败） |
| `-32002` | prompt in flight |

## 通知（r2 → 宿主，无 id）

prompt 运行期间的事件流：

```json
{"jsonrpc":"2.0","method":"event","params":{"type":"message_update","data":{"text":"……"}}}
```

| `params.type` | `data` | 时机 |
|---|---|---|
| `agent_start` | `{}` | 一轮开始 |
| `message_update` | `{text}` | 模型流式输出增量 |
| `tool_call` | `{name, arguments}` | 工具调用开始 |
| `tool_result` | `{name, output}` | 工具执行完成 |
| `steered` | `{instruction}` | 中途转向指令已生效 |
| `done` | `{final_text}` | 一轮结束（先于 prompt 响应到达） |
| `error` | `{message}` | 出错 |

## Node.js 宿主示例

```js
const { spawn } = require("node:child_process");
const readline = require("node:readline");

const r2 = spawn("r2", ["serve", "--config", "/path/to/config.toml"]);
const rl = readline.createInterface({ input: r2.stdout });

let seq = 0;
const pending = new Map(); // id → {resolve, reject}

rl.on("line", (line) => {
  const msg = JSON.parse(line);
  if (msg.id === undefined) {
    // 通知：事件流
    if (msg.params.type === "message_update") process.stdout.write(msg.params.data.text);
    return;
  }
  const p = pending.get(msg.id);
  if (!p) return;
  pending.delete(msg.id);
  msg.error ? p.reject(new Error(msg.error.message)) : p.resolve(msg.result);
});

function call(method, params = {}) {
  const id = ++seq;
  r2.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
  return new Promise((resolve, reject) => pending.set(id, { resolve, reject }));
}

const { session_id } = await call("initialize");
const { final_text } = await call("prompt", { input: "用一句话介绍这个项目" });
await call("shutdown");
```

## 手工冒烟（curl 式会话）

```console
$ printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize"}' \
  '{"jsonrpc":"2.0","id":2,"method":"shutdown"}' | r2 serve
{"jsonrpc":"2.0","id":1,"result":{"session_id":"a1b2…","version":"0.1.0"}}
{"jsonrpc":"2.0","id":2,"result":{}}
```

配置缺失/损坏时返回友好错误响应（`-32001`），不会 panic。
