#!/usr/bin/env bash
# 麒麟控制端的实时播放（M7 的最后一格）—— **在真机上跑**，不是从开发机编排。
#
#   被控端（合成 440 Hz 正弦）→ 加密的 audio 通道 → 控制端 ffmpeg 解码出 s16le PCM
#   → 交给本机播放命令（aplay）→ 再用 parecord 从信宿的 monitor 把声音录回来
#   → 在录回来的波形上断言「440 Hz 还是主角」。
#
# 为什么必须录回来：统计里那个「播放 N 字节」只证明字节写进了子进程的 stdin，
# 而管道能一声不响地吞掉一整晚。判据要落在 PulseAudio 上，不能落在我自己的记账上。
#
# 为什么是 aplay 而不是 paplay：这台麒麟的 paplay（PulseAudio 13.99）只吃 libsndfile
# 认得的文件，喂 raw 一律「打开声音文件失败」（退出码 1）；而 ALSA 的 default 本来就
# 经 pulse 插件落到同一个信宿 —— 对照组（没人播时录回来）证明测到的确实是这条路上播的东西。
#
# 用法（真机上）：bash scripts/verify-audio-playback-e2e.sh
#   可覆盖：UULAN_BIN=<二进制目录> UULAN_SECONDS=10 UULAN_PORT=21998
# 退出码：0 = 全过；1 = 断言失败（产品问题）；2 = 环境给不出这条判据（台架问题，别记成缺陷）。
set -u

BIN="${UULAN_BIN:-$HOME/.local/uulan/src/target/release}"
PORT="${UULAN_PORT:-21998}"
RUN_SECS="${UULAN_SECONDS:-10}"
TS=$(date +%Y%m%d-%H%M%S)
WORK="/tmp/uulan-audio-play.$TS"
NO_COLOR=1
export NO_COLOR
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"

PASS=0; FAIL=0; SKIP=0
ok()   { echo "  [PASS] $*"; PASS=$((PASS+1)); }
bad()  { echo "  [FAIL] $*"; FAIL=$((FAIL+1)); }
skip() { echo "  [SKIP] $*"; SKIP=$((SKIP+1)); }
env_out() { echo "  [环境] $*"; echo "退出码 2：这台机器此刻给不出这条判据，别把它记成产品缺陷"; exit 2; }

mkdir -p "$WORK" || { echo "建不了 $WORK"; exit 2; }

SRV="$BIN/uulan-server"; HOST="$BIN/uulan-host"; CLI="$BIN/uulan-client"
echo "== 0. 预检 =="
for f in "$SRV" "$HOST" "$CLI"; do
  [ -x "$f" ] || env_out "缺可执行文件 $f（先 cargo build --release -p uulan-server -p uulan-host -p uulan-client）"
done
echo "  二进制构建时间：server $(stat -c %y "$SRV" 2>/dev/null | cut -d. -f1)"
echo "                  host   $(stat -c %y "$HOST" 2>/dev/null | cut -d. -f1)"
echo "                  client $(stat -c %y "$CLI" 2>/dev/null | cut -d. -f1)"
sha256sum "$CLI" | awk '{print "  控制端 sha256：", $1}'
for c in ffmpeg pactl parecord aplay python3; do
  command -v "$c" >/dev/null 2>&1 || env_out "这台机器上没有 $c"
done
ok "三个二进制与 ffmpeg/pactl/parecord/aplay 都在"

NSINK=$(pactl list sinks short 2>/dev/null | wc -l)
[ "$NSINK" -ge 1 ] || env_out "pactl 问不到任何信宿（没有声卡 / PulseAudio 没起来）"
SINK=$(pactl list sinks short | awk 'NR==1{print $2}')
if [ "$NSINK" -gt 1 ]; then
  # 多块声卡时「播到哪块」由默认信宿说了算，录错块的判据会假红。
  # pactl 13.99 没有 get-default-sink（回「未指定有效的命令」），只能从 info 的输出里认。
  ALT=$(pactl list sinks short | awk '{printf "%s|", $2}' | sed 's/|$//')
  DEF=$(pactl info 2>/dev/null | grep -oE "$ALT" | head -1)
  [ -n "$DEF" ] && SINK="$DEF"
fi
MON="$SINK.monitor"
pactl list sources short 2>/dev/null | grep -qF "$MON" || env_out "信宿 $SINK 没有 monitor 源，录不回来"
ok "信宿 $SINK（共 $NSINK 块，将录它的 $MON）"

