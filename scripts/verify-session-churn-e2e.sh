#!/usr/bin/env bash
# 会话反复开关的泄漏回归（**开发机 git-bash 跑，需与真机同二层**）：
#
#   每一轮会话都会在被控端起一个 ffmpeg 编码子进程、打开一次 Xlib 显示、注册一个 bulk 流。
#   这类东西"开一次没问题、开二十次开始漏"是最常见也最难在单次验收里发现的缺陷，
#   所以这里专门反复开关会话，盯四个**会单调增长**的量：
#     · VmRSS（内存）
#     · Threads（线程）
#     · /proc/<pid>/fd 条数（句柄）
#     · 子进程数 / ffmpeg 子进程数（收尾后必须归零，否则就是编码器没被回收）
#
#   两段：
#     A. **串行** 12 条会话（--headless --seconds 3 --fps 5），每收一条就采一次样；
#     B. **并发** 4 轮 × 3 条会话同时（同一台被控端），每轮收工后采样。
#   最后静置 8 秒再采一次终值。
#
#   判据（都是硬判据，数字全打印出来）：
#     1. 每段结束 ffmpeg 子进程必须归零（编码器真的被回收）
#     2. fd / 线程 / RSS 终值对基线的增量在阈值内
#     3. 预热之后不再单调增长（末值与"串行第 5 条之后"的差 ÷ 会话数 ≤ 阈值）—— 这才是"漏"的定义
#     4. 被控端 PID 全程不变、日志无 panic/ERROR
#
# 用法：
#   UULAN_CLIENT=D:/uulan-bin/uulan-client.exe bash scripts/verify-session-churn-e2e.sh
# 可覆盖：KYLIN_TARGET / KYLIN_IP / UULAN_CH_PORT(默认 21989) / UULAN_CH_SECS(默认 3)
#         UULAN_CH_SERIAL(默认 12) / UULAN_CH_ROUNDS(默认 4)
# 退出码：0 = 全过；1 = 断言失败（产品问题）；2 = 台架/环境给不出判据（别记成产品缺陷）。
set -u

KY="${KYLIN_TARGET:-kylin}"
KY_IP="${KYLIN_IP:-192.168.223.15}"
PORT="${UULAN_CH_PORT:-21989}"
SECS="${UULAN_CH_SECS:-3}"
NSER="${UULAN_CH_SERIAL:-12}"
NROUND="${UULAN_CH_ROUNDS:-4}"
CLI="${UULAN_CLIENT:?要 UULAN_CLIENT=<工作区外的 Windows 控制端 exe，D:/ 形式>}"
TS=$(date +%Y%m%d-%H%M%S)
WORKW="D:/uulan-bin/ch.$TS"
WORKK="/tmp/uulan-ch.$TS"
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
WORK="$1"; PORT="$2"
export DISPLAY=:0
mkdir -p "$WORK" || exit 1
echo "$PORT" > "$WORK/port"
: > "$WORK/supid"
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
  --capture "x11::0.0" --name ch-bench >"$WORK/host.log" 2>&1 &
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

# 采样：被控端进程的四个「会涨」的量
cat > "$WORKW/sample.sh" <<'SMP'
#!/usr/bin/env bash
W="$1"
PID=$(cat "$W/hostpid" 2>/dev/null)
[ -n "$PID" ] || { echo "NOPID"; exit 1; }
[ -d "/proc/$PID" ] || { echo "DEAD"; exit 1; }
RSS=$(awk '/^VmRSS:/{print $2}' "/proc/$PID/status" 2>/dev/null)
TH=$(awk '/^Threads:/{print $2}' "/proc/$PID/status" 2>/dev/null)
FD=$(ls "/proc/$PID/fd" 2>/dev/null | wc -l)
CHLD=$(ps --no-headers -o pid= --ppid "$PID" 2>/dev/null | wc -l)
FF=$(ps --no-headers -o comm= --ppid "$PID" 2>/dev/null | grep -c '^ffmpeg' || true)
echo "$PID rss=${RSS:-0} threads=${TH:-0} fds=${FD:-0} children=${CHLD:-0} ffmpeg=${FF:-0}"
SMP

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
  tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -q "uulan-ch" && kill "$pid" 2>/dev/null
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

samp() {   # samp <标签> ；结果落到 $WORKW/samples.tsv
  local tag="$1" out
  out=$(ssh "$KY" "bash $WORKK/sample.sh $WORKK" 2>/dev/null | tail -1)
  case "$out" in
    ''|NOPID|DEAD) bad "采样失败（$tag）：${out:-空}" ; return 1 ;;
  esac
  echo -e "$tag\t$out" >> "$WORKW/samples.tsv"
  echo "      [$tag] $out"
}

