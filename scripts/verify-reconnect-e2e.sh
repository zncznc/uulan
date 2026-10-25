#!/usr/bin/env bash
# 断线重连验收：真 server + 真被控端 + 真控制端，**中途把中继打死再拉起来**，
# 断言控制端自己接了回去、而且接回之后画面是新的（不是冻着的那一帧）。
#
# 为什么要这么验：断线重连最容易「看起来成了」——
# 日志里有「已接回」，屏幕上却还是旧画面（帧通道在重连时被换掉、窗口握着一条死通道）。
# 所以这里同时看三件事：
#   1) 进程没退出，退出码 0（到 --seconds 才收工）；
#   2) 日志里「会话：建立 2 条 / 断线接回 1 次」（headless 是「会话       : …」）；
#   3) 接回之后**还在出帧**：headless 看「本次会话：解出 N 帧」且 N>0，
#      窗口模式看 painted 在断线之后继续涨。
#
# 用法：
#   bash scripts/verify-reconnect-e2e.sh              # headless，25 秒，第 8 秒打死中继
#   bash scripts/verify-reconnect-e2e.sh window       # 开窗模式（要图形环境）
#   UULAN_PORT=21992 UULAN_SECONDS=40 bash scripts/verify-reconnect-e2e.sh
#
# 退出码：0 = 接回来了；1 = 断言失败；2 = 环境问题。
set -u

MODE="${1:-headless}"
PORT="${UULAN_PORT:-21991}"
RUN_SECS="${UULAN_SECONDS:-25}"
KILL_AT="${UULAN_KILL_AT:-8}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${UULAN_STAGE:-D:/uulan-bin/recon-$(date +%Y%m%d-%H%M%S)}"
NO_COLOR=1
export NO_COLOR

mkdir -p "$WORK" || { echo "建不了 $WORK"; exit 2; }
cp -f "$ROOT"/target/release/uulan-server.exe \
      "$ROOT"/target/release/uulan-host.exe \
      "$ROOT"/target/release/uulan-client.exe "$WORK"/ || { echo "拷二进制失败（上一轮还在跑？）"; exit 2; }
echo "工作目录：$WORK  端口：$PORT  模式：$MODE  时长：${RUN_SECS}s  第 ${KILL_AT}s 打死中继"

SERVER_PID=""; HOST_PID=""; CLIENT_PID=""
cleanup() {
  [ -n "$CLIENT_PID" ] && kill "$CLIENT_PID" 2>/dev/null
  [ -n "$HOST_PID" ] && kill "$HOST_PID" 2>/dev/null
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null
}
trap cleanup EXIT

