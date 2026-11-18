#!/usr/bin/env bash
# 端口映射白名单对 **IPv6 目标**是否真的生效（安全相关的那一半）
#
# 背景：默认白名单是 `127.0.0.1/8, ::1/128`。IPv6 支持是 2026-11-18 新加的，所以必须
# 证明「白名单也管 IPv6」—— 否则新语法就成了绕过白名单的后门。
# 做法：把被控端配置里的 `portmap_allow` 收窄成只有 `127.0.0.1/8`（**不含 ::1**），
# 再让控制端请求 `[::1]` 目标，必须被拒且理由说得出话。
#
# 正向（默认白名单下 [::1] 能通）已由 verify-portmap-ipv6-e2e.sh 覆盖，这里只验拒绝。
# 退出码：0 通过 / 1 断言失败 / 2 环境问题。
set -u

PORT="${UULAN_PORT:-21989}"
TARGET_PORT="${UULAN_TARGET_PORT:-21988}"
TARGET_PORT4="${UULAN_TARGET_PORT4:-21986}"   # 反面对照用的 IPv4 目标
LPORT="${UULAN_LOCAL_PORT:-21987}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${UULAN_STAGE:-$HOME/pmallow-$(date +%Y%m%d-%H%M%S)}"
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) EXE=".exe" ;; *) EXE="" ;; esac
PY=$(command -v python3 2>/dev/null || command -v python)
NO_COLOR=1; export NO_COLOR
FAIL=0
check() { if [ "$1" = 0 ]; then echo "  PASS $2"; else echo "  FAIL $2"; FAIL=1; fi; }

mkdir -p "$WORK" || { echo "建不了 $WORK"; exit 2; }
for b in server host client; do
  cp -f "$ROOT/target/release/uulan-$b$EXE" "$WORK/" || { echo "缺 uulan-$b$EXE"; exit 2; }
done
SRV="$WORK/uulan-server$EXE"; HST="$WORK/uulan-host$EXE"; CLI="$WORK/uulan-client$EXE"

cat >"$WORK/echo6.py" <<'PY'
import socket, sys, threading
port = int(sys.argv[1])
srv = socket.socket(socket.AF_INET6, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("::1", port)); srv.listen(8)
print("ready", flush=True)
def serve(c):
    with c:
        while True:
            b = c.recv(65536)
            if not b: break
            c.sendall(b)
while True:
    c, _ = srv.accept()
    threading.Thread(target=serve, args=(c,), daemon=True).start()
PY

cat >"$WORK/echo4.py" <<'PY'
import socket, sys, threading
port = int(sys.argv[1])
srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", port)); srv.listen(8)
print("ready", flush=True)
def serve(c):
    with c:
        while True:
            b = c.recv(65536)
            if not b: break
            c.sendall(b)
while True:
    c, _ = srv.accept()
    threading.Thread(target=serve, args=(c,), daemon=True).start()
PY

cat >"$WORK/probe4.py" <<'PY'
# 经本机监听端口灌几个字节、等回显（不连它，被控端就没有理由去连目标）
import socket, sys
port = int(sys.argv[1])
s = socket.create_connection(("127.0.0.1", port), timeout=10)
s.sendall(b"allowlist-control-1234")
got = s.recv(64)
s.close()
print("ECHO", got.decode(errors="replace"))
sys.exit(0 if got == b"allowlist-control-1234" else 1)
PY

echo "──────── ⓿ 前置 ────────"
"$PY" - <<'PY' || { echo "  本机 ::1 不可连，退出 2（别据此判定白名单有问题）"; exit 2; }
import socket, sys
srv = socket.socket(socket.AF_INET6, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("::1", 0)); srv.listen(1)
p = srv.getsockname()[1]
c = socket.socket(socket.AF_INET6, socket.SOCK_STREAM); c.settimeout(3)
try:
    c.connect(("::1", p)); rc = 0
except OSError as e:
    print(f"  ::1 连不上：{e}"); rc = 1
finally:
    c.close(); srv.close()
sys.exit(rc)
PY
check 0 "本机 IPv6 回环可连"

"$PY" "$WORK/echo6.py" "$TARGET_PORT" >"$WORK/echo6.log" 2>&1 &
ECHO_PID=$!
for _ in $(seq 1 100); do grep -q ready "$WORK/echo6.log" 2>/dev/null && break; sleep 0.1; done
grep -q ready "$WORK/echo6.log" || { echo "echo 目标没起来"; exit 2; }
check 0 "echo 目标在 [::1]:$TARGET_PORT 监听"

"$PY" "$WORK/echo4.py" "$TARGET_PORT4" >"$WORK/echo4.log" 2>&1 &
ECHO4_PID=$!
for _ in $(seq 1 100); do grep -q ready "$WORK/echo4.log" 2>/dev/null && break; sleep 0.1; done
grep -q ready "$WORK/echo4.log" || { echo "IPv4 echo 没起来"; exit 2; }
check 0 "IPv4 echo 目标在 127.0.0.1:$TARGET_PORT4 监听（反面对照用）"