echo "== 0. 前置 =="
ssh -o ConnectTimeout=6 "$KY" true >/dev/null 2>&1 || env_out "$KY 不可达（先按失联判法分辨真假）"
[ -f "$CLI" ] || env_out "控制端 exe 不存在：$CLI（先 cargo build --release 再拷出工作区）"
BUSY=$(ssh "$KY" "ss -ltn 2>/dev/null | awk '{print \$4}' | grep -cE ':$PORT\$'" 2>/dev/null | tail -1)
[ "${BUSY:-0}" = "0" ] || env_out "真机上 $PORT 已被占用（上一轮台架没清干净？）"
ssh "$KY" "mkdir -p $WORKK" || env_out "远端建不了 $WORKK"
scp -q "$WORKW/remote.sh" "$WORKW/sample.sh" "$WORKW/clean.sh" "$KY:$WORKK/" || env_out "scp 台架脚本上不去"
: > "$WORKW/samples.tsv"

echo "== 1. 起远端台架（中继 :$PORT + 被控端抓真 :0 桌面）=="
ssh "$KY" "setsid bash $WORKK/remote.sh $WORKK $PORT > $WORKK/bed.out 2>&1 < /dev/null &" >/dev/null 2>&1 || true
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
HOSTPID0=$(ssh "$KY" "cat $WORKK/hostpid" 2>/dev/null | tail -1)
sleep 2
samp "baseline" >/dev/null
BASE=$(tail -1 "$WORKW/samples.tsv" | cut -f2)
echo "      基线：$BASE"
ok "台架就绪（设备 $DEV，被控端 PID $HOSTPID0）"

one() {   # one <tag>：跑一条会话，返回 0/1
  local tag="$1" rc
  "$CLI" --server "$KY_IP:$PORT" --device "$DEV" --password-file "$WORKW/pw.txt" \
    --headless --fps 5 --seconds "$SECS" --json > "$WORKW/s-$tag.out" 2> "$WORKW/s-$tag.err"
  rc=$?
  return "$rc"
}

echo "== 2. 串行 $NSER 条会话（每收一条采一次样）=="
SER_FAIL=0
for i in $(seq 1 "$NSER"); do
  if ! one "ser$i"; then SER_FAIL=$((SER_FAIL + 1)); fi
  sleep 1.2
  samp "ser$i" >/dev/null
done
[ "$SER_FAIL" = "0" ] && ok "$NSER 条串行会话全部 rc=0" || bad "$SER_FAIL/$NSER 条串行会话没跑成"
ff_now=$(tail -1 "$WORKW/samples.tsv" | sed -n 's/.*ffmpeg=\([0-9]*\).*/\1/p')
ch_now=$(tail -1 "$WORKW/samples.tsv" | sed -n 's/.*children=\([0-9]*\).*/\1/p')
[ "${ff_now:-9}" = "0" ] && ok "串行段收工后 ffmpeg 子进程归零" || bad "串行段收工后还有 ${ff_now:-?} 个 ffmpeg 子进程（编码器没回收）"
[ "${ch_now:-9}" = "0" ] && ok "串行段收工后子进程归零" || bad "串行段收工后还有 ${ch_now:-?} 个子进程"

echo "== 3. 并发 $NROUND 轮 × 3 条会话 =="
CON_FAIL=0
for r in $(seq 1 "$NROUND"); do
  CPIDS=""
  for n in 1 2 3; do
    "$CLI" --server "$KY_IP:$PORT" --device "$DEV" --password-file "$WORKW/pw.txt" \
      --headless --fps 5 --seconds "$SECS" > "$WORKW/c$r-$n.out" 2> "$WORKW/c$r-$n.err" &
    CPIDS="$CPIDS $!"
  done
  for p in $CPIDS; do wait "$p" || CON_FAIL=$((CON_FAIL + 1)); done
  CPIDS=""
  sleep 2
  samp "round$r" >/dev/null
done
[ "$CON_FAIL" = "0" ] && ok "$NROUND 轮并发（$((NROUND * 3)) 条会话）全部 rc=0" || bad "并发段有 $CON_FAIL 条会话失败"

echo "== 4. 静置 8 秒后终值 =="
sleep 8
samp "final" >/dev/null

