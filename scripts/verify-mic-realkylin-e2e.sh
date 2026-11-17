#!/usr/bin/env bash
# 麦克风转接（M7 上行：控制端 → 被控端的音箱）—— **在真机上跑**，不是从开发机编排。
#
#   控制端采集（合成 440 Hz 正弦）→ Opus/Ogg → 加密 audio 通道**上行**
#   → 被控端 ffmpeg 解码 → aplay 播到本机默认信宿
#   → 再用 parecord 从信宿的 monitor 把声音录回来 → 440 Hz 必须是主角。
#
# 判据为什么这样定（全部沿用 2026-11-16/17 两轮真机教训）：
#   * 「播放 N 字节」只证明字节进了管道 —— 声音必须从 PulseAudio 录回来才算数；
#   * 对照组（没人送麦时录 3 秒）必须测不出 tone —— 环境底噪/别的进程都能造假绿；
#   * 「有界源放满 → 控制端采集退 → 写半端 drop → 被控端 read EOF 收尾」：
#     协议里**没有** MicStop，本脚本的第 3 段就是钉这条 EOF 纪律的。
#
# 用法（真机上）：bash scripts/verify-mic-realkylin-e2e.sh
#   UULAN_EXTERNAL_CLIENT=1  不等本机控制端，等**外部**（例如开发机上的 Windows）控制端
#                            连进来再开录 —— 跨机（Windows→麒麟）那轮用它跑。
#   可覆盖：UULAN_BIN=<二进制目录> UULAN_SECONDS=8 UULAN_PORT=21999
# 退出码：0 = 全过；1 = 断言失败（产品问题）；2 = 环境给不出判据（台架问题，别记成缺陷）。
set -u

BIN="${UULAN_BIN:-$HOME/.local/uulan/src/target/release}"
PORT="${UULAN_PORT:-21999}"
RUN_SECS="${UULAN_SECONDS:-8}"
EXT="${UULAN_EXTERNAL_CLIENT:-0}"
TS=$(date +%Y%m%d-%H%M%S)
WORK="/tmp/uulan-mic.$TS"
NO_COLOR=1
export NO_COLOR
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"

PASS=0; FAIL=0; SKIP=0
ok()   { echo "  [PASS] $*"; PASS=$((PASS+1)); }
bad()  { echo "  [FAIL] $*"; FAIL=$((FAIL+1)); }
env_out() { echo "  [环境] $*"; echo "退出码 2：这台机器此刻给不出这条判据，别把它记成产品缺陷"; exit 2; }

mkdir -p "$WORK" || { echo "建不了 $WORK"; exit 2; }
clean_ansi() { sed -E 's/\x1b\[[0-9;]*m//g' "$1"; }

SRV="$BIN/uulan-server"; HOST="$BIN/uulan-host"; CLI="$BIN/uulan-client"
echo "== 0. 预检 =="
for f in "$SRV" "$HOST" "$CLI"; do
  [ -x "$f" ] || env_out "缺可执行文件 $f（真机 cargo build --release -p uulan-server -p uulan-host -p uulan-client）"
done
echo "  二进制构建时间：host   $(stat -c %y "$HOST" 2>/dev/null | cut -d. -f1)"
echo "                  client $(stat -c %y "$CLI" 2>/dev/null | cut -d. -f1)"
sha256sum "$HOST" "$CLI" | awk '{print "  sha256:", $2, $1}'
for c in ffmpeg pactl parecord aplay python3; do
  command -v "$c" >/dev/null 2>&1 || env_out "这台机器上没有 $c"
done
ok "三个二进制与 ffmpeg/pactl/parecord/aplay 都在"

NSINK=$(pactl list sinks short 2>/dev/null | wc -l)
[ "$NSINK" -ge 1 ] || env_out "pactl 问不到任何信宿（没有声卡 / PulseAudio 没起来）"
SINK=$(pactl list sinks short | awk 'NR==1{print $2}')
if [ "$NSINK" -gt 1 ]; then
  ALT=$(pactl list sinks short | awk '{printf "%s|", $2}' | sed 's/|$//')
  DEF=$(pactl info 2>/dev/null | grep -oE "$ALT" | head -1)
  [ -n "$DEF" ] && SINK="$DEF"
