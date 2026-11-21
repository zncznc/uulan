#!/usr/bin/env bash
# 并发双控制端（麒麟本地 + Windows 同连一台被控端）下的 **bulk 并发 / 音频·麦克风 / 中继抖动** 验收。
#
# 上一轮（docs/milestones/并发双控制端-真机-2026-11-21.txt）只验了「两台控制端同时看画面 +
# 各一次移动注入」，剩下三格一直挂着：
#   ① 并发做文件传输 / 终端 / 端口映射（bulk 会不会互相拖垮、终端注册表串不串）
#   ② 两条会话同时 --audio（下行）与 --mic（上行）的争用
#   ③ 并发下中继重启时两台控制端各自接回
# 这个台架把三格一次性钉住。
#
# 用法（开发机 git-bash，需与真机同二层；先 cargo build --release 并把客户端拷出工作区）：
#   UULAN_CLIENT=D:/uulan-bin/uulan-client.exe bash scripts/verify-concurrent-bulk-e2e.sh
# 可覆盖：KYLIN_TARGET=kylin KYLIN_IP=192.168.223.15 UULAN_CONC_PORT=21994 UULAN_MIB=16
# 退出码：0 = 全过；1 = 断言失败（产品问题）；2 = 台架/环境给不出判据（别记成产品缺陷）。
set -u

KY="${KYLIN_TARGET:-kylin}"
KY_IP="${KYLIN_IP:-192.168.223.15}"
PORT="${UULAN_CONC_PORT:-21994}"
ECHO_PORT=$((PORT + 1))
LPORT="${UULAN_CONC_LPORT:-23322}"
MIB="${UULAN_MIB:-16}"
CLI="${UULAN_CLIENT:?要 UULAN_CLIENT=<工作区外的 Windows 控制端 exe，D:/ 形式>}"
TS=$(date +%Y%m%d-%H%M%S)
RID="$TS"                                 # 本轮专用前缀：清理只删自己这批文件（跨轮互不误删）
WORKW="D:/uulan-bin/conc.$TS"
WORKK="/tmp/uulan-conc.$TS"
NO_COLOR=1
export NO_COLOR

PASS=0; FAIL=0; SKIP=0
ok()   { echo "  [PASS] $*"; PASS=$((PASS+1)); }
bad()  { echo "  [FAIL] $*"; FAIL=$((FAIL+1)); }
skip() { echo "  [SKIP] $*"; SKIP=$((SKIP+1)); }
env_out() { echo "  [环境] $*"; echo "退出码 2：这台机器此刻给不出这条判据，别把它记成产品缺陷"; exit 2; }
now_ms() { python -c 'import time;print(int(time.time()*1000))'; }

mkdir -p "$WORKW" || env_out "建不了 $WORKW"

# ── 远端台架脚本（落文件送过去再 bash：ssh 命令串里的嵌套引号会被守卫判 malformed）──
cat > "$WORKW/remote.sh" <<'RSH'
#!/usr/bin/env bash
# 真机侧台架：中继（监督循环，方便本轮 KILL 抖动）+ 回环 echo 靶子 + 被控端（lavfi 画面 + 440Hz）
set -u
BIN="$HOME/.local/uulan/src/target/release"
WORK="$1"; PORT="$2"; ECHO="$3"
mkdir -p "$WORK" || exit 1
cat > "$WORK/echo.py" <<'PY'
import socket, threading, sys
port = int(sys.argv[1])
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", port)); s.listen(16)
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
: > "$WORK/supid"
python3 "$WORK/echo.py" "$ECHO" >"$WORK/echo.log" 2>&1 &
echo $! >> "$WORK/supid"
# 中继监督循环：子进程被 KILL 后 0.3 s 自己起来（第 4 段靠它造「中继重启」）
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
  --capture "lavfi:testsrc2=size=1280x720:rate=20" \
  --audio-source "lavfi:sine=frequency=440:sample_rate=48000" \
  --name conc-bench >"$WORK/host.log" 2>&1 &
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
# 监督循环的子进程可能换了 PID（循环重启过）——按「绑定端口」再扫一遍，否则下一轮端口预检会拦下自己。
if [ -f "$W/port" ]; then
  P=$(cat "$W/port")
  pgrep -f "uulan-server --bind 0.0.0.0:$P" 2>/dev/null | while read -r pid; do kill "$pid" 2>/dev/null; done
