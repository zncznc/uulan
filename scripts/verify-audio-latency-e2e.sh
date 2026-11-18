#!/usr/bin/env bash
# 音频延迟与信宿争用（M7 剩下三格）—— **在真机上跑**，不是从开发机编排。
#
#   下行：被控端 lavfi 440 Hz → 加密 audio 通道 → 控制端解码 → aplay 出声。
#         播放延迟 = 「被控端打出『音频已开始（Opus/Ogg）』」→「信宿 monitor 里 440 起声」。
#   上行：控制端 lavfi 880 Hz（--mic-source）→ 上行 → 被控端 mic_sink=speaker 出声。
#         「控制端打出『麦克风转接已开始』」→「monitor 里 880 起声」。
#   两个频率天然分开，双向同会话一轮就同时量出**双向延迟**；
#   各方向另给一个「末端段」（出声 − 本端播放后端就绪），把管道段和落地缓冲分开看。
#
#   争用段：下行正响时，另一路声音（paplay 1000 Hz，与本链路无关的外部程序）
#   打进同一个信宿 —— 判「uulan 的声音被挤掉没有、控制端报错/挂死没有」。
#
#   2026-11-18 夜批首轮实测：总延迟几乎全部落在「播放后端就绪 → 出声」段
#   （下行 1.29 ≈ 末端 1.29，上行 1.19 ≈ 末端 1.19）—— 瓶颈在 aplay/Pulse 缓冲，不在网络。
#   要降延迟先动播放命令的缓冲参数（候选表在 uulan-media::playback），不是动协议。
#
# 时间基：起声时刻 = parecord 首块数据落盘时刻 + 波形上的 onset 位置（±0.15 s，
#   落盘抖动与窗能量判据各占一半）；锚点时刻 = 两端 INFO 行的 UTC 时间戳。
#   onset 判据：0.4 s 窗 / 0.05 s 滑动的带通能量过半稳态，取命中窗起点 + 半窗。
#
# 用法（真机上）：bash scripts/verify-audio-latency-e2e.sh
#   可覆盖：UULAN_BIN=<二进制目录> UULAN_SECONDS=12
# 退出码：0 = 全过；1 = 断言失败（产品问题）；2 = 这台机器给不出判据（台架问题）。
set -u

BIN="${UULAN_BIN:-$HOME/.local/uulan/src/target/release}"
RUN_SECS="${UULAN_SECONDS:-12}"
TS=$(date +%Y%m%d-%H%M%S)
WORK="/tmp/uulan-audio-latency.$TS"
PORT_D=21997
PORT_C=21996
NO_COLOR=1
export NO_COLOR
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"

PASS=0; FAIL=0; SKIP=0
ok()   { echo "  [PASS] $*"; PASS=$((PASS+1)); }
bad()  { echo "  [FAIL] $*"; FAIL=$((FAIL+1)); }
skip() { echo "  [SKIP] $*"; SKIP=$((SKIP+1)); }
env_out() { echo "  [环境] $*"; echo "退出码 2：这台机器此刻给不出这条判据，别把它记成产品缺陷"; exit 2; }

mkdir -p "$WORK" || { echo "建不了 $WORK"; exit 2; }
# 测试台的 server/host/parecord 命令行里都带 $WORK 路径，一把清干净；
# 正式服务（/opt、21116）的命令行里没有它，绝不会误伤。
cleanup() { pkill -f "$WORK" 2>/dev/null; return 0; }
trap cleanup EXIT

calc() { awk "BEGIN{printf \"%.4f\", $1}"; }          # 浮点算式在程序文本里求值 —— -v 传字符串会被当数字前缀截断（mawk 实测），这是坑
epoch_now() { date -u +%s.%N; }

# 日志时间戳 → epoch（tracing 默认 UTC ISO8601；取第一条命中）
log_epoch() { # $1=文件 $2=字样
  local ts
  ts=$(sed -E 's/\x1b\[[0-9;]*m//g' "$1" 2>/dev/null \
      | grep -aF "$2" | head -1 | sed -E 's/^([0-9T:.Z+-]+) .*/\1/')
  [ -n "$ts" ] || { echo ""; return; }
  date -d "$ts" +%s.%N 2>/dev/null || echo ""
}

