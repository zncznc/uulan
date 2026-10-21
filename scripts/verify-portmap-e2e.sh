#!/usr/bin/env bash
# M4 端口映射验收（真二进制、真 socket、真多路复用）：
#   真 server + 真被控端（lavfi 合成画面）+ 真控制端 `--forward`，
#   本机起一个真 TCP echo 服务当「被控端内网里的目标」。
#
# 断言的都是机器可查的量：
#   ① 本机监听真的起来了（控制端打出「转发已就绪」）
#   ② 256 KiB 数据经被控端往返后 sha256 一致（单条流）
#   ③ 两条连接同时跑、各自内容不同，都不串（bulk 按 id 分派）
#   ④ 被控端日志里能看到「端口映射已建立」——证明目标连接是**被控端**发起的
#
# 为什么不从工作区里直接跑：D:/default-workspace/ 之下的 exe 在这台开发机上
# 写不了任何文件（被控端要落盘 host.json）。拷出去再跑。
#
# 用法：
#   bash scripts/verify-portmap-e2e.sh
#   UULAN_PORT=21991 UULAN_STAGE=D:/uulan-bin-pm bash scripts/verify-portmap-e2e.sh
#
# 退出码：0 = 通过；1 = 断言失败；2 = 环境问题（端口占着 / 进程没起来）。
set -u

PORT="${UULAN_PORT:-21997}"
TARGET_PORT="${UULAN_TARGET_PORT:-21998}"
LOCAL_PORT="${UULAN_LOCAL_PORT:-21999}"
BYTES="${UULAN_BYTES:-262144}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${UULAN_STAGE:-D:/uulan-bin/pm-$(date +%Y%m%d-%H%M%S)}"
NO_COLOR=1
export NO_COLOR

mkdir -p "$WORK" || { echo "建不了 $WORK"; exit 2; }
cp -f "$ROOT"/target/release/uulan-server.exe \
      "$ROOT"/target/release/uulan-host.exe \
      "$ROOT"/target/release/uulan-client.exe "$WORK"/ || {
  echo "拷二进制失败（上一轮还在跑？）"; exit 2; }
echo "工作目录：$WORK  中继端口：$PORT  目标端口：$TARGET_PORT  本机监听：$LOCAL_PORT"

cat >"$WORK/echo.py" <<'PY'
# 「被控端内网里的目标」：把收到的字节原样送回去。每个连接一个线程。
import socket, sys, threading

port = int(sys.argv[1])
srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", port))
srv.listen(16)
print("echo-ready", flush=True)


def serve(c):
    with c:
        while True:
            b = c.recv(65536)
            if not b:
                break
            c.sendall(b)


while True:
    c, _ = srv.accept()
    threading.Thread(target=serve, args=(c,), daemon=True).start()
PY

cat >"$WORK/probe.py" <<'PY'
# 经本机监听端口灌数据、等回显、比对 sha256。
import hashlib, socket, sys

port = int(sys.argv[1])
n = int(sys.argv[2])
seed = int(sys.argv[3]) if len(sys.argv) > 3 else 0
data = bytes(((i * 37 + 11 + seed * 97) % 256) for i in range(n))
s = socket.create_connection(("127.0.0.1", port), timeout=20)
s.sendall(data)
got = b""
while len(got) < n:
    b = s.recv(65536)
    if not b:
        break
    got += b
s.close()
print("sent", n, "got", len(got), "sha256", hashlib.sha256(got).hexdigest()[:16])
sys.exit(0 if got == data else 1)
PY

SERVER_PID=""; HOST_PID=""; ECHO_PID=""; CLI_PID=""
cleanup() {
  [ -n "$CLI_PID" ] && kill "$CLI_PID" 2>/dev/null
  [ -n "$HOST_PID" ] && kill "$HOST_PID" 2>/dev/null
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null
  [ -n "$ECHO_PID" ] && kill "$ECHO_PID" 2>/dev/null
  # MSYS 的路径转换是关的：`//F` 会被原样传给 taskkill 而报参数错，必须显式关掉转换。
  MSYS2_ARG_CONV_EXCL='*' taskkill /F /IM uulan-host.exe >/dev/null 2>&1
  MSYS2_ARG_CONV_EXCL='*' taskkill /F /IM uulan-server.exe >/dev/null 2>&1
  MSYS2_ARG_CONV_EXCL='*' taskkill /F /IM uulan-client.exe >/dev/null 2>&1
}
trap cleanup EXIT

strip() { sed -E 's/\x1b\[[0-9;]*m//g' "$1"; }
FAIL=0
check() { if [ "$1" = 0 ]; then echo "  ✓ $2"; else echo "  ✗ $2"; FAIL=1; fi; }

