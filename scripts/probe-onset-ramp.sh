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

# ---- 第 0 步：判据自己的标定（不碰声卡、纯数学）----
# 造一个「t=2.000 s 处硬起始」的 440 Hz 文件，让脚本里那两版起声判据各量一次。
# 偏差不是 0 就说明后面所有「起声时刻」都带着系统误差 —— 2026-11-19 实测两版都是
# +0.000 s（旧「过半」靠 -0.10 的人工标定凑正，中位数版天生就对），
# 所以「monitor 台架说 0.44~0.59 s 而 Pulse 说 0.064 s」那段差**不是判据偏晚**造成的，
# 是「PCM 进 aplay 之前」真的花了那么多时间。
cat > "$WORK/calib.py" <<'PY'
import math, os, struct, subprocess, sys
wav = os.path.join(sys.argv[1], "calib.wav")
subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi",
                "-i", "aevalsrc=0.5*sin(2*PI*440*t)*gte(t\\,2):s=48000:d=6",
                "-ac", "2", "-sample_fmt", "s16", wav], check=True)
b = open(wav, "rb").read()
off, data = 12, b""
while off + 8 <= len(b):
    cid = b[off:off+4]; sz = struct.unpack_from("<I", b, off+4)[0]
    if cid == b"data":
        sz = sz or (len(b) - off - 8); data = b[off+8:off+8+sz]; break
    off += 8 + sz + (sz & 1)
ch = struct.unpack_from("<H", b, 22)[0]; rate = struct.unpack_from("<I", b, 24)[0]
n = len(data)//2; sm = struct.unpack_from("<%dh" % n, data, 0)
mono = [(sm[i]+sm[i+1])//2 for i in range(0, n-(n % 2), 2)]
def gtr(seg):
    c = 2*math.cos(2*math.pi*440.0/rate); a = q = 0.0
    for v in seg:
        z = v + c*a - q; q, a = a, z
    return a*a + q*q - c*a*q
WIN, HOP = int(0.4*rate), int(0.05*rate)
pw = []; i = 0
while i + WIN <= len(mono):
    pw.append(gtr(mono[i:i+WIN])/(WIN*WIN)); i += HOP
top = sorted(pw)[max(1, int(len(pw)*0.75)):]; steady = top[len(top)//2]
TRUE = 2.0
o50 = next((k for k, p in enumerate(pw) if p >= 0.5*steady), None)
half = 4; o90 = None
for k in range(half, len(pw)-half):
    seg = sorted(pw[k-half:k+half+1])
    if seg[len(seg)//2] >= 0.9*steady:
        o90 = k; break
p50 = o50*0.05 + 0.2 - 0.10 if o50 is not None else float("nan")
p90 = (o90-half)*0.05 + 0.2 if o90 is not None else float("nan")
print("  真值起声 2.000 s —— 旧「过半」报 %.3f s（偏差 %+.3f）；中位数版报 %.3f s（偏差 %+.3f）"
      % (p50, p50-TRUE, p90, p90-TRUE))
PY
echo "== 0. 判据标定（纯数学，不碰声卡）=="
python3 "$WORK/calib.py" "$WORK" || echo "  标定跑不了（缺 ffmpeg 或 python3）—— 后面的起声时刻请少一分信任"

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
