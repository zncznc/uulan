#!/usr/bin/env bash
# 广域网恶劣链路台架（抖动 / 周期性重传停顿 / 带宽上限）· 真机 192.168.223.15 验收
#
# 场景：
#   A. jitter+stall（delay=25ms, jitter=30ms, stall=200ms/2s）：1080p30 视频推流 8s
#      判据：退出码 0、不断线、画面相关系数 >= 0.99、实测 fps >= 20
#   B. rate=4000kbps（4 Mbps 窄带宽）：auto（~8Mbps）vs --quality fast（~1.2Mbps）对照
#      判据：两者均不断线、画面相关系数 >= 0.99；fast 档 fps >= 25 且 drop == 0
#   C. jitter+rate（delay=25ms, jitter=20ms, rate=8000kbps）：bulk 端口映射 4 MiB 全双工（w=16 vs w=64）
#      判据：sha256 全一致、不卡死、不丢字节
#
# 退出码：0 = 全过；1 = 断言失败；2 = 环境问题。
set -u

KYLIN="${UULAN_KYLIN:-mis@192.168.223.15}"
KYLIN_IP="${KYLIN#*@}"
RPORT="${UULAN_WAN_RPORT:-21986}"
ECHO_PORT="${UULAN_WAN_ECHO:-21987}"
LOCAL_PORT="${UULAN_WAN_LOCAL:-21988}"
PXA="${UULAN_WAN_PXA:-23381}"
SECS="${UULAN_WAN_SECS:-8}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TS="$(date +%Y%m%d-%H%M%S)"
WORK="${UULAN_STAGE:-C:/Users/chaoc/AppData/Local/uulan/stage/wanimp-$TS}"
RWORK="/tmp/uulan-wanimp.$TS"
NO_COLOR=1
export NO_COLOR

PASS=0; FAIL=0; SKIP=0
ok()      { echo "  [PASS] $*"; PASS=$((PASS+1)); }
bad()     { echo "  [FAIL] $*"; FAIL=$((FAIL+1)); }
env_out() { echo "  [环境] $*"; exit 2; }

SSH_OPTS="-o BatchMode=yes -o ConnectTimeout=5 -o StrictHostKeyChecking=no -o LogLevel=ERROR"
rsh() { ssh $SSH_OPTS "$KYLIN" "$@"; }

mkdir -p "$WORK" || env_out "建不了 $WORK"
cp -f "$ROOT/target/release/uulan-client.exe" "$WORK/uulan-client.exe" || env_out "拷 uulan-client.exe 失败"
echo "本机工作目录：$WORK  真机目录：$RWORK  中继 $KYLIN_IP:$RPORT"

PA=""; FPID=""
cleanup() {
  for p in "$FPID" "$PA"; do
    [ -n "$p" ] && kill "$p" 2>/dev/null || true
    [ -n "$p" ] && python "$ROOT/scripts/kill-pid.py" "$p" >/dev/null 2>&1 || true
  done
  rsh "bash '$RWORK/clean.sh' 2>/dev/null; rm -rf '$RWORK'" >/dev/null 2>&1 || true
  rm -f "$WORK/pw.txt" 2>/dev/null || true
}
trap cleanup EXIT

echo "== 0. 前置检查 =="
rsh "mkdir -p '$RWORK' && test -x \$HOME/.local/uulan/bin/uulan-host && test -x \$HOME/.local/uulan/bin/uulan-server" \
  || env_out "真机不可达或缺少二进制"

