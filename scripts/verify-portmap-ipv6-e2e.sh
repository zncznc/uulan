#!/usr/bin/env bash
# 端口映射 **IPv6** 端到端验收（真 server + 真被控端 + 真控制端，全在本机）
#
# 为什么要单独一条：2026-11-18 之前 `--forward` 遇到 `[` 直接报错「不支持 IPv6 字面量」，
# 理由是不想猜分隔符。现在改成方括号解析（`[::1]:2222:[::1]:22`），单元测试钉了语法，
# 但语法对不等于跑得通 —— 这条用真二进制把**字节真的在 IPv6 上往返**验掉。
#
# 两个方向都测：
#   A) 本机监听与目标都是 IPv6（`[::1]:LP:[::1]:TP`）
#   B) 本机监听 IPv4、目标 IPv6（`127.0.0.1:LP:[::1]:TP`）—— 混合族，现实里最常见
#
# 退出码：0 = 通过；1 = 断言失败；2 = 环境问题（端口占着 / 进程没起来）。
set -u

PORT="${UULAN_PORT:-21996}"
TARGET_PORT="${UULAN_TARGET_PORT:-21995}"
LPORT_A="${UULAN_LOCAL_PORT:-21994}"
LPORT_B="${UULAN_LOCAL_PORT_B:-21993}"
BYTES="${UULAN_BYTES:-131072}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${UULAN_STAGE:-D:/uulan-bin/pm6-$(date +%Y%m%d-%H%M%S)}"
NO_COLOR=1
export NO_COLOR

# 同一套脚本要在两处跑：本机 Windows（.exe）与真麒麟（无后缀）。硬编码 .exe 会让真机
# 版直接死在「拷二进制失败」上 —— 那不是产品问题，是脚本不肯跨平台。
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) EXE=".exe" ;; *) EXE="" ;; esac
PY=$(command -v python3 2>/dev/null || command -v python)
[ -n "$PY" ] || { echo "没有 python（探针要用它）"; exit 2; }
SRV="$WORK/uulan-server$EXE"; HST="$WORK/uulan-host$EXE"; CLI="$WORK/uulan-client$EXE"

mkdir -p "$WORK" || { echo "建不了 $WORK"; exit 2; }
# cargo 刚写完时 hardlink 还没落定，`cp` 会瞬时 stat 不到（2026-11-18 撞过两次）——
# 重试几次，别把构建竞态报成「上一轮还在跑」。
for _ in 1 2 3 4 5; do
  cp -f "$ROOT"/target/release/uulan-server$EXE \
        "$ROOT"/target/release/uulan-host$EXE \
        "$ROOT"/target/release/uulan-client$EXE "$WORK"/ 2>/dev/null && break
  sleep 1