fi
MON="$SINK.monitor"
pactl list sources short 2>/dev/null | grep -qF "$MON" || env_out "信宿 $SINK 没有 monitor 源，录不回来"
ok "信宿 $SINK（共 $NSINK 块，将录它的 $MON）"

# 分析器与 2026-11-17 那条同判据：440 Hz 对 1000 Hz 的 Goertzel 功率比。
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

SERVER_PID=""; HOST_PID=""; REC_PID=""
cleanup() {
  [ -n "$REC_PID" ] && kill "$REC_PID" 2>/dev/null
  [ -n "$HOST_PID" ] && kill "$HOST_PID" 2>/dev/null
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null
  return 0
}
trap cleanup EXIT

echo "== 1. 测试台（独立中继 + 独立配置与台账，不碰正式服务的身份与口令）=="
BIND="127.0.0.1"
[ "$EXT" = "1" ] && BIND="0.0.0.0"   # 外部控制端模式：Windows 那边要能从局域网拨进来
"$SRV" --bind "$BIND:$PORT" --state "$WORK/devices.json" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 100); do
  (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && { exec 3>&-; break; }
  sleep 0.1
done
# 被控端**不带** --audio-source：下行不参与本测试；mic_sink 用生产默认 speaker。
RUST_LOG=info "$HOST" --server "127.0.0.1:$PORT" --config "$WORK/host.json" \
  --capture "lavfi:testsrc2=size=640x480:rate=15" \
  --name mic-host >"$WORK/host.log" 2>&1 &
HOST_PID=$!
DEVICE=""
for _ in $(seq 1 200); do
  DEVICE=$(grep -oE "设备 ID : [0-9]{6,}" "$WORK/host.log" 2>/dev/null | tail -1 | sed 's/.*: *//' | tr -d '[:space:]')
  [ -n "$DEVICE" ] && break
  sleep 0.1
done
[ -n "$DEVICE" ] || { echo "被控端没拿到设备 ID："; grep -vE "访问密码" "$WORK/host.log" | sed -n 1,15p; exit 2; }
grep -vE "访问密码|口令" "$WORK/host.log" | tail -3 | sed 's/^/    /'
sed -n 's/.*"password": *"\([^"]*\)".*/\1/p' "$WORK/host.json" | head -1 > "$WORK/pw.txt"
[ -s "$WORK/pw.txt" ] || { echo "host.json 里读不到口令字段"; exit 2; }
chmod 600 "$WORK/pw.txt"
# 配置里 mic_sink 缺省就是 speaker；这条断言钉「老 host.json 不带新字段也起得来」（serde default）。
grep -q mic_sink "$WORK/host.json" || ok "host.json 没有 mic_sink 字段也能起（serde default 兼容老配置）"
ok "测试台起来了（设备 ID $DEVICE；口令走 0600 文件，不上屏）"

start_record() { # $1=输出 $2=秒数
  parecord -d "$MON" --format=s16le --rate=48000 --channels=2 --file-format=wav "$1" \
    >"$WORK/record.err" 2>&1 &
  REC_PID=$!
}
stop_record() { [ -n "$REC_PID" ] && kill "$REC_PID" 2>/dev/null; REC_PID=""; sleep 0.4; }

echo "== 2. 对照组：没人送麦时，录回来的不该有 440 Hz =="
BASE="$WORK/baseline.wav"; rm -f "$BASE"
start_record "$BASE"; sleep 3; stop_record
[ -s "$BASE" ] || { head -3 "$WORK/record.err"; env_out "parecord 录不到 $MON —— 这条判据在这台机器上给不出"; }
if python3 "$WORK/check440.py" "$BASE" quiet; then
  ok "对照组干净（没有 tone）"
else
  bad "对照组就测出了 440 Hz —— 环境里有别的声音在响，本轮判据作废"
fi

echo "== 3. 主测：$RUN_SECS 秒的 440 Hz 从控制端的上行里播出来 =="
TONE="$WORK/tone.wav"; rm -f "$TONE"
start_record "$TONE"
sleep 1
if [ "$EXT" = "1" ]; then
  echo "  （外部控制端模式：等开发机那边的 uulan-client --mic 连进来）"
  GOT=0
  for _ in $(seq 1 120); do
    clean_ansi "$WORK/host.log" | grep -q "麦克风转接已接上" && { GOT=1; break; }
    sleep 0.5
  done
  [ "$GOT" = "1" ] || bad "等了 60 秒，外部控制端没能把被控端的播放管道起来"
  # 等外部控制端自己跑完（--seconds 的它到点会退；写半端 EOF 后这边收尾）
  for _ in $(seq 1 120); do
    clean_ansi "$WORK/host.log" | grep -q "麦克风转接收尾" && break
    sleep 0.5
  done
else
  RUST_LOG=info timeout $((RUN_SECS + 20)) "$CLI" --server "127.0.0.1:$PORT" \
    --device "$DEVICE" --password-file "$WORK/pw.txt" \
    --headless --seconds "$RUN_SECS" \
    --mic --mic-source "lavfi:sine=frequency=440:sample_rate=48000" \
    >"$WORK/client.log" 2>&1
  CRC=$?
  [ "$CRC" = "0" ] || bad "控制端退出码 $CRC（应 0）"
  clean_ansi "$WORK/client.log" | grep -q "已请求被控端播放本机麦克风" \
    && ok "控制端发了 MicStart" || bad "控制端日志里没有「已请求被控端播放本机麦克风」"
  clean_ansi "$WORK/client.log" | grep -q "被控端麦克风状态：.* → 被控端正在推流" \
    && ok "MicState running:true 被控制端接住（相位迁移到 Running）" \
    || bad "没等到 MicState 的 Running 迁移"
  UP=$(clean_ansi "$WORK/client.log" | grep -oE "上行 [0-9]+ 帧 / [0-9]+ 字节" | tail -1 | grep -oE "[0-9]+" | tail -1)
  [ "${UP:-0}" -gt 8000 ] && ok "上行字节数 $UP（>${RUN_SECS} 秒 64kbps 的下限）" \
    || bad "上行只有 ${UP:-0} 字节 —— 没到 ${RUN_SECS} 秒正弦的量级"
  sleep 2   # 等对面的「收尾」那句落进日志（EOF 传播 + 播放器释放）
fi
stop_record
clean_ansi "$WORK/host.log" | grep -q "实时播放已接上 program=aplay" \
  && ok "被控端真起了解码器 + aplay（可 grep 证据）" \
  || bad "被控端日志没有「实时播放已接上」"
if python3 "$WORK/check440.py" "$TONE" tone; then
  ok "录回来的声音里 440 Hz 是主角（判据在 PulseAudio 上，不在自己的账本上）"
else
  bad "录回来的音频测不出 440 Hz 主导 —— 麦克风转接没真出声"
fi

echo "== 4. EOF 即「停」：会话结束后 3 秒必须重新安静 + 收尾日志带字节数 =="
AFTER="$WORK/after.wav"; rm -f "$AFTER"
start_record "$AFTER"; sleep 3; stop_record
if python3 "$WORK/check440.py" "$AFTER" quiet; then
  ok "控制端退场后 $MON 上再无 tone（EOF 收尾 + 播放器释放，没有幽灵音）"
else
  bad "会话结束后还在响 —— 收尾没把播放管道关掉？"
fi
FIN=$(clean_ansi "$WORK/host.log" | grep -oE "麦克风转接收尾.*" | tail -1)
echo "    $FIN"
FB=$(echo "$FIN" | sed -n 's/.*bytes=\([0-9]*\).*/\1/p')
FP=$(echo "$FIN" | sed -n 's/.*peak=\([0-9]*\).*/\1/p')
[ "${FB:-0}" -gt 8000 ] && ok "被控端收到 $FB 字节（与上行同量级）" || bad "被控端收尾日志 bytes=${FB:-?}（应 >8000）"
[ "${FP:-0}" -gt 100 ] && ok "被控端解出的 PCM 峰值 $FP（>100：不是静音流）" || bad "被控端收尾 peak=${FP:-?} —— 收到字节却全是静音？"
# 没 MicStop 这条消息：**协议侧**的口径由 mic_path.rs 钉；这里钉的是「对面真按 EOF 收场」。
clean_ansi "$WORK/host.log" | grep -q "麦克风转接收尾" \
  && ok "被控端把「写半端关闭」如实当成结束（EOF 语义接线正确）" \
  || bad "被控端没有「麦克风转接收尾」—— EOF 收场没走到？"

echo "== 5. 反向断言：没要麦的会话与源名不合法的会话，都不许惊动播放管道 =="
MICREQ=$(clean_ansi "$WORK/host.log" | grep -c "控制端要求把它的麦克风播到本机")
[ "${MICREQ:-0}" = "1" ] && ok "整轮里 MicStart 恰好被受理 $MICREQ 次" \
  || bad "MicStart 受理次数 = ${MICREQ:-0}（本轮应当恰好 1）"
# 5b. 源名不带前缀 = 本机解析失败：MicStart 不发、统计行给出「照抄就能改」的报错。
if [ "$EXT" != "1" ]; then
  RUST_LOG=info timeout 25 "$CLI" --server "127.0.0.1:$PORT" \
    --device "$DEVICE" --password-file "$WORK/pw.txt" \
    --headless --seconds 3 --mic --mic-source "default" \
    >"$WORK/client-bad.log" 2>&1
  BRC=$?
  [ "$BRC" = "0" ] || bad "源名不合法的会话退出码 $BRC（会话本身不该死，应 0）"
  clean_ansi "$WORK/client-bad.log" | grep -q "本机采集没起来" \
    && ok "坏源名走 LocalDead：统计行明说「本机采集没起来」" \
    || bad "坏源名没进 LocalDead（看 client-bad.log）"
  sleep 1
  MICREQ2=$(clean_ansi "$WORK/host.log" | grep -c "控制端要求把它的麦克风播到本机")
  [ "${MICREQ2:-0}" = "1" ] && ok "坏源名没有把 MicStart 发到对面（受理次数仍是 1）" \
    || bad "坏源名也发了 MicStart（本机失败该在发消息之前拦下）"
else
  skip "外部控制端模式：5b 的反例只能在真机本机控制端上跑"
fi
# 5c. 不要麦的普通会话：也不许惊动。
RUST_LOG=info timeout 30 "$CLI" --server "127.0.0.1:$PORT" \
  --device "$DEVICE" --password-file "$WORK/pw.txt" \
  --headless --seconds 3 >"$WORK/client-nomic.log" 2>&1
sleep 1
MICREQ3=$(clean_ansi "$WORK/host.log" | grep -c "控制端要求把它的麦克风播到本机")
[ "${MICREQ3:-0}" = "1" ] && ok "没给 --mic 的会话零惊动（受理次数仍是 1）" \
  || bad "不要麦的会话也触发了麦克风管道"

echo "== 6. 日志脱敏：本测试台四份日志里 host.json 的口令命中必须为 0 =="
HITS=$( (cat "$WORK/server.log" "$WORK/host.log" "$WORK/client.log" "$WORK/client-bad.log" "$WORK/client-nomic.log" 2>/dev/null; \
        ) | clean_ansi /dev/stdin | grep -cF "$(cat "$WORK/pw.txt")" || true)
[ "${HITS:-0}" = "0" ] && ok "口令在所有日志里 0 命中" || bad "口令在日志里出现 $HITS 次 —— 回归了"

echo
echo "== 结果：$PASS 通过 / $FAIL 失败 =="
[ "$FAIL" = "0" ] || exit 1
exit 0