SRV="$BIN/uulan-server"; HOST="$BIN/uulan-host"; CLI="$BIN/uulan-client"
echo "== 0. 预检 =="
for f in "$SRV" "$HOST" "$CLI"; do
  [ -x "$f" ] || env_out "缺可执行文件 $f（先 cargo build --release -p uulan-server -p uulan-host -p uulan-client）"
done
for c in ffmpeg pactl parecord paplay aplay python3; do
  command -v "$c" >/dev/null 2>&1 || env_out "这台机器上没有 $c"
done
ok "三个二进制与 ffmpeg/pactl/parecord/paplay/aplay 都在"

NSINK=$(pactl list sinks short 2>/dev/null | wc -l)
[ "$NSINK" -ge 1 ] || env_out "pactl 问不到任何信宿"
SINK=$(pactl list sinks short | awk 'NR==1{print $2}')
if [ "$NSINK" -gt 1 ]; then
  ALT=$(pactl list sinks short | awk '{printf "%s|", $2}' | sed 's/|$//')
  DEF=$(pactl info 2>/dev/null | grep -oE "$ALT" | head -1)
  [ -n "$DEF" ] && SINK="$DEF"
fi
MON="$SINK.monitor"
pactl list sources short 2>/dev/null | grep -qF "$MON" || env_out "信宿 $SINK 没有 monitor 源"
# -x 精确进程名：不加 -x 时正则 "aplay" 会把 paplay 数进来（2026-11-18 夜批实测自坑）
FF_BASE=$(pgrep -c -u "$(id -u)" -x ffmpeg || true); AP_BASE=$(pgrep -c -u "$(id -u)" -x aplay || true)
echo "  会话前基线进程数：ffmpeg=${FF_BASE:-0} aplay=${AP_BASE:-0}"
ok "信宿 $SINK（将录它的 $MON；共 $NSINK 块）"

# **挂起的信宿不写样本**（11-17 实测：没人播时 monitor 录回来只有 44 字节头）。
# 本脚本的时间基要录到「起声之前」，所以先挂一条整场存在的静音流把信宿钉在 RUNNING，
# 再开录。判据不受影响：静音流的样本是零，峰与带通功率都不涨。
ffmpeg -v error -y -f lavfi -i "anullsrc=r=48000:cl=stereo" -t 300 "$WORK/silence.wav" \
  || env_out "本机 ffmpeg 造不出静音流（时间基无从建立）"
paplay -d "$SINK" "$WORK/silence.wav" >"$WORK/keepalive.err" 2>&1 &
sleep 2
PROBE="$WORK/probe.wav"
# 只看数据量，别看退出码：timeout 杀 parecord 必回 124，那不是判据。
timeout 2 parecord -d "$MON" --format=s16le --rate=48000 --channels=2 --file-format=wav "$PROBE" >/dev/null 2>&1
if [ "$(stat -c %s "$PROBE" 2>/dev/null || echo 0)" -gt 20000 ]; then
  ok "静音保活流已把信宿钉活（2 秒探录 $(stat -c %s "$PROBE") 字节 > 20000）"
else
  env_out "静音保活后 monitor 仍不吐样本（$(stat -c %s "$PROBE" 2>/dev/null || echo 0) 字节）—— 这台机器此刻给不出时间基"
fi

# ---- 波形分析器。判据器口径沿用麦克风转接那轮修好的：交织立体声先混单声道帧，
#      Goertzel 的 rate 用帧率；能量按窗长归一，跨频/跨窗可比。----
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

WIN = 0.4
HOP = 0.05

def winpowers(samples, rate, freq):
    w = int(WIN * rate); h = int(HOP * rate)
    out = []
    i = 0
    while i + w <= len(samples):
        out.append(goertzel(samples[i:i+w], freq, rate) / (w * w))
        i += h
    return out

