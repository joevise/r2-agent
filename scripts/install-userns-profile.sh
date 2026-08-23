#!/usr/bin/env bash
# install-userns-profile.sh — R2 strict 沙箱的 AppArmor userns 放行 profile 安装器
#
# 背景：Ubuntu 23.10+ 默认 kernel.apparmor_restrict_unprivileged_userns=1，
#   非特权进程 unshare(CLONE_NEWUSER) 后写 uid_map 返回 EPERM——R2 的 strict
#   沙箱（namespace 隔离）因此静默降级 container 档。
#   AppArmor 支持按二进制路径放行（flatpak/steam 同款机制）：给 r2 二进制挂
#   带 `userns,` 规则的 profile 即可解锁，内核原生、零新增依赖、重启自动加载。
#
# 用法：
#   ./scripts/install-userns-profile.sh              # 安装/更新（默认 target/release/r2）
#   ./scripts/install-userns-profile.sh --bin PATH   # 指定 r2 二进制
#   ./scripts/install-userns-profile.sh --test       # 追加测试二进制通配 profile
#   ./scripts/install-userns-profile.sh --remove     # 卸载全部
#
# 验证原理：profile 生效后 r2 内部 can_namespace() 的 fork 探测（unshare+写
#   uid_map）会通过，strict 档自动启用；卸载后探测失败自动降级——双向安全。
set -euo pipefail

PROFILE_NAME="r2-userns"
TEST_PROFILE_NAME="r2-test-userns"
AA_DIR="/etc/apparmor.d"
DEFAULT_BIN="$(cd "$(dirname "$0")/.." && pwd)/target/release/r2"
NEED_SUDO=0
[[ $EUID -ne 0 ]] && NEED_SUDO=1

as_root() {
  # 默认 sudo；支持外部 SUDO 环境变量覆盖（如 "sudo -A" 非交互场景）
  if [[ $NEED_SUDO -eq 1 ]]; then ${SUDO:-sudo} "$@"; else "$@"; fi
}

usage() { grep '^#' "$0" | sed 's/^# \?//'; exit 1; }

BIN_PATH="$DEFAULT_BIN"
DO_TEST=0
DO_REMOVE=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --bin)    BIN_PATH="$2"; shift 2 ;;
    --test)   DO_TEST=1; shift ;;
    --remove) DO_REMOVE=1; shift ;;
    -h|--help) usage ;;
    *) echo "未知参数: $1"; usage ;;
  esac
done

# ---------- 卸载 ----------
if [[ $DO_REMOVE -eq 1 ]]; then
  echo "→ 卸载 profile：$PROFILE_NAME / $TEST_PROFILE_NAME"
  for f in "$PROFILE_NAME" "$TEST_PROFILE_NAME"; do
    if [[ -f "$AA_DIR/$f" ]]; then
      as_root /sbin/apparmor_parser -R "$AA_DIR/$f" 2>/dev/null || true
      as_root rm -f "$AA_DIR/$f"
      echo "  ✓ 已卸载 $f"
    else
      echo "  - $f 不存在，跳过"
    fi
  done
  echo "完成。r2 下次启动探测失败会自动降级 container 档（行为安全）。"
  exit 0
fi

# ---------- 前置检查 ----------
if [[ ! -x "$BIN_PATH" ]]; then
  echo "✗ 二进制不存在：$BIN_PATH（先 cargo build --release，或用 --bin 指定）" >&2
  exit 1
fi
BIN_REAL="$(readlink -f "$BIN_PATH")"

if [[ ! -d "$AA_DIR" ]] || [[ ! -x /sbin/apparmor_parser ]]; then
  echo "✗ 未找到 AppArmor（需 Ubuntu/Debian + apparmor 包）" >&2
  exit 1
fi

# 本机是否真有限制（无限制则无需安装，但装了也无害）
RESTRICT=$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns 2>/dev/null || echo 0)

# ---------- 安装 ----------
write_profile() {
  # 注意：local 的所有参数在赋值前先展开——name 必须先单独声明再引用，否则 set -u 炸
  local name="$1" attach="$2"
  local file="$AA_DIR/$name" tmp="/tmp/$name.tmp"
  cat > "$tmp" <<EOF
# 由 install-userns-profile.sh 生成：放行 r2 的 unprivileged userns
# （strict 沙箱依赖；探测逻辑见 crates/r2-core/src/namespaces.rs）
abi <abi/4.0>,
include <tunables/global>

profile $name $attach flags=(unconfined) {
  userns,

  include if exists <local/$name>
}
EOF
  as_root mv "$tmp" "$file"
  as_root /sbin/apparmor_parser -r "$file"
  echo "  ✓ profile $name → $attach"
}

echo "→ 安装 AppArmor userns 放行 profile（sysctl restrict=$RESTRICT）"
write_profile "$PROFILE_NAME" "$BIN_REAL"

if [[ $DO_TEST -eq 1 ]]; then
  TEST_GLOB="$(dirname "$BIN_REAL")/deps/r2_core-*"
  write_profile "$TEST_PROFILE_NAME" "$TEST_GLOB"
fi

# ---------- 验证（与 r2 内部 fork 探测同一条链） ----------
echo "→ 验证 userns 链路（unshare → uid_map → mount/pid/net ns）"
PROBE_OUT=$(unshare --user --map-root-user --mount --pid --net /bin/sh -c 'echo "uid=$(id -u) pid_ns_ok" ' 2>&1) || true
# 注意：unshare 工具自身也可能被限制——此处探测依赖系统 unshare 是否有 profile。
# 更可靠的验证是 r2 自身探测：直接调 r2 二进制跑一条 strict 沙箱内命令。
if echo "$PROBE_OUT" | grep -q "uid=0"; then
  echo "  ✓ 全链通过：$PROBE_OUT"
else
  echo "  ⚠ 系统 unshare 未放行（正常——它没有 profile）。真正的判据是下面的 r2 探测。"
fi

echo "→ r2 二进制真实探测（内部 fork 探测 + strict 沙箱执行）"
"$BIN_REAL" sandbox run --ephemeral --timeout 120 \
  "请用bash执行: id && echo STRICT_PROBE_DONE" 2>&1 | tail -5 || {
  echo "（若上方报 LLM 网络错误属 API 问题，与 profile 无关；核心判据是启动横幅不再出现降级 WARN）"
}

echo "完成。profile 重启自动加载；r2 的 can_namespace() 探测通过后 strict 档自动启用。"
