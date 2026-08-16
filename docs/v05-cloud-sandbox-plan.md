# R2 v0.5 云沙箱计划 — 开发文档

> 定位：让 R2 具备云服务内核的安全资格——「每会话沙箱」双层隔离架构
> 日期：2026-08-16 | 前置：v0.4.1（cgroup pids + 越界拦截已就绪）
> 决策背景：目标场景是公网多人使用的通用智能体服务（对标 Manus/E2B 模式）

---

## 一、问题定义

### 威胁模型（公网多人场景）

| # | 威胁 | 现有防护 | 缺口 |
|---|------|---------|------|
| T1 | 提示注入驱动 bash（读密钥/反弹 shell/数据外传） | 启发式拦截（可绕过） | ❌ 结构性缺口 |
| T2 | 用户间横向越权（A 会话读到 B 数据） | 无 | ❌ 云服务致命 |
| T3 | 恶意代码持久化（写 shell 配置/计划任务） | 无 | ❌ |
| T4 | 内核漏洞逃逸 | 无 | 规模化阶段处理（Firecracker） |
| T5 | fork 炸弹/资源耗尽 | ✅ v0.4.1 cgroup | 已解决 |
| T6 | 误操作（删错文件） | ✅ 启发式+路径校验 | 已解决 |

### 目标架构：双层隔离

```
┌─────────────────────────────────────────────────────┐
│  外层：每会话沙箱实例（主防线——边界在实例级）           │
│  ┌─────────────────────────────────────────────┐    │
│  │  Docker 容器（v0.5）→ Firecracker 微虚机（v0.6+） │    │
│  │  ┌─────────────────────────────────────┐    │    │
│  │  │  内层：r2 进程内 namespace（纵深兜底）  │    │    │
│  │  │  mount ns：假根目录（只有 work_dir）    │    │    │
│  │  │  pid ns：只见自己                     │    │    │
│  │  │  net ns：断网或白名单                  │    │    │
│  │  │  + cgroup pids（已有）+ rlimits（已有） │    │    │
│  │  └─────────────────────────────────────┘    │    │
│  │  沙箱内：r2 serve（2MB）+ JSONL 会话 + 临时文件  │    │
│  └─────────────────────────────────────────────┘    │
│  用完即毁：会话结束 → 容器销毁 → 一切痕迹消失           │
└─────────────────────────────────────────────────────┘
```

**为什么双层**：外层是行业验证的主防线（Manus/E2B 同款思路，防 T1/T2/T3）；内层 namespace 是 r2 自带的兜底——即使编排层配置失误，单跑的 r2 也有结构性隔离。R2 的 2MB 单二进制正是"沙箱即用即弃"模式的理想 Runtime。

---

## 二、模块拆分

### 模块 A：r2 进程内 namespace（内层，~600行）

**A1. mount namespace + 最小根目录**
- `unshare(CLONE_NEWNS)` + pivot_root 到 `work_dir/.sandbox-root/`
- 最小根目录内容（build 时准备模板，运行时 bind-mount）：
  - `/bin/busybox`（静态链接，或 bash + 依赖库拷贝）
  - `/dev/{null,zero,random,urandom,fifo}`（tmpfs + mknod）
  - `/tmp`（tmpfs，大小限 64MB）
  - `/proc`（挂载，只读选项评估）
  - work_dir 整体 bind 到 `/work`
- fallback：无 root 权限时（非特权场景）→ 降级模式：只做 chroot 到最小根（需要 root）也不行时 → 保持现状 + warn（namespace 是 strict 档位的能力）

**A2. pid namespace**
- `unshare(CLONE_NEWPID)` + fork（首个子进程成为新 ns 的 init）
- bash 在新 ns 内 PID=1 视角，看不见宿主进程

**A3. net namespace**
- `unshare(CLONE_NEWNET)` → 默认只有 lo（断网）
- 可选 veth 对接宿主（白名单模式，v0.5 只做断网 + 配置项）

**A4. 集成到沙箱分级**
```
level = "off"        → 无（现状）
level = "container"  → rlimits + env清洗 + cgroup（现状）
level = "strict"     → 以上全部 + mount/pid/net namespace（升级点）
```
- strict 需要 root 或 user namespace 支持；无权限自动降级 container + warn（现有降级链延长）

**A5. 测试**
- 单元：ns 可用性检测（/proc/self/ns 类型对比）、配置解析
- 集成（root 环境，CI 跳过标记）：strict 下 `ls /` 只见最小根、`ps` 只见自己、`curl` 不通（DNS/路由不存在）、work_dir 读写正常、逃逸尝试（`/proc/1/root` 访问）被 ns 阻断
- 降级路径：伪造无权限环境验证 warn + 行为回退

### 模块 B：每会话沙箱编排（外层，~700行）

**B1. `r2 sandbox` 命令（编排器原型）**
```
r2 sandbox run [--image r2-runtime:latest] [--memory 512m] [--cpus 1] [--net off]
  - 起 Docker 容器（最小镜像：busybox + r2 二进制 + ca-certificates）
  - 容器内执行 r2 serve --host 0.0.0.0 --port 7443
  - 宿主编排器通过 docker exec 或端口映射建立 JSON-RPC 通道
  - 会话结束（EOF/shutdown）→ 容器强制销毁（docker rm -f）
```

