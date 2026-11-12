#!/usr/bin/env bash
# 跨机剪贴板验收：Windows 控制端 ↔ 麒麟被控端，**两个方向都真跑一遍**。
#
# 为什么单独一个脚本：`verify-clipboard-e2e.sh` 验的是「两端都在本机 Windows」的回环，
# 它只能证明协议通路；跨机那一段要看的是「本机的真实剪贴板 ↔ 真麒麟的真实剪贴板」。
# 2026-10-31 那一轮就是在这里卡住的 —— 而且当时的判读是错的（详见
# docs/milestones/剪贴板跨机-环境判定-2026-10-31.txt）。
#
# **必须在交互式桌面会话里跑**：无人值守/受限的会话拿不到 Windows 剪贴板
# （连 clip.exe 都报「拒绝访问」），脚本会在第 0 步就判定环境不行并退出 2，
# 而不是把「读不到」当成「没同步」。
#
# 用法：
#   bash scripts/verify-clipboard-cross.sh --device <设备ID> --password-file <0600 文件>
#   可选：--server 192.168.223.15:21116   --ssh kylin   --client D:/uulan-bin/uulan-client.exe
#         --seconds 18（每条会话跑多久）
# 退出码：
#   0 = 两个方向都通过
#   1 = 有断言失败（真缺陷）
#   2 = 环境问题（本会话拿不到剪贴板 / 缺参数 / 连不上真机 / 缺探针）
#
# 判据（机器可查）：
#   A 向 Windows→麒麟：麒麟上 x11_clip_probe 的 `GET-BEFORE <<<…>>>` 里出现本轮标记；
#                       控制端统计「发送 ≥ 1」。
#   B 向 麒麟→Windows：本机剪贴板读回来等于本轮标记；控制端统计「接收 ≥ 1」。
# 两道保护：跑完把原来的 Windows 剪贴板内容写回去；标记只用 ASCII + 中文，不碰别的格式。
set -u

SERVER="${UULAN_SERVER:-192.168.223.15:21116}"
SSH_HOST="${UULAN_SSH:-kylin}"
CLIENT="${UULAN_CLIENT:-D:/uulan-bin/uulan-client.exe}"
DEVICE="${UULAN_DEVICE:-}"
PWFILE="${UULAN_PASSWORD_FILE:-}"
SECS="${UULAN_CLIP_SECONDS:-18}"
WORK="${UULAN_STAGE:-D:/uulan-bin/clip-cross-$(date +%Y%m%d-%H%M%S)}"

while [ $# -gt 0 ]; do
  case "$1" in
    --server)        SERVER="$2"; shift 2 ;;
    --ssh)           SSH_HOST="$2"; shift 2 ;;
    --client)        CLIENT="$2"; shift 2 ;;
    --device)        DEVICE="$2"; shift 2 ;;
    --password-file) PWFILE="$2"; shift 2 ;;
    --seconds)       SECS="$2"; shift 2 ;;
    *) echo "未知参数：$1"; exit 2 ;;
  esac
done

PASS=0; FAIL=0
say() { printf '%s\n' "$*"; }
ok()  { PASS=$((PASS + 1)); say "  [ OK ] $*"; }
bad() { FAIL=$((FAIL + 1)); say "  [FAIL] $*"; }
env_bad() { say "  [FAIL·环境] $*"; exit 2; }

mkdir -p "$WORK" || env_bad "建不了工作目录 $WORK"
say "== 跨机剪贴板验收（server=$SERVER ssh=$SSH_HOST client=$CLIENT）=="

# ── 0. 环境：本会话到底能不能碰 Windows 剪贴板 ──────────────────────────
# 这一步是整件事的关键：能写能读，后面的断言才有意义。
get_clip() {
    # 必须显式把 PowerShell 的输出编码设成 UTF-8：默认的 OEM 代码页会把非 ASCII
    # 直接吐成 （U+FFFD），于是「麒麟送来的中文标记」永远比对失败 ——
    # 那是**判据坏了**，不是剪贴板不通。2026-11-12 就是被这条误判过一次。
    powershell.exe -NoProfile -Command '[Console]::OutputEncoding=[Text.Encoding]::UTF8; Get-Clipboard -Raw' 2>/dev/null | tr -d '\r'
}
set_clip() { powershell.exe -NoProfile -Command "Set-Clipboard -Value '$1'" >/dev/null 2>&1; }

PROBE_TAG="uulan-env-$$"
set_clip "$PROBE_TAG"
if [ "$(get_clip | tr -d '[:space:]')" != "$PROBE_TAG" ]; then
  say "  本机剪贴板读回的是：'$(get_clip)'"
  env_bad "这个会话拿不到 Windows 剪贴板（Set-Clipboard/Get-Clipboard 都不生效）。
         无人值守会话就是这样（连 clip.exe 都报「拒绝访问」）——请到交互式桌面会话里重跑本脚本，
         不要据此判定「跨机剪贴板不通」。"
