#!/usr/bin/env bash
# 多控制端并发验收（**开发机 git-bash 跑，需与真机同二层**）：
#
#   六个控制端进程**同时**连同一台被控端：
#     · 三个视频端（--headless --fps 10 --seconds N --json --save-frame）
#     · 三个端口映射端（--forward 各自本地端口 → **同一个**回环 echo 目标）
#   三个映射端各打一条 1 MiB 全双工流（写读两个线程，内容各异）。
#
#   判据（每条都要机器可查）：
#     A. 被控端日志里 6 条「会话已授权，开始推流」= 6 条并发会话真的起来了
#     B. 三条跨会话流各自 sha256 一致 + 字节数对得上（同一目标、三个会话互不串）
#     C. 三个视频端各自的存帧与真机自己的截图逐个比对（画面是那台机器的真桌面）
#     D. 三个视频端都没被并发拖死（rc=0、丢帧有限、首帧不退化）
#     E. 被控端进程身份不变（同一 PID 还活着）且日志无 panic/ABRT —— 并发会话下
#        Xlib 没有内部锁，这条是 2026-10-26 那次 SIGABRT 的回归判据
#     F. bulk 按流窗口对每条会话都协商成功（window=Some(16) 出现 6 次）
#
#   为什么放在真机：一条连接上的按流窗口/端口映射/视频通道在**多会话**下的隔离性，
#   只有两条以上真连接同时压才看得出来；本机的假被控端造不出真 X11/x264。
#
# 用法：
#   UULAN_CLIENT=D:/uulan-bin/uulan-client.exe bash scripts/verify-multi-client-e2e.sh
# 可覆盖：KYLIN_TARGET / KYLIN_IP / UULAN_MC_PORT(默认 21987) / UULAN_MC_LP1(默认 23361)
#         UULAN_MC_BYTES(默认 1048576) / UULAN_MC_SECONDS(默认 20)
# 退出码：0 = 全过；1 = 断言失败（产品问题）；2 = 台架/环境给不出判据（别记成产品缺陷）。
set -u

KY="${KYLIN_TARGET:-kylin}"
KY_IP="${KYLIN_IP:-192.168.223.15}"
PORT="${UULAN_MC_PORT:-21987}"
E1=$((PORT + 1))                        # 三个会话共同打这一个回环 echo 目标
LP1="${UULAN_MC_LP1:-23361}"
LP2=$((LP1 + 1))
LP3=$((LP1 + 2))
BYTES="${UULAN_MC_BYTES:-1048576}"
SECS="${UULAN_MC_SECONDS:-20}"
CLI="${UULAN_CLIENT:?要 UULAN_CLIENT=<工作区外的 Windows 控制端 exe，D:/ 形式>}"
TS=$(date +%Y%m%d-%H%M%S)
WORKW="D:/uulan-bin/mc.$TS"
WORKK="/tmp/uulan-mc.$TS"
NO_COLOR=1
export NO_COLOR

PASS=0; FAIL=0; SKIP=0
ok()   { echo "  [PASS] $*"; PASS=$((PASS+1)); }
bad()  { echo "  [FAIL] $*"; FAIL=$((FAIL+1)); }
skip() { echo "  [SKIP] $*"; SKIP=$((SKIP+1)); }
env_out() { echo "  [环境] $*"; echo "退出码 2：这台机器此刻给不出这条判据，别把它记成产品缺陷"; exit 2; }

mkdir -p "$WORKW" || env_out "建不了 $WORKW"

