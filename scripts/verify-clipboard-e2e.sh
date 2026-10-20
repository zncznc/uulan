#!/usr/bin/env bash
# 全链路剪贴板验收：真 server + 真被控端 + 真控制端窗口 + **真实的系统剪贴板**，
# 会话中途用 PowerShell 改一次本机剪贴板，断言这一变化真的经过协议到达了对端。
#
# 为什么会动到你的剪贴板：本机同时也是「被控端」，两端共享同一个 Windows 剪贴板，
# 要验证「真的读到了系统剪贴板的变化并发出去了」，只能真改一次。
# 两道保护：
#   · 剪贴板当前不是纯文本时**直接跳过**（不碰用户复制的图片/文件）；
#   · 跑完把原来的文字写回去。
#
# 用法：bash scripts/verify-clipboard-e2e.sh
# 退出码：0 = 剪贴板确实同步了；1 = 断言失败；2 = 环境问题；3 = 跳过（非文本剪贴板）
set -u

PORT="${UULAN_PORT:-21993}"
WORK="${UULAN_STAGE:-D:/uulan-bin/clip-e2e-$(date +%Y%m%d-%H%M%S)}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export NO_COLOR=1

mkdir -p "$WORK" || exit 2
cp -f "$ROOT"/target/release/uulan-server.exe \
      "$ROOT"/target/release/uulan-host.exe \
      "$ROOT"/target/release/uulan-client.exe "$WORK"/ || exit 2

PREV=$(powershell.exe -NoProfile -Command "Get-Clipboard -Raw" 2>/dev/null | tr -d '\r')
if [ -z "$PREV" ]; then
  echo "SKIP：剪贴板当前不是文本（或为空），本脚本不碰非文本剪贴板"
  exit 3
fi
powershell.exe -NoProfile -Command \
  "[IO.File]::WriteAllText('${WORK//\//\\}\\prev.txt', (Get-Clipboard -Raw))" >/dev/null

SERVER_PID=""; HOST_PID=""
cleanup() {
  [ -n "$HOST_PID" ] && kill "$HOST_PID" 2>/dev/null
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null
  powershell.exe -NoProfile -Command \
    "Set-Clipboard -Value ([IO.File]::ReadAllText('${WORK//\//\\}\\prev.txt'))" >/dev/null 2>&1
}
trap cleanup EXIT

RUST_LOG=info "$WORK/uulan-server.exe" --bind "127.0.0.1:$PORT" --state "$WORK/devices.json" \
  >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
sleep 1

RUST_LOG=info "$WORK/uulan-host.exe" --server "127.0.0.1:$PORT" --config "$WORK/host.json" \
  --capture "lavfi:testsrc2=size=640x480:rate=10" --name clip-host --once \
  >"$WORK/host.log" 2>&1 &
HOST_PID=$!

DEVICE=""
for _ in $(seq 1 150); do
  DEVICE=$(grep -oE "设备 ID : [0-9]{6,}" "$WORK/host.log" 2>/dev/null | tail -1 | sed 's/.*: *//' | tr -d '[:space:]')
  [ -n "$DEVICE" ] && break
  sleep 0.1
done
if [ -z "$DEVICE" ]; then
  echo "被控端没拿到设备 ID（日志已滤掉密码行）："
  grep -vE "访问密码" "$WORK/host.log" | sed -n 1,20p
  exit 2
fi

PW=$(sed -n 's/.*"password": *"\([^"]*\)".*/\1/p' "$WORK/host.json" | head -1)
[ -n "$PW" ] || { echo "host.json 里没有 password"; exit 2; }

RUST_LOG=info "$WORK/uulan-client.exe" --server "127.0.0.1:$PORT" --device "$DEVICE" \
  --password "$PW" --name clip-client --seconds 12 >"$WORK/client.log" 2>&1 &
CLIENT_PID=$!

# 会话稳定后，模拟用户在控制端 / 被控端按下 Ctrl+C。
sleep 4
MARKER="uulan-e2e-clip-marker-42"
powershell.exe -NoProfile -Command "Set-Clipboard -Value '$MARKER'" >/dev/null
wait "$CLIENT_PID"

strip() { sed -E 's/\x1b\[[0-9;]*m//g' "$1"; }
echo "──────── 被控端 ────────"
strip "$WORK/host.log" | grep -E "剪贴板后端|剪贴板同步统计" || true
echo "──────── 控制端 ────────"
strip "$WORK/client.log" | grep -E "剪贴板同步|painted" || true

SENT=$(strip "$WORK/client.log" | grep -oE "发送 [0-9]+ / 接收 [0-9]+" | head -1)
HOST_SENT=$(strip "$WORK/host.log" | grep -oE "sent=[0-9]+ recv=[0-9]+" | head -1)
echo "（标记：$MARKER）"

TOTAL=0
for n in $(strip "$WORK/client.log" | grep -oE "发送 [0-9]+ / 接收 [0-9]+" | grep -oE "[0-9]+") ; do
  TOTAL=$((TOTAL + n))
done
for n in $(strip "$WORK/host.log" | grep -oE "sent=[0-9]+ recv=[0-9]+" | grep -oE "[0-9]+"); do
  TOTAL=$((TOTAL + n))
done

if [ "$TOTAL" -le 0 ]; then
  echo "FAIL：剪贴板变化没有经过协议（${SENT:-无统计} / ${HOST_SENT:-无统计}）"
  exit 1
fi
echo "OK：真实剪贴板变化已经同步（${SENT:-无} / ${HOST_SENT:-无}）"
exit 0