RUST_LOG=info "$SRV" --bind "127.0.0.1:$PORT" --state "$WORK/devices.json" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
sleep 1.5

echo "──────── ① 先让被控端生成配置，再把白名单收窄成只有 IPv4 回环 ────────"
RUST_LOG=info "$HST" --server "127.0.0.1:$PORT" --config "$WORK/host.json" \
  --capture "lavfi:testsrc2=size=320x240:rate=5" --name allow6 >"$WORK/host0.log" 2>&1 &
H0=$!
for _ in $(seq 1 200); do [ -f "$WORK/host.json" ] && grep -q '"device_id"' "$WORK/host.json" 2>/dev/null && break; sleep 0.1; done
kill "$H0" 2>/dev/null; sleep 1; kill -9 "$H0" 2>/dev/null
[ -f "$WORK/host.json" ] || { echo "被控端没生成 host.json"; exit 2; }

"$PY" - "$WORK/host.json" <<'PY'
import json, sys
p = sys.argv[1]
c = json.load(open(p))
c["portmap_allow"] = ["127.0.0.1/8"]      # 故意不含 ::1
json.dump(c, open(p, "w"), ensure_ascii=False, indent=2)
print("  已把 portmap_allow 收窄为", c["portmap_allow"])
PY
check $? "配置里 portmap_allow 只剩 127.0.0.1/8（不含 ::1）"

RUST_LOG=info "$HST" --server "127.0.0.1:$PORT" --config "$WORK/host.json" \
  --capture "lavfi:testsrc2=size=320x240:rate=5" --name allow6 >"$WORK/host.log" 2>&1 &
HOST_PID=$!
DEVICE=""
for _ in $(seq 1 200); do
  DEVICE=$(grep -oE "设备 ID : [0-9]{6,}" "$WORK/host.log" 2>/dev/null | tail -1 | sed 's/.*: *//' | tr -d '[:space:]')
  [ -n "$DEVICE" ] && break; sleep 0.1
done
[ -n "$DEVICE" ] || { echo "被控端没注册上"; grep -vE "访问密码" "$WORK/host.log" | tail -5; exit 2; }
PW=$(sed -n 's/.*"password": *"\([^"]*\)".*/\1/p' "$WORK/host.json" | head -1)
check 0 "被控端带着收窄后的白名单上线（设备 ID $DEVICE）"

echo "──────── ② 请求 [::1] 目标：必须被白名单拒 ────────"
"$CLI" --server "127.0.0.1:$PORT" --device "$DEVICE" --password "$PW" --name allow6-cli \
  --forward "[::1]:$LPORT:[::1]:$TARGET_PORT" --seconds 8 >"$WORK/client.log" 2>&1
RC=$?
check $RC "控制端会话正常收工（rc=$RC）"

REJ=$(grep -a "白名单" "$WORK/host.log" | tail -1)
[ -n "$REJ" ] && check 0 "被控端明确以白名单为由拒绝：$(printf '%s' "$REJ" | sed 's/.*portmap: //' | cut -c1-90)" \
  || { check 1 "被控端没有白名单拒绝记录（可能被绕过了！）"; grep -a "端口映射" "$WORK/host.log" | tail -3; }

# 反面对照：同一份配置下 IPv4 回环目标必须仍然放行（证明不是「全拒」）。
# 注意必须**真的连一次本地监听端口** —— 不连，被控端就没有理由去连目标，
# 日志里自然没有「端口映射已建立」，那是判据自己没制造条件（2026-11-18 首跑就栽在这）。
"$CLI" --server "127.0.0.1:$PORT" --device "$DEVICE" --password "$PW" --name allow6-cli4 \
  --forward "127.0.0.1:$((LPORT+1)):127.0.0.1:$TARGET_PORT4" --seconds 10 >"$WORK/client4.log" 2>&1 &
CLI4=$!
for _ in $(seq 1 100); do
  "$PY" -c "import socket,sys; s=socket.socket(); s.settimeout(0.3); sys.exit(0 if s.connect_ex(('127.0.0.1',$((LPORT+1))))==0 else 1)" && break
  sleep 0.1
done
CTL=$("$PY" "$WORK/probe4.py" "$((LPORT+1))" 2>&1)
check $? "反面对照：IPv4 回环目标放行且字节原样回显（$CTL）"
wait "$CLI4" 2>/dev/null
grep -aq "端口映射已建立" "$WORK/host.log" && check 0 "被控端日志有「端口映射已建立」（IPv4 目标确实被放行）" \
  || check 1 "IPv4 目标没建立（白名单收窄过头，判据失去意义）"

echo
echo "──────── 收尾 ────────"
kill "$HOST_PID" "$SERVER_PID" "$ECHO_PID" "$ECHO4_PID" 2>/dev/null; sleep 1
kill -9 "$HOST_PID" "$SERVER_PID" "$ECHO_PID" "$ECHO4_PID" 2>/dev/null
if [ "$FAIL" = 0 ]; then echo "全过：白名单对 IPv6 目标同样生效"; exit 0; else echo "有断言失败，日志在 $WORK"; exit 1; fi