fi
# 只删本轮自己的文件：固定名字的删除会让「上一轮迟到的清理」把下一轮的证据抹掉（本轮真踩到）。
python3 - "$RID" <<'PY'
import glob, os, sys
for p in glob.glob("/tmp/conc-%s-*" % sys.argv[1]):
    try:
        os.remove(p)
    except OSError:
        pass
PY
CLS

# 真机侧：A 控制端（麒麟本地自编客户端）的各阶段动作。每个阶段把 rc 写进 rc-<phase>。
cat > "$WORKW/kylin_phase.sh" <<'KPH'
#!/usr/bin/env bash
set -u
BIN="$HOME/.local/uulan/src/target/release"
WORK="$1"; PHASE="$2"; RID="$3"
CLI="$BIN/uulan-client"
DEV=$(cat "$WORK/dev")
PW="$WORK/pw"
PORT=$(cat "$WORK/port")
RC="$WORK/rc-$PHASE"
: > "$RC"
bg() { # bg <tag> <args...>
  local tag="$1"; shift
  ( "$@" >"$WORK/$tag.out" 2>"$WORK/$tag.err"; echo "$tag=$?" >>"$RC" ) &
}
case "$PHASE" in
  bulk)
    bg a_video "$CLI" --server "127.0.0.1:$PORT" --device "$DEV" --password-file "$PW" \
        --headless --seconds 55 --json --fps 20
    sleep 8
    for i in 1 2 3; do
      bg "a_put$i" "$CLI" --server "127.0.0.1:$PORT" --device "$DEV" --password-file "$PW" \
          --put "$WORK/src-a.bin" "/tmp/conc-$RID-a-$i.bin"
      sleep 1
    done
    cat > "$WORK/a_term.sh" <<EOF
#!/usr/bin/env bash
printf 'echo CONC-TERM-OK\nexit 3\n' | "$CLI" --server "127.0.0.1:$PORT" --device "$DEV" --password-file "$PW" --term
exit \$?
EOF
    chmod +x "$WORK/a_term.sh"
    script -qec "bash $WORK/a_term.sh" /dev/null >"$WORK/a_term.out" 2>&1
    echo "a_term=$?" >> "$RC"
    wait
    ;;
  audio)
    bg a_audio "$CLI" --server "127.0.0.1:$PORT" --device "$DEV" --password-file "$PW" \
        --headless --seconds 28 --audio
    wait
    ;;
  restart)
    bg a_restart_video "$CLI" --server "127.0.0.1:$PORT" --device "$DEV" --password-file "$PW" \
        --headless --seconds 40 --json --fps 20 --reconnect 8
    wait
    ;;
esac
exit 0
KPH

# 真机侧：中继抖动（连打 3 次 KILL，间隔 5 s；监督循环负责拉起）
cat > "$WORKW/kylin_jitter.sh" <<'KJ'
#!/usr/bin/env bash
W="$1"
sleep 12
for i in 1 2 3; do
  kill -s KILL "$(cat "$W/srvpid")" 2>/dev/null
  echo "kill$i $(date -u +%T)"
  sleep 5
done
KJ