# ─────────────────────────────────────────────────────────────────────
# 真机侧台架：中继 + 一个回环 echo 靶子 + 被控端（**真 :0 桌面**，好与截图比对）
# ─────────────────────────────────────────────────────────────────────
cat > "$WORKW/remote.sh" <<'RSH'
#!/usr/bin/env bash
set -u
BIN="$HOME/.local/uulan/src/target/release"
WORK="$1"; PORT="$2"; E1="$3"
export DISPLAY=:0
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
# 屏幕尺寸现场量，别把 1920x1080 写死（这台机器改过分辨率）
SZW=$(xdpyinfo 2>/dev/null | awk '/dimensions:/{split($2,a,"x"); print a[1]; exit}')
SZH=$(xdpyinfo 2>/dev/null | awk '/dimensions:/{split($2,a,"x"); print a[2]; exit}')
echo "${SZW:-0}x${SZH:-0}" > "$WORK/screen"
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
RUST_LOG=info NO_COLOR=1 "$BIN/uulan-host" --server "127.0.0.1:$PORT" --config "$WORK/host.json" \
  --capture "x11::0.0" --name mc-bench >"$WORK/host.log" 2>&1 &
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
  tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -q "uulan-mc" && kill "$pid" 2>/dev/null
done
# 口令落盘纪律：台架目录里的 pw / host.json 收尾就删，别在 /tmp 留现网口令副本
python3 - "$W" <<'PY'
import os, sys
w = sys.argv[1]
for n in ("pw", "host.json", "pw.raw"):
    try: os.remove(os.path.join(w, n))
    except OSError: pass
PY
CLS

