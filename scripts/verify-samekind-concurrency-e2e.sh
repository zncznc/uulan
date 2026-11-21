#!/usr/bin/env bash
# 同种工具并发验收（**开发机 git-bash 跑，需与真机同二层**）：
#
#   A. **一个控制会话里两条 `--forward` 规则**，规则各 3 条并发连接，全部**全双工**
#      （一边灌一边收，writer/reader 两个线程），6 条流内容互不相同 —— 判据是逐流 sha256
#      与原数据一致 + 收尾统计「连接 6 / 上行 = 下行 = 期望字节」。串流/丢字节/掐流都会红。
#   C. **坏目标**（单独一条会话）：规则指向没人监听的端口时，本机连接必须**立刻被关掉**
#      并把原因说清楚（不能挂着等用户自己的超时），统计里 连接 0 / 上行 0 / 下行 0。
#
#   B. **两条终端会话同时**（两个控制端进程各拿一条 `--term`，都在真机侧跑），
#      每条在远端写下 begin/end 时间戳 —— 判据是两个时间区间**真的重叠**，且各自只看到
#      自己的哨兵（终端注册表不串）。
#
# 为什么放在真机：端口映射是「本机监听 → 被控端连目标」，被控端那半边是 Linux socket；
# 终端要真 /dev/ptmx；两者都得在两台机器之间过真 wire。
#
# 用法：
#   UULAN_CLIENT=D:/uulan-bin/uulan-client.exe bash scripts/verify-samekind-concurrency-e2e.sh
# 可覆盖：KYLIN_TARGET / KYLIN_IP / UULAN_SK_PORT(默认 21986) / UULAN_SK_LP1(默认 23351) / UULAN_SK_BYTES
# 退出码：0 = 全过；1 = 断言失败（产品问题）；2 = 台架/环境给不出判据（别记成产品缺陷）。
set -u

KY="${KYLIN_TARGET:-kylin}"
KY_IP="${KYLIN_IP:-192.168.223.15}"
PORT="${UULAN_SK_PORT:-21986}"
E1=$((PORT + 1))
E2=$((PORT + 2))
EP=$((PORT + 3))                        # 没人监听：坏目标（错误路径）
LP1="${UULAN_SK_LP1:-23351}"
LP3=$((LP1 + 2))
LP4=$((LP1 + 3))
LP5=$((LP1 + 4))
LP2=$((LP1 + 1))
BYTES="${UULAN_SK_BYTES:-786432}"        # 单流负载 768 KiB（6 条流 ≈ 4.5 MiB 双向）
CLI="${UULAN_CLIENT:?要 UULAN_CLIENT=<工作区外的 Windows 控制端 exe，D:/ 形式>}"
TS=$(date +%Y%m%d-%H%M%S)
RID="$TS"
WORKW="D:/uulan-bin/sk.$TS"
WORKK="/tmp/uulan-sk.$TS"
NO_COLOR=1
export NO_COLOR

PASS=0; FAIL=0; SKIP=0
ok()   { echo "  [PASS] $*"; PASS=$((PASS+1)); }
bad()  { echo "  [FAIL] $*"; FAIL=$((FAIL+1)); }
skip() { echo "  [SKIP] $*"; SKIP=$((SKIP+1)); }
env_out() { echo "  [环境] $*"; echo "退出码 2：这台机器此刻给不出这条判据，别把它记成产品缺陷"; exit 2; }

mkdir -p "$WORKW" || env_out "建不了 $WORKW"

# ─────────────────────────────────────────────────────────────────────
# 真机侧台架：中继 + 两个回环 echo 靶子 + 被控端（合成画面，够握手用）
# ─────────────────────────────────────────────────────────────────────
cat > "$WORKW/remote.sh" <<'RSH'
#!/usr/bin/env bash
set -u
BIN="$HOME/.local/uulan/src/target/release"
WORK="$1"; PORT="$2"; E1="$3"; E2="$4"
mkdir -p "$WORK" || exit 1
cat > "$WORK/echo.py" <<'PY'
import socket, threading, sys
port = int(sys.argv[1])
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", port)); s.listen(32)

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
python3 "$WORK/echo.py" "$E1" >"$WORK/echo1.log" 2>&1 &
echo $! >> "$WORK/supid"
python3 "$WORK/echo.py" "$E2" >"$WORK/echo2.log" 2>&1 &
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
RUST_LOG=info NO_COLOR=1 "$BIN/uulan-host" --server "127.0.0.1:$PORT" --config "$WORK/host.json" \
  --capture "lavfi:testsrc2=size=640x360:rate=10" --name sk-bench >"$WORK/host.log" 2>&1 &
