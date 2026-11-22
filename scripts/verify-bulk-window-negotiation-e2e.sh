#!/usr/bin/env bash
# 按流窗口**帧数协商**的端到端验收（**开发机 git-bash 跑，需与真机同二层**）：
#
#   M8 第一版的窗口是常数 16 帧（= 1 MiB 在途）。2026-11-23 01:00 班把「高 RTT 下吞吐
#   被窗口封顶」量成数（RTT 100 ms 时 1 MiB 往返 377 ms / 裸代理地板 144 ms）。
#   这一轮把窗口帧数变成**握手协商值**（`ClientHello.bulk_window_frames` /
#   `HostHelloOk.bulk_window_frames`，口径 `uulan_bulk::negotiate_window`，两端取 min），
#   本脚本就是它的验收：
#
#     A. 两端都写 64 → 两端都算出 Some(64)，1 MiB 全双工 sha256 一致
#     B. 一端 64、一端 24 → 两端都算出 Some(24)（取 min 的口径真的在线上生效）
#     C. 一端 64、另一端不声明（0，等同老端）→ Some(16)（老行为不变，不明着放大）
#     D. 50 ms 单向延迟下同一对端 16 帧 vs 64 帧：4 MiB 全双工都比 16 帧快，
#        且 sha256 都一致（放大窗口不是靠丢字节换吞吐）
#
#   判据来源：控制端自己打「按流窗口 : Some(n)」；被控端日志打
#   `window=Some(n) client_frames=… host_frames=…`。两边各自算一次、必须相同 ——
#   只信一边就等于没验协商。
#
# 用法：
#   UULAN_CLIENT=D:/uulan-bin/uulan-client.exe bash scripts/verify-bulk-window-negotiation-e2e.sh
# 可覆盖：KYLIN_TARGET / KYLIN_IP / UULAN_NEG_PORT(默认 21997) / UULAN_NEG_DELAY(默认 50)
# 退出码：0 = 全过；1 = 断言失败；2 = 台架/环境给不出判据（别记成产品缺陷）。
set -u

KY="${KYLIN_TARGET:-kylin}"
KY_IP="${KYLIN_IP:-192.168.223.15}"
PORT="${UULAN_NEG_PORT:-21997}"
E1=$((PORT + 1))
DELAY="${UULAN_NEG_DELAY:-50}"
LP=$((PORT + 100))
PX=$((PORT + 200))
CLI="${UULAN_CLIENT:?要 UULAN_CLIENT=<工作区外的 Windows 控制端 exe，D:/ 形式>}"
TS=$(date +%Y%m%d-%H%M%S)
WORKW="D:/uulan-bin/neg.$TS"
WORKK="/tmp/uulan-neg.$TS"
NO_COLOR=1
export NO_COLOR

PASS=0; FAIL=0; SKIP=0
ok()   { echo "  [PASS] $*"; PASS=$((PASS+1)); }
bad()  { echo "  [FAIL] $*"; FAIL=$((FAIL+1)); }
skip() { echo "  [SKIP] $*"; SKIP=$((SKIP+1)); }
env_out() { echo "  [环境] $*"; echo "退出码 2：这台机器此刻给不出这条判据，别把它记成产品缺陷"; exit 2; }

mkdir -p "$WORKW" || env_out "建不了 $WORKW"

# ── 真机侧：中继 + echo + 可重复重启的被控端（窗口帧数由参数给） ──────────────
cat > "$WORKW/remote.sh" <<'RSH'
#!/usr/bin/env bash
# remote.sh <WORK> <PORT> <E1> <HOSTWIN> <start|stophost|stopall>
set -u
BIN="$HOME/.local/uulan/src/target/release"
WORK="$1"; PORT="$2"; E1="$3"; HOSTWIN="$4"; ACTION="$5"
mkdir -p "$WORK" || exit 1

if [ "$ACTION" = "start" ]; then
  # echo 目标（全双工回吐）
  if [ ! -f "$WORK/echo.pid" ]; then
    cat > "$WORK/echo.py" <<'PY'
import socket, threading, sys
port = int(sys.argv[1])
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", port)); s.listen(64)
def h(c):
    try:
        while True:
            d = c.recv(65536)
            if not d: break
            c.sendall(d)
    except Exception: pass
    finally:
        try: c.close()
        except Exception: pass
while True:
    c, _ = s.accept()
    threading.Thread(target=h, args=(c,), daemon=True).start()
