#!/usr/bin/env bash
# bulk 滑动窗口在「高延迟链路」上的表现（**开发机 git-bash 跑，需与真机同二层**）：
#
#   M8 的按流窗口是常数 16 帧（64 KiB 一帧 ⇒ **1 MiB 在途**）。内网 RTT < 1 ms 时
#   1 MiB 在途足够；跨广域网就不一定了 —— 这一轮把「RTT 变大时吞吐到底掉到多少」
#   量出来，而不是靠推算。
#
#   做法：控制端 ↔ 中继这一腿插一个**用户态延迟代理**（scripts/latency-proxy.py，
#   协议字节盲转发、两侧都能过，加密仍在两端），档位 0 / 25 / 50 / 100 ms 单向，
#   每档跑两条全双工流（1 MiB + 4 MiB），判据：
#     A. 每一档每条流 sha256 一致（高 RTT 下信用不会把数据弄丢/弄乱/卡死）
#     B. 每一档都能在有限时间内跑完（没有「信用等不到 → 挂到会话收尾」）
#     C. 实测吞吐与「窗口 / RTT」模型同量级（同量级=模型对，账号/窗口真的在起作用；
#        远低于模型说明别的地方也在等，远高于模型说明有额外的流水线）
#
#   受限条件：本机是「控制端」侧，延迟只加在控制端↔中继这一腿 —— 真实广域网是两端都慢；
#   所以这里的 RTT 是**下界**，量出来的吞吐是**上界**。报告里必须这么写。
#
# 用法：
#   UULAN_CLIENT=D:/uulan-bin/uulan-client.exe bash scripts/verify-bulk-window-wan-e2e.sh
# 可覆盖：KYLIN_TARGET / KYLIN_IP / UULAN_WAN_PORT(默认 21991) / UULAN_WAN_LP(默认 23371)
#         UULAN_WAN_DELAYS("0 25 50 100")
# 退出码：0 = 全过；1 = 断言失败；2 = 台架/环境给不出判据（别记成产品缺陷）。
set -u

KY="${KYLIN_TARGET:-kylin}"
KY_IP="${KYLIN_IP:-192.168.223.15}"
PORT="${UULAN_WAN_PORT:-21991}"
E1=$((PORT + 1))
LP="${UULAN_WAN_LP:-23371}"
PXBASE=23381
DELAYS="${UULAN_WAN_DELAYS:-0 25 50 100}"
WINDOW_BYTES=1048576                    # uulan_bulk::STREAM_WINDOW(16) × 64 KiB
CLI="${UULAN_CLIENT:?要 UULAN_CLIENT=<工作区外的 Windows 控制端 exe，D:/ 形式>}"
TS=$(date +%Y%m%d-%H%M%S)
WORKW="D:/uulan-bin/wan.$TS"
WORKK="/tmp/uulan-wan.$TS"
NO_COLOR=1
export NO_COLOR

PASS=0; FAIL=0; SKIP=0
ok()   { echo "  [PASS] $*"; PASS=$((PASS+1)); }
bad()  { echo "  [FAIL] $*"; FAIL=$((FAIL+1)); }
skip() { echo "  [SKIP] $*"; SKIP=$((SKIP+1)); }
env_out() { echo "  [环境] $*"; echo "退出码 2：这台机器此刻给不出这条判据，别把它记成产品缺陷"; exit 2; }

mkdir -p "$WORKW" || env_out "建不了 $WORKW"

cat > "$WORKW/remote.sh" <<'RSH'
#!/usr/bin/env bash
set -u
BIN="$HOME/.local/uulan/src/target/release"
WORK="$1"; PORT="$2"; E1="$3"
mkdir -p "$WORK" || exit 1
cat > "$WORK/echo.py" <<'PY'
import socket, threading, sys
port = int(sys.argv[1])
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", port)); s.listen(64)

def h(c):
    try:
        while True:
            d = c.recv(65536)
            if not d:
                break
            c.sendall(d)
    except Exception:
        pass
    finally:
        try: c.close()
        except Exception: pass