echo $! >> "$WORK/supid"
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
W="$1"; RID="$2"
if [ -f "$W/supid" ]; then
  while read -r pid; do kill "$pid" 2>/dev/null; done < "$W/supid"
fi
if [ -f "$W/srvpid" ]; then kill "$(cat "$W/srvpid")" 2>/dev/null; fi
if [ -f "$W/port" ]; then
  P=$(cat "$W/port")
  for pid in $(pgrep -f "uulan-server --bind 0.0.0.0:$P" 2>/dev/null); do kill "$pid" 2>/dev/null; done
fi
for pid in $(pgrep -f "uulan-client.*$RID" 2>/dev/null); do kill "$pid" 2>/dev/null; done
# 口令落盘纪律：台架目录里的 pw / host.json 收尾就删，别在 /tmp 留现网口令副本
python3 - "$W" <<'PY'
import os, sys
w = sys.argv[1]
for n in ("pw", "host.json", "pw.raw"):
    try: os.remove(os.path.join(w, n))
    except OSError: pass
PY
python3 - "$RID" <<'PY'
import glob, os, sys
for p in glob.glob("/tmp/sk-%s-*" % sys.argv[1]):
    try: os.remove(p)
    except OSError: pass
PY
CLS

# 真机侧：两条终端会话**同时**开（各自写 begin/end 时间戳，用来证明时间区间重叠）
cat > "$WORKW/term_twice.sh" <<'TTS'
#!/usr/bin/env bash
# 两条终端会话**同时**开。判据两条：
#   ① 远端真的执行了命令（写出 begin/end 时间戳文件）—— 不看本地回显，回显只能证明字节到了 pty；
#   ② 两个时间区间重叠 —— 证明是并发，不是排队。
# ⚠ 输入必须**保持打开**：`printf | client --term` 在最后一行就 EOF，客户端按纪律发 EOT，
#   3 秒看门狗会把还在 sleep 的 shell 杀掉（退出码 129），sleep 期间的命令就白等 ——
#   所以外面套一层 `{ printf …; sleep 12; }` 让 stdin 活到命令跑完。
set -u
BIN="$HOME/.local/uulan/src/target/release"
WORK="$1"; RID="$2"
CLI="$BIN/uulan-client"
DEV=$(cat "$WORK/dev")
PW="$WORK/pw"
PORT=$(cat "$WORK/port")
: > "$WORK/term-rc"
for i in 1 2; do
  cat > "$WORK/w$i.sh" <<EOF
#!/usr/bin/env bash
"$CLI" --server 127.0.0.1:$PORT --device $DEV --password-file "$PW" --term
exit \$?
EOF
  chmod +x "$WORK/w$i.sh"
  (
    {
      printf 'date +%%s.%%N > /tmp/sk-%s-%s.begin\nsleep 7\ndate +%%s.%%N > /tmp/sk-%s-%s.end\necho SK-MARK-%s\nexit 0\n' \
        "$RID" "$i" "$RID" "$i" "$i"
      sleep 12
    } | script -qec "bash $WORK/w$i.sh" /dev/null >"$WORK/term$i.out" 2>&1
    echo "term$i=$?" >>"$WORK/term-rc"
  ) &
done
wait
TTS