cleanup() {
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

echo "== 0. 前置：真机可达 + 物料 =="
ssh -o ConnectTimeout=6 "$KY" true >/dev/null 2>&1 || env_out "$KY 不可达（先按失联判法分辨真假，别记产品失败）"
[ -f "$CLI" ] || env_out "控制端 exe 不存在：$CLI（先 cargo build --release 再拷出工作区）"
ssh "$KY" "mkdir -p $WORKK" || env_out "远端建不了 $WORKK"
scp -q "$WORKW/remote.sh" "$WORKW/clean.sh" "$WORKW/kylin_phase.sh" "$WORKW/kylin_jitter.sh" "$KY:$WORKK/" \
  || env_out "scp 台架脚本上不去"
echo "   工作目录：$WORKW / $WORKK  端口：$PORT（echo $ECHO_PORT，转发 $LPORT）"
# 端口预检：上一轮要是留下台架，本轮会悄悄连到别人的中继/echo 上（本轮首跑就是这么被坑的）。
BUSY=$(ssh "$KY" "ss -ltn 2>/dev/null | awk '{print \$4}' | grep -cE ':$PORT\$|:$ECHO_PORT\$'" 2>/dev/null | tail -1)
[ "${BUSY:-0}" = "0" ] || env_out "真机上 $PORT/$ECHO_PORT 已被占用（上一轮台架没清干净？先清再跑）"
BUSYL=$(netstat -ano 2>/dev/null | grep -c "127.0.0.1:$LPORT " || true)
[ "${BUSYL:-0}" = "0" ] || env_out "本机 $LPORT 已被占用（上一轮转发没退？）"

echo "== 1. 起远端台架（中继 0.0.0.0:$PORT + 回环 echo + 被控端 lavfi/440Hz）=="
# 必须把台架彻底 detach：ssh 会一直等到 stdout 管道关闭，而台架里的中继监督循环/被控端
# 会继承那个管道 —— 直接 `ssh "bash remote.sh"` 会把本脚本永久挂住（本轮首跑就栽在这）。
ssh "$KY" "setsid bash $WORKK/remote.sh $WORKK $PORT $ECHO_PORT > $WORKK/bed.out 2>&1 < /dev/null &" \
  > /dev/null 2>&1 || true
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
[ -s "$WORKW/pw.txt" ] || env_out "口令不合格式（传输被污染？）"
printf '%s\n' "$PORT" > "$WORKW/port.txt"
scp -q "$WORKW/port.txt" "$KY:$WORKK/port"
HOSTPID=$(ssh "$KY" "tail -1 $WORKK/supid" 2>/dev/null | tr -d '[:space:]')
ok "台架就绪（设备 $DEV，被控端 PID $HOSTPID，口令已落 0600 文件）"

python - "$WORKW" "$MIB" <<'PY'
import os, sys
w, mib = sys.argv[1], int(sys.argv[2])
for name, seed in (("src-a.bin", 0xA1), ("src-b.bin", 0xB2)):
    p = os.path.join(w, name)
    with open(p, "wb") as f:
        left = mib * 1024 * 1024
        while left > 0:
            n = min(left, 1 << 20)
            f.write(bytes([seed]) * n)
            left -= n
print("   物料就绪：src-a.bin / src-b.bin 各 %d MiB" % mib)
PY
LA=$(sha256sum "$WORKW/src-a.bin" | awk '{print $1}')
LB=$(sha256sum "$WORKW/src-b.bin" | awk '{print $1}')
scp -q "$WORKW/src-a.bin" "$KY:$WORKK/src-a.bin"
scp -q "$WORKW/src-b.bin" "$KY:$WORKK/src-b.bin"
: > "$WORKW/b_rc"

echo "== 2. 并发窗口：两台控制端同时看画面 + 文件传输 / 终端 / 端口映射 =="
T0=$(now_ms)
ssh "$KY" "bash $WORKK/kylin_phase.sh $WORKK bulk $RID" > "$WORKW/a_bulk.log" 2>&1 &
ASESS=$!
"$CLI" --server "$KY_IP:$PORT" --device "$DEV" --password-file "$WORKW/pw.txt" \
  --headless --seconds 55 --json --fps 20 > "$WORKW/b_video.json" 2>"$WORKW/b_video.err" &
BVID=$!
sleep 9
for i in 1 2 3; do
  "$CLI" --server "$KY_IP:$PORT" --device "$DEV" --password-file "$WORKW/pw.txt" \
    --put "$WORKW/src-b.bin" "/tmp/conc-$RID-b-$i.bin" > "$WORKW/b_put$i.out" 2>"$WORKW/b_put$i.err"
  echo "b_put$i=$?" >> "$WORKW/b_rc"
  sleep 1
done
# B 的端口映射：本地 $LPORT → 被控端 127.0.0.1:$ECHO_PORT；慢速推 8 MiB 让流活过 ~16 秒
"$CLI" --server "$KY_IP:$PORT" --device "$DEV" --password-file "$WORKW/pw.txt" \
  --forward "127.0.0.1:$LPORT:127.0.0.1:$ECHO_PORT" --seconds 26 > "$WORKW/b_fwd.out" 2>"$WORKW/b_fwd.err" &
BFWD=$!
sleep 1.5
python - "$LPORT" > "$WORKW/echo_test.out" 2>&1 <<'PY'
import socket, sys, time, hashlib
port = int(sys.argv[1])
chunk = bytes(range(256)) * 4096          # 1 MiB
h = hashlib.sha256()
s = None
for attempt in range(15):                  # 转发监听是异步起来的：连不上就等，别把台架时序当产品失败
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout=60)
        break
    except OSError:
        time.sleep(1.0)