cleanup() {
  for p in ${CPIDS:-}; do kill "$p" 2>/dev/null; done
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

# 本地：全双工探针（writer 线程灌 + reader 线程收，两个方向真的同时在跑）
cat > "$WORKW/probe.py" <<'PY'
import json, socket, sys, threading, time

port = int(sys.argv[1]); n = int(sys.argv[2]); seed = int(sys.argv[3])
data = bytes(((i * 37 + 11 + seed * 97) % 256) for i in range(n))
out = {"ok": False, "sent": 0, "got": 0, "err": "", "ms": 0}
t_start = time.time()
try:
    s = socket.create_connection(("127.0.0.1", port), timeout=60)
    got = bytearray()
    err = []

    def reader():
        try:
            while len(got) < n:
                b = s.recv(65536)
                if not b:
                    break
                got.extend(b)
        except Exception as e:  # noqa
            err.append("read: %s" % e)

    t = threading.Thread(target=reader)
    t.start()
    step = 65536
    for i in range(0, n, step):
        s.sendall(data[i:i + step])
    t.join(timeout=60)
    s.close()
    out.update(sent=n, got=len(got), err=";".join(err), ms=int((time.time() - t_start) * 1000))
    out["ok"] = (bytes(got) == data and not err)
except Exception as e:
    out["err"] = str(e)
print(json.dumps(out))
sys.exit(0 if out["ok"] else 1)
PY

echo "== 0. 前置：真机可达 + 端口干净 + 物料 =="
ssh -o ConnectTimeout=6 "$KY" true >/dev/null 2>&1 || env_out "$KY 不可达（先按失联判法分辨真假）"
[ -f "$CLI" ] || env_out "控制端 exe 不存在：$CLI（先 cargo build --release 再拷出工作区）"
BUSY=$(ssh "$KY" "ss -ltn 2>/dev/null | awk '{print \$4}' | grep -cE ':$PORT\$|:$E1\$'" 2>/dev/null | tail -1)
[ "${BUSY:-0}" = "0" ] || env_out "真机上 $PORT/$E1 已被占用（上一轮台架没清干净？）"
BUSYL=0
for p in "$LP1" "$LP2" "$LP3"; do
  c=$(netstat -ano 2>/dev/null | grep -E "LISTENING" | grep -c "127.0.0.1:$p " || true)
  BUSYL=$((BUSYL + c))
done
[ "$BUSYL" = "0" ] || env_out "本机 $LP1/$LP2/$LP3 已被占用（上一轮转发没退？）"
ssh "$KY" "mkdir -p $WORKK" || env_out "远端建不了 $WORKK"
scp -q "$WORKW/remote.sh" "$WORKW/clean.sh" "$KY:$WORKK/" || env_out "scp 台架脚本上不去"
echo "   工作目录：$WORKW / $WORKK"

echo "== 1. 起远端台架（中继 :$PORT + echo :$E1 + 被控端抓真 :0 桌面）=="
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
SCREEN=$(ssh "$KY" "cat $WORKK/screen" 2>/dev/null | tail -1)
SW=${SCREEN%x*}; SH=${SCREEN#*x}
case "$SW" in ''|*[!0-9]*) env_out "拿不到屏幕尺寸：$SCREEN" ;; esac
ssh "$KY" "cat $WORKK/pw" > "$WORKW/pw.raw" 2>/dev/null
grep -aoE '^[A-Za-z0-9]{8,16}$' "$WORKW/pw.raw" | head -1 > "$WORKW/pw.txt"
chmod 600 "$WORKW/pw.txt"
[ -s "$WORKW/pw.txt" ] || env_out "口令不合格式"
HOSTPID0=$(ssh "$KY" "cat $WORKK/hostpid" 2>/dev/null | tail -1)
ok "台架就绪（设备 $DEV，屏幕 ${SW}x${SH}，被控端 PID $HOSTPID0）"

echo "== 2. 六个控制端同时连：三个看画面 + 三个做端口映射（同一个 echo 目标）=="
CPIDS=""
for n in 1 2 3; do
  # 视频端：只收画面（--forward 会独占会话、丢掉视频通道，所以必须分开）
  "$CLI" --server "$KY_IP:$PORT" --device "$DEV" --password-file "$WORKW/pw.txt" \
    --headless --fps 10 --seconds "$SECS" --json --save-frame "$WORKW/c$n.ppm" \
    > "$WORKW/v$n.out" 2> "$WORKW/v$n.err" &
  CPIDS="$CPIDS $!"
  eval "VPID$n=$!"
done
for n in 1 2 3; do
  eval "lp=\$LP$n"
  "$CLI" --server "$KY_IP:$PORT" --device "$DEV" --password-file "$WORKW/pw.txt" \
    --forward "127.0.0.1:$lp:127.0.0.1:$E1" --seconds "$SECS" \
    > "$WORKW/f$n.out" 2> "$WORKW/f$n.err" &
  CPIDS="$CPIDS $!"
  eval "FPID$n=$!"
done
READY=0
for _ in $(seq 1 40); do
  READY=$(grep -al "转发已就绪" "$WORKW"/f?.out 2>/dev/null | wc -l | tr -d ' ')
  [ "$READY" = "3" ] && break
  sleep 0.5
done
[ "$READY" = "3" ] || { for n in 1 2 3; do echo "--- f$n"; cat "$WORKW/f$n.out" "$WORKW/f$n.err"; done; env_out "三个映射端没有都打出「转发已就绪」（$READY/3）"; }
ok "六个控制端都连上了：三个视频端 + 三个映射端（都就绪）"

sleep 2
echo "   同时打三条 1 MiB 全双工流（都指向同一个回环目标 $E1）"
PPIDS=""
for n in 1 2 3; do
  eval "lp=\$LP$n"
  python "$WORKW/probe.py" "$lp" "$BYTES" "$n" > "$WORKW/pr$n.json" 2>&1 &
  PPIDS="$PPIDS $!"
done
NOK=0
for p in $PPIDS; do wait "$p" || true; done
for n in 1 2 3; do
  if grep -q '"ok": true' "$WORKW/pr$n.json" 2>/dev/null; then
    NOK=$((NOK + 1)); echo "      流 $n：$(cat "$WORKW/pr$n.json")"
  else
    echo "      流 $n 失败：$(cat "$WORKW/pr$n.json" 2>/dev/null | head -c 300)"
  fi
done
[ "$NOK" = "3" ] && ok "三条跨会话流全部 sha256 一致（同一目标、三个会话互不串）" \
                 || bad "只有 $NOK/3 条跨会话流通过"

# 参考截图：会话进行中，用真机自己的 x11grab 抓一帧当独立判据
ssh "$KY" "DISPLAY=:0 ffmpeg -loglevel error -f x11grab -video_size ${SW}x${SH} -i :0 -frames:v 1 -y $WORKK/shot.ppm" 2>/dev/null
scp -q "$KY:$WORKK/shot.ppm" "$WORKW/shot.ppm" 2>/dev/null
[ -s "$WORKW/shot.ppm" ] && ok "参考截图已取回（${SW}x${SH}）" || skip "参考截图没取回，画面比对跳过"

echo "== 3. 映射端收工统计（连接/字节数必须是各自的）=="
for n in 1 2 3; do
  eval "p=\$FPID$n"; wait "$p" 2>/dev/null; eval "RC$n=\$?"
  STAT=$(grep -a "端口映射结束" "$WORKW/f$n.out" | tail -1)
  echo "$STAT" > "$WORKW/s$n.txt"
  echo "      f$n：rc=$(eval echo \$RC$n)  $STAT"
done
for n in 1 2 3; do
  RCV=$(eval echo \$RC$n)
  [ "$RCV" = "0" ] && ok "映射端 $n 收工 rc=0" || bad "映射端 $n 收工 rc=$RCV"
  S=$(cat "$WORKW/s$n.txt")
  echo "$S" | grep -q "连接 1 " && ok "映射端 $n 统计：连接 1（只算自己的流）" || bad "映射端 $n 连接数不对：$S"
  echo "$S" | grep -qE "上行 $BYTES 字节" && ok "映射端 $n 上行 = $BYTES 字节" || bad "映射端 $n 上行不符：$S"
  echo "$S" | grep -qE "下行 $BYTES 字节" && ok "映射端 $n 下行 = $BYTES 字节" || bad "映射端 $n 下行不符：$S"
done

echo "== 4. 视频端收工统计 =="
for n in 1 2 3; do
  eval "p=\$VPID$n"; wait "$p" 2>/dev/null; eval "VRC$n=\$?"
done
CPIDS=""
python - "$WORKW" <<'PY' > "$WORKW/judge.txt" 2>&1
import json, sys
w = sys.argv[1]
lines = []
for n in (1, 2, 3):
    src = "%s/v%d.out" % (w, n)
    d = None
    for ln in open(src, errors="replace"):
        ln = ln.strip()
        if ln.startswith("{") and '"stats"' in ln:
            try: d = json.loads(ln)
            except Exception: d = None
    if d is None:
        lines.append("c%d\tNO_JSON" % n); continue
    st = d.get("stats", {})
    lines.append("c%d_decoded\t%s" % (n, st.get("decoded", 0)))
    lines.append("c%d_dropped\t%s" % (n, st.get("dropped", 0)))
    lines.append("c%d_first_ms\t%s" % (n, st.get("first_frame_ms", 0)))
    lines.append("c%d_aus\t%s" % (n, st.get("aus", 0)))
print("\n".join(lines))
PY
cat "$WORKW/judge.txt" | sed 's/^/      /'
jd() { awk -F'\t' -v k="$1" '$1==k{print $2}' "$WORKW/judge.txt" | head -1; }
for n in 1 2 3; do
  RCV=$(eval echo \$VRC$n)
  [ "$RCV" = "0" ] && ok "视频端 $n 收工 rc=0" || bad "视频端 $n 收工 rc=$RCV"
  D=$(jd "c${n}_decoded"); DR=$(jd "c${n}_dropped"); FF=$(jd "c${n}_first_ms")
  if [ "${D:-0}" -ge 80 ] 2>/dev/null; then ok "视频端 $n 解出 $D 帧（≥80）"; else bad "视频端 $n 只解出 ${D:-?} 帧"; fi
  if [ "${DR:-999}" -le 5 ] 2>/dev/null; then ok "视频端 $n 解码丢帧 $DR（≤5）"; else bad "视频端 $n 丢帧 $DR（>5）"; fi
  if [ "${FF:-99999}" -le 1500 ] 2>/dev/null; then ok "视频端 $n 首帧 ${FF} ms（≤1500）"; else bad "视频端 $n 首帧 ${FF} ms"; fi
  grep -aqiE "panic|ERROR" "$WORKW/v$n.err" && bad "视频端 $n stderr 有 panic/ERROR" || ok "视频端 $n stderr 干净"
done
for n in 1 2 3; do
  if [ -s "$WORKW/c$n.ppm" ] && [ -s "$WORKW/shot.ppm" ]; then
    CMP=$(python scripts/compare-ppm.py "$WORKW/c$n.ppm" "$WORKW/shot.ppm" 2>&1)
    CORR=$(echo "$CMP" | sed -n 's/^相关系数 *: *\([-0-9.]*\)$/\1/p')
    MAE=$(echo "$CMP" | sed -n 's/^平均绝对误差: *[0-9.]*\/255 = *\([0-9.]*\)%$/\1/p')
    echo "      视频端 $n 画面：corr=$CORR mae=${MAE}%"
    awk -v c="${CORR:-0}" 'BEGIN{exit !(c>=0.99)}' && ok "视频端 $n 存帧与真机截图相关 $CORR（≥0.99）" \
      || bad "视频端 $n 存帧与真机截图相关只有 ${CORR:-?}"
    awk -v m="${MAE:-99}" 'BEGIN{exit !(m<=3.0)}' && ok "视频端 $n 平均绝对误差 ${MAE}%（≤3%）" \
      || bad "视频端 $n 平均绝对误差 ${MAE}%（>3%）"
  else
    skip "视频端 $n 的存帧或参考截图缺失"
  fi
done

echo "== 5. 被控端侧：六条会话 + 窗口协商 + 进程稳定 =="
ssh "$KY" "cat $WORKK/host.log" > "$WORKW/host.log" 2>/dev/null || true
NS=$(grep -ac "会话已授权，开始推流" "$WORKW/host.log" || true)
[ "${NS:-0}" = "6" ] && ok "被控端日志里有 6 条「会话已授权，开始推流」" || bad "被控端只记到 ${NS:-0} 条已授权会话"
NW=$(grep -ac "window=Some(16)" "$WORKW/host.log" || true)
[ "${NW:-0}" = "6" ] && ok "六条会话都协商出按流窗口 window=Some(16)" || bad "窗口协商只出现 ${NW:-0}/6 次"
grep -aiE "panic|ABRT|invalid pointer|memory corruption" "$WORKW/host.log" > "$WORKW/hostbad.txt" 2>/dev/null || true
if [ -s "$WORKW/hostbad.txt" ]; then
  bad "被控端日志里有崩溃/Xlib 并发问题：$(head -3 "$WORKW/hostbad.txt")"
else
  ok "被控端日志无 panic/ABRT/invalid pointer（并发会话下 Xlib 没炸）"
fi
ALIVE=$(ssh "$KY" "kill -0 $HOSTPID0 2>/dev/null && echo YES || echo NO" 2>/dev/null | tail -1)
[ "$ALIVE" = "YES" ] && ok "被控端进程仍是同一个 PID $HOSTPID0（没崩、没被拉起）" || bad "被控端进程不在了（原 PID $HOSTPID0）"
NCL=$(ssh "$KY" "pgrep -c -x uulan-client || true" 2>/dev/null | tail -1)
[ "${NCL:-0}" = "0" ] && ok "真机上没有残留的 uulan-client" || skip "真机上还有 ${NCL} 个 uulan-client（可能是别的会话）"

echo
echo "==== 结果：$PASS 通过 / $FAIL 失败 / $SKIP 跳过 ===="
echo "证据目录：$WORKW（真机侧 $WORKK）"
[ "$FAIL" = "0" ] && exit 0 || exit 1