cleanup() {
  [ -n "${CPID:-}" ] && kill "$CPID" 2>/dev/null
  ssh "$KY" "bash $WORKK/clean.sh $WORKK $RID" >/dev/null 2>&1
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
import hashlib, json, socket, sys, threading, time

port = int(sys.argv[1]); n = int(sys.argv[2]); seed = int(sys.argv[3])
data = bytes(((i * 37 + 11 + seed * 97) % 256) for i in range(n))
out = {"ok": False, "sent": 0, "got": 0, "err": ""}
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

# 本地：坏目标探针 —— 连上去之后应该很快被关掉（EOF/RST），而不是等到自己的超时
cat > "$WORKW/probe_closed.py" <<'PY'
import json, socket, sys, time
port = int(sys.argv[1])
out = {"closed": False, "ms": 0, "err": ""}
t0 = time.time()
try:
    s = socket.create_connection(("127.0.0.1", port), timeout=10)
    s.settimeout(12)
    try:
        b = s.recv(16)
        out["closed"] = (b == b"")
        if b:
            out["err"] = "unexpected %r" % b[:16]
    except ConnectionResetError:
        out["closed"] = True
    s.close()
except Exception as e:
    out["err"] = str(e)
out["ms"] = int((time.time() - t0) * 1000)
print(json.dumps(out))
sys.exit(0 if out["closed"] and not out["err"] else 1)
PY

# 停读探针：灌满对端下行队列后**故意不读** pause 秒，再读回来（量「停读会拖住谁」）
cat > "$WORKW/stalled.py" <<'PY'
import json, socket, sys, time
port = int(sys.argv[1]); n = int(sys.argv[2]); pause = float(sys.argv[3]); out_path = sys.argv[4]
data = bytes(((i * 53 + 7) % 256) for i in range(65536))
res = {"ok": False, "sent": 0, "got": 0, "pause_s": pause, "err": ""}
try:
    s = socket.create_connection(("127.0.0.1", port), timeout=30)
    sent = 0
    while sent < n:
        k = min(65536, n - sent)
        s.sendall(data[:k]); sent += k
    res["sent"] = sent
    t0 = time.time()
    time.sleep(pause)                      # 停读窗口
    got = bytearray()
    s.settimeout(25)
    while len(got) < n:
        b = s.recv(262144)
        if not b: break
        got.extend(b)
    res.update(got=len(got), ok=(len(got) == n))
    res["read_s"] = round(time.time() - t0 - pause, 3)
except Exception as e:
    res["err"] = str(e)
open(out_path, "w").write(json.dumps(res))
PY

echo "== 0. 前置：真机可达 + 端口干净 + 物料 =="
ssh -o ConnectTimeout=6 "$KY" true >/dev/null 2>&1 || env_out "$KY 不可达（先按失联判法分辨真假）"
[ -f "$CLI" ] || env_out "控制端 exe 不存在：$CLI（先 cargo build --release 再拷出工作区）"
BUSY=$(ssh "$KY" "ss -ltn 2>/dev/null | awk '{print \$4}' | grep -cE ':$PORT\$|:$E1\$|:$E2\$|:$EP\$'" 2>/dev/null | tail -1)
[ "${BUSY:-0}" = "0" ] || env_out "真机上 $PORT/$E1/$E2 已被占用（上一轮台架没清干净？）"
BUSYL=0
for p in "$LP1" "$LP2" "$LP4" "$LP5"; do
  c=$(netstat -ano 2>/dev/null | grep -E "LISTENING" | grep -c "127.0.0.1:$p " || true)
  BUSYL=$((BUSYL + c))
done
[ "$BUSYL" = "0" ] || env_out "本机 $LP1/$LP2 已被占用（上一轮转发没退？）"
ssh "$KY" "mkdir -p $WORKK" || env_out "远端建不了 $WORKK"
scp -q "$WORKW/remote.sh" "$WORKW/clean.sh" "$WORKW/term_twice.sh" "$KY:$WORKK/" || env_out "scp 台架脚本上不去"
echo "   工作目录：$WORKW / $WORKK"

echo "== 1. 起远端台架（中继 :$PORT + 两个 echo :$E1/:$E2 + 被控端）=="
ssh "$KY" "setsid bash $WORKK/remote.sh $WORKK $PORT $E1 $E2 > $WORKK/bed.out 2>&1 < /dev/null &" >/dev/null 2>&1 || true
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

echo "== 2. 一个会话里两条 --forward 规则，规则各 3 条并发全双工流 =="
"$CLI" --server "$KY_IP:$PORT" --device "$DEV" --password-file "$WORKW/pw.txt" \
  --forward "127.0.0.1:$LP1:127.0.0.1:$E1" \
  --forward "127.0.0.1:$LP2:127.0.0.1:$E2" --seconds 22 \
  > "$WORKW/fwd.out" 2> "$WORKW/fwd.err" &
CPID=$!
READY=NO
for _ in $(seq 1 40); do
  grep -aq "转发已就绪（2 条）" "$WORKW/fwd.out" 2>/dev/null && { READY=YES; break; }
  sleep 0.5
done
[ "$READY" = "YES" ] || { cat "$WORKW/fwd.out" "$WORKW/fwd.err"; env_out "控制端没打出「转发已就绪（2 条）」"; }
ok "控制端起来了（PID $CPID），两条转发规则就绪"

# 6 条并发：LP1 三条（种子 1/2/3），LP2 三条（种子 4/5/6），长度略有差别
T0=$(python -c 'import time;print(int(time.time()*1000))')
PIDS=""; IDX=0
for i in 1 2 3; do
  IDX=$((IDX + 1))
  python "$WORKW/probe.py" "$LP1" $((BYTES + i * 1024)) "$i" > "$WORKW/p1-$i.json" 2>&1 &
  PIDS="$PIDS $!:$i:p1-$i"
done
for i in 4 5 6; do
  python "$WORKW/probe.py" "$LP2" $((BYTES + i * 1024)) "$i" > "$WORKW/p2-$i.json" 2>&1 &
  PIDS="$PIDS $!:$i:p2-$i"
done
NOK=0; SUM=0
for ent in $PIDS; do
  pid="${ent%%:*}"; rest="${ent#*:}"; idx="${rest%%:*}"; tag="${rest#*:}"
  if wait "$pid"; then :; fi
  if grep -q '"ok": true' "$WORKW/$tag.json" 2>/dev/null; then
    NOK=$((NOK + 1))
    echo "      流 $idx：$(cat "$WORKW/$tag.json")"
  else
    echo "      流 $idx 失败：$(cat "$WORKW/$tag.json" 2>/dev/null | head -c 300)"
  fi
done
T1=$(python -c 'import time;print(int(time.time()*1000))')
SUM=$(python - "$WORKW" <<'PY'
import glob, json, os, sys
w = sys.argv[1]; tot = 0
for p in glob.glob(os.path.join(w, "p[12]-*.json")):
    try:
        d = json.load(open(p))
        if d.get("ok"): tot += int(d.get("sent", 0))
    except Exception: pass
print(tot)
PY
)
if kill -0 "$CPID" 2>/dev/null; then ok "六条流跑完时控制端进程仍活着（没被并发拖死）"; else bad "控制端在并发期间退出了"; fi
[ "$NOK" = "6" ] && ok "6 条全双工流全部 sha256 一致（并发窗口 $((T1 - T0)) ms，合计 $SUM 字节/方向）" \
                 || bad "只有 $NOK/6 条流通过（见上面每行）"

echo "== 3. 两条终端会话同时开（时间区间必须重叠）=="
ssh "$KY" "bash $WORKK/term_twice.sh $WORKK $RID" > "$WORKW/term_run.log" 2>&1
RCK=$(ssh "$KY" "cat $WORKK/term-rc" 2>/dev/null)
echo "$RCK" | sed 's/^/      kylin: /'
echo "$RCK" | grep -q '^term1=0$' && ok "终端 1 退出码 0" || bad "终端 1 退出码不是 0"
echo "$RCK" | grep -q '^term2=0$' && ok "终端 2 退出码 0" || bad "终端 2 退出码不是 0"
for i in 1 2; do
  ssh "$KY" "cat /tmp/sk-$RID-$i.begin /tmp/sk-$RID-$i.end" > "$WORKW/t$i.ts" 2>/dev/null
done
OV=$(python - "$WORKW" <<'PY'
import sys
w = sys.argv[1]
def rng(i):
    try:
        a, b = [float(x) for x in open("%s/t%d.ts" % (w, i)).read().split()]
        return a, b
    except Exception:
        return None
r1, r2 = rng(1), rng(2)
if not r1 or not r2:
    print("NO_TIMESTAMPS"); sys.exit(1)
overlap = r1[0] < r2[1] and r2[0] < r1[1]
print("OVERLAP %.1fs" % (min(r1[1], r2[1]) - max(r1[0], r2[0])) if overlap else "NO_OVERLAP")
sys.exit(0 if overlap else 1)
PY
)
echo "      $OV"
case "$OV" in
  OVERLAP*) ok "两条终端会话真的同时在跑（$OV）" ;;
  *) bad "两条终端会话时间区间不重叠（$OV）—— 要么串了、要么一条把另一条挤掉了" ;;
