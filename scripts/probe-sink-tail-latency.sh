#!/usr/bin/env bash
# 播放末端段的**归因探针**：把「aplay 的缓冲参数」单独拎出来量，不牵进网络与被控端。
#
# 为什么要它：2026-11-18 夜批把总延迟定位到「播放末端段 ≈ 1.2 s」（verify-audio-latency-e2e.sh），
# 但那条台架里同时有采集、编码、通道、解码 —— 要改参数得先知道**这 1.2 s 里有多少是 aplay
# 自己的缓冲、多少是这块声卡/Pulse 信宿的固有缓冲**。后者改参数改不动，只能换机器配置。
#
# 台架口径与 verify-audio-latency-e2e.sh 一致（同一段 onset 判据、同样的静音保活流、
# REC_T0 = parecord 首块数据落盘时刻），所以这里的数与那边「末端段」可比。
#
# 用法（真机上）：bash scripts/probe-sink-tail-latency.sh
#   可覆盖：PROBE_TONE_HZ=440 PROBE_SECONDS=2
# 退出码：0 = 至少量出一个配置；2 = 这台机器给不出判据（台架问题，别记成产品缺陷）。
set -u

TS=$(date +%Y%m%d-%H%M%S)
WORK="/tmp/uulan-tail-probe.$TS"
TONE_HZ="${PROBE_TONE_HZ:-440}"
TONE_SECS="${PROBE_SECONDS:-3}"
mkdir -p "$WORK" || { echo "建不了 $WORK"; exit 2; }
cleanup() { pkill -f "$WORK" 2>/dev/null; return 0; }
trap cleanup EXIT

calc() { awk "BEGIN{printf \"%.4f\", $1}"; }
epoch_now() { date -u +%s.%N; }

for c in ffmpeg pactl parecord paplay aplay python3; do
  command -v "$c" >/dev/null 2>&1 || { echo "这台机器上没有 $c"; exit 2; }
done
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"

NSINK=$(pactl list sinks short 2>/dev/null | wc -l)
[ "$NSINK" -ge 1 ] || { echo "pactl 问不到任何信宿"; exit 2; }
SINK=$(pactl list sinks short | awk 'NR==1{print $2}')
MON="$SINK.monitor"
pactl list sources short 2>/dev/null | grep -qF "$MON" || { echo "信宿 $SINK 没有 monitor"; exit 2; }
echo "信宿：$SINK"
pactl list sinks 2>/dev/null | sed -n '1,40p' | grep -aE "Name:|State:|Sample Specification:|Driver:|Owner Module:" | sed 's/^ */  /'
echo "daemon.conf 里的缓冲相关设置（没设就是 Pulse 自带默认）："
grep -aE "^\s*(default-fragments|default-fragment-size-msec|tsched|high-priority)" /etc/pulse/daemon.conf 2>/dev/null | sed 's/^/  /' || echo "  （一条都没有）"

