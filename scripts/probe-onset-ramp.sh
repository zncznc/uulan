#!/usr/bin/env bash
# 那个「起声后约 2.2 s 的启动爬坡」到底是谁的？三条独立假设，逐个排除：
#   ① 信宿从 SUSPENDED 醒过来 —— 排除条件：本探针全程挂着静音保活流（信宿一直 RUNNING）
#   ② paplay（Pulse 原生客户端）也有爬坡 → 那是 Pulse/信宿层面的事，与 uulan 无关
#   ③ 只有 aplay（ALSA→pulse 插件）这条路有爬坡 → 与播放后端的起流方式有关
# 用法：bash ~/probe-ramp.sh
set -u
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
WORK=$(mktemp -d /tmp/uulan-ramp.XXXXXX)
cleanup() { pkill -f "$WORK" 2>/dev/null; return 0; }
trap cleanup EXIT
SINK=$(pactl list sinks short | awk 'NR==1{print $2}')
MON="$SINK.monitor"

ffmpeg -v error -y -f lavfi -i "anullsrc=r=48000:cl=stereo" -t 120 "$WORK/sil.wav" || exit 2
paplay -d "$SINK" "$WORK/sil.wav" >/dev/null 2>&1 & KEEP=$!
sleep 2
echo "信宿状态（保活流挂着，应当是 RUNNING）：$(pactl list sinks short | awk '{print $NF}')"

cat > "$WORK/prof.py" <<'PY'
import struct, sys, math
b = open(sys.argv[1], "rb").read()
off, data = 12, b""
while off + 8 <= len(b):
    cid = b[off:off+4]; sz = struct.unpack_from("<I", b, off+4)[0]
    if cid == b"data":
        sz = sz or (len(b)-off-8); data = b[off+8:off+8+sz]; break
    off += 8 + sz + (sz & 1)
ch = struct.unpack_from("<H", b, 22)[0]; rate = struct.unpack_from("<I", b, 24)[0]
n = len(data)//2; s = struct.unpack_from("<%dh" % n, data, 0)
if ch == 2:
    s = [(s[i]+s[i+1])//2 for i in range(0, n-(n % 2), 2)]
freq = float(sys.argv[2])
def goertzel(seg):
    coeff = 2*math.cos(2*math.pi*freq/rate); s1 = s2 = 0.0
    for v in seg:
        s0 = v + coeff*s1 - s2; s2, s1 = s1, s0
    return s1*s1 + s2*s2 - coeff*s1*s2
WIN, HOP = int(0.4*rate), int(0.05*rate)
pw = []; i = 0
while i + WIN <= len(s):
    pw.append(goertzel(s[i:i+WIN])/(WIN*WIN)); i += HOP
if not pw:
    print("  没录到东西"); sys.exit(0)
steady = sorted(pw)[int(len(pw)*0.75):]
steady = steady[len(steady)//2]
print("  稳态中位 %.4g —— 逐窗比值（前 3.0 s，每 0.15 s 报一个）：" % steady)
line = []
for k in range(0, min(len(pw), int(3.0/0.05)), 3):
    line.append("%.2f:%.2f" % (k*0.05, pw[k]/steady))
print("   " + "  ".join(line))
first90 = next((k*0.05 for k in range(len(pw)-9) if all(pw[j] >= 0.9*steady for j in range(k, k+9))), None)
print("  第一次「达到稳态 90％ 并稳住 0.45 s」的时刻：" + ("测不出" if first90 is None else "%.2f s" % first90))
PY

run_case() { # $1=标签 $2...=播放器命令
  local tag="$1"; shift
  local rec="$WORK/$tag.wav"
  parecord -d "$MON" --format=s16le --rate=48000 --channels=2 --file-format=wav "$rec" >/dev/null 2>&1 &
  local rp=$!
  sleep 1.2
  local t0; t0=$(date +%s.%N)
  "$@" >/dev/null 2>&1
  local t1; t1=$(date +%s.%N)
  sleep 0.8
  kill $rp 2>/dev/null; sleep 0.4
  echo "== $tag（播放器耗时 $(awk "BEGIN{printf \"%.2f\", $t1-$t0}") s）=="
  python3 "$WORK/prof.py" "$rec" 440
}

# 合成一个 1.5 s 的 440 WAV（paplay 要用文件）
ffmpeg -v error -y -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=1.5" -ac 2 -sample_fmt s16 "$WORK/tone.wav" || exit 2

run_case native-paplay paplay "$WORK/tone.wav"
run_case alsa-aplay-file aplay -f S16_LE -c 2 -r 48000 "$WORK/tone.wav"
run_case alsa-aplay-stdin-re sh -c "ffmpeg -hide_banner -loglevel error -re -f lavfi -i 'sine=frequency=440:sample_rate=48000' -t 1.5 -f s16le -ac 2 - 2>/dev/null | aplay -q -t raw -f S16_LE -c 2 -r 48000 -"
run_case alsa-aplay-stdin-re-tight sh -c "ffmpeg -hide_banner -loglevel error -re -f lavfi -i 'sine=frequency=440:sample_rate=48000' -t 1.5 -f s16le -ac 2 - 2>/dev/null | aplay -q -B 50000 -F 12500 -t raw -f S16_LE -c 2 -r 48000 -"
kill $KEEP 2>/dev/null
echo "读法：如果四条的「第一次站住 90%」时刻都接近，爬坡就是这台机器/Pulse 的性质；"
echo "如果只有 uulan 那条路（stdin + 网络）有爬坡，才回头看我们自己的起流。"