# 分析器：录回来的 WAV 里 440 Hz 到底是不是主角（与被控端那条正弦同源的判据）。
cat > "$WORK/check440.py" <<'PY'
import struct, sys, math
path, want = sys.argv[1], (sys.argv[2] == "tone")
b = open(path, "rb").read()
if b[:4] != b"RIFF" or b[8:12] != b"WAVE":
    print("  不是 WAV"); sys.exit(3)
off, data = 12, b""
while off + 8 <= len(b):
    cid = b[off:off+4]; sz = struct.unpack_from("<I", b, off+4)[0]
    if cid == b"data":
        # parecord 被提前收尾时头里的长度可能没回填：宁可读到文件尾，也别报「空」。
        sz = sz or (len(b) - off - 8)
        data = b[off+8:off+8+sz]; break
    off += 8 + sz + (sz & 1)
ch = struct.unpack_from("<H", b, 22)[0]
rate = struct.unpack_from("<I", b, 24)[0]
bits = struct.unpack_from("<H", b, 34)[0]
n = len(data) // (bits // 8)
samples = struct.unpack_from("<%dh" % n, data, 0)
peak = max(abs(s) for s in samples) if samples else 0

def goertzel(seg, freq):
    coeff = 2 * math.cos(2 * math.pi * freq / rate)
    s1 = s2 = 0.0
    for s in seg:
        s0 = s + coeff * s1 - s2
        s2, s1 = s1, s0
    return s1 * s1 + s2 * s2 - coeff * s1 * s2

w = max(1, int(rate * ch * 2))
p440 = p1000 = 0.0
wins = 0
for i in range(0, len(samples), w):
    seg = samples[i:i+w]
    if len(seg) < w // 2:
        break
    p440 += goertzel(seg, 440.0); p1000 += goertzel(seg, 1000.0); wins += 1
ratio = p440 / max(p1000, 1e-9)
print("  WAV %d 字节 / %.2f 秒 / 峰值 %d / %d 个 2 秒窗 / 440Hz:1000Hz = %.0f 倍"
      % (len(b), n / rate / ch, peak, wins, ratio))
live = peak > 100 and ratio >= 10
if want:
    sys.exit(0 if live else 1)
sys.exit(1 if live else 0)
PY

SERVER_PID=""; HOST_PID=""
cleanup() {
  [ -n "$HOST_PID" ] && kill "$HOST_PID" 2>/dev/null
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null
  return 0
}
trap cleanup EXIT

