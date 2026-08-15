#!/usr/bin/env python3
"""R2 Agent L3 跨会话记忆 — E2E 能力边界测试套件

8 个场景，每个场景独立记忆库（session dir 隔离），真实 LLM 调用。
评估标准：回答包含期望关键词 +（可选）唤起日志出现。
"""
import subprocess, re, json, sys, time, os, tempfile, shutil

R2 = "/home/elttilz/joe_ai_projects/local_claws/joevise_projects/r2-agent/target/release/r2"
KEY = None

def get_key():
    global KEY
    if KEY is None:
        with open(os.path.expanduser("~/.r2/config.toml")) as f:
            KEY = re.search(r'api_key = "(.+)"', f.read()).group(1)
    return KEY

def make_config(session_dir, work_dir):
    """每个场景独立 session dir → 独立 memory.db（key 从 ~/.r2/config.toml 读取，不硬编码）"""
    return f"""[model]
provider = "openai_compat"
[model.openai_compat]
base_url = "https://open.bigmodel.cn/api/coding/paas/v4"
api_key = "{get_key()}"
model = "glm-5.2"

[agent]
work_dir = "{work_dir}"

[context]
l3_enabled = true

[session]
dir = "{session_dir}"
"""

def run_once(session_dir, prompt, timeout=120):
    cfg = make_config(session_dir, session_dir + "-work")
    fd, cfgpath = tempfile.mkstemp(suffix=".toml"); os.write(fd, cfg.encode()); os.close(fd)
    try:
        r = subprocess.run([R2, "--config", cfgpath, "--once", prompt],
                           capture_output=True, text=True, timeout=timeout)
        return r.stdout + r.stderr
    finally:
        os.unlink(cfgpath)

RESULTS = []
def check(name, desc, out, expect_any=None, expect_all=None, expect_none=None, want_memory=None):
    ok, notes = True, []
    def has(kw): return kw in out
    if expect_all:
        for kw in expect_all:
            if not has(kw): ok = False; notes.append(f"缺'{kw}'")
    if expect_any:
        if not any(has(kw) for kw in expect_any): ok = False; notes.append(f"应含其一{expect_any}")
    if expect_none:
        for kw in expect_none:
            if has(kw): ok = False; notes.append(f"不应含'{kw}'")
    mem = "[memory]" in out
    if want_memory is True and not mem: ok = False; notes.append("未唤起记忆")
    if want_memory is False and mem: ok = False; notes.append("误唤起记忆")
    RESULTS.append((name, desc, "PASS" if ok else "FAIL", "; ".join(notes) or "—", mem))
    print(f"[{'✅' if ok else '❌'}] {name}")
    return ok

def newdir(tag):
    d = f"/tmp/r2-l3-{tag}"
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(d, exist_ok=True)
    return d

print("=" * 70)
print("R2 L3 跨会话记忆 E2E 能力边界测试  (GLM-5.2 真实调用)")
print("=" * 70)

# ---------- T1 基础跨会话召回（回归） ----------
d = newdir("t1")
run_once(d, "我的项目代号是凤凰计划，负责人是大Joe。请确认收到。")
time.sleep(1)
out = run_once(d, "我的项目代号是什么？负责人是谁？")
check("T1 基础召回", "跨会话记住事实", out, expect_all=["凤凰"], want_memory=True)

# ---------- T2 语义变换（换词重述） ----------
d = newdir("t2")
run_once(d, "我养了一只橘猫，名字叫咪咪，今年三岁。")
time.sleep(1)
out = run_once(d, "我的宠物叫什么名字？")
check("T2 语义变换", "'宠物'≠原文'橘猫'，测语义桥接", out, expect_any=["咪咪"], want_memory=None)

# ---------- T3 干扰区分（同类记忆排序） ----------
d = newdir("t3")
run_once(d, "我自己最喜欢的颜色是蓝色。")
time.sleep(1)
run_once(d, "我弟弟最喜欢的颜色是绿色。")
time.sleep(1)
out = run_once(d, "我本人最喜欢的颜色是什么？（注意：问的是我本人，不是我弟弟）")
ok3 = check("T3 干扰区分", "我=蓝色 vs 弟弟=绿色", out, expect_any=["蓝"])
if "绿" in out.split("【跨会话记忆】")[-1] if "【跨会话记忆】" in out else out:
    pass  # 绿色出现在记忆注入区是正常的；只检查回答倾向（上面 expect_any）

# ---------- T4 防误召回（precision） ----------
d = newdir("t4")
run_once(d, "我最喜欢的食物是火锅，最喜欢的数字是42。")
time.sleep(1)
out = run_once(d, "解释一下 Rust 的所有权机制和生命周期。")
check("T4 防误召回", "无关问题不应唤起记忆", out, expect_none=["火锅"], want_memory=False)

# ---------- T5 记忆更新（新旧冲突） ----------
d = newdir("t5")
run_once(d, "我以前住在北京。")
time.sleep(1)
run_once(d, "注意，我已经搬家了，现在住在上海。")
time.sleep(1)
out = run_once(d, "我现在住在哪个城市？")
check("T5 记忆更新", "北京→上海，应以新信息为准", out, expect_any=["上海"], want_memory=True)

# ---------- T6 多记忆联合召回 ----------
d = newdir("t6")
run_once(d, "记住：我最喜欢的颜色是蓝色。")
time.sleep(1)
run_once(d, "记住：我最喜欢的食物是火锅。")
time.sleep(1)
out = run_once(d, "我最喜欢的颜色和食物分别是什么？")
check("T6 多记忆联合", "一次问两个不同记忆点", out, expect_all=["蓝", "火锅"], want_memory=True)

# ---------- T7 长文本埋藏事实 ----------
d = newdir("t7")
run_once(d, "我跟你聊聊我的近况：这几个月在做一个 Rust 项目，遇到了不少借用检查器的坑，不过都解决了。团队扩了两个人，一个负责前端一个负责测试。对了，有个重要的事：我的生日是3月14日。周末打算去爬山放松一下。")
time.sleep(1)
out = run_once(d, "我的生日是哪一天？")
check("T7 长文本埋藏", "100+字闲聊中埋一个关键事实", out, expect_any=["3月14", "三月十四"], want_memory=True)

# ---------- T8 英文记忆中文查询（跨语言） ----------
d = newdir("t8")
run_once(d, "My favorite programming language is Rust, and I use it every day.")
time.sleep(1)
out = run_once(d, "我最喜欢的编程语言是什么？")
check("T8 跨语言", "英文存中文取", out, expect_any=["Rust", "rust"], want_memory=None)

# ---------- 汇总 ----------
print()
print("=" * 70)
n_pass = sum(1 for r in RESULTS if r[2] == "PASS")
print(f"结果汇总: {n_pass}/{len(RESULTS)} PASS")
print("-" * 70)
for name, desc, status, note, mem in RESULTS:
    m = "唤起✓" if mem else "未唤起"
    print(f"{status:4} | {m} | {name} — {desc} | {note}")
print("=" * 70)
sys.exit(0 if n_pass == len(RESULTS) else 1)
