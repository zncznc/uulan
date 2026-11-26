#!/usr/bin/env bash
# 两跳延迟下的视频首帧与卡顿（真机 192.168.223.15 + Windows 控制端）
#
# 拓扑：
#   Windows 控制端 --(代理A 127.0.0.1:23371, d ms)--> 真机中继 192.168.223.15:21985
#                  <--(代理B 127.0.0.1:23372, d ms)-- 真机被控端 (DISPLAY=:0 1920x1080@30fps)
#   d=0 时两端直连中继（对照组）；总路程 RTT ≈ 4d ms。
#
# 口径：用户态延迟代理只引入固定单向延迟，不含丢包/带宽上限/抖动；
#       不动真机正式 uulan-host / uulan-server 服务与配置，独立端口 + 临时目录，退出即清。
#
# 退出码：0 = 全过；1 = 断言失败；2 = 环境问题。
set -u

KYLIN="${UULAN_KYLIN:-mis@192.168.223.15}"
KYLIN_IP="${KYLIN#*@}"
RPORT="${UULAN_VDL_RPORT:-21985}"     # 真机临时中继端口
PXA="${UULAN_VDL_PXA:-23371}"         # 本机控制端腿代理
PXB="${UULAN_VDL_PXB:-23372}"         # 真机被控端腿代理
DELAYS="${UULAN_VDL_DELAYS:-0 25 50 100}"
SECS="${UULAN_VDL_SECS:-8}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TS="$(date +%Y%m%d-%H%M%S)"
WORK="${UULAN_STAGE:-C:/Users/chaoc/AppData/Local/uulan/stage/vdl-$TS}"
RWORK="/tmp/uulan-vdl.$TS"
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
echo "本机工作目录：$WORK  真机目录：$RWORK  中继 $KYLIN_IP:$RPORT  档位：$DELAYS ms"

PA=""
cleanup() {
  [ -n "$PA" ] && kill "$PA" 2>/dev/null || true
  [ -n "$PA" ] && python "$ROOT/scripts/kill-pid.py" "$PA" >/dev/null 2>&1 || true
  rsh "bash '$RWORK/clean.sh' 2>/dev/null; rm -rf '$RWORK'" >/dev/null 2>&1 || true
  rm -f "$WORK/pw.txt" 2>/dev/null || true
}
trap cleanup EXIT

echo "== 0. 前置检查 =="
rsh "mkdir -p '$RWORK' && test -x \$HOME/.local/uulan/bin/uulan-host && test -x \$HOME/.local/uulan/bin/uulan-server" \
  || env_out "真机不可达或缺少二进制"
scp $SSH_OPTS "$ROOT/scripts/latency-proxy.py" "$KYLIN:$RWORK/latency-proxy.py" >/dev/null \
  || env_out "拷 latency-proxy.py 到真机失败"

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

# 真机参考截图（:0 实机 x11grab）
rsh "DISPLAY=:0 ffmpeg -y -loglevel error -f x11grab -video_size 1920x1080 -i :0.0 -frames:v 1 '$RWORK/ref.ppm'" \
  || env_out "真机 :0 抓参考截图失败"
scp $SSH_OPTS "$KYLIN:$RWORK/ref.ppm" "$WORK/ref.ppm" >/dev/null || env_out "拉取参考截图失败"
ok "已抓真机 :0 参考截图"

echo "== 1. 真机起独立中继 :$RPORT =="
rsh "RUST_LOG=info nohup \$HOME/.local/uulan/bin/uulan-server --bind 0.0.0.0:$RPORT --state '$RWORK/devices.json' >'$RWORK/server.log' 2>&1 </dev/null & echo \$! >'$RWORK/server.pid'"
sleep 0.5
rsh "ss -ltn | grep -q ':$RPORT '" && ok "真机独立中继监听 :$RPORT" || env_out "真机独立中继未起来"