rsh "cat > '$RWORK/clean.sh' <<'EOF'
#!/usr/bin/env bash
for f in '$RWORK'/*.pid; do
  [ -f \"\$f\" ] || continue
  p=\$(cat \"\$f\" 2>/dev/null)
  [ -n \"\$p\" ] && kill \"\$p\" 2>/dev/null || true
done
sleep 0.3
for f in '$RWORK'/*.pid; do
  [ -f \"\$f\" ] || continue
  p=\$(cat \"\$f\" 2>/dev/null)
  [ -n \"\$p\" ] && kill -9 \"\$p\" 2>/dev/null || true
done
EOF
chmod +x '$RWORK/clean.sh'"

rsh "DISPLAY=:0 ffmpeg -y -loglevel error -f x11grab -video_size 1920x1080 -i :0.0 -frames:v 1 '$RWORK/ref.ppm'" \
  || env_out "真机 :0 抓参考截图失败"
scp $SSH_OPTS "$KYLIN:$RWORK/ref.ppm" "$WORK/ref.ppm" >/dev/null || env_out "拉取参考截图失败"
ok "已抓真机 :0 参考截图"

echo "== 1. 真机起独立中继 :$RPORT + echo :$ECHO_PORT + 被控端（窗口 64）=="
rsh "cat > '$RWORK/echo.py' <<'PY'
import socket, sys, threading
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(('127.0.0.1', int(sys.argv[1]))); s.listen(32)
def h(c):
    try:
        while True:
            b = c.recv(65536)
            if not b: break
            c.sendall(b)
    except Exception: pass
    finally:
        try: c.close()
        except Exception: pass
while True:
    c, _ = s.accept()
    threading.Thread(target=h, args=(c,), daemon=True).start()
PY
nohup python3 '$RWORK/echo.py' $ECHO_PORT >'$RWORK/echo.log' 2>&1 </dev/null & echo \$! >'$RWORK/echo.pid'
RUST_LOG=info nohup \$HOME/.local/uulan/bin/uulan-server --bind 0.0.0.0:$RPORT --state '$RWORK/devices.json' >'$RWORK/server.log' 2>&1 </dev/null & echo \$! >'$RWORK/server.pid'
sleep 0.4
DISPLAY=:0 RUST_LOG=info NO_COLOR=1 nohup \$HOME/.local/uulan/bin/uulan-host --config '$RWORK/host.json' --server 127.0.0.1:$RPORT --bulk-window 64 --name wanimp-bench >'$RWORK/host.log' 2>&1 </dev/null & echo \$! >'$RWORK/host.pid'"

DEVID=""
for _ in $(seq 1 80); do
  DEVID="$(rsh "grep -oE '设备 ID : [0-9]{6,}' '$RWORK/host.log' 2>/dev/null | tail -1 | sed 's/.*: *//' | tr -d '[:space:]'")"
  [ -n "$DEVID" ] && break
  sleep 0.15
done
[ -n "$DEVID" ] || env_out "真机被控端未上线"
rsh "sed -n 's/.*\"password\": *\"\([^\"]*\)\".*/\1/p' '$RWORK/host.json' | head -1" | tr -d '\r\n' > "$WORK/pw.txt"
[ -s "$WORK/pw.txt" ] || env_out "读不到自动生成的口令"
ok "真机测试台就绪（device_id=$DEVID）"

start_proxy() {
  local tag="$1"; shift
  [ -n "$PA" ] && kill "$PA" 2>/dev/null || true
  [ -n "$PA" ] && python "$ROOT/scripts/kill-pid.py" "$PA" >/dev/null 2>&1 || true
  PA=""
  python "$ROOT/scripts/latency-proxy.py" "$PXA" "$KYLIN_IP:$RPORT" "$@" >"$WORK/proxy-$tag.log" 2>&1 &
  PA=$!
  for _ in $(seq 1 40); do
    grep -aq PROXY_READY "$WORK/proxy-$tag.log" 2>/dev/null && break
    sleep 0.1
  done
  grep -aq PROXY_READY "$WORK/proxy-$tag.log" 2>/dev/null || { bad "代理 $tag 未就绪"; return 1; }
  return 0
}

run_video() {
  local label="$1"
  local extra="$2"
  local min_fps="$3"
  local max_drop="$4"
  local out="$WORK/vid-$label.log"
  local ppm="$WORK/vid-$label.ppm"
  rm -f "$out" "$ppm"
  "$WORK/uulan-client.exe" \
    --server "127.0.0.1:$PXA" \
    --device "$DEVID" \
    --password-file "$WORK/pw.txt" \
    --headless --seconds "$SECS" \
    --save-frame "$ppm" \
    $extra >"$out" 2>&1
  local rc=$?
  if [ "$rc" -ne 0 ]; then
    bad "$label 退出码 $rc（$(tail -n 3 "$out" | tr '\r\n' ' ')）"
    return 1
  fi
  local summary
  summary="$(python - "$out" "$ppm" "$WORK/ref.ppm" "$ROOT/scripts/compare-ppm.py" <<'PY'
import re, subprocess, sys
txt = open(sys.argv[1], encoding="utf-8", errors="replace").read()
def grab(pat):
    m = re.search(pat, txt)
    return m.group(1) if m else "NA"
aus = grab(r"收到 AU\s*:\s*(\d+)")
dec = grab(r"解出帧\s*:\s*(\d+)")
drop = grab(r"解码丢帧\s*:\s*(\d+)")
fps = grab(r"实测帧率\s*:\s*([0-9.]+)")
ff = grab(r"首帧延迟\s*:\s*(\d+)\s*ms")
kbps = grab(r"平均码率\s*:\s*(\d+)\s*kbps")
cmp_out = subprocess.check_output([sys.executable, sys.argv[4], sys.argv[3], sys.argv[2]], text=True, stderr=subprocess.STDOUT)
cm = re.search(r"corr[=:]\s*([0-9.]+)", cmp_out, re.I) or re.search(r"相关系数\s*[:：]?\s*([0-9.]+)", cmp_out)
corr = cm.group(1) if cm else "NA"
print(f"aus={aus} dec={dec} drop={drop} fps={fps} first_ms={ff} kbps={kbps} corr={corr}")
PY
)"
  echo "  [$label] $summary"
  python - "$summary" "$min_fps" "$max_drop" <<'PY'