**B2. Dockerfile.runtime（沙箱镜像，~30行）**
```dockerfile
FROM busybox:stable
COPY r2 /usr/local/bin/r2
ENTRYPOINT ["r2", "serve", "--host", "0.0.0.0", "--port", "7443"]
```
- 镜像目标 <20MB（busybox 5MB + r2 2.6MB + 证书）

**B3. 会话生命周期**
- create（起容器）→ attach（JSON-RPC 桥）→ teardown（销毁 + 会话文件留宿主卷）
- work_dir 用 docker volume 映射（会话产物持久化，容器本身无状态）
- 资源限额：`--memory --cpus --pids-limit`（Docker 原生，外层兜底）

**B4. 测试**
- 编排器单元：参数拼装、容器名/卷名生成
- 集成（需 Docker）：完整生命周期（起容器→prompt 往返→销毁→卷内文件存在）、异常路径（容器崩溃→编排器清理）、并发 2 会话互不可见（T2 验证）

### 模块 C：文档与安全声明
- README 沙箱章节升级为完整威胁模型表
- SECURITY.md 新增：部署检查清单（公网部署必读——host 绑定、沙箱档位、密钥隔离）
- docs/sandbox.md：双层架构图 + 各层职责 + 已知边界（内核逃逸不在 v0.5 范围）

---

## 三、任务顺序与依赖

```
A1 mount ns ──► A2 pid ──► A3 net ──► A4 分级集成 ──► A5 测试
                                                      │
B1 编排器 ◄── B2 镜像 ── B3 生命周期 ──► B4 测试 ◄────┘（strict 档在容器内验证）
```

| 任务 | 预估 | 依赖 |
|------|------|------|
| A1 mount ns + 最小根 | 1天 | 无 |
| A2+A3 pid+net ns | 0.5天 | A1 |
| A4+A5 分级集成+测试 | 0.5天 | A2A3 |
| B2 镜像 | 0.5天 | 无（可并行） |
| B1+B3 编排器 | 1天 | B2 |
| B4+C 测试+文档 | 0.5天 | 全部 |
| **合计** | **~3.5天** | |

## 四、验收标准（安全验收，逐项打勾）

### 模块 A（进程内 namespace）
- [ ] strict 档 `ls /` 只见 `/bin /dev /proc /tmp /work`
- [ ] `cat ~/.ssh/*` → 文件不存在（不是权限拒绝，是不存在）
- [ ] `ps aux` 只见 bash 自身
- [ ] `curl/wget/nc` 全部不通（无路由）
- [ ] `/proc/1/root` 逃逸尝试失败
- [ ] work_dir 读写完整正常（read/write/edit/bash 工具全过）
- [ ] 无 root 权限环境自动降级 + warn（不失败）
- [ ] 155+ 既有测试不回归

### 模块 B（沙箱编排）
- [ ] `r2 sandbox run` 一条命令起完整会话（容器+serve+桥接）
- [ ] prompt → GLM 真实回复 → 工具调用 → 文件写入 volume 全链路
- [ ] 会话结束容器销毁（docker ps 无残留）
- [ ] volume 内会话文件/作品保留（容器毁数据不毁）
- [ ] 并发 2 个沙箱会话：A 读不到 B 的任何文件（横向越权验证）
- [ ] 容器 OOM/崩溃 → 编排器正确清理
- [ ] 镜像 <20MB

### 整体
- [ ] 公网部署检查清单（SECURITY.md）覆盖 host 绑定/沙箱档位/密钥环境变量隔离
- [ ] cargo test 全绿（新增 ≥15）

## 五、明确不做（v0.5 边界）

- Firecracker/gVisor 微虚机（v0.6+，规模化阶段）
- 网络白名单细粒度控制（v0.5 net ns 只有断网模式）
- 多租户配额/计费（平台层职责）
- Windows/macOS 的 namespace 等价物（Linux only，文档声明）
- seccomp 白名单扩展（保留现有 feature，不在本轮扩充）

## 六、风险与对策

| 风险 | 对策 |
|------|------|
| mount ns 在无 root 环境不可用 | 已有降级链；文档明确 strict 档推荐部署形态（容器内跑 r2 天然有 root） |
| 最小根目录缺库导致命令失败 | busybox 静态链接优先；缺什么补什么的模板机制；错误信息明确提示 |
| Docker 编排在无 Docker 环境 | `r2 sandbox` 检测 docker 不存在 → 明确报错（不降级到裸跑——安全功能不许静默失效） |
| pivot_root 复杂度 | 先 chroot 实现（简单），pivot_root 作为增强（更彻底），分两步走 |
| 逃逸手法迭代 | SECURITY.md 声明威胁模型边界；内核逃逸明确不在承诺范围 |

---

## 七、版本计划

- 开发周期：~3.5 天（可拆两段：模块 A 一段，模块 B+C 一段）
- 发布：**v0.5.0**（namespace + 沙箱编排）
- 后续：v0.6 = Firecracker 评估 + 网络白名单 + 云调度原型（多沙箱池）

*R2 Agent · 云沙箱计划 v1.0 · 2026-08-16*