if s is None:
    print("ECHO_NO_LISTENER"); sys.exit(1)
got = 0
try:
    for _ in range(8):                     # 8 MiB，每块停 2 s → 与视频/文件/终端真并发
        s.sendall(chunk); h.update(chunk); got += len(chunk)
        time.sleep(2.0)
    s.shutdown(socket.SHUT_WR)
    back = b""
    while True:
        d = s.recv(65536)
        if not d:
            break
        back += d
    if len(back) != got:
        print("ECHO_MISMATCH", len(back), got); sys.exit(1)
    if hashlib.sha256(back).hexdigest() != h.hexdigest():
        print("ECHO_SHA_DIFF"); sys.exit(1)
    print("ECHO_OK", got)
except Exception as e:
    print("ECHO_ERR", e); sys.exit(1)
PY
echo "echo_test=$?" >> "$WORKW/b_rc"
wait "$BVID"; echo "b_video=$?" >> "$WORKW/b_rc"
wait "$ASESS"
wait "$BFWD"; echo "b_fwd=$?" >> "$WORKW/b_rc"
T1=$(now_ms)
echo "   并发窗口 $(( (T1 - T0) / 1000 )) s（两条视频 + 6 次文件传输 + 终端 + 端口映射同时在跑）"

echo "   -- 文件传输 / 终端 / 端口映射 --"
RCK=$(ssh "$KY" "cat $WORKK/rc-bulk" 2>/dev/null)
echo "$RCK" | sed 's/^/      kylin: /'
echo "$RCK" | grep -q '^a_video=0$' && ok "A 视频会话 rc=0" || bad "A 视频会话 rc≠0"
echo "$RCK" | grep -q '^a_term=3$' && ok "A 终端退出码 3（= 远端 shell 的 exit 3）" || bad "A 终端退出码不是 3：$(echo "$RCK" | grep a_term)"
ssh "$KY" "cat $WORKK/a_term.out" 2>/dev/null > "$WORKW/a_term.out"
grep -aq "CONC-TERM-OK" "$WORKW/a_term.out" && ok "终端输出里有哨兵 CONC-TERM-OK" || bad "终端输出里没有哨兵（$(head -c 200 "$WORKW/a_term.out" | tr -d '\r')）"
NA_OK=0
for i in 1 2 3; do
  if echo "$RCK" | grep -q "^a_put$i=0$"; then NA_OK=$((NA_OK+1)); fi