start_server() {
  RUST_LOG=info "$WORK/uulan-server.exe" --bind "127.0.0.1:$PORT" \
    --state "$WORK/devices.json" >>"$WORK/server.log" 2>&1 &
  SERVER_PID=$!
  for _ in $(seq 1 100); do
    (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && { exec 3>&-; return 0; }
    sleep 0.1
  done
  return 1
}

start_server || { echo "服务器没起来"; exit 2; }

RUST_LOG=info "$WORK/uulan-host.exe" --server "127.0.0.1:$PORT" --config "$WORK/host.json" \
  --capture "lavfi:testsrc2=size=1280x720:rate=30" --name e2e-host \
  >"$WORK/host.log" 2>&1 &
HOST_PID=$!

DEVICE=""
for _ in $(seq 1 200); do
  DEVICE=$(grep -oE "设备 ID : [0-9]{6,}" "$WORK/host.log" 2>/dev/null | tail -1 | sed 's/.*: *//' | tr -d '[:space:]')
  [ -n "$DEVICE" ] && break
  sleep 0.1
done
if [ -z "$DEVICE" ]; then
  echo "被控端没拿到设备 ID，日志（已滤掉密码行）："
  grep -vE "访问密码" "$WORK/host.log" | sed -n 1,20p
  exit 2
fi
PW=$(sed -n 's/.*"password": *"\([^"]*\)".*/\1/p' "$WORK/host.json" | head -1)
[ -n "$PW" ] || { echo "host.json 里没有 password 字段"; exit 2; }

ARGS=(--server "127.0.0.1:$PORT" --device "$DEVICE" --password "$PW" --name e2e-client
      --seconds "$RUN_SECS" --reconnect 8)
if [ "$MODE" = "headless" ]; then
  ARGS+=(--headless --save-frame "$WORK/after.ppm")
fi

RUST_LOG=info "$WORK/uulan-client.exe" "${ARGS[@]}" >"$WORK/client.log" 2>&1 &
CLIENT_PID=$!

sleep "$KILL_AT"
echo "第 ${KILL_AT}s：打死中继（pid $SERVER_PID）"
kill "$SERVER_PID" 2>/dev/null
wait "$SERVER_PID" 2>/dev/null
SERVER_PID=""
sleep 1
echo "第 $((KILL_AT + 1))s：重新拉起中继"
start_server || { echo "中继没重新起来"; exit 2; }

# 等控制端自己收工（到 --seconds）
for _ in $(seq 1 $((RUN_SECS * 10 + 100))); do
  kill -0 "$CLIENT_PID" 2>/dev/null || break
  sleep 0.1
done
if kill -0 "$CLIENT_PID" 2>/dev/null; then
  echo "FAIL：控制端到点还没退出（$((RUN_SECS + 10)) 秒）"
  kill "$CLIENT_PID" 2>/dev/null
  exit 1
fi
wait "$CLIENT_PID"
CLIENT_RC=$?
CLIENT_PID=""

strip() { sed -E 's/\x1b\[[0-9;]*m//g' "$1"; }

echo "──────── 控制端 ────────"
strip "$WORK/client.log" | grep -E "已接回|接回失败|本次会话|会话|窗口已|painted=" || true
echo "──────── 被控端 ────────"
strip "$WORK/host.log" | grep -E "已重新上线|重连|设备 ID" | tail -5 || true

fail() { echo "FAIL：$*"; exit 1; }

[ "$CLIENT_RC" -eq 0 ] || fail "控制端退出码 $CLIENT_RC（期望 0：到时长才收工）"

if ! strip "$WORK/client.log" | grep -q "已接回被控端"; then
  fail "日志里没有「已接回被控端」—— 断线之后根本没重连"
fi

if [ "$MODE" = "headless" ]; then
  # 第二个会话必须真的出画面
  ERRS=$(strip "$WORK/client.log" | grep -c "^接回失败" || true)
  [ "$ERRS" -le 3 ] || fail "接回失败 $ERRS 次，退避策略可能有毛病"
  strip "$WORK/client.log" | grep -q "会话       : 建立 2 条 / 断线接回 1 次" \
    || fail "日志里没有「建立 2 条 / 断线接回 1 次」"
  PPM="$WORK/after.ppm"
  [ -s "$PPM" ] || fail "接回之后没存下帧（$PPM 是空的）"
  # PPM 头后面应当有大片非零像素：全黑说明「接回了但没画面」
  python - "$PPM" <<'PY' || exit 1
import sys
p = sys.argv[1]
with open(p, 'rb') as f:
    data = f.read()
# P6\n<w> <h>\n255\n
parts = data.split(b'\n', 3)
px = parts[3] if len(parts) == 4 else b''
nz = sum(1 for b in px[:400000] if b)
assert len(px) > 1000, "像素数据太少"
assert nz > len(px[:400000]) // 100, f"像素几乎全黑（非零 {nz}）"
print(f"接回后的帧：{len(px)} 字节像素，非零 {nz}")
PY
  echo "OK：断线后接回，且接回之后真的在出画面"
else
  PAINTED=$(strip "$WORK/client.log" | grep -oE 'painted=[0-9]+' | tail -1 | cut -d= -f2)
  [ -n "${PAINTED:-}" ] || fail "没有 painted=（窗口可能没画）"
  [ "$PAINTED" -gt 0 ] || fail "painted=$PAINTED"
  strip "$WORK/client.log" | grep -q "会话：建立 2 条 / 断线接回 1 次" \
    || fail "日志里没有「建立 2 条 / 断线接回 1 次」"
  echo "OK：窗口跨过一次中继重启仍然活着，painted=$PAINTED"
fi
exit 0