# ---- 判据器（与 verify-audio-latency-e2e.sh 同口径：先混单声道帧再 Goertzel）----
cat > "$WORK/an.py" <<'PY'
import struct, sys, math
def load(path):
    b = open(path, "rb").read()
    if b[:4] != b"RIFF" or b[8:12] != b"WAVE":
        sys.exit("不是 WAV")
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
    if bits != 16:
        sys.exit("只支持 16 bit")
    n = len(data) // 2
    s = struct.unpack_from("<%dh" % n, data, 0)
    if ch == 2:
        s = [(s[i] + s[i+1]) // 2 for i in range(0, n - (n % 2), 2)]
    return s, rate
def goertzel(seg, freq, rate):
    coeff = 2 * math.cos(2 * math.pi * freq / rate)
    s1 = s2 = 0.0
    for v in seg:
        s0 = v + coeff * s1 - s2
        s2, s1 = s1, s0
    return s1 * s1 + s2 * s2 - coeff * s1 * s2
WIN, HOP = 0.4, 0.05
def winpowers(samples, rate, freq):
    w = int(WIN * rate); h = int(HOP * rate); out = []; i = 0
    while i + w <= len(samples):
        out.append(goertzel(samples[i:i+w], freq, rate) / (w * w)); i += h
    return out
def steady_of(pw):
    if not pw: return 0.0
    top = sorted(pw)[max(1, int(len(pw) * 0.75)):]
    return top[len(top) // 2]
cmd = sys.argv[1]
samples, rate = load(sys.argv[2])
if cmd == "dur":
    print("%.2f" % (len(samples) / rate)); sys.exit(0)
if cmd == "onset":
    freq = float(sys.argv[3]); tfrom = float(sys.argv[4]) if len(sys.argv) > 4 else 0.0
    pw = winpowers(samples, rate, freq)
    steady = steady_of(pw)
    if steady < 1e-8:
        print("NONE 0 0"); sys.exit(0)
    thr = 0.5 * steady
    for i, p in enumerate(pw):
        if i * HOP < tfrom: continue
        if p >= thr:
            print("%.3f %.3g %.3g" % (i * HOP + WIN / 2 - 0.10, steady, p)); sys.exit(0)
    print("NONE %.3g 0"); sys.exit(0)
sys.exit("用法: an.py {dur|onset} rec.wav [频率] [起搜时刻]")
PY

# ---- 挂起的信宿连 monitor 都不吐样本：先用静音流把它钉在 RUNNING（11-18 实测坑）----
ffmpeg -v error -y -f lavfi -i "anullsrc=r=48000:cl=stereo" -t 300 "$WORK/silence.wav" \
  || { echo "造不出静音流"; exit 2; }
paplay -d "$SINK" "$WORK/silence.wav" >"$WORK/keepalive.err" 2>&1 &
KEEP=$!
sleep 2
PROBE="$WORK/probe.wav"
timeout 2 parecord -d "$MON" --format=s16le --rate=48000 --channels=2 --file-format=wav "$PROBE" >/dev/null 2>&1
SZ=$(stat -c %s "$PROBE" 2>/dev/null || echo 0)
if [ "${SZ:-0}" -le 20000 ]; then
  echo "静音保活后 monitor 仍不吐样本（$SZ 字节）—— 时间基建不起来"
  exit 2
fi
echo "静音保活流已把信宿钉活（探录 $SZ 字节）"

start_rec() { # $1=wav ; 置 RECPID / REC_T0
  RECPID=""; local lt
  lt=$(epoch_now)
  parecord -d "$MON" --format=s16le --rate=48000 --channels=2 --file-format=wav "$1" \
    >"$WORK/record.err" 2>&1 &
  RECPID=$!
  REC_T0=""
  for _ in $(seq 1 40); do
    S=$(stat -c %s "$1" 2>/dev/null || echo 0)
    if [ "${S:-0}" -gt 1024 ]; then REC_T0=$(epoch_now); break; fi
    sleep 0.05
  done
  [ -n "$REC_T0" ] || { kill "$RECPID" 2>/dev/null; RECPID=""; echo "parecord 录不出数据"; exit 2; }
}
stop_rec() { [ -n "${RECPID:-}" ] && kill "$RECPID" 2>/dev/null; RECPID=""; sleep 0.4; }

# 一次配置一轮：录 → 起声 → 停录 → 算末端段。anchor = 起流命令的时刻（本机同一时钟，无对表）。
probe_cfg() { # $1=名字 $2...=aplay 之后的参数（无参数时用生产参数）
  local name="$1"; shift
  local rec="$WORK/$name.wav"
  start_rec "$rec"
  sleep 1
  local t0; t0=$(epoch_now)
  # -re 让合成源按实时节奏出字节 —— 与真链路一致（否则一次灌满，量的就不是缓冲是带宽）
  ffmpeg -hide_banner -loglevel error -re -f lavfi \
    -i "sine=frequency=$TONE_HZ:sample_rate=48000" -t "$TONE_SECS" -f s16le -ac 2 - 2>"$WORK/$name-ff.err" |
    aplay -q "$@" - >"$WORK/$name-aplay.err" 2>&1
  local arc=$?
  sleep 1
  stop_rec
  local pos; pos=$(python3 "$WORK/an.py" onset "$rec" "$TONE_HZ" 0.0 | awk '{print $1}')
  if [ "$pos" = "NONE" ] || [ -z "$pos" ]; then
    echo "  $name: 没测出起声（aplay rc=$arc；stderr: $(head -c 160 "$WORK/$name-aplay.err" | tr '\n' ' ')）"
    echo "RESULT $name NONE"
    return 1
  fi
  local lat; lat=$(calc "$REC_T0 + $pos - $t0")
  echo "  $name: 末端段 ${lat} s（aplay rc=$arc，起声 @${pos}s，录 $(python3 "$WORK/an.py" dur "$rec")s）"
  echo "RESULT $name $lat"
}

echo "== 各播放参数的末端段（同一台架、同一轮口径）=="
# 生产现行参数（uulan-media::playback::player_candidates）
probe_cfg prod-current -t raw -f S16_LE -c 2 -r 48000
# 显式小缓冲（微秒）：ALSA 的 -B=buffer time、-F=period time
probe_cfg b100f25  -B 100000 -F 25000  -t raw -f S16_LE -c 2 -r 48000
probe_cfg b50f12   -B 50000  -F 12500  -t raw -f S16_LE -c 2 -r 48000
probe_cfg b20f5    -B 20000  -F 5000   -t raw -f S16_LE -c 2 -r 48000
# 只给 period 不给 buffer（让 ALSA 自己定 buffer = 2 个 period）
probe_cfg f10only   -F 10000  -t raw -f S16_LE -c 2 -r 48000

echo "== aplay -v 实际协商到的缓冲/周期（现场看「ALSA 到底给了多少」，不看请求值）=="
for spec in "prod:" "b100f25:-B 100000 -F 25000" "b20f5:-B 20000 -F 5000"; do
  label=${spec%%:*}; extra=${spec#*:}
  out=$( (ffmpeg -hide_banner -loglevel error -re -f lavfi -i "sine=frequency=$TONE_HZ:sample_rate=48000" -t 1 -f s16le -ac 2 - | \
         aplay -v $extra -t raw -f S16_LE -c 2 -r 48000 - ) 2>&1 | tr -d '\r' )
  echo "  [$label] $(echo "$out" | grep -aiE 'buffer size|period size|buffer time|period time|access|sample format' | sed 's/^ *//' | tr '\n' ' | ')"
done

kill "$KEEP" 2>/dev/null
echo
echo "读数说明：末端段 = 「PCM 进 aplay stdin 的时刻」→「信宿 monitor 里 440 起声」，±0.15 s。"
echo "各配置之间的**差**比绝对值更可信（同一台架、同一时间基）。"
exit 0