import re, sys
kv = dict(re.findall(r"(\w+)=([^\s]+)", sys.argv[1]))
min_fps = float(sys.argv[2]); max_drop = int(sys.argv[3])
fps = float(kv["fps"]) if kv.get("fps","NA") != "NA" else 0.0
drop = int(kv["drop"]) if kv.get("drop","").isdigit() else 999
corr = float(kv["corr"]) if kv.get("corr","NA") != "NA" else 0.0
sys.exit(0 if (fps >= min_fps and drop <= max_drop and corr >= 0.99) else 1)
PY
  [ $? -eq 0 ] && ok "$label 达标（$summary）" || bad "$label 未达标（$summary）"
}

run_bulk() {
  local label="$1"
  local win="$2"
  local out="$WORK/bulk-$label.log"
  rm -f "$out"
  "$WORK/uulan-client.exe" \
    --server "127.0.0.1:$PXA" \
    --device "$DEVID" \
    --password-file "$WORK/pw.txt" \
    --bulk-window "$win" \
    --forward "127.0.0.1:${LOCAL_PORT}:127.0.0.1:${ECHO_PORT}" >"$out" 2>&1 &
  FPID=$!
  for _ in $(seq 1 60); do
    python -c "import socket; s=socket.create_connection(('127.0.0.1',$LOCAL_PORT),0.3); s.close()" >/dev/null 2>&1 && break
    sleep 0.15
  done
  local res
  res="$(python - "$LOCAL_PORT" <<'PY'
import hashlib, json, socket, sys, threading, time
port = int(sys.argv[1]); n = 4 * 1024 * 1024
data = bytes(((i * 31 + 7) % 256) for i in range(n))
t0 = time.monotonic()
s = socket.create_connection(("127.0.0.1", port), timeout=120)
got = bytearray(); err = []
def rd():
    try:
        while len(got) < n:
            b = s.recv(65536)
            if not b: break
            got.extend(b)
    except Exception as e:
        err.append(str(e))
t = threading.Thread(target=rd); t.start()
for i in range(0, n, 65536):
    s.sendall(data[i:i+65536])
t.join(timeout=120)
s.close()
ms = int((time.monotonic() - t0) * 1000)
ok = (len(got) == n and hashlib.sha256(got).digest() == hashlib.sha256(data).digest() and not err)
print(json.dumps({"ok": ok, "ms": ms, "mbps": round(n * 8 / max(ms,1) / 1000.0, 2), "got": len(got), "err": err}))
sys.exit(0 if ok else 1)
PY
)"
  local rc=$?
  kill "$FPID" 2>/dev/null || true
  python "$ROOT/scripts/kill-pid.py" "$FPID" >/dev/null 2>&1 || true
  FPID=""
  echo "  [$label] $res"
  [ "$rc" -eq 0 ] && ok "$label 4MiB 全双工一致（$res）" || bad "$label 失败（$res）"
}

echo "== 2. 场景 A：高抖动 + 周期重传停顿（delay=25ms, jitter=30ms, stall=200ms/2s）=="
start_proxy "jitter-stall" 25 --jitter-ms 30 --stall-every-s 2 --stall-ms 200 && \
  run_video "jitter-stall-auto" "" 20.0 10

echo "== 3. 场景 B：窄带宽 4 Mbps（delay=25ms, rate=4000kbps）auto vs fast =="
start_proxy "rate4m" 25 --rate-kbps 4000 && {
  run_video "rate4m-auto" "" 5.0 999
  run_video "rate4m-fast" "--quality fast" 25.0 0
}

echo "== 4. 场景 C：抖动 + 限速 8 Mbps（delay=25ms, jitter=20ms, rate=8000kbps）bulk 4MiB 全双工 =="
start_proxy "bulk-wan" 25 --jitter-ms 20 --rate-kbps 8000 && {
  run_bulk "wan-w16" 16
  run_bulk "wan-w64" 64
}

echo "=========================================="
echo "结果：$PASS 通过 / $FAIL 失败 / $SKIP 跳过"
echo "证据目录：$WORK"
[ "$FAIL" -eq 0 ]