esac
for i in 1 2; do
  ssh "$KY" "cat $WORKK/term$i.out" > "$WORKW/term$i.out" 2>/dev/null
  grep -aq "SK-MARK-$i" "$WORKW/term$i.out" && ok "终端 $i 只看到自己的哨兵 SK-MARK-$i" || bad "终端 $i 输出里没有自己的哨兵"
done
grep -aq "SK-MARK-2" "$WORKW/term1.out" && bad "终端 1 的输出里出现了终端 2 的哨兵（会话串了）" || ok "终端 1 没有混进终端 2 的输出"
grep -aq "SK-MARK-1" "$WORKW/term2.out" && bad "终端 2 的输出里出现了终端 1 的哨兵（会话串了）" || ok "终端 2 没有混进终端 1 的输出"
NFF=$(ssh "$KY" "pgrep -c -x uulan-client || true" 2>/dev/null | tail -1)
[ "${NFF:-0}" = "0" ] && ok "收尾后真机上没有残留的 uulan-client" || skip "真机上还有 ${NFF} 个 uulan-client（可能是别的会话）"

echo "== 4. 收尾统计（等 --seconds 22 到点收工）=="
wait "$CPID"; FRC=$?
echo "      控制端退出码 $FRC"
[ "$FRC" = "0" ] && ok "控制端按 --seconds 到点正常收工（rc=0）" || bad "控制端退出码 $FRC（不是 0）"
for _ in $(seq 1 30); do
  grep -aq "端口映射结束" "$WORKW/fwd.out" 2>/dev/null && break
  sleep 0.3