fi
ok "本会话能读写 Windows 剪贴板"
get_clip > "$WORK/prev-clip.txt"

cleanup() {
  if [ -s "$WORK/prev-clip.txt" ]; then
    set_clip "$(cat "$WORK/prev-clip.txt")" || true
  fi
}
trap cleanup EXIT

[ -n "$DEVICE" ] || env_bad "缺 --device（设备 ID）"
[ -n "$PWFILE" ] || env_bad "缺 --password-file（口令文件；不要把口令写在命令行上）"
[ -x "$CLIENT" ] || env_bad "找不到控制端可执行：$CLIENT（先把 target/release/uulan-client.exe 拷到工作区外）"

kx() { ssh "$SSH_HOST" "$@" 2>/dev/null; }
kx "true" || env_bad "ssh $SSH_HOST 连不上（先确认本机在 192.168.223.0/24）"

PROBE="\$HOME/.local/uulan/src/target/release/examples/x11_clip_probe"
if ! kx "test -x $PROBE"; then
  say "  真机上没有剪贴板探针，先编一个…"
  kx "export PATH=\$HOME/.cargo/bin:\$PATH; cd ~/.local/uulan/src && cargo build --release --example x11_clip_probe -p uulan-clip" >/dev/null \
    || env_bad "探针编译失败"
  kx "test -x $PROBE" || env_bad "探针没编出来"
fi
ok "真机探针就位"

run_client() {  # $1 = 日志路径
  "$CLIENT" --server "$SERVER" --device "$DEVICE" --password-file "$PWFILE" \
    --headless --seconds "$SECS" > "$1" 2>&1
}
stat_of() {  # $1 = 日志；输出「发送 N / 接收 M」
  grep -oE "发送 [0-9]+ / 接收 [0-9]+" "$1" | tail -1
}

# ── A 向：Windows → 麒麟 ────────────────────────────────────────────────
# 基线是会话开头读的（连接之前就设好的值不会外推），所以标记必须在连上之后才写。
MARK_A="uulan-A-$(date +%H%M%S)-中文标记"
say "A 向（Windows → 麒麟）标记：$MARK_A"
run_client "$WORK/client-a.log" &
CLIENT_PID=$!
sleep 7
set_clip "$MARK_A"
sleep 4
kx "export DISPLAY=:0; timeout 20 $PROBE 'probe-a-$$' 1" > "$WORK/probe-a.txt" 2>&1 || true
wait "$CLIENT_PID" 2>/dev/null || true

GET_A=$(grep -oE 'GET-BEFORE .*' "$WORK/probe-a.txt" | head -1)
case "$GET_A" in
  *"$MARK_A"*) ok "麒麟端读到了本轮标记（$GET_A）" ;;
  *)           bad "麒麟端没读到标记；探针输出：${GET_A:-<空>}（日志 $WORK/probe-a.txt）" ;;
esac
SENT_A=$(stat_of "$WORK/client-a.log" | sed -E 's/发送 ([0-9]+) .*/\1/')
if [ -n "${SENT_A:-}" ] && [ "$SENT_A" -ge 1 ]; then
  ok "控制端统计：发送 $SENT_A"
else
  bad "控制端统计没显示发送 ≥ 1（$(stat_of "$WORK/client-a.log")）；整份日志 $WORK/client-a.log"
fi

# ── B 向：麒麟 → Windows ──────────────────────────────────────────────
MARK_B="uulan-B-$(date +%H%M%S)-反向标记"
say "B 向（麒麟 → Windows）标记：$MARK_B"
run_client "$WORK/client-b.log" &
CLIENT_PID=$!
sleep 7
# 会话中途让探针 SET 并持有：host 的 500ms 轮询才能看到「真的变了」。
kx "export DISPLAY=:0; timeout 25 $PROBE '$MARK_B' 20" > "$WORK/probe-b.txt" 2>&1 &
PROBE_SSH_PID=$!
sleep 6
GOT_B=$(get_clip | tr -d '[:space:]')
wait "$CLIENT_PID" 2>/dev/null || true
wait "$PROBE_SSH_PID" 2>/dev/null || true

if [ "$GOT_B" = "$(printf '%s' "$MARK_B" | tr -d '[:space:]')" ]; then
  ok "Windows 端剪贴板被改写成了本轮标记"
else
  bad "Windows 端剪贴板是「$GOT_B」，不是标记「$MARK_B」"
fi
RECV_B=$(stat_of "$WORK/client-b.log" | sed -E 's/.*接收 ([0-9]+).*/\1/')
if [ -n "${RECV_B:-}" ] && [ "$RECV_B" -ge 1 ]; then
  ok "控制端统计：接收 $RECV_B"
else
  bad "控制端统计没显示接收 ≥ 1（$(stat_of "$WORK/client-b.log")）；整份日志 $WORK/client-b.log"
fi

say "== 结果：$PASS 通过 / $FAIL 失败（工作目录 $WORK）=="
[ "$FAIL" -eq 0 ] && exit 0 || exit 1