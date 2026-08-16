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
│  外层：r2 自孵化 supervisor（主防线——每会话一个隔离子进程）  │
│  ┌─────────────────────────────────────────────┐    │
│  │  子 r2 进程（cgroup 会话组：pids+memory）        │    │
│  │  ┌─────────────────────────────────────┐    │    │
│  │  │  内层：r2 进程内 namespace（纵深兜底）  │    │    │
│  │  │  mount ns：假根目录（只有 work_dir）    │    │    │
│  │  │  pid ns：只见自己                     │    │    │
│  │  │  net ns：断网或白名单                  │    │    │
│  │  │  + cgroup pids（已有）+ rlimits（已有） │    │    │
│  │  └─────────────────────────────────────┘    │    │
│  │  会话内：r2 --once/serve + JSONL + work 文件     │    │
│  └─────────────────────────────────────────────┘    │
│  用完即毁：进程退出 → ns 内核回收 → cgroup systemd 回收  │
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

### 模块 B：r2 自孵化 Supervisor（外层主防线，~400行）【8/16 方向修正】

> 原方案为 Docker 每会话编排。修正原因：Docker 的隔离本体就是 namespace+cgroup+seccomp
> ——这三个内核原语 R2 已全部自实现。Docker 模式=雇 200MB 管家调用我们已会调的内核接口。
> 自孵化方案：每会话开销 ~15-25MB（纯进程）vs Docker ~40-60MB（shim+容器）；
> 启动 <100ms vs 1-2s；零外部依赖 vs Docker daemon 常驻。隔离强度相同（同内核原语）。
> Docker 版降级为附录参考（运维托管/K8s 生态对接时再考虑）。

**B1. `r2 sandbox run` 子命令（supervisor 入口）**
```
r2 sandbox run [--memory MB] [--pids N] [--ephemeral] "prompt"
```
流程：
1. 建会话目录 r2-sessions/sess-{时间戳}-{pid}/
2. 从当前配置生成会话配置（model 段含密钥**整段文件到文件复制**，0600 权限；
   work_dir=会话目录；sandbox.level=**strict**；max_processes=--pids）
3. 建会话 cgroup（pids.max + 可选 memory.max，复用 v0.4.1 层级探测）
4. spawn 子 r2（--once --config 会话配置）：
   - 子进程入会话 cgroup（env R2_CGROUP_JOIN 注入，bash 树统一核算）
   - env_clear：只留 PATH/HOME=会话目录/TERM——宿主环境零泄漏
   - rlimits 继承（CPU/AS/FSIZE）
5. 子 r2 的 bash 走 strict 档（模块A namespace：假根/pid隔离/断网）
6. 会话结束：进程退出→cgroup 空组被 systemd 自动回收→namespace 内核自动销毁
   （--ephemeral 时连会话目录一起删）

**B2. cgroup 会话级核算**
- attach_child_to_cgroup 扩展：检测 R2_CGROUP_JOIN 环境变量→直接入会话组
  （会话组 pids.max/memory.max 对**整个子树**生效——含 bash 后代，层级计数是内核语义）
- 独立 r2（无 supervisor）行为不变：自建 r2-agent-{pid} 组

**B3. 安全性质（自孵化 vs Docker 等价性声明）**
- mount/pid/net ns：同内核原语，同强度 ✓
- 资源限额：cgroup pids/memory + rlimits，同强度 ✓
- 镜像/卷泄漏点：自孵化**更少**（无镜像层/orphan 容器/shim 残留）
- 内核逃逸：两者同样共享宿主内核，同样不防（诚实边界，Firecracker 是 v0.6+）
- 密钥边界：API 密钥经配置文件进子进程；bash 断网（net ns）→ 密钥无法经 bash 外传

**B4. 测试**
- 单元：会话配置生成（strict 注入/密钥段复制/0600）、目录命名
- 本机 E2E（降级链）：GLM 真实调用跑通 sandbox run（本机 AppArmor→自动降级 container）
- **Docker 内 root E2E（全链路）**：ubuntu 容器跑 r2 sandbox run，
  验证 ls / 只见最小根、宿主文件不可见、网络断——模块A+模块B 内外层闭环
- 并发两会话互不可见（各自 mount ns + 独立会话目录）

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
| B2 cgroup 会话核算 | 0.5天 | A |
| B1 sandbox run | 1天 | B2 |
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

### 模块 B（自孵化 supervisor）
- [ ] `r2 sandbox run` 一条命令完整会话（spawn+隔离+输出+回收）
- [ ] GLM 真实回复 → bash 工具 → 文件写入会话目录全链路
- [ ] 子进程 env 只有 PATH/HOME/TERM（env 零泄漏验证）
- [ ] 会话 cgroup 创建且 bash 树入组（pids/memory 会话级核算）
- [ ] Docker 容器内（root）：模块A namespace 全链路生效（ls / 最小根/断网/宿主不可见）
- [ ] 并发 2 个 sandbox 会话互不可见
- [ ] --ephemeral 会话目录清理
- [ ] 159+ 既有测试不回归

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