done
STAT=$(grep -a "端口映射结束" "$WORKW/fwd.out" | tail -1)
echo "      $STAT"
echo "$STAT" | grep -q "连接 6 " && ok "统计里的连接数 = 6（6 条流各占一个 id）" || bad "统计连接数不是 6"
echo "$STAT" | grep -qE "上行 $SUM 字节" && ok "上行 = $SUM 字节（与 6 条流发送总量一致）" || bad "上行字节与期望 $SUM 不符"
echo "$STAT" | grep -qE "下行 $SUM 字节" && ok "下行 = $SUM 字节（回显全数回来）" || bad "下行字节与期望 $SUM 不符"
grep -aqiE "panic|ERROR" "$WORKW/fwd.err" && bad "控制端 stderr 里有 panic/ERROR：$(grep -aiE 'panic|ERROR' "$WORKW/fwd.err" | head -3)" \
                                            || ok "控制端 stderr 干净"

echo "== 5. 坏目标：规则指向没人监听的端口（错误路径）=="
"$CLI" --server "$KY_IP:$PORT" --device "$DEV" --password-file "$WORKW/pw.txt" \
  --forward "127.0.0.1:$LP3:127.0.0.1:$EP" --seconds 12 \
  > "$WORKW/bad.out" 2> "$WORKW/bad.err" &
BPID=$!
SYNC=NO
for _ in $(seq 1 30); do
  grep -aq "转发已就绪" "$WORKW/bad.out" 2>/dev/null && { SYNC=YES; break; }
  sleep 0.5
done
[ "$SYNC" = "YES" ] || env_out "坏目标会话没起来"
sleep 1
BAD=$(python "$WORKW/probe_closed.py" "$LP3" 2>&1)
echo "      $BAD"
echo "$BAD" | grep -q '"closed": true' && ok "本机连接被立刻关掉（用户不会对着死连接干等）" \
                                        || bad "本机连接没有被关掉：$BAD"