start_host_for_delay() {
  local d="$1"
  # 停掉旧 host 与代理 B
  rsh "for k in host proxyB; do [ -f '$RWORK/'\$k'.pid' ] && kill \$(cat '$RWORK/'\$k'.pid') 2>/dev/null || true; rm -f '$RWORK/'\$k'.pid'; done; sleep 0.3"
  local hserver="127.0.0.1:$RPORT"
  if [ "$d" != "0" ]; then
    rsh "nohup python3 '$RWORK/latency-proxy.py' $PXB 127.0.0.1:$RPORT $d >'$RWORK/proxyB-$d.log' 2>&1 </dev/null & echo \$! >'$RWORK/proxyB.pid'"
    for _ in $(seq 1 40); do
      rsh "grep -q PROXY_READY '$RWORK/proxyB-$d.log' 2>/dev/null" && break
      sleep 0.1
    done
    rsh "grep -q PROXY_READY '$RWORK/proxyB-$d.log' 2>/dev/null" || { bad "真机代理B d=$d 未就绪"; return 1; }
    hserver="127.0.0.1:$PXB"
  fi
  rsh "DISPLAY=:0 RUST_LOG=info NO_COLOR=1 nohup \$HOME/.local/uulan/bin/uulan-host --config '$RWORK/host.json' --server '$hserver' --name vdl-bench >'$RWORK/host-$d.log' 2>&1 </dev/null & echo \$! >'$RWORK/host.pid'"
  DEVID=""
  for _ in $(seq 1 80); do
    DEVID="$(rsh "grep -oE '设备 ID : [0-9]{6,}' '$RWORK/host-$d.log' 2>/dev/null | tail -1 | sed 's/.*: *//' | tr -d '[:space:]'")"
    [ -n "$DEVID" ] && break
    sleep 0.15
  done
  [ -n "$DEVID" ] || { bad "真机被控端 d=$d 未上线（$(rsh "tail -n 3 '$RWORK/host-$d.log' 2>/dev/null" | tr '\r\n' ' ')）"; return 1; }
  if [ ! -s "$WORK/pw.txt" ]; then
    rsh "sed -n 's/.*\"password\": *\"\([^\"]*\)\".*/\1/p' '$RWORK/host.json' | head -1" | tr -d '\r\n' > "$WORK/pw.txt"
    [ -s "$WORK/pw.txt" ] || { bad "读不到被控端自动生成的口令"; return 1; }
  fi
  return 0
}

start_client_proxy() {
  local d="$1"
  [ -n "$PA" ] && kill "$PA" 2>/dev/null || true
  [ -n "$PA" ] && python "$ROOT/scripts/kill-pid.py" "$PA" >/dev/null 2>&1 || true
  PA=""
  if [ "$d" = "0" ]; then
    CSERVER="$KYLIN_IP:$RPORT"
    return 0
  fi
  python "$ROOT/scripts/latency-proxy.py" "$PXA" "$KYLIN_IP:$RPORT" "$d" >"$WORK/proxyA-$d.log" 2>&1 &
  PA=$!
  for _ in $(seq 1 40); do
    grep -aq PROXY_READY "$WORK/proxyA-$d.log" 2>/dev/null && break
    sleep 0.1
  done
  grep -aq PROXY_READY "$WORK/proxyA-$d.log" 2>/dev/null || { bad "本机代理A d=$d 未就绪"; return 1; }
  CSERVER="127.0.0.1:$PXA"
  return 0
}

run_case() {
  local label="$1"
  local extra_args="$2"
  local out="$WORK/client-$label.log"
  local ppm="$WORK/frame-$label.ppm"
  rm -f "$out" "$ppm"
  local e2e_ms
  e2e_ms="$(python - "$WORK/uulan-client.exe" "$CSERVER" "$DEVID" "$WORK/pw.txt" "$SECS" "$ppm" "$out" "$extra_args" <<'PY'
import shlex, subprocess, sys, time
exe, srv, dev, pwf, secs, ppm, outp, extra = sys.argv[1:9]
cmd = [exe, "--server", srv, "--device", dev, "--password-file", pwf,
       "--headless", "--seconds", secs, "--save-frame", ppm] + shlex.split(extra)
t0 = time.monotonic()
first_e2e = -1
p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, encoding="utf-8", errors="replace")
with open(outp, "w", encoding="utf-8") as f:
    for line in p.stdout:
        f.write(line)
        if first_e2e < 0 and "第 1 帧" in line:
            first_e2e = int((time.monotonic() - t0) * 1000)