echo "──────── ① 起 echo 目标 / 中继 / 被控端 ────────"
python "$WORK/echo.py" "$TARGET_PORT" >"$WORK/echo.log" 2>&1 &
ECHO_PID=$!
for _ in $(seq 1 100); do
  (exec 3<>"/dev/tcp/127.0.0.1/$TARGET_PORT") 2>/dev/null && { exec 3>&-; break; }
  sleep 0.1
done
check $? "echo 目标已在 127.0.0.1:$TARGET_PORT 监听"

RUST_LOG=info "$WORK/uulan-server.exe" --bind "127.0.0.1:$PORT" \
  --state "$WORK/devices.json" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 100); do
  (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && { exec 3>&-; break; }
  sleep 0.1
done
grep -q "" "$WORK/server.log" 2>/dev/null || { echo "服务器没起来"; exit 2; }

RUST_LOG=info "$WORK/uulan-host.exe" --server "127.0.0.1:$PORT" --config "$WORK/host.json" \
  --capture "lavfi:testsrc2=size=640x360:rate=5" --name e2e-pm \
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
echo "  设备 ID：$DEVICE"

# 密码从 host.json 读，**不回显**。
PW=$(sed -n 's/.*"password": *"\([^"]*\)".*/\1/p' "$WORK/host.json" | head -1)
[ -n "$PW" ] || { echo "host.json 里没有 password 字段"; exit 2; }

echo "──────── ② 控制端起监听（真 --forward）────────"
"$WORK/uulan-client.exe" --server "127.0.0.1:$PORT" --device "$DEVICE" \
  --password "$PW" --name e2e-pm-client \
  --forward "127.0.0.1:$LOCAL_PORT:127.0.0.1:$TARGET_PORT" --seconds 30 \
  >"$WORK/client.log" 2>&1 &
CLI_PID=$!

READY=0
for _ in $(seq 1 150); do
  if grep -q "转发已就绪" "$WORK/client.log" 2>/dev/null; then READY=1; break; fi
  sleep 0.1
done
[ "$READY" = 1 ]; check $? "控制端打出「转发已就绪」（本机监听 $LOCAL_PORT）"
grep -E "端口映射：本机|转发已就绪" "$WORK/client.log" | sed 's/^/    /'

echo "──────── ③ 单条流：$BYTES 字节往返 ────────"
if python "$WORK/probe.py" "$LOCAL_PORT" "$BYTES" 0 >"$WORK/probe1.log" 2>&1; then
  check 0 "回显与发送逐字节一致"
else
  check 1 "回显与发送不一致（见下面日志）"
fi
sed 's/^/    /' "$WORK/probe1.log"

echo "──────── ④ 两条流同时跑（bulk 按 id 分派）────────"
python "$WORK/probe.py" "$LOCAL_PORT" 131072 1 >"$WORK/probe2.log" 2>&1 &
P2=$!
python "$WORK/probe.py" "$LOCAL_PORT" 196608 2 >"$WORK/probe3.log" 2>&1 &
P3=$!
wait $P2; R2=$?
wait $P3; R3=$?
check $R2 "第 1 条并发流回显一致（128 KiB）"
check $R3 "第 2 条并发流回显一致（192 KiB）"
sed 's/^/    /' "$WORK/probe2.log"
sed 's/^/    /' "$WORK/probe3.log"

echo "──────── ⑤ 等控制端收尾并核对统计 ────────"
for _ in $(seq 1 400); do
  grep -q "端口映射结束" "$WORK/client.log" 2>/dev/null && break
  sleep 0.1
done
strip "$WORK/client.log" | grep -E "端口映射结束" | sed 's/^/    /'
CONNS=$(strip "$WORK/client.log" | sed -n 's/.*端口映射结束：连接 \([0-9]*\).*/\1/p' | tail -1)
[ "${CONNS:-0}" -ge 3 ] && check 0 "控制端统计到 $CONNS 条转发连接" || check 1 "转发的连接数不对（$CONNS）"

strip "$WORK/host.log" | grep -E "端口映射白名单|端口映射已建立|端口映射结束" | tail -8 | sed 's/^/    /'
strip "$WORK/host.log" | grep -q "端口映射已建立" && check 0 "被控端真的连上了目标（日志有「端口映射已建立」）" \
  || check 1 "被控端日志里没有「端口映射已建立」"

echo "──────── 错误检查 ────────"
strip "$WORK/host.log" | grep -E " ERROR |panic" | head -5 | sed 's/^/    /'
strip "$WORK/client.log" | grep -E " ERROR |panic|panic" | head -5 | sed 's/^/    /'

if [ "$FAIL" = 0 ]; then
  echo "OK：端口映射全链路通过（$BYTES 字节单流 + 两条并发流，回显逐字节一致）"
  exit 0
else
  echo "FAIL：见上面的 ✗ 行"
  exit 1
fi