echo "== 5. 判据 =="
ssh "$KY" "cat $WORKK/host.log" > "$WORKW/host.log" 2>/dev/null || true
cat "$WORKW/samples.tsv"
python - "$WORKW" <<'PY' > "$WORKW/verdict.txt" 2>&1
import sys
w = sys.argv[1]
rows = []
for ln in open("%s/samples.tsv" % w):
    parts = ln.strip().split("\t")
    if len(parts) != 2:
        continue
    tag, rest = parts
    d = {"tag": tag}
    for kv in rest.split():
        if "=" in kv:
            k, v = kv.split("=", 1)
            d[k] = int(v.replace("KB", ""))
    rows.append(d)
by = {r["tag"]: r for r in rows}
base = by.get("baseline"); fin = by.get("final")
if not base or not fin:
    print("NO_DATA"); raise SystemExit(1)

def g(row, k):
    return row.get(k, 0)

print("delta_rss_kb\t%d" % (g(fin, "rss") - g(base, "rss")))
print("delta_threads\t%d" % (g(fin, "threads") - g(base, "threads")))
print("delta_fds\t%d" % (g(fin, "fds") - g(base, "fds")))
print("final_ffmpeg\t%d" % g(fin, "ffmpeg"))
print("final_children\t%d" % g(fin, "children"))
# 预热之后是否还在单调增长：拿 串行第 5 条 与 终值 之间摊到每条会话的 RSS 增量
warm = by.get("ser5")
if warm:
    n_after = 7 + 4 * 3
    per = (g(fin, "rss") - g(warm, "rss")) / float(n_after)
    print("rss_per_session_after_warmup_kb\t%.0f" % per)
mx = max([g(r, "rss") for r in rows] or [0])
print("max_rss_kb\t%d" % mx)
PY
cat "$WORKW/verdict.txt" | sed 's/^/      /'
vd() { awk -F'\t' -v k="$1" '$1==k{print $2}' "$WORKW/verdict.txt" | head -1; }

DR=$(vd delta_rss_kb); DT=$(vd delta_threads); DF=$(vd delta_fds)
FF=$(vd final_ffmpeg); CH=$(vd final_children); PER=$(vd rss_per_session_after_warmup_kb)
[ "${FF:-9}" = "0" ] && ok "终态 ffmpeg 子进程 0（编码器全回收）" || bad "终态还有 ${FF:-?} 个 ffmpeg 子进程"
[ "${CH:-9}" = "0" ] && ok "终态子进程 0" || bad "终态还有 ${CH:-?} 个子进程"
[ "${DF:-99}" -le 6 ] 2>/dev/null && ok "句柄增量 ${DF}（≤6）" || bad "句柄增量 ${DF}（>6，像 fd 泄漏）"
[ "${DT:-99}" -le 8 ] 2>/dev/null && ok "线程增量 ${DT}（≤8）" || bad "线程增量 ${DT}（>8，像线程泄漏）"
[ "${DR:-999999}" -le 40000 ] 2>/dev/null && ok "内存增量 ${DR} KB（≤40 MB）" || bad "内存增量 ${DR} KB（>40 MB）"
if [ -n "${PER:-}" ]; then
  awk -v p="$PER" 'BEGIN{exit !(p<=2048)}' && ok "预热后每条会话平均多占 ${PER} KB（≤2 MB，不单调涨）" \
    || bad "预热后每条会话平均多占 ${PER} KB（>2 MB，像内存泄漏）"
else
  skip "预热基线缺失，无法判断单调增长"
fi
NEND=$(grep -ac "会话结束" "$WORKW/host.log" || true)
EXPECT=$((NSER + NROUND * 3))
[ "${NEND:-0}" -ge "$EXPECT" ] 2>/dev/null && ok "被控端记到 $NEND 条「会话结束」（期望 ≥$EXPECT）" \
  || bad "被控端只记到 ${NEND:-0} 条「会话结束」（期望 ≥$EXPECT）"
grep -aiE "panic|ABRT|invalid pointer|memory corruption" "$WORKW/host.log" > "$WORKW/hostbad.txt" 2>/dev/null || true
[ -s "$WORKW/hostbad.txt" ] && bad "被控端日志有崩溃迹象：$(head -2 "$WORKW/hostbad.txt")" || ok "被控端日志无 panic/ABRT"
ALIVE=$(ssh "$KY" "kill -0 $HOSTPID0 2>/dev/null && echo YES || echo NO" 2>/dev/null | tail -1)
[ "$ALIVE" = "YES" ] && ok "被控端 PID $HOSTPID0 全程没换" || bad "被控端进程没了（原 PID $HOSTPID0）"

echo
echo "==== 结果：$PASS 通过 / $FAIL 失败 / $SKIP 跳过 ===="
echo "证据目录：$WORKW（真机侧 $WORKK）"
[ "$FAIL" = "0" ] && exit 0 || exit 1
