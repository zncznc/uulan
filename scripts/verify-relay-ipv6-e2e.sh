#!/usr/bin/env bash
# 中继走 IPv6 的端到端验收：真 server + 真 被控端 + 真 控制端，全部连 `[::1]`
#
# 为什么单独一条：DESIGN 记着「IPv6 中继没验」—— `--bind` / `--server` 都是字符串直传，
# 理论上 `[::1]:port` 能解析，但「理论上能解析」和「三个进程真的在 IPv6 上跑通画面」
# 是两件事。这条把它们钉住。
#
# 运行环境：真麒麟（`::1` 可连）。本机 Windows 的 `::1` 能 bind 不能 connect，会退出 2。
# 用法：bash scripts/verify-relay-ipv6-e2e.sh
# 退出码：0 通过 / 1 断言失败 / 2 环境问题。
set -u

PORT="${UULAN_PORT:-21990}"
SECS="${UULAN_SECS:-10}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${UULAN_STAGE:-$HOME/relay6-$(date +%Y%m%d-%H%M%S)}"
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) EXE=".exe" ;; *) EXE="" ;; esac
PY=$(command -v python3 2>/dev/null || command -v python)
NO_COLOR=1; export NO_COLOR
FAIL=0
check() { if [ "$1" = 0 ]; then echo "  PASS $2"; else echo "  FAIL $2"; FAIL=1; fi; }

mkdir -p "$WORK" || { echo "建不了 $WORK"; exit 2; }
for b in server host client; do
  src="$ROOT/target/release/uulan-$b$EXE"
  [ -f "$src" ] || { echo "缺 $src（先 cargo build --release -p uulan-server -p uulan-host -p uulan-client）"; exit 2; }
  cp -f "$src" "$WORK/" || exit 2
done
SRV="$WORK/uulan-server$EXE"; HST="$WORK/uulan-host$EXE"; CLI="$WORK/uulan-client$EXE"
echo "工作目录：$WORK  中继 [::1]:$PORT"

cat >"$WORK/conn6.py" <<'PY'
# 能不能连上 ::1 的某端口（前置检查 + 就绪等待都用它）
import socket, sys, time
port, tries = int(sys.argv[1]), int(sys.argv[2])
fam = socket.AF_INET6
for _ in range(tries):
    s = socket.socket(fam, socket.SOCK_STREAM); s.settimeout(0.4)
    try:
        s.connect(("::1", port)); sys.exit(0)
    except OSError:
        time.sleep(0.1)
    finally:
        s.close()
sys.exit(1)
PY
wait6() { "$PY" "$WORK/conn6.py" "$PORT" "${1:-150}"; }

echo "──────── ⓿ 前置：本机 ::1 能连吗 ────────"
if ! "$PY" - <<'PY'
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
then
  echo "  本机 IPv6 回环不可连，**不要据此判定 IPv6 中继有问题**，退出 2。"; exit 2
fi
check 0 "本机 IPv6 回环可连"

echo "──────── ① 中继绑 [::1] ────────"
RUST_LOG=info "$SRV" --bind "[::1]:$PORT" --state "$WORK/devices.json" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
wait6 150 && check 0 "中继在 [::1]:$PORT 听上了（--bind 认 IPv6）" \
  || { check 1 "中继没在 [::1]:$PORT 听上"; sed -n 1,8p "$WORK/server.log"; exit 1; }

echo "──────── ② 被控端连 [::1] ────────"
RUST_LOG=info "$HST" --server "[::1]:$PORT" --config "$WORK/host.json" \
  --capture "lavfi:testsrc2=size=640x360:rate=15" --name relay6 >"$WORK/host.log" 2>&1 &
HOST_PID=$!
DEVICE=""
for _ in $(seq 1 200); do
  DEVICE=$(grep -oE "设备 ID : [0-9]{6,}" "$WORK/host.log" 2>/dev/null | tail -1 | sed 's/.*: *//' | tr -d '[:space:]')
  [ -n "$DEVICE" ] && break; sleep 0.1
done
[ -n "$DEVICE" ] && check 0 "被控端注册成功，设备 ID $DEVICE（--server 认 IPv6）" \
  || { check 1 "被控端没注册上"; grep -vE "访问密码" "$WORK/host.log" | tail -6; exit 1; }
PW=$(sed -n 's/.*"password": *"\([^"]*\)".*/\1/p' "$WORK/host.json" | head -1)
[ -n "$PW" ] || { echo "host.json 没有 password"; exit 2; }

echo "──────── ③ 控制端连 [::1] 取画面 ────────"
"$CLI" --server "[::1]:$PORT" --device "$DEVICE" --password "$PW" \
  --headless --seconds "$SECS" --no-clipboard --no-input >"$WORK/client.log" 2>&1
RC=$?
AU=$(grep -aoE "收到 AU *: [0-9]+" "$WORK/client.log" | tail -1 | grep -oE "[0-9]+$")
# 标签是「解出帧」而不是「解出」—— 正则写窄了就会在账目里印出一个 `?`，
# 那等于判据自己说不出话（2026-11-18 首跑就是这样）。
DEC=$(grep -aoE "解出帧? *: [0-9]+" "$WORK/client.log" | tail -1 | grep -oE "[0-9]+$")
FIRST=$(grep -aoE "首帧延迟 *: [0-9]+ ms" "$WORK/client.log" | tail -1 | grep -oE "[0-9]+")
check $RC "控制端会话正常收工（rc=$RC）"
[ "${AU:-0}" -ge 20 ] && check 0 "真的收到了画面：AU=$AU 解出=${DEC:-?} 首帧=${FIRST:-?}ms" \
  || { check 1 "没收到画面（AU=${AU:-0}）"; tail -8 "$WORK/client.log"; }

echo "──────── ④ 账目：中继日志里要能看见两端都是 IPv6 ────────"
grep -aq "\[::1\]" "$WORK/server.log" && check 0 "中继日志出现 IPv6 对端地址" \
  || check 1 "中继日志里没有 IPv6 对端（$(grep -aoE "peer=[^ ]+" "$WORK/server.log" | tail -1)）"
grep -aq "被控端上线" "$WORK/server.log" && check 0 "中继记录了被控端上线" || check 1 "中继没记上线"

echo
echo "──────── 收尾 ────────"
kill "$HOST_PID" "$SERVER_PID" 2>/dev/null; sleep 1; kill -9 "$HOST_PID" "$SERVER_PID" 2>/dev/null
if [ "$FAIL" = 0 ]; then echo "全过：中继走 IPv6 端到端可用"; exit 0; else echo "有断言失败，日志在 $WORK"; exit 1; fi