rc = p.wait()
print(f"{rc} {first_e2e}")
PY
)"
  local rc="${e2e_ms%% *}"
  local first_e2e="${e2e_ms#* }"
  if [ "$rc" -ne 0 ]; then
    bad "$label 控制端退出码 $rc（$(tail -n 3 "$out" | tr '\r\n' ' ')）"
    return 1
  fi
  local summary
  summary="$(python - "$out" "$ppm" "$WORK/ref.ppm" "$ROOT/scripts/compare-ppm.py" "$first_e2e" <<'PY'
import re, subprocess, sys
txt = open(sys.argv[1], encoding="utf-8", errors="replace").read()
def grab(pat):
    m = re.search(pat, txt)
    return m.group(1) if m else "NA"
aus = grab(r"收到 AU\s*:\s*(\d+)")
dec = grab(r"解出帧\s*:\s*(\d+)")
drop = grab(r"解码丢帧\s*:\s*(\d+)")
f1_drop = grab(r"第 1 帧：.*?丢包\s+(\d+)")
steady_drop = str(max(0, int(drop) - int(f1_drop))) if (drop.isdigit() and f1_drop.isdigit()) else "NA"
fps = grab(r"实测帧率\s*:\s*([0-9.]+)")
hfps = grab(r"被控端帧率\s*:\s*([0-9.]+)")
ff = grab(r"首帧延迟\s*:\s*(\d+)\s*ms")
kbps = grab(r"平均码率\s*:\s*(\d+)\s*kbps")
cmp_out = subprocess.check_output([sys.executable, sys.argv[4], sys.argv[3], sys.argv[2]], text=True, stderr=subprocess.STDOUT)
cm = re.search(r"corr[=:]\s*([0-9.]+)", cmp_out, re.I) or re.search(r"相关系数\s*[:：]?\s*([0-9.]+)", cmp_out)
corr = cm.group(1) if cm else "NA"
print(f"aus={aus} dec={dec} drop={drop} steady_drop={steady_drop} fps={fps} hfps={hfps} first_ms={ff} e2e_first_ms={sys.argv[5]} kbps={kbps} corr={corr}")
PY
)"
  echo "  [$label] $summary"
  python - "$summary" "$label" <<'PY'
import re, sys
s = sys.argv[1]
kv = dict(re.findall(r"(\w+)=([^\s]+)", s))
dec = int(kv["dec"]) if kv.get("dec","").isdigit() else 0
sdrop = int(kv["steady_drop"]) if kv.get("steady_drop","").isdigit() else 999
fps = float(kv["fps"]) if kv.get("fps","NA") != "NA" else 0.0
corr = float(kv["corr"]) if kv.get("corr","NA") != "NA" else 0.0
e2e = int(kv["e2e_first_ms"]) if kv.get("e2e_first_ms","").lstrip("-").isdigit() else -1
ok = (dec >= 150 and sdrop == 0 and fps >= 20.0 and corr >= 0.99 and e2e > 0)
sys.exit(0 if ok else 1)
PY
  if [ $? -eq 0 ]; then
    ok "$label 达标（$summary）"
  else
    bad "$label 未达标（$summary）"
  fi
}

echo "== 2. 逐档测量 =="
for d in $DELAYS; do
  echo "--- 档位 d=${d}ms（两跳单向，路程 RTT ≈ $((d*4))ms）---"
  start_host_for_delay "$d" || continue
  start_client_proxy "$d" || continue
  run_case "d${d}-auto" ""
  if [ "$d" = "50" ]; then
    run_case "d${d}-fast" "--quality fast"
  fi
done

echo "=========================================="
echo "结果：$PASS 通过 / $FAIL 失败 / $SKIP 跳过"
echo "证据目录：$WORK"
[ "$FAIL" -eq 0 ]