while True:
    c, _ = s.accept()
    threading.Thread(target=h, args=(c,), daemon=True).start()
PY
echo "$PORT" > "$WORK/port"
: > "$WORK/supid"
python3 "$WORK/echo.py" "$E1" >"$WORK/echo.log" 2>&1 &
echo $! >> "$WORK/supid"
(
  while true; do
    "$BIN/uulan-server" --bind "0.0.0.0:$PORT" --state "$WORK/devices.json" >>"$WORK/server.log" 2>&1 &
    sp=$!
    echo "$sp" > "$WORK/srvpid"
    wait "$sp"
    sleep 0.3
  done
) &
echo $! >> "$WORK/supid"
for _ in $(seq 1 100); do
  (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && { exec 3>&-; break; }
  sleep 0.1
done
# 合成采集源：这一轮只关心 bulk 的窗口，别让三个 x264 抢 CPU、把延迟数量级搅浑
RUST_LOG=info NO_COLOR=1 "$BIN/uulan-host" --server "127.0.0.1:$PORT" --config "$WORK/host.json" \
  --capture "lavfi:testsrc2=size=640x360:rate=10" --name wan-bench >"$WORK/host.log" 2>&1 &
echo $! >> "$WORK/supid"
echo $! > "$WORK/hostpid"
dev=""
for _ in $(seq 1 300); do
  dev=$(grep -oE "设备 ID : [0-9]{6,}" "$WORK/host.log" 2>/dev/null | tail -1 | sed 's/.*: *//' | tr -d '[:space:]')
  [ -n "$dev" ] && break
  sleep 0.1
done
[ -n "$dev" ] || { echo "NO_DEVICE"; exit 1; }
echo "$dev" > "$WORK/dev"
sed -n 's/.*"password": *"\([^"]*\)".*/\1/p' "$WORK/host.json" | head -1 > "$WORK/pw"
chmod 600 "$WORK/host.json" "$WORK/pw"
[ -s "$WORK/pw" ] || { echo "NO_PASSWORD"; exit 1; }
touch "$WORK/ready"
RSH

cat > "$WORKW/clean.sh" <<'CLS'
#!/usr/bin/env bash
W="$1"
if [ -f "$W/supid" ]; then
  while read -r pid; do kill "$pid" 2>/dev/null; done < "$W/supid"
fi
if [ -f "$W/srvpid" ]; then kill "$(cat "$W/srvpid")" 2>/dev/null; fi
if [ -f "$W/port" ]; then
  P=$(cat "$W/port")
  for pid in $(pgrep -f "uulan-server --bind 0.0.0.0:$P" 2>/dev/null); do kill "$pid" 2>/dev/null; done
fi
for pid in $(pgrep -x uulan-client 2>/dev/null); do
  tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -q "uulan-wan" && kill "$pid" 2>/dev/null
done
python3 - "$W" <<'PY'
import os, sys
w = sys.argv[1]
for n in ("pw", "host.json", "pw.raw"):
    try: os.remove(os.path.join(w, n))
    except OSError: pass
PY
CLS

cleanup() {
  [ -n "${CPID:-}" ] && kill "$CPID" 2>/dev/null
  [ -n "${PPID_:-}" ] && kill "$PPID_" 2>/dev/null
  ssh "$KY" "bash $WORKK/clean.sh $WORKK" >/dev/null 2>&1
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
    s = socket.create_connection(("127.0.0.1", port), timeout=120)
    got = bytearray(); err = []

    def reader():
        try:
            while len(got) < n:
                b = s.recv(65536)
                if not b:
                    break
                got.extend(b)
        except Exception as e:  # noqa
            err.append("read: %s" % e)

    t = threading.Thread(target=reader); t.start()
    for i in range(0, n, 65536):
        s.sendall(data[i:i + 65536])
    t.join(timeout=120)
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
scp -q "$WORKW/remote.sh" "$WORKW/clean.sh" "$KY:$WORKK/" || env_out "scp 台架脚本上不去"

echo "== 1. 起远端台架（中继 :$PORT + echo :$E1 + 被控端合成源）=="
ssh "$KY" "setsid bash $WORKK/remote.sh $WORKK $PORT $E1 > $WORKK/bed.out 2>&1 < /dev/null &" >/dev/null 2>&1 || true
R=NO
for _ in $(seq 1 60); do
  R=$(ssh "$KY" "[ -f $WORKK/ready ] && echo YES || echo NO" 2>/dev/null | tail -1)
  [ "$R" = "YES" ] && break
  sleep 0.4
done
[ "$R" = "YES" ] || { ssh "$KY" "tail -20 $WORKK/host.log" 2>/dev/null; env_out "台架没就绪"; }
DEV=$(ssh "$KY" "cat $WORKK/dev" 2>/dev/null | grep -oE "[0-9]{6,}" | head -1)
[ -n "$DEV" ] || env_out "拿不到设备 ID"
ssh "$KY" "cat $WORKK/pw" > "$WORKW/pw.raw" 2>/dev/null
grep -aoE '^[A-Za-z0-9]{8,16}$' "$WORKW/pw.raw" | head -1 > "$WORKW/pw.txt"
chmod 600 "$WORKW/pw.txt"
[ -s "$WORKW/pw.txt" ] || env_out "口令不合格式"
ok "台架就绪（设备 $DEV）"

run_level() {   # run_level <delay_ms>
  local d="$1" px=$((PXBASE + d / 25)) srvf
  if [ "$d" = "0" ]; then
    srvf="$KY_IP:$PORT"; PPID_=""
  else
    python scripts/latency-proxy.py "$px" "$KY_IP:$PORT" "$d" > "$WORKW/proxy-$d.out" 2>&1 &
    PPID_=$!
    for _ in $(seq 1 40); do grep -aq PROXY_READY "$WORKW/proxy-$d.out" 2>/dev/null && break; sleep 0.2; done
    grep -aq PROXY_READY "$WORKW/proxy-$d.out" 2>/dev/null || { bad "延迟代理 $d ms 没起来"; return 1; }
    srvf="127.0.0.1:$px"
  fi
  "$CLI" --server "$srvf" --device "$DEV" --password-file "$WORKW/pw.txt" \
    --forward "127.0.0.1:$LP:127.0.0.1:$E1" --seconds 10 \
    > "$WORKW/fwd-$d.out" 2> "$WORKW/fwd-$d.err" &
  CPID=$!
  local ready=NO
  for _ in $(seq 1 40); do
    grep -aq "转发已就绪" "$WORKW/fwd-$d.out" 2>/dev/null && { ready=YES; break; }
    sleep 0.3
  done
  if [ "$ready" != "YES" ]; then
    bad "延迟 $d ms：控制端没能就绪（$(head -c 200 "$WORKW/fwd-$d.err")）"
    kill "$CPID" 2>/dev/null; CPID=""; [ -n "$PPID_" ] && kill "$PPID_" 2>/dev/null; PPID_=""
    return 1
  fi
  python "$WORKW/probe.py" "$LP" 1048576 1 > "$WORKW/p1-$d.json" 2>&1
  python "$WORKW/probe.py" "$LP" 4194304 2 > "$WORKW/p4-$d.json" 2>&1
  wait "$CPID" 2>/dev/null; local rc=$?
  CPID=""
  [ -n "$PPID_" ] && kill "$PPID_" 2>/dev/null; PPID_=""
  echo "      $d ms：$(cat "$WORKW/p1-$d.json") / $(cat "$WORKW/p4-$d.json")"
  echo "      统计：$(grep -a '端口映射结束' "$WORKW/fwd-$d.out" | tail -1)"
  [ "$rc" = "0" ] || bad "延迟 $d ms：控制端收工 rc=$rc"
  return 0
}

echo "== 2. 逐档跑（单向延迟：$DELAYS ms）=="
for d in $DELAYS; do
  echo "   档位 $d ms（RTT ≈ $((d * 2)) ms）"
  run_level "$d"
done

echo "== 3. 判据 =="
python - "$WORKW" "$WINDOW_BYTES" "$DELAYS" <<'PY' > "$WORKW/verdict.txt" 2>&1
import json, os, sys
w, win = sys.argv[1], int(sys.argv[2])
delays = [int(x) for x in sys.argv[3].split()]
rows = []
for d in delays:
    row = {"delay": d}
    for tag, size in (("p1", 1048576), ("p4", 4194304)):
        try:
            j = json.load(open("%s/%s-%d.json" % (w, tag, d)))
        except Exception:
            row[tag] = None
            continue
        row[tag] = j
    rows.append(row)
for r in rows:
    d = r["delay"]; rtt = 2 * d / 1000.0
    okp = all(r[t] and r[t].get("ok") for t in ("p1", "p4"))
    print("delay=%d ok=%d" % (d, 1 if okp else 0))
    if not okp:
        continue
    for t, size in (("p1", 1048576), ("p4", 4194304)):
        ms = r[t]["ms"]
        # 全双工：两个方向各 size 字节，吞吐按「单方向 size / 时间」算
        mbs = size / (ms / 1000.0) / 1e6 if ms else 0.0
        print("  %s ms=%d bytes=%d 单方向吞吐=%.2f MB/s（%.0f Mbps）" % (t, ms, size, mbs, mbs * 8))
        if d > 0:
            cap = win / rtt / 1e6 if rtt else 0.0
            print("     模型上限（窗口 %.0f KiB / RTT %.0f ms）= %.2f MB/s，实测/模型 = %.2f"
                  % (win / 1024.0, rtt * 1000, cap, mbs / cap if cap else 0.0))
try:
    st = open("%s/fwd-100.out" % w).read()
    print("fwd100_stats=%s" % [l for l in st.splitlines() if "端口映射结束" in l][-1:])
except Exception:
    pass
PY
cat "$WORKW/verdict.txt" | sed 's/^/      /'
for d in $DELAYS; do
  grep -q "^delay=$d ok=1" "$WORKW/verdict.txt" && ok "延迟 $d ms：1 MiB + 4 MiB 两条流 sha256 均一致" \
    || bad "延迟 $d ms：有流不通过（见上面 JSON）"
  S=$(cat "$WORKW/fwd-$d.out" 2>/dev/null | grep -a "端口映射结束" | tail -1)
  echo "$S" | grep -qE "上行 5242880 字节 / 下行 5242880 字节" \
    && ok "延迟 $d ms：统计上行 = 下行 = 5 MiB（两条流都走完）" || bad "延迟 $d ms：统计字节不符：$S"
done
# 吞吐随 RTT 单调下降（模型行为的定性判据）：50 ms 档必须明显低于 0 ms 档
T0=$(awk '/^delay=0/{f=1;next} f&&/p1 ms=/{print $2;exit}' "$WORKW/verdict.txt" | sed 's/ms=//')
T50=$(awk '/^delay=50/{f=1;next} f&&/p1 ms=/{print $2;exit}' "$WORKW/verdict.txt" | sed 's/ms=//')
if [ -n "${T0:-}" ] && [ -n "${T50:-}" ]; then
  awk -v a="$T0" -v b="$T50" 'BEGIN{exit !(b>a*3)}' \
    && ok "RTT 变大后吞吐真的降下来了（1 MiB 往返 0 ms:${T0}ms → 50 ms:${T50}ms）—— 窗口/信用在起作用" \
    || bad "50 ms 档没有变慢（0 ms:${T0}ms / 50 ms:${T50}ms）—— 窗口可能没生效"
fi
NCL=$(ssh "$KY" "pgrep -c -x uulan-client || true" 2>/dev/null | tail -1)
[ "${NCL:-0}" = "0" ] && ok "真机上没有残留的 uulan-client" || skip "真机上还有 ${NCL} 个 uulan-client"

echo
echo "==== 结果：$PASS 通过 / $FAIL 失败 / $SKIP 跳过 ===="
echo "证据目录：$WORKW（真机侧 $WORKK）"
echo "口径提醒：延迟只加在控制端↔中继这一腿，RTT 是下界、吞吐是上界。"
[ "$FAIL" = "0" ] && exit 0 || exit 1