grep -aqiE "拒绝|连不上|映射" "$WORKW/bad.err" "$WORKW/bad.out" \
  && ok "原因说清楚了：$(grep -aiE '拒绝|连不上|映射' "$WORKW/bad.err" "$WORKW/bad.out" | head -1)" \
  || bad "没有任何一条说明为什么连不上"
wait "$BPID"; BRC=$?
BST=$(grep -a "端口映射结束" "$WORKW/bad.out" | tail -1)
echo "      rc=$BRC  $BST"
echo "$BST" | grep -q "连接 0 " && ok "坏目标没有计入连接数（连接 0）" || bad "坏目标被计成了正常连接：$BST"
echo "$BST" | grep -qE "上行 0 字节 / 下行 0 字节" && ok "坏目标没有搬运任何字节" || bad "坏目标搬运了字节：$BST"

echo "== 6. 观察项：一条流停读时，别的流会不会被拖住（DESIGN 写明「反压是全连接级」）=="
# 这不是缺陷断言，是把「读任务单线程分派」的代价量出来 —— 卡住的是一条还是全部，决定现场
# 遇到「别的转发突然不动了」时该往哪儿查。判据：B 流在 A 停读的窗口里**能不能按时跑完**。
"$CLI" --server "$KY_IP:$PORT" --device "$DEV" --password-file "$WORKW/pw.txt" \
  --forward "127.0.0.1:$LP4:127.0.0.1:$E1" \
  --forward "127.0.0.1:$LP5:127.0.0.1:$E1" --seconds 45 \
  > "$WORKW/stall.out" 2> "$WORKW/stall.err" &
SPID=$!
SYNC=NO
for _ in $(seq 1 30); do
  grep -aq "转发已就绪（2 条）" "$WORKW/stall.out" 2>/dev/null && { SYNC=YES; break; }
  sleep 0.5
done
if [ "$SYNC" != "YES" ]; then
  skip "停读观察台架没起来（不影响前面的判据）"
else
  # A：灌 4 MiB 后**10 秒不读**（本地 socket 的接收窗口 + 流队列 8 帧会被填满）
  python "$WORKW/stalled.py" "$LP4" 4194304 10 "$WORKW/stall_a.json" >/dev/null 2>&1 &
  SPID_A=$!
  sleep 3
  # B：正常读写的 1 MiB，单独计时
  python "$WORKW/probe.py" "$LP5" 1048576 9 > "$WORKW/stall_b.json" 2>&1
  echo "      B（正常的 1 MiB 往返）：$(cat "$WORKW/stall_b.json" 2>/dev/null | head -c 160)"
  wait "$SPID_A" 2>/dev/null
  echo "      A（停读 10 s 后恢复）：$(cat "$WORKW/stall_a.json" 2>/dev/null | head -c 200)"
  BT=$(python - "$WORKW" <<'PY'
import json, os, sys
w = sys.argv[1]
try:
    a = json.load(open(os.path.join(w, "stall_a.json")))
    b = json.load(open(os.path.join(w, "stall_b.json")))
except Exception:
    print("NO_DATA"); raise SystemExit(1)
if not b.get("ok"):
    print("B_FAILED")
elif b.get("ms", 0) < 2000:
    print("B_FAST %d" % b.get("ms", 0))
else:
    print("B_STALLED %d" % b.get("ms", 0))
PY
)
  echo "      结论标记：$BT"
  case "$BT" in
    B_FAST*) ok "停读的那条流**没有**拖住别的流（$BT）—— 与 DESIGN 的「全连接级反压」不一致，是更好的形态" ;;
    B_STALLED*) skip "别的流被停读的流拖住了（$BT，>2 s）—— 与 DESIGN 写明的代价一致；这是协议没有按流流控的结果，记进文档，不当缺陷" ;;
    *) skip "观察数据不完整（$BT）" ;;
  esac
  kill "$SPID" 2>/dev/null
fi

echo
echo "==== 结果：$PASS 通过 / $FAIL 失败 / $SKIP 跳过 ===="
echo "证据目录：$WORKW（真机侧 $WORKK）"
[ "$FAIL" = "0" ] && exit 0 || exit 1