done
[ "$NA_OK" = "3" ] && ok "A 上传 3 轮全 rc=0" || bad "A 上传只有 $NA_OK/3 轮成功：$(echo "$RCK" | grep a_put)"
grep -q '^b_put1=0$' "$WORKW/b_rc" && grep -q '^b_put2=0$' "$WORKW/b_rc" && grep -q '^b_put3=0$' "$WORKW/b_rc" \
  && ok "B 上传 3 轮全 rc=0" || bad "B 上传有失败：$(cat "$WORKW/b_rc")"
REMOTE_SHA=$(ssh "$KY" "sha256sum /tmp/conc-$RID-a-*.bin /tmp/conc-$RID-b-*.bin 2>/dev/null; ls -l /tmp/conc-$RID-*.bin 2>/dev/null | wc -l")
echo "$REMOTE_SHA" | grep -v sha256sum | sed 's/^/      remote: /' | head -8
NA=$(echo "$REMOTE_SHA" | grep -c "conc-$RID-a-"); NB=$(echo "$REMOTE_SHA" | grep -c "conc-$RID-b-")
[ "$NA" = "3" ] && [ "$NB" = "3" ] && ok "远端 6 个文件都在（A 3 + B 3）" || bad "远端文件不全：A=$NA B=$NB（$(echo "$REMOTE_SHA" | tail -1)）"
A_OK=$(echo "$REMOTE_SHA" | grep "conc-$RID-a-" | awk '{print $1}' | grep -c "^$LA$")
B_OK=$(echo "$REMOTE_SHA" | grep "conc-$RID-b-" | awk '{print $1}' | grep -c "^$LB$")
[ "$A_OK" = "3" ] && ok "A 的 3 个文件 sha256 全一致（${LA:0:12}…）" || bad "A 文件 sha256 不一致：$A_OK/3"
[ "$B_OK" = "3" ] && ok "B 的 3 个文件 sha256 全一致（${LB:0:12}…）" || bad "B 文件 sha256 不一致：$B_OK/3"
grep -q "ECHO_OK" "$WORKW/echo_test.out" && ok "端口映射回环 echo 逐字节一致（$(grep -o 'ECHO_OK [0-9]*' "$WORKW/echo_test.out")）" \
  || bad "端口映射 echo 失败：$(cat "$WORKW/echo_test.out")｜转发端：$(grep -a '端口映射结束' "$WORKW/b_fwd.out" "$WORKW/b_fwd.err" 2>/dev/null | tail -1)"
grep -q '^b_fwd=0$' "$WORKW/b_rc" && ok "端口映射会话 rc=0" || bad "端口映射会话 rc≠0"

echo "   -- 两条视频会话（并发窗口内）--"
json_stats() { # $1=json 文本 $2=标签
  python - "$1" "$2" <<'PY'
import json, sys
txt, tag = sys.argv[1], sys.argv[2]
i = txt.find("{")
if i < 0:
    print(tag, "NO_JSON"); raise SystemExit
d = json.loads(txt[i:])
st = d.get("stats", {})
print("%s decoded=%s dropped=%s sessions=%s reconnects=%s first_frame_ms=%s host_fps=%.1f" % (
    tag, st.get("decoded"), st.get("dropped"), d.get("sessions"), d.get("reconnects"),
    st.get("first_frame_ms"), st.get("host_fps", 0)))
PY
}
json_stats "$(cat "$WORKW/b_video.json")" B | tee "$WORKW/b_video_chk.out"
json_stats "$(ssh "$KY" "cat $WORKK/a_video.out" 2>/dev/null)" A | tee "$WORKW/a_video_chk.out"
for f in "$WORKW/a_video_chk.out" "$WORKW/b_video_chk.out"; do
  D=$(grep -oE "decoded=[0-9]+" "$f" | cut -d= -f2)
  DR=$(grep -oE "dropped=[0-9]+" "$f" | cut -d= -f2)
  if [ -n "${D:-}" ] && [ "$D" -gt 100 ] && [ "${DR:-99}" -le 10 ]; then
    ok "$(basename "$f" .out) 画面解出 $D 帧 / 丢帧 $DR"
  else
    bad "$(basename "$f" .out) 画面不达标：$(cat "$f")"
  fi
done
HLOG=$(ssh "$KY" "cat $WORKK/host.log" 2>/dev/null)
echo "$HLOG" > "$WORKW/host_phase2.log"
echo "$HLOG" | grep -qiE "panic|ABRT|invalid pointer" && bad "被控端日志有 panic/ABRT" || ok "被控端日志无 panic/ABRT"
SESS_END=$(echo "$HLOG" | grep -c "会话结束")
[ "$SESS_END" -ge 8 ] && ok "被控端记录了 $SESS_END 条会话收尾（6 次上传 + 终端 + 2 条视频）" || bad "会话收尾只有 $SESS_END 条（预期 ≥8）"
RSS=$(ssh "$KY" "grep VmRSS /proc/$HOSTPID/status 2>/dev/null" | awk '{print $2}')
echo "      被控端 VmRSS=${RSS:-?} kB"

echo "== 3. 并发音频：A 下行 --audio / B 上行 --mic（两条会话同时）=="
ssh "$KY" "bash $WORKK/kylin_phase.sh $WORKK audio $RID" > "$WORKW/a_audio.log" 2>&1 &
AA=$!
sleep 1
"$CLI" --server "$KY_IP:$PORT" --device "$DEV" --password-file "$WORKW/pw.txt" \
  --headless --seconds 28 --mic --mic-source "lavfi:sine=frequency=880:sample_rate=48000" \
  > "$WORKW/b_mic.out" 2>"$WORKW/b_mic.err"
echo "b_mic=$?" >> "$WORKW/b_rc"
wait "$AA"
ssh "$KY" "cat $WORKK/a_audio.out" 2>/dev/null > "$WORKW/a_audio.out"
grep -q "音频状态 : 被控端正在推流" "$WORKW/a_audio.out" && ok "A 下行音频状态「被控端正在推流」" \
  || bad "A 下行音频状态不是推流中：$(grep '音频状态' "$WORKW/a_audio.out")"
APB=$(grep -oE "播放 [0-9]+ 字节" "$WORKW/a_audio.out" | head -1 | grep -oE '[0-9]+')
[ -n "${APB:-}" ] && [ "$APB" -gt 100000 ] && ok "A 下行播放 $APB 字节（不是零字节自证）" || bad "A 下行播放字节不足：${APB:-无}"
BUP=$(grep -oE "上行 [0-9]+ 帧 / [0-9]+ 字节" "$WORKW/b_mic.out" | head -1)
BBYTES=$(echo "$BUP" | grep -oE "/ [0-9]+ 字节" | grep -oE '[0-9]+')
[ -n "${BBYTES:-}" ] && [ "$BBYTES" -gt 10000 ] && ok "B 上行 $BUP（本机真的采到并送上去了）" || bad "B 上行为空/过小：${BUP:-无}"
grep -q "麦克风状态 : 被控端正在推流" "$WORKW/b_mic.out" && ok "B 麦克风状态「被控端正在推流」" || bad "B 麦克风状态不对：$(grep '麦克风状态' "$WORKW/b_mic.out")"
MHOST=$(ssh "$KY" "grep '麦克风转接收尾' $WORKK/host.log | tail -1" 2>/dev/null)
echo "      host: ${MHOST:-无收尾行}"
echo "$MHOST" | grep -qE 'bytes=[1-9][0-9]{4,}' && ok "被控端收到上行音频（$(echo "$MHOST" | grep -oE 'bytes=[0-9]+ frames=[0-9]+')）" \
  || bad "被控端没收上行音频"