echo "== 1. 测试台（独立中继 + 独立配置与台账，不碰正式服务的身份与口令）=="
"$SRV" --bind "127.0.0.1:$PORT" --state "$WORK/devices.json" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 100); do
  (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && { exec 3>&-; break; }
  sleep 0.1
done
RUST_LOG=info "$HOST" --server "127.0.0.1:$PORT" --config "$WORK/host.json" \
  --capture "lavfi:testsrc2=size=640x480:rate=15" \
  --audio-source "lavfi:sine=frequency=440:sample_rate=48000" \
  --name playback-host --once >"$WORK/host.log" 2>&1 &
HOST_PID=$!
DEVICE=""
for _ in $(seq 1 200); do
  DEVICE=$(grep -oE "设备 ID : [0-9]{6,}" "$WORK/host.log" 2>/dev/null | tail -1 | sed 's/.*: *//' | tr -d '[:space:]')
  [ -n "$DEVICE" ] && break
  sleep 0.1
done
[ -n "$DEVICE" ] || { echo "被控端没拿到设备 ID："; grep -vE "访问密码" "$WORK/host.log" | sed -n 1,15p; exit 2; }
sed -n 's/.*"password": *"\([^"]*\)".*/\1/p' "$WORK/host.json" | head -1 > "$WORK/pw.txt"
[ -s "$WORK/pw.txt" ] || { echo "host.json 里读不到口令字段"; exit 2; }
chmod 600 "$WORK/pw.txt"
ok "测试台起来了（设备 ID $DEVICE；口令走 0600 文件，不上屏）"

echo "== 2. 对照组：没人播的时候，录回来的不该有 440 Hz =="
BASE="$WORK/baseline.wav"
timeout 3 parecord -d "$MON" --format=s16le --rate=48000 --channels=2 --file-format=wav "$BASE" \
  >"$WORK/baseline.err" 2>&1
[ -s "$BASE" ] || { head -3 "$WORK/baseline.err"; env_out "parecord 录不到 $MON —— 这条判据在这台机器上给不出"; }
python3 "$WORK/check440.py" "$BASE" quiet \
  && ok "对照组干净（没人播时录回来的是静音，440 Hz 不是这间屋子的背景）" \
  || env_out "对照组里就有一段 440 Hz —— 台架判据不成立，这台机器此刻测不了这件事"

echo "== 3. 正跑：控制端只开 --audio（不 --audio-save），同时从信宿 monitor 录 =="
REC="$WORK/playback.wav"
timeout $((RUN_SECS + 4)) parecord -d "$MON" --format=s16le --rate=48000 --channels=2 \
  --file-format=wav "$REC" >"$WORK/rec.err" 2>&1 &
REC_PID=$!
sleep 1
RUST_LOG=info "$CLI" --server "127.0.0.1:$PORT" --device "$DEVICE" \
  --password-file "$WORK/pw.txt" --headless --seconds "$RUN_SECS" \
  --no-input --no-clipboard --audio >"$WORK/client.log" 2>&1
RC=$?
sleep 1; kill "$REC_PID" 2>/dev/null; wait "$REC_PID" 2>/dev/null
echo "  控制端退出码：$RC"
sed -E 's/\x1b\[[0-9;]*m//g' "$WORK/client.log" > "$WORK/client.plain"
grep -E "音频|播放" "$WORK/client.plain" | sed -n 1,12p
[ "$RC" -eq 0 ] && ok "控制端正常收工" || bad "控制端退出码 $RC"

grep -q "实时播放已接上" "$WORK/client.plain" \
  && ok "本机真的起了播放进程（$(grep -o '实时播放已接上.*' "$WORK/client.plain" | head -1)）" \
  || bad "日志里没有「实时播放已接上」—— 这轮等于什么都没播（老二进制在这里会说「打不开音频输出设备」）"
grep -q "实时播放已收尾" "$WORK/client.plain" \
  && ok "播放命令自己退出的（没走到宽限 kill，收尾的尾巴没被切）" \
  || bad "没等到「实时播放已收尾」：要么被宽限 kill，要么根本没起来"

PLAYED=$(grep -o "播放 [0-9]* 字节" "$WORK/client.plain" | tail -1 | tr -dc '0-9')
ERRS=$(grep -o "错误 [0-9]*" "$WORK/client.plain" | tail -1 | tr -dc '0-9')
echo "  播放字节数=${PLAYED:-?} / 解码播放错误=${ERRS:-?}"
[ -n "${PLAYED:-}" ] && [ "${PLAYED:-0}" -gt 100000 ] \
  && ok "播放出去的字节数量级对得上（$PLAYED 字节 > 100000）" || bad "播放字节数不可信：${PLAYED:-空}"
[ "${ERRS:-1}" -eq 0 ] && ok "解码/播放错误 0 次" || bad "解码/播放错误 $ERRS 次"

echo "== 4. 独立判据：录回来的声音里 440 Hz 是不是主角 =="
if [ -s "$REC" ]; then
  python3 "$WORK/check440.py" "$REC" tone \
    && ok "声音真的进了信宿：parecord 从 $MON 录回来的就是那条 440 Hz 正弦" \
    || bad "录回来没有 440 Hz —— 播放链路只到管道，没到 PulseAudio"
else
  head -3 "$WORK/rec.err"
  bad "整段没录到东西（parecord 失败，判据无从谈起）"
fi

echo "== 5. 同会话的画面没被音频拖垮（两条通道共用同一次握手与同一套子密钥）=="
VFRAME=$(grep -o "解出帧     : [0-9]*" "$WORK/client.plain" | tail -1 | tr -dc '0-9')
VDROP=$(grep -o "解码丢帧   : [0-9]*" "$WORK/client.plain" | tail -1 | tr -dc '0-9')
VFIRST=$(grep -o "首帧延迟   : [0-9]*" "$WORK/client.plain" | tail -1 | tr -dc '0-9')
echo "  画面：解出 ${VFRAME:-?} 帧 / 丢帧 ${VDROP:-?} / 首帧 ${VFIRST:-?} ms"
[ "${VFRAME:-0}" -gt 20 ] && ok "开音频的同会话画面照出（${VFRAME} 帧）" || bad "画面只有 ${VFRAME:-0} 帧"
[ "${VDROP:-1}" -eq 0 ] && ok "解码丢帧 0" || bad "解码丢帧 $VDROP"

echo
echo "结论：$PASS 通过 / $FAIL 失败 / $SKIP 跳过（工作目录 $WORK）"
[ "$FAIL" -eq 0 ] || exit 1
exit 0