PY
    python3 "$WORK/echo.py" "$E1" >"$WORK/echo.log" 2>&1 &
    echo $! > "$WORK/echo.pid"
  fi
  # 中继（独立端口，与现网 21116 无关）
  if ! (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then
    "$BIN/uulan-server" --bind "0.0.0.0:$PORT" --state "$WORK/devices.json" >>"$WORK/server.log" 2>&1 &
    echo $! > "$WORK/srv.pid"
    for _ in $(seq 1 100); do (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && break; sleep 0.1; done
  fi
  # 被控端：合成采集源（这一轮只看 bulk 窗口，别让 x264 抢 CPU 搅浑延迟）
  if [ -f "$WORK/host.pid" ] && kill -0 "$(cat "$WORK/host.pid")" 2>/dev/null; then
    kill "$(cat "$WORK/host.pid")" 2>/dev/null
    for _ in $(seq 1 50); do kill -0 "$(cat "$WORK/host.pid")" 2>/dev/null || break; sleep 0.1; done
  fi
  : > "$WORK/host.log"
  RUST_LOG=info NO_COLOR=1 "$BIN/uulan-host" --server "127.0.0.1:$PORT" \
    --config "$WORK/host.json" --bulk-window "$HOSTWIN" \
    --capture "lavfi:testsrc2=size=640x360:rate=10" --name neg-bench >>"$WORK/host.log" 2>&1 &
  echo $! > "$WORK/host.pid"
  dev=""
  for _ in $(seq 1 300); do
    dev=$(grep -oE "设备 ID : [0-9]{6,}" "$WORK/host.log" 2>/dev/null | tail -1 | sed 's/.*: *//' | tr -d '[:space:]')
    [ -n "$dev" ] && break
    sleep 0.1
  done
  [ -n "$dev" ] || { echo NO_DEVICE; exit 1; }
  echo "$dev" > "$WORK/dev"
  sed -n 's/.*"password": *"\([^"]*\)".*/\1/p' "$WORK/host.json" | head -1 > "$WORK/pw"
  chmod 600 "$WORK/host.json" "$WORK/pw"
  [ -s "$WORK/pw" ] || { echo NO_PASSWORD; exit 1; }
  echo READY
  exit 0
fi

if [ "$ACTION" = "stophost" ]; then
  [ -f "$WORK/host.pid" ] && kill "$(cat "$WORK/host.pid")" 2>/dev/null
  for _ in $(seq 1 50); do kill -0 "$(cat "$WORK/host.pid" 2>/dev/null)" 2>/dev/null || break; sleep 0.1; done
  echo STOPPED
  exit 0
fi

if [ "$ACTION" = "stopall" ]; then
  for f in host.pid srv.pid echo.pid; do
    [ -f "$WORK/$f" ] && kill "$(cat "$WORK/$f")" 2>/dev/null
  done
  python3 - "$WORK" <<'PY'
import os, sys
w = sys.argv[1]
for n in ("pw", "host.json", "pw.raw"):
    try: os.remove(os.path.join(w, n))
    except OSError: pass
PY
  echo STOPPED
  exit 0
fi
RSH

cleanup() {
  [ -n "${CPID:-}" ] && kill "$CPID" 2>/dev/null
  [ -n "${PPID_:-}" ] && kill "$PPID_" 2>/dev/null
  ssh "$KY" "bash $WORKK/remote.sh $WORKK $PORT $E1 16 stopall" >/dev/null 2>&1
  python - "$WORKW" <<'PY' 2>/dev/null
import os, sys
w = sys.argv[1]
for n in os.listdir(w):
    if n.startswith("pw"):
        try: os.remove(os.path.join(w, n))
        except OSError: pass
PY
  return 0
}
trap cleanup EXIT

cat > "$WORKW/probe.py" <<'PY'
import json, socket, sys, threading, time
port = int(sys.argv[1]); n = int(sys.argv[2]); seed = int(sys.argv[3])
data = bytes(((i * 37 + 11 + seed * 97) % 256) for i in range(n))
out = {"ok": False, "sent": 0, "got": 0, "err": "", "ms": 0}
t0 = time.time()
try:
    s = socket.create_connection(("127.0.0.1", port), timeout=180)
    got = bytearray(); err = []
    def reader():
        try:
            while len(got) < n:
                b = s.recv(65536)
                if not b: break
                got.extend(b)
        except Exception as e:  # noqa
            err.append("read: %s" % e)
    t = threading.Thread(target=reader); t.start()
    for i in range(0, n, 65536):
        s.sendall(data[i:i + 65536])
    t.join(timeout=180)
    s.close()
    out.update(sent=n, got=len(got), err=";".join(err), ms=int((time.time() - t0) * 1000))
    out["ok"] = (bytes(got) == data and not err)
except Exception as e:
    out["err"] = str(e)
print(json.dumps(out))
sys.exit(0 if out["ok"] else 1)
PY

echo "== 0. 前置 =="
ssh -o ConnectTimeout=6 "$KY" true >/dev/null 2>&1 || env_out "$KY 不可达（先按失联判法分辨真假）"
[ -f "$CLI" ] || env_out "控制端 exe 不存在：$CLI（先 cargo build --release 再拷出工作区）"
BUSY=$(ssh "$KY" "ss -ltn 2>/dev/null | awk '{print \$4}' | grep -cE ':$PORT\$|:$E1\$'" 2>/dev/null | tail -1)
[ "${BUSY:-0}" = "0" ] || env_out "真机上 $PORT/$E1 已被占用（上一轮台架没清干净？）"
c=$(netstat -ano 2>/dev/null | grep -E "LISTENING" | grep -c "127.0.0.1:$LP " || true)
[ "${c:-0}" = "0" ] || env_out "本机 $LP 已被占用"
ssh "$KY" "mkdir -p $WORKK" || env_out "远端建不了 $WORKK"
scp -q "$WORKW/remote.sh" "$KY:$WORKK/" || env_out "scp 台架脚本上不去"

# 起一次台架拿设备 ID 与口令（后续每个阶段只换被控端窗口，不重装）
echo "== 1. 起远端台架（中继 :$PORT / echo :$E1 / 合成源被控端）=="
ssh "$KY" "bash $WORKK/remote.sh $WORKK $PORT $E1 64 start" > "$WORKW/bed.out" 2>&1
grep -q READY "$WORKW/bed.out" || { ssh "$KY" "tail -20 $WORKK/host.log" 2>/dev/null; env_out "台架没就绪"; }
DEV=$(ssh "$KY" "cat $WORKK/dev" 2>/dev/null | grep -oE "[0-9]{6,}" | head -1)
[ -n "$DEV" ] || env_out "拿不到设备 ID"
ssh "$KY" "cat $WORKK/pw" > "$WORKW/pw.raw" 2>/dev/null
grep -aoE '^[A-Za-z0-9]{8,16}$' "$WORKW/pw.raw" | head -1 > "$WORKW/pw.txt"
chmod 600 "$WORKW/pw.txt"
[ -s "$WORKW/pw.txt" ] || env_out "口令不合格式"
ok "台架就绪（设备 $DEV）"

start_host() {   # start_host <hostwin>
  ssh "$KY" "bash $WORKK/remote.sh $WORKK $PORT $E1 $1 start" > "$WORKW/bed-$1.out" 2>&1
  grep -q READY "$WORKW/bed-$1.out" || { bad "被控端（窗口 $1）没起来"; return 1; }
  return 0
}

# run_stage <名字> <host窗> <client窗|-> <延迟ms> <字节数> <期望端口窗口|none>
run_stage() {
  local name="$1" hw="$2" cw="$3" d="$4" nbytes="$5" expect="$6"
  local srvf="$KY_IP:$PORT"
  if [ "$d" != "0" ]; then
    python scripts/latency-proxy.py "$PX" "$KY_IP:$PORT" "$d" > "$WORKW/proxy-$name.out" 2>&1 &
    PPID_=$!
    for _ in $(seq 1 40); do grep -aq PROXY_READY "$WORKW/proxy-$name.out" 2>/dev/null && break; sleep 0.2; done
    grep -aq PROXY_READY "$WORKW/proxy-$name.out" 2>/dev/null || { bad "$name：延迟代理没起来"; PPID_=""; return 1; }
    srvf="127.0.0.1:$PX"
  fi
  local cargs=(--server "$srvf" --device "$DEV" --password-file "$WORKW/pw.txt"
               --forward "127.0.0.1:$LP:127.0.0.1:$E1" --seconds 12)
  [ "$cw" != "-" ] && cargs+=(--bulk-window "$cw")
  "$CLI" "${cargs[@]}" > "$WORKW/c-$name.out" 2> "$WORKW/c-$name.err" &
  CPID=$!
  local ready=NO
  for _ in $(seq 1 60); do
    grep -aq "转发已就绪" "$WORKW/c-$name.out" 2>/dev/null && { ready=YES; break; }
    sleep 0.3
  done
  if [ "$ready" != "YES" ]; then
    bad "$name：控制端没能就绪（$(head -c 200 "$WORKW/c-$name.err")）"
    kill "$CPID" 2>/dev/null; CPID=""; [ -n "${PPID_:-}" ] && kill "$PPID_" 2>/dev/null; PPID_=""
    return 1
  fi
  python "$WORKW/probe.py" "$LP" "$nbytes" 7 > "$WORKW/p-$name.json" 2>&1
  wait "$CPID" 2>/dev/null; local rc=$?
  CPID=""
  [ -n "${PPID_:-}" ] && kill "$PPID_" 2>/dev/null; PPID_=""

  # 控制端自己算出来的窗口
  local cli_win
  cli_win=$(sed -n 's/^按流窗口 : //p' "$WORKW/c-$name.out" | head -1)
  # 对端的窗口（真机日志；只找本条会话那条 INFO 行）
  local host_win
  host_win=$(ssh "$KY" "grep -a 'bulk 多路复用' $WORKK/host.log | tail -1" 2>/dev/null | tr -d '\r')
  echo "      $name：控制端[$cli_win] / 被控端[$host_win]"
  echo "      数据：$(cat "$WORKW/p-$name.json")"
  grep -q '"ok": true' "$WORKW/p-$name.json" 2>/dev/null \
    && ok "$name：$((nbytes / 1048576)) MiB 全双工 sha256 一致" \
    || bad "$name：数据不通过（$(cat "$WORKW/p-$name.json")）"
  [ "$rc" = "0" ] || bad "$name：控制端收工 rc=$rc"
  if [ "$expect" != "none" ]; then
    [ "$cli_win" = "Some($expect)" ] \
      && ok "$name：控制端算出 $cli_win" \
      || bad "$name：控制端算出的窗口是 [$cli_win]，期望 Some($expect)"
    echo "$host_win" | grep -q "window=Some($expect)" \
      && ok "$name：被控端也算出 window=Some($expect)（两端一致）" \
      || bad "$name：被控端日志是 [$host_win]，期望 window=Some($expect)"
  fi
}

echo "== 2. 阶段 A：两端都写 64 =="
start_host 64 || exit 1
sleep 0.5
run_stage A 64 64 0 1048576 64

echo "== 3. 阶段 B：一端 64、一端 24 → 取 min = 24 =="
start_host 64 || exit 1
sleep 0.5
run_stage B 64 24 0 1048576 24

echo "== 4. 阶段 C：控制端不声明（等同老端）→ 16，不被对面拉大 =="
start_host 64 || exit 1
sleep 0.5
run_stage C 64 - 0 1048576 16

echo "== 5. 阶段 D：${DELAY} ms 单向延迟下 16 帧 vs 64 帧（4 MiB）=="
start_host 16 || exit 1
sleep 0.5
run_stage D16 16 16 "$DELAY" 4194304 none
start_host 64 || exit 1
sleep 0.5
run_stage D64 64 64 "$DELAY" 4194304 none
T16=$(python - "$WORKW" <<'PY'
import json, sys
print(json.load(open(sys.argv[1] + "/p-D16.json"))["ms"])
PY
)
T64=$(python - "$WORKW" <<'PY'
import json, sys
print(json.load(open(sys.argv[1] + "/p-D64.json"))["ms"])
PY
)
echo "      16 帧：${T16} ms / 64 帧：${T64} ms（${DELAY} ms 单向延迟，4 MiB 全双工）"
awk -v a="$T16" -v b="$T64" 'BEGIN{exit !(b < a)}' \
  && ok "放大窗口真的更快（${T16} ms → ${T64} ms）—— 窗口不再是无条件的吞吐上限" \
  || bad "64 帧没有比 16 帧快（${T16} ms vs ${T64} ms）：协商值可能没落到信用上"
awk -v a="$T16" -v b="$T64" 'BEGIN{exit !(b*100 <= a*80)}' \
  && ok "提速幅度 ≥20%（$(awk -v a="$T16" -v b="$T64" 'BEGIN{printf "%.0f", (1-b/a)*100}')%）" \
  || skip "提速 <20%（$(awk -v a="$T16" -v b="$T64" 'BEGIN{printf "%.0f", (1-b/a)*100}')%）—— 只作观察，不判红（台架时间基有抖动）"

echo
echo "==== 结果：$PASS 通过 / $FAIL 失败 / $SKIP 跳过 ===="
echo "证据目录：$WORKW（真机侧 $WORKK）"
[ "$FAIL" = "0" ] && exit 0 || exit 1