BDEC=$(grep -oE "解出帧     : [0-9]+" "$WORKW/b_mic.out" | grep -oE '[0-9]+')
[ -n "${BDEC:-}" ] && [ "$BDEC" -gt 100 ] && ok "B 同一会话还有画面（解出 $BDEC 帧）" || bad "B 会话画面不足：${BDEC:-无}"
ADEC=$(grep -oE "解出帧     : [0-9]+" "$WORKW/a_audio.out" | grep -oE '[0-9]+')
[ -n "${ADEC:-}" ] && [ "$ADEC" -gt 100 ] && ok "A 同一会话还有画面（解出 $ADEC 帧）" || bad "A 会话画面不足：${ADEC:-无}"

echo "== 4. 中继抖动：两台控制端正在看画面时连打 3 次 KILL =="
ssh "$KY" "bash $WORKK/kylin_phase.sh $WORKK restart $RID" > "$WORKW/a_restart.log" 2>&1 &
AR=$!
"$CLI" --server "$KY_IP:$PORT" --device "$DEV" --password-file "$WORKW/pw.txt" \
  --headless --seconds 40 --json --reconnect 8 > "$WORKW/b_restart.json" 2>"$WORKW/b_restart.err" &
BR=$!
ssh "$KY" "bash $WORKK/kylin_jitter.sh $WORKK" > "$WORKW/jitter.out" 2>&1
wait "$AR"
wait "$BR"; BRR=$?
sed 's/^/      /' "$WORKW/jitter.out"
[ "$BRR" = "0" ] && ok "B 在 3 次中继 KILL 后仍正常收工（rc=0）" || bad "B 抖动轮 rc=$BRR"
RCK2=$(ssh "$KY" "cat $WORKK/rc-restart" 2>/dev/null)
echo "$RCK2" | grep -q '^a_restart_video=0$' && ok "A 在 3 次中继 KILL 后仍正常收工（rc=0）" || bad "A 抖动轮 rc≠0：$RCK2"
JLOG=$(ssh "$KY" "cat $WORKK/host.log" 2>/dev/null)
RE=$(echo "$JLOG" | grep -c "已用原有设备 ID 上线")
[ "$RE" -ge 2 ] && ok "被控端重连上线 $RE 次（中继确实被拉起过）" || bad "被控端重连上线只有 $RE 次"
HPID_AFTER=$(ssh "$KY" "tail -1 $WORKK/supid" 2>/dev/null | tr -d '[:space:]')
[ -n "$HOSTPID" ] && [ "$HOSTPID" = "$HPID_AFTER" ] && ok "被控端进程本身没重启（PID $HOSTPID 不变）" \
  || bad "被控端 PID 变了（$HOSTPID → $HPID_AFTER）"
json_stats "$(cat "$WORKW/b_restart.json")" B | tee "$WORKW/b_restart_chk.out"
json_stats "$(ssh "$KY" "cat $WORKK/a_restart_video.out" 2>/dev/null)" A | tee "$WORKW/a_restart_chk.out"
RECA=$(grep -oE "reconnects=[0-9]+" "$WORKW/a_restart_chk.out" | cut -d= -f2)
RECB=$(grep -oE "reconnects=[0-9]+" "$WORKW/b_restart_chk.out" | cut -d= -f2)
[ -n "${RECA:-}" ] && [ "$RECA" -ge 2 ] && ok "A 断线接回 $RECA 次" || bad "A 接回次数不足：${RECA:-无}"
[ -n "${RECB:-}" ] && [ "$RECB" -ge 2 ] && ok "B 断线接回 $RECB 次" || bad "B 接回次数不足：${RECB:-无}"
echo "$JLOG" | grep -qiE "panic|ABRT|invalid pointer" && bad "抖动轮之后被控端日志出现 panic/ABRT" || ok "抖动轮之后被控端无 panic/ABRT"

echo "== 汇总 =="
echo "  通过 $PASS / 失败 $FAIL / 跳过 $SKIP"
[ "$FAIL" = "0" ] || exit 1
exit 0