done
[ -f "$CLI" ] && [ -f "$HST" ] && [ -f "$SRV" ] || {
  echo "拷二进制失败。target/release 现状："
  ls -l "$ROOT"/target/release/*"$EXE" 2>&1 | sed 's/^/  /'
  echo "  （若是「没有那个文件」，先 cargo build --release -p uulan-server -p uulan-host -p uulan-client）"
  exit 2; }
echo "工作目录：$WORK  中继 127.0.0.1:$PORT  目标 [::1]:$TARGET_PORT  本机监听 A=[::1]:$LPORT_A B=127.0.0.1:$LPORT_B"

cat >"$WORK/echo6.py" <<'PY'
# 「被控端内网里的目标」——这次是 IPv6 的。把收到的字节原样送回。
import socket, sys, threading

port = int(sys.argv[1])
srv = socket.socket(socket.AF_INET6, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("::1", port))
srv.listen(16)
print("echo6-ready", flush=True)


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

cat >"$WORK/probe6.py" <<'PY'
# 连本机监听端口灌数据、等回显、比 sha256。argv: host port bytes
import hashlib, socket, sys

host, port, n = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
data = bytes(((i * 37 + 11) % 256) for i in range(n))
fam = socket.AF_INET6 if ":" in host else socket.AF_INET
s = socket.socket(fam, socket.SOCK_STREAM)
s.settimeout(25)
s.connect((host, port))
s.sendall(data)
got = b""
while len(got) < n:
    b = s.recv(65536)
    if not b:
        break
    got += b
s.close()
ok = hashlib.sha256(got).digest() == hashlib.sha256(data).digest()
print(f"{'MATCH' if ok else 'MISMATCH'} sent={n} got={len(got)}")
sys.exit(0 if ok else 1)
PY

FAIL=0
check() { if [ "$1" = 0 ]; then echo "  PASS $2"; else echo "  FAIL $2"; FAIL=1; fi; }

# 等端口能连上（用 python，别用 bash 的 /dev/tcp —— 它不认 [::1] 写法）
wait_port() { # wait_port <host> <port>
  "$PY" - "$1" "$2" <<'PY' >/dev/null 2>&1
import socket, sys, time
host, port = sys.argv[1], int(sys.argv[2])
fam = socket.AF_INET6 if ":" in host else socket.AF_INET
for _ in range(120):
    s = socket.socket(fam, socket.SOCK_STREAM); s.settimeout(0.3)
    try:
        s.connect((host, port)); s.close(); sys.exit(0)
    except OSError:
        time.sleep(0.1)
    finally:
        s.close()
sys.exit(1)
PY
}

cat >"$WORK/probe_loop.py" <<'PY'
# 本机 IPv6 回环能不能**连**（能 bind 不等于能 connect：这台 Windows 上 singbox_tun
# 把 ::1 的路由也接走了，connect 直接 WinError 10013）。
import socket, sys
srv = socket.socket(socket.AF_INET6, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("::1", 0)); srv.listen(1)
port = srv.getsockname()[1]
c = socket.socket(socket.AF_INET6, socket.SOCK_STREAM); c.settimeout(3)
try:
    c.connect(("::1", port)); rc = 0
except OSError as e:
    print(f"  ::1 连不上：{e}"); rc = 1
finally:
    c.close(); srv.close()
sys.exit(rc)
PY
echo "──────── ⓿ 前置：本机 IPv6 回环真的能连吗 ────────"
if ! "$PY" "$WORK/probe_loop.py"; then
  echo "  本机 IPv6 回环不可连（能 bind 不能 connect）。**不要据此判定 IPv6 端口映射有问题**，退出 2。"
  exit 2
fi
check 0 "本机 IPv6 回环可连（前置成立，后面的断言才有意义）"

echo "──────── ① 起 IPv6 echo 目标 / 中继 / 被控端 ────────"
"$PY" "$WORK/echo6.py" "$TARGET_PORT" >"$WORK/echo6.log" 2>&1 &
ECHO_PID=$!
for _ in $(seq 1 100); do grep -q echo6-ready "$WORK/echo6.log" 2>/dev/null && break; sleep 0.1; done
grep -q echo6-ready "$WORK/echo6.log" || { echo "IPv6 echo 目标没起来：$(cat "$WORK/echo6.log")"; exit 2; }
check 0 "echo 目标已在 [::1]:$TARGET_PORT 监听（IPv6 回环可用）"

RUST_LOG=info "$SRV" --bind "127.0.0.1:$PORT" \
  --state "$WORK/devices.json" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
wait_port 127.0.0.1 "$PORT" || { echo "中继没起来"; exit 2; }

RUST_LOG=info "$HST" --server "127.0.0.1:$PORT" --config "$WORK/host.json" \
  --capture "lavfi:testsrc2=size=640x360:rate=5" --name e2e-pm6 \
  >"$WORK/host.log" 2>&1 &
HOST_PID=$!

DEVICE=""
for _ in $(seq 1 200); do
  DEVICE=$(grep -oE "设备 ID : [0-9]{6,}" "$WORK/host.log" 2>/dev/null | tail -1 | sed 's/.*: *//' | tr -d '[:space:]')
  [ -n "$DEVICE" ] && break
  sleep 0.1
done
[ -n "$DEVICE" ] || { echo "被控端没拿到设备 ID："; grep -vE "访问密码" "$WORK/host.log" | sed -n 1,15p; exit 2; }
echo "  设备 ID：$DEVICE"
PW=$(sed -n 's/.*"password": *"\([^"]*\)".*/\1/p' "$WORK/host.json" | head -1)
[ -n "$PW" ] || { echo "host.json 里没有 password 字段"; exit 2; }

echo "──────── ② A 案：本机监听与目标都是 IPv6 ────────"
"$CLI" --server "127.0.0.1:$PORT" --device "$DEVICE" \
  --password "$PW" --name e2e-pm6-A \
  --forward "[::1]:$LPORT_A:[::1]:$TARGET_PORT" --seconds 14 \
  >"$WORK/client-a.log" 2>&1 &
CLI_A=$!
wait_port ::1 "$LPORT_A" && check 0 "控制端在 [::1]:$LPORT_A 听上了（IPv6 绑定地址解析可用）" \
  || { check 1 "控制端没在 [::1]:$LPORT_A 听上"; grep -vE "访问密码" "$WORK/client-a.log" | tail -5; }

OUT_A=$("$PY" "$WORK/probe6.py" ::1 "$LPORT_A" "$BYTES" 2>&1)
check $? "A 案 $BYTES 字节经 [::1] 转发并原样回显（$OUT_A）"

echo "──────── ③ B 案：本机监听 IPv4、目标 IPv6（混合族）────────"
"$CLI" --server "127.0.0.1:$PORT" --device "$DEVICE" \
  --password "$PW" --name e2e-pm6-B \
  --forward "127.0.0.1:$LPORT_B:[::1]:$TARGET_PORT" --seconds 14 \
  >"$WORK/client-b.log" 2>&1 &
CLI_B=$!
wait_port 127.0.0.1 "$LPORT_B" && check 0 "控制端在 127.0.0.1:$LPORT_B 听上了" \
  || check 1 "B 案控制端没听上"
OUT_B=$("$PY" "$WORK/probe6.py" 127.0.0.1 "$LPORT_B" "$BYTES" 2>&1)
check $? "B 案 $BYTES 字节经混合族转发并原样回显（$OUT_B）"

echo "──────── ④ 账目：两端都要说得出话 ────────"
# 「端口映射结束：连接 N」是**会话收尾时**才打印的。上一版这里提前 kill 进程，
# 于是永远读不到 —— 那是判据时机错，不是功能没上报。
wait_pid() { local p=$1 i=0; while kill -0 "$p" 2>/dev/null && [ $i -lt 200 ]; do sleep 0.2; i=$((i+1)); done; }
wait_pid "$CLI_A"
wait_pid "$CLI_B"
sleep 1
CONN_A=$(grep -aoE "端口映射结束：连接 [0-9]+" "$WORK/client-a.log" | tail -1 | grep -oE "[0-9]+$")
CONN_B=$(grep -aoE "端口映射结束：连接 [0-9]+" "$WORK/client-b.log" | tail -1 | grep -oE "[0-9]+$")
[ "${CONN_A:-0}" != "0" ] && check 0 "A 控制端自报连接数 ${CONN_A:-?}" || check 1 "A 控制端连接数没上报（${CONN_A:-空}）"
[ "${CONN_B:-0}" != "0" ] && check 0 "B 控制端自报连接数 ${CONN_B:-?}" || check 1 "B 控制端连接数没上报（${CONN_B:-空}）"
grep -aq "端口映射已建立" "$WORK/host.log" && check 0 "被控端日志有「端口映射已建立」（真连到了 [::1] 目标）" \
  || check 1 "被控端日志没有「端口映射已建立」"

echo "──────── ⑤ 反面对照：不带方括号的 IPv6 必须报错，不许猜 ────────"
ERR=$("$CLI" --server "127.0.0.1:$PORT" --device "$DEVICE" --password "$PW" \
      --forward "::1:$LPORT_A:$TARGET_PORT" --seconds 1 2>&1)
printf '%s' "$ERR" | grep -q "方括号" && check 0 "无方括号的 IPv6 被明确拒绝：$(printf '%s' "$ERR" | grep -o '方括号[^，。]*' | head -1)" \
  || check 1 "无方括号的 IPv6 没有给出方括号提示（实际：$(printf '%s' "$ERR" | tail -1 | cut -c1-90)）"

echo
echo "──────── 收尾 ────────"
kill "$CLI_A" "$CLI_B" "$HOST_PID" "$SERVER_PID" "$ECHO_PID" 2>/dev/null
sleep 1
kill -9 "$CLI_A" "$CLI_B" "$HOST_PID" "$SERVER_PID" "$ECHO_PID" 2>/dev/null
if [ "$FAIL" = 0 ]; then echo "全过：IPv6 端口映射端到端可用"; exit 0; else echo "有断言失败，日志在 $WORK"; exit 1; fi