def steady_of(pw):
    if not pw:
        return 0.0
    top = sorted(pw)[max(1, int(len(pw) * 0.75)):]
    return top[len(top) // 2]

samples, rate = load(sys.argv[2])
cmd = sys.argv[1]

if cmd == "dur":
    print("%.2f" % (len(samples) / rate)); sys.exit(0)
if cmd == "onset":
    freq = float(sys.argv[3])
    pw = winpowers(samples, rate, freq)
    steady = steady_of(pw)
    if steady < 1e-8:
        print("NONE 0 0"); sys.exit(0)
    thr = 0.5 * steady
    for i, p in enumerate(pw):
        if p >= thr:
            # 命中窗的覆盖约 3/4（本机合成音硬起始标定：恒偏晚 0.10 s，已扣）
            print("%.3f %.3g %.3g" % (i * HOP + WIN / 2 - 0.10, steady, p)); sys.exit(0)
    print("NONE %.3g 0" % steady); sys.exit(0)
if cmd == "onset90":
    # 「真的响了」而不是「爬坡爬过一半」：首次达到稳态 90% 且随后 0.45 s 都站得住。
    # 为什么不用 50%：这块信宿在会话起来后有约 2.2 s 的两路同步启动爬坡（2026-11-19 逐窗
    # 功率实测），50% 阈值落在爬坡里，起声时刻随爬坡形状抽签（同一二进制两轮差 0.75 s）。
    freq = float(sys.argv[3]); hold = int(sys.argv[4]) if len(sys.argv) > 4 else 9
    pw = winpowers(samples, rate, freq)
    steady = steady_of(pw)
    if steady < 1e-8:
        print("NONE 0 0"); sys.exit(0)
    thr = 0.9 * steady
    for i in range(len(pw) - hold + 1):
        if all(pw[j] >= thr for j in range(i, i + hold)):
            print("%.3f %.3g %.3g" % (i * HOP + WIN / 2, steady, pw[i])); sys.exit(0)
    print("NONE %.3g 0" % steady); sys.exit(0)
if cmd == "powermin":
    # 稳态段里的最小窗功率 / 全体窗功率中位数 —— 播放缓冲压薄之后，
    # 一次 xrun 就是一次「掉下去一块」，这里不靠日志也能抓到。
    freq = float(sys.argv[3]); t0 = float(sys.argv[4]); t1 = float(sys.argv[5])
    pw = winpowers(samples, rate, freq)
    i0 = max(0, int(round(t0 / HOP))); i1 = min(len(pw) - 1, int(round(t1 / HOP)))
    seg = pw[i0:i1+1]
    if not seg:
        print("0 0"); sys.exit(0)
    print("%.4g %.4g" % (min(seg), steady_of(pw))); sys.exit(0)
if cmd == "power":
    freq = float(sys.argv[3]); t0 = float(sys.argv[4]); t1 = float(sys.argv[5])
    pw = winpowers(samples, rate, freq)
    i0 = max(0, int(round(t0 / HOP))); i1 = min(len(pw) - 1, int(round(t1 / HOP)))
    if i1 <= i0:
        print("0"); sys.exit(0)
    seg = pw[i0:i1+1]
    print("%.4g" % (sum(seg) / len(seg))); sys.exit(0)
sys.exit("用法: an.py {dur|onset|power} rec.wav ...")
PY

start_host() { # $1=配置 $2=名字 $3=中继端口 ; 置 <名字>_DEVICE / <名字>_PW
  local cfg="$1" name="$2" port="$3" dev=""
  local lg="$WORK/$name.log"
  "$SRV" --bind "127.0.0.1:$port" --state "$WORK/$name-devices.json" >"$WORK/$name-server.log" 2>&1 &
  for _ in $(seq 1 100); do
    (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null && { exec 3>&-; break; }
    sleep 0.1
  done
  RUST_LOG=info "$HOST" --server "127.0.0.1:$port" --config "$cfg" \
    --capture "lavfi:testsrc2=size=640x480:rate=15" \
    --audio-source "lavfi:sine=frequency=440:sample_rate=48000" \
    --name "$name" --once >"$lg" 2>&1 &
  for _ in $(seq 1 200); do
    dev=$(grep -oE "设备 ID : [0-9]{6,}" "$lg" 2>/dev/null | tail -1 | sed 's/.*: *//' | tr -d '[:space:]')
    [ -n "$dev" ] && break
    sleep 0.1
  done
  [ -n "$dev" ] || { echo "被控端 $name 没拿到设备 ID：" >&2; sed -n '1,12p' "$lg" >&2; return 1; }
  sed -n 's/.*"password": *"\([^"]*\)".*/\1/p' "$cfg" | head -1 > "$WORK/$name-pw.txt"
  [ -s "$WORK/$name-pw.txt" ] || { echo "$name 的口令字段读不到" >&2; return 1; }
  chmod 600 "$WORK/$name-pw.txt"
  eval "${name}_DEVICE=\$dev"
  eval "${name}_PW=\"$WORK/$name-pw.txt\""
}

start_rec() { # $1=wav ; 置 RECPID / REC_T0（首块数据落盘时刻）
  RECPID=""
  local lt
  lt=$(epoch_now)
  parecord -d "$MON" --format=s16le --rate=48000 --channels=2 --file-format=wav "$1" \
    >"$WORK/record.err" 2>&1 &
  RECPID=$!
  REC_T0=""
  for _ in $(seq 1 40); do
    SZ=$(stat -c %s "$1" 2>/dev/null || echo 0)
    if [ "${SZ:-0}" -gt 1024 ]; then REC_T0=$(epoch_now); break; fi
    sleep 0.05
  done
  [ -n "$REC_T0" ] || { kill "$RECPID" 2>/dev/null; RECPID=""; env_out "parecord 录不出数据（$MON）—— 台架问题"; }
}
stop_rec() { [ -n "${RECPID:-}" ] && kill "$RECPID" 2>/dev/null; RECPID=""; sleep 0.5; }

echo "== 1. 测试台与对照组 =="
start_host "$WORK/duplex.json" duplex "$PORT_D" || env_out "测试台起不来"
ok "测试台起来了（设备 $duplex_DEVICE；口令走 0600 文件，不上屏）"

start_rec "$WORK/baseline.wav"; sleep 3; stop_rec
O4=$(python3 "$WORK/an.py" onset "$WORK/baseline.wav" 440)
O8=$(python3 "$WORK/an.py" onset "$WORK/baseline.wav" 880)
if [[ "$O4" == NONE* ]] && [[ "$O8" == NONE* ]]; then
  ok "对照组干净（440 与 880 都测不出起声）"
else
  env_out "对照组就有一段正弦（440:$O4 880:$O8）—— 这台机器此刻测不了这件事"
fi

echo "== 2. 双向延迟同会话（--audio + --mic；下行 440 / 上行 880，${RUN_SECS}s）=="
start_rec "$WORK/loop.wav"
# 会话中途（起流后约 5 s，稳态里）采一次 PulseAudio 自己的延迟账。
# 为什么要第二个口径：monitor 那套的时间基是「parecord 首块落盘」，而首块是成块刷出来的
# （2026-11-19 实测：spawn→首块可见 1.2~2.2 s），绝对值天然偏高；pactl 的
# 「缓冲延迟 + 信宿延迟」是 Pulse 按设备进度算的，两者互为对照，差值才是可信的那部分。
( sleep 5; LC_ALL=C pactl list sink-inputs > "$WORK/si-during.txt" 2>&1 ) &
SAMPLER=$!
sleep 1
RUST_LOG=info timeout 60 "$CLI" --server "127.0.0.1:$PORT_D" --device "$duplex_DEVICE" \
  --password-file "$duplex_PW" --headless --seconds "$RUN_SECS" \
  --no-input --no-clipboard \
  --audio --mic --mic-source "lavfi:sine=frequency=880:sample_rate=48000" \
  >"$WORK/client-dup.log" 2>&1
CRC=$?
wait "$SAMPLER" 2>/dev/null
stop_rec
sed -E 's/\x1b\[[0-9;]*m//g' "$WORK/client-dup.log" > "$WORK/client-dup.plain"
[ "$CRC" -eq 0 ] && ok "双向会话控制端正常收工（退出码 0）" || bad "控制端退出码 $CRC"

T_HOSTAUDIO=$(log_epoch "$WORK/duplex.log" "音频已开始（Opus/Ogg）")
T_HOSTMIC=$(log_epoch "$WORK/duplex.log" "麦克风转接已接上")
T_CLIMIC=$(log_epoch "$WORK/client-dup.plain" "麦克风转接已开始")
T_CLISINK=$(log_epoch "$WORK/client-dup.plain" "实时播放已接上")
MISS=""
for v in T_HOSTAUDIO T_HOSTMIC T_CLIMIC T_CLISINK; do
  [ -n "$(eval echo \$$v)" ] || MISS="$MISS $v"
done
if [ -z "$MISS" ]; then
  ok "四个时间锚点齐（被控端下行启动/被控端播放就绪/控制端上行启动/控制端播放就绪）"
else
  bad "锚点日志缺一：$MISS（本轮延迟数作废）"
  echo; echo "结论：$PASS 通过 / $FAIL 失败 / $SKIP 跳过（$WORK）"; exit 1
fi

POS4=$(python3 "$WORK/an.py" onset90 "$WORK/loop.wav" 440 | awk '{print $1}')
POS8=$(python3 "$WORK/an.py" onset90 "$WORK/loop.wav" 880 | awk '{print $1}')
RECDUR=$(python3 "$WORK/an.py" dur "$WORK/loop.wav")
echo "  录制 ${RECDUR}s；440 起声 @${POS4}s，880 起声 @${POS8}s（相对录音数据起点；"
echo "    起声判据 = 达到稳态 90% 且稳住 0.45 s（2026-11-19 起用这条；旧「过半」落在启动爬坡里，偏早）"
{ [ -n "$POS4" ] && [ "$POS4" != "NONE" ]; } || bad "录回来的声音里没有 440 起声 —— 下行没出声，延迟无从谈起"
{ [ -n "$POS8" ] && [ "$POS8" != "NONE" ]; } || bad "录回来的声音里没有 880 起声 —— 上行没出声，延迟无从谈起"
if [ "$POS4" = "NONE" ] || [ "$POS8" = "NONE" ] || [ -z "$POS4" ] || [ -z "$POS8" ]; then
  echo; echo "结论：$PASS 通过 / $FAIL 失败 / $SKIP 跳过（$WORK）"; exit 1
fi

L_DOWN=$(calc "$REC_T0 + $POS4 - $T_HOSTAUDIO")
L_UP=$(calc "$REC_T0 + $POS8 - $T_CLIMIC")
TAIL_DOWN=$(calc "$REC_T0 + $POS4 - $T_CLISINK")
TAIL_UP=$(calc "$REC_T0 + $POS8 - $T_HOSTMIC")
echo "  下行总延迟 ≈ ${L_DOWN} s（被控端「音频已开始」→ 信宿出声）"
echo "  上行总延迟 ≈ ${L_UP} s（控制端「麦克风转接已开始」→ 信宿出声）"
echo "  下行末端段 ≈ ${TAIL_DOWN} s（控制端播放后端就绪 → 出声）"
echo "  上行末端段 ≈ ${TAIL_UP} s（被控端播放后端就绪 → 出声）"
echo "  （时间基 ±0.15 s；下行还含 AudioStart 命令在途的一小段，未拆分）"

in_range() { awk -v v="$1" 'BEGIN{exit !(v>0.02 && v<3.0)}'; }
in_range "$L_DOWN" && ok "下行总延迟在合理区间 (0.02, 3.0)s：${L_DOWN} s" || bad "下行延迟不可信：${L_DOWN} s"
in_range "$L_UP" && ok "上行总延迟在合理区间 (0.02, 3.0)s：${L_UP} s" || bad "上行延迟不可信：${L_UP} s"
awk -v v="$TAIL_DOWN" 'BEGIN{exit !(v>-0.2 && v<2.0)}' && ok "下行末端段自洽（就绪→出声，${TAIL_DOWN} s）" || bad "下行末端段异常：${TAIL_DOWN} s（锚点顺序可疑）"
awk -v v="$TAIL_UP" 'BEGIN{exit !(v>-0.2 && v<2.0)}' && ok "上行末端段自洽（${TAIL_UP} s）" || bad "上行末端段异常：${TAIL_UP} s"

# 起声同刻在这台机器上是**预期**（两路共用同一个信宿的启动爬坡，onset90 量的就是爬到稳态
# 那一刻），所以它不是失败判据；「两个频率没串」由下面两条功率比判据负责。
SEP=$(calc "$POS4 - $POS8"); SEP=${SEP#-}
awk -v v="$SEP" 'BEGIN{exit !(v>0.05)}' \
  && ok "两个起声相互独立（相差 ${SEP} s > 0.05 s，两个频率没串）" \
  || skip "两个起声同刻（${SEP} s）—— 两路共用这台机器的启动爬坡，同刻是预期；串扰看下面的功率比"

# —— 本轮到底用的是哪一档播放缓冲？让二进制自己说（旧二进制没这个字段 → 记 SKIP，别记成失败）
TAILLINE=$(grep -aoE "实时播放已接上 program=[a-z]+ buffer_us=[0-9]+ period_us=[0-9]+" "$WORK/client-dup.plain" | head -1)
if [ -z "$TAILLINE" ]; then
  skip "客户端日志里没有 buffer_us/period_us 字段（旧二进制 = 不给缓冲参数那一档）"
else
  ok "档位自证：$TAILLINE"
fi
# —— 第二个口径：PulseAudio 自己算的这条 aplay 流的延迟（缓冲延迟 + 信宿延迟）。
#    采样时用 LC_ALL=C 把 pactl 的输出钉成英文，判据不跟着现场语言走。
PA_US=$(python3 - "$WORK/si-during.txt" <<'PY'
import re, sys
try:
    t = open(sys.argv[1], encoding="utf-8", errors="replace").read()
except OSError:
    print(""); sys.exit(0)
for b in re.split(r"^Sink Input #\d+", t, flags=re.M)[1:]:
    if 'application.process.binary = "aplay"' not in b:
        continue
    bl = re.search(r"Buffer Latency:\s*(\d+) usec", b)
    sl = re.search(r"Sink Latency:\s*(\d+) usec", b)
    if bl and sl:
        print(int(bl.group(1)) + int(sl.group(1))); sys.exit(0)
print("")
PY
)
if [ -n "$PA_US" ]; then
  echo "  PulseAudio 自报（会话中途，这条 aplay 流）：末端段 $(calc "$PA_US / 1000000") s（缓冲延迟+信宿延迟，与录音时间基无关）"
else
  echo "  PulseAudio 的账采不到（$WORK/si-during.txt 里没有 aplay 那条流）—— 只报 monitor 口径"
fi
# —— 小缓冲的代价是 xrun：稳态里若掉过声音，最小窗功率会明显低于稳态中位数。
#    窗口从「起声 +0.6 s」起（起声用的是 onset90，本身已经站在稳态上）：
#    会话起来后约 2.2 s 内有一个**两路一起**的启动暂态
#    （功率冲到稳态的 63% → 掉回 0 → 再爬回稳态），那是流起来那一下的事，不是稳态 xrun；
#    把它算进判据会把每次运行都判红（首跑就是这么红的，已核对逐窗功率数据）。
for pair in "下行:440:$POS4" "上行:880:$POS8"; do
  LBL=${pair%%:*}; rest=${pair#*:}; FQ=${rest%%:*}; AT=${rest#*:}
  read -r PMIN PSTEADY <<<"$(python3 "$WORK/an.py" powermin "$WORK/loop.wav" "$FQ" $(calc "$AT + 0.6") $(calc "$AT + 4.6"))"
  if [ -z "${PMIN:-}" ] || [ "$PMIN" = "0" ]; then
    bad "$LBL 稳态段读不出功率（${FQ} Hz）—— 判不出有没有 xrun"
  else
    awk -v a="$PMIN" -v b="$PSTEADY" 'BEGIN{exit !(a > 0.5*b)}' \
      && ok "$LBL 稳态里没掉过声音（${FQ} Hz 最小窗功率 ${PMIN} > 稳态 ${PSTEADY} 的一半）" \
      || bad "$LBL 稳态里功率掉过块（最小 ${PMIN} vs 稳态 ${PSTEADY}）—— 播放缓冲太薄的表现"
  fi
done

P4=$(python3 "$WORK/an.py" power "$WORK/loop.wav" 440 "$POS8" $(calc "$POS8 + 5"))
P8=$(python3 "$WORK/an.py" power "$WORK/loop.wav" 880 "$POS8" $(calc "$POS8 + 5"))
P1=$(python3 "$WORK/an.py" power "$WORK/loop.wav" 1000 "$POS8" $(calc "$POS8 + 5"))
awk -v a="$P4" -v b="$P1" 'BEGIN{exit !(a>10*(b+1e-15))}' && ok "稳态里 440 仍是主角（对 1000）" || bad "440 主导性不成立：$P4 vs $P1"
awk -v a="$P8" -v b="$P1" 'BEGIN{exit !(a>10*(b+1e-15))}' && ok "稳态里 880 也是主角（双向同会话没互踩）" || bad "880 主导性不成立：$P8 vs $P1"
# 独立性硬证：**先起的那个频率在「另一个还没起」的区间里得是压倒性主角** ——
# 若 880 只是 440 的二次谐波泄漏，它不可能在 440 独响时反而更强。
LATE_F=880; EARLY_F=440
if awk -v a="$POS4" -v b="$POS8" 'BEGIN{exit !(a<b)}'; then LATE_F=440; EARLY_F=880; fi
POS_MIN=$(awk -v a="$POS4" -v b="$POS8" 'BEGIN{print (a<b)?a:b}')
POS_MAX=$(awk -v a="$POS4" -v b="$POS8" 'BEGIN{print (a<b)?b:a}')
WIN0=$(calc "$POS_MIN + 0.4"); WIN1=$(calc "$POS_MAX - 0.2")
PE=$(python3 "$WORK/an.py" power "$WORK/loop.wav" "$EARLY_F" "$WIN0" "$WIN1")
PL=$(python3 "$WORK/an.py" power "$WORK/loop.wav" "$LATE_F" "$WIN0" "$WIN1")
echo "  独立起声证明窗 [$WIN0, $WIN1]：先响的 $EARLY_F Hz=$PE，后响的 $LATE_F Hz=$PL"
if awk -v x="$WIN1" -v y="$WIN0" 'BEGIN{exit !(x-y>=0.3)}'; then
  awk -v a="$PE" -v b="$PL" 'BEGIN{exit !(a>10*(b+1e-15))}' \
    && ok "两个频率各自独立成立（先响者在后响者起声前已独享，比值 >10）" \
    || bad "后响的频率在先响阶段就有功率（$PL），独立性存疑（谐波泄漏或串扰）"
else
  skip "两个起声太接近（差 $(calc "$POS_MAX - $POS_MIN") s），独立性证明窗给不出"
fi

VF=$(grep -o "解出帧 *: [0-9]*" "$WORK/client-dup.plain" | tail -1 | tr -dc '0-9')
VD=$(grep -o "解码丢帧 *: [0-9]*" "$WORK/client-dup.plain" | tail -1 | tr -dc '0-9')
echo "  同会话画面：${VF:-?} 帧 / 丢帧 ${VD:-?}"
{ [ "${VF:-0}" -ge 100 ] && [ "${VD:-9}" = "0" ]; } \
  && ok "量延迟的同会话画面照出 ${VF} 帧 / 丢帧 0" || bad "画面异常：${VF:-?} 帧 / 丢帧 ${VD:-?}"

echo "== 3. 信宿争用：另一路声音（paplay 1000 Hz）打进同一个信宿 =="
start_host "$WORK/cont.json" cont "$PORT_C" || env_out "争用段测试台起不来"
ffmpeg -v error -y -f lavfi -i "sine=frequency=1000:sample_rate=48000" -t 5 -ac 2 "$WORK/comp.wav" \
  || env_out "本机 ffmpeg 造不出干扰音"
start_rec "$WORK/cont.wav"
sleep 1
RUST_LOG=info timeout 60 "$CLI" --server "127.0.0.1:$PORT_C" --device "$cont_DEVICE" \
  --password-file "$cont_PW" --headless --seconds "$((RUN_SECS + 2))" \
  --no-input --no-clipboard --audio >"$WORK/client-cont.log" 2>&1 &
CLI_PID=$!
sleep 5   # 让下行先响起来（留 ≥2 s 稳态在前窗口里）
T_CONT0=$(epoch_now)
paplay -d "$SINK" "$WORK/comp.wav" >"$WORK/paplay.err" 2>&1
RC_PAPLAY=$?
T_CONT1=$(epoch_now)
wait "$CLI_PID"; CRC2=$?
stop_rec
[ "$RC_PAPLAY" -eq 0 ] && ok "干扰音 paplay 退出码 0（它自己也得是老实的）" || echo "  [提示] paplay 退出码 $RC_PAPLAY（以录回功率为准）"
[ "$CRC2" -eq 0 ] && ok "争用段控制端照常收工（退出码 0，没挂死）" || bad "争用段控制端退出码 $CRC2"
sed -E 's/\x1b\[[0-9;]*m//g' "$WORK/client-cont.log" > "$WORK/client-cont.plain"
grep -qE "播放后端写入失败|实时播放后端启动失败|实时播放失败" "$WORK/client-cont.plain" \
  && bad "争用把播放弄坏了：$(grep -oE '播放后端写入失败.*|实时播放后端启动失败.*|实时播放失败.*' "$WORK/client-cont.plain" | head -1)" \
  || ok "播放侧零句失败（争用只是叠加，不是抢占）"

R0=$(calc "$T_CONT0 - $REC_T0")
R1=$(calc "$T_CONT1 - $REC_T0")
PRE0=$(calc "$R0 - 2.0"); PRE1=$(calc "$R0 - 0.3")
D0=$(calc "$R0 + 0.6"); D1=$(calc "$R1 - 0.6")
PO0=$(calc "$R1 + 0.8"); PO1=$(calc "$R1 + 3.0")
P4_PRE=$(python3 "$WORK/an.py" power "$WORK/cont.wav" 440 "$PRE0" "$PRE1")
P4_DUR=$(python3 "$WORK/an.py" power "$WORK/cont.wav" 440 "$D0" "$D1")
P1_DUR=$(python3 "$WORK/an.py" power "$WORK/cont.wav" 1000 "$D0" "$D1")
P1_PRE=$(python3 "$WORK/an.py" power "$WORK/cont.wav" 1000 "$PRE0" "$PRE1")
P4_POST=$(python3 "$WORK/an.py" power "$WORK/cont.wav" 440 "$PO0" "$PO1")
echo "  分段功率：争用前440=$P4_PRE / 争用中440=$P4_DUR / 争用后440=$P4_POST"
echo "            争用前1000=$P1_PRE / 争用中1000=$P1_DUR"
awk -v a="$P1_DUR" -v b="$P1_PRE" 'BEGIN{exit !(a>20*(b+1e-15))}' \
  && ok "干扰音真的进了这个信宿（录回里 1000 Hz 功率跳变 >20 倍）" \
  || bad "没录到干扰音（$P1_PRE→$P1_DUR）—— 争用场景没构造成功"
awk -v a="$P4_DUR" -v b="$P4_PRE" 'BEGIN{exit !(a>0.3*b)}' \
  && ok "争用期间 440 没被挤掉（功率保持 >30%）" || bad "争用期间 440 塌了：$P4_DUR vs $P4_PRE"
awk -v a="$P4_POST" -v b="$P4_PRE" 'BEGIN{exit !(a>0.5*b)}' \
  && ok "干扰收掉之后 440 照常响（没有「抢过一次就永久哑」）" || bad "争用之后 440 没回来：$P4_POST vs $P4_PRE"

echo "== 4. 收尾账 =="
# aplay 收尾有尾巴（关 stdin → 播完缓冲自退），轮询到回落基线，别拿固定 sleep 抽签。
FF_END=0; AP_END=0
for _ in $(seq 1 16); do
  FF_END=$(pgrep -c -u "$(id -u)" -x ffmpeg || true); AP_END=$(pgrep -c -u "$(id -u)" -x aplay || true)
  { [ "${FF_END:-0}" -le "${FF_BASE:-0}" ] && [ "${AP_END:-0}" -le "${AP_BASE:-0}" ]; } && break
  sleep 0.5
done
{ [ "${FF_END:-0}" -le "${FF_BASE:-0}" ] && [ "${AP_END:-0}" -le "${AP_BASE:-0}" ]; } \
  && ok "ffmpeg/aplay 回到会话前基线（${FF_BASE:-0}→${FF_END:-0} / ${AP_BASE:-0}→${AP_END:-0}，轮询≤8s）" \
  || { bad "子进程没归位：ffmpeg=${FF_END:-?} aplay=${AP_END:-?}（基线 ${FF_BASE:-0}/${AP_BASE:-0}）"
       for p in $(pgrep -u "$(id -u)" -x aplay); do
         ps -o pid,ppid,lstart,args -p "$p" | tail -1
         echo "    fd0 -> $(readlink /proc/$p/fd/0 2>/dev/null)"
       done; }

echo
echo "结论：$PASS 通过 / $FAIL 失败 / $SKIP 跳过（工作目录 $WORK）"
echo "延迟汇总：下行 ${L_DOWN:-?} s / 上行 ${L_UP:-?} s（末端段 ${TAIL_DOWN:-?} / ${TAIL_UP:-?} s；时间基 ±0.15 s）"
[ "$FAIL" -eq 0 ] || exit 1
exit 0
