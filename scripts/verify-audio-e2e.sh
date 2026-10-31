#!/usr/bin/env bash
# 音频（M7）全链路验收：真 server + 真被控端 + 真控制端。
#
# 被控端用 `lavfi:sine=frequency=440` 合成源（这台开发机没有 PulseAudio，
# 而真机上的 `-f pulse` 那一段另验），控制端把收到的声音落成 WAV，
# 然后**在波形上断言**：440 Hz 占主导、峰值不为零、时长与跑的时间相符。
#
# 只数字节数是没用的：一段白噪声也能凑够字节。这里查的是「解出来的还是不是那个正弦波」。
#
# 为什么要拷出去跑：位于 D:/default-workspace/ 之下的 exe 在这台开发机上写不了任何文件
# （被控端要写 host.json、控制端要写 WAV）—— 与代码无关，见 DESIGN 的构建陷阱 2。
#
# 用法：
#   bash scripts/verify-audio-e2e.sh
#   UULAN_SECONDS=10 UULAN_PORT=21997 bash scripts/verify-audio-e2e.sh
#
# 退出码：0 = 通过；1 = 断言失败；2 = 环境问题（端口占用 / 进程没起来 / 没有 ffmpeg）。
set -u

PORT="${UULAN_PORT:-21993}"
RUN_SECS="${UULAN_SECONDS:-8}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${UULAN_STAGE:-D:/uulan-bin/audio-$(date +%Y%m%d-%H%M%S)}"
NO_COLOR=1
export NO_COLOR

mkdir -p "$WORK" || { echo "建不了 $WORK"; exit 2; }
cp -f "$ROOT"/target/release/uulan-server.exe \
      "$ROOT"/target/release/uulan-host.exe \
      "$ROOT"/target/release/uulan-client.exe "$WORK"/ || { echo "拷二进制失败（上一轮还在跑？）"; exit 2; }
echo "工作目录：$WORK  端口：$PORT  时长：${RUN_SECS}s"

SERVER_PID=""; HOST_PID=""
cleanup() {
  [ -n "$HOST_PID" ] && kill "$HOST_PID" 2>/dev/null
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null
}
trap cleanup EXIT

RUST_LOG=info "$WORK/uulan-server.exe" --bind "127.0.0.1:$PORT" \
  --state "$WORK/devices.json" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!

for _ in $(seq 1 100); do
  (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && { exec 3>&-; break; }
  sleep 0.1
done

RUST_LOG=info "$WORK/uulan-host.exe" --server "127.0.0.1:$PORT" --config "$WORK/host.json" \
  --capture "lavfi:testsrc2=size=640x480:rate=15" \
  --audio-source "lavfi:sine=frequency=440:sample_rate=48000" \
  --name audio-host --once >"$WORK/host.log" 2>&1 &
HOST_PID=$!

DEVICE=""
for _ in $(seq 1 200); do
  DEVICE=$(grep -oE "设备 ID : [0-9]{6,}" "$WORK/host.log" 2>/dev/null | tail -1 | sed 's/.*: *//' | tr -d '[:space:]')
  [ -n "$DEVICE" ] && break
  sleep 0.1
done
if [ -z "$DEVICE" ]; then
  echo "被控端没拿到设备 ID，日志（已滤掉密码行）："
  grep -vE "访问密码" "$WORK/host.log" | sed -n 1,20p
  exit 2
fi

PW=$(sed -n 's/.*"password": *"\([^"]*\)".*/\1/p' "$WORK/host.json" | head -1)
[ -n "$PW" ] || { echo "host.json 里没有 password 字段"; exit 2; }
printf '%s\n' "$PW" > "$WORK/pw.txt"
unset PW

echo "设备 ID：$DEVICE（口令走文件，不上屏）"
"$WORK/uulan-client.exe" --server "127.0.0.1:$PORT" --device "$DEVICE" \
  --password-file "$WORK/pw.txt" --headless --seconds "$RUN_SECS" \
  --no-input --no-clipboard --audio --audio-save "$WORK/out.wav" \
  >"$WORK/client.log" 2>&1
RC=$?
echo "客户端退出码：$RC"
grep -E "音频|解出帧|丢帧" "$WORK/client.log" | sed -n 1,10p

if [ "$RC" -ne 0 ]; then echo "客户端没正常收工"; exit 1; fi
[ -f "$WORK/out.wav" ] || { echo "没有写出 WAV"; exit 1; }

# python 是原生 Windows 程序：路径必须是 C:/ 形式（MSYS 的 /d/... 它不认）。
WAV_NATIVE=$(cygpath -m "$WORK/out.wav" 2>/dev/null || echo "$WORK/out.wav")
PY="$WORK/check.py"
cat >"$PY" <<'PY'
import struct, sys, math

path = sys.argv[1]
min_sec = float(sys.argv[2])
b = open(path, "rb").read()
assert b[:4] == b"RIFF" and b[8:12] == b"WAVE", "不是 WAV"
ch = struct.unpack_from("<H", b, 22)[0]
rate = struct.unpack_from("<I", b, 24)[0]
bits = struct.unpack_from("<H", b, 34)[0]
declared = struct.unpack_from("<I", b, 40)[0]
print(f"WAV: {len(b)} 字节 / {declared} 字节数据 / {rate} Hz / {ch} ch / {bits} bit")
assert ch == 2 and rate == 48000 and bits == 16, "参数不对"
assert declared == len(b) - 44, "头里的长度没回填（收尾没走 finish）"
samples = struct.unpack_from("<%dh" % (declared // 2), b, 44)
sec = len(samples) / rate / ch
peak = max(abs(s) for s in samples)
print(f"时长 {sec:.2f} s / 峰值 {peak}")

def goertzel(seg, freq):
    n = len(seg)
    w = 2 * math.pi * (freq / rate) * n / n
    coeff = 2 * math.cos(2 * math.pi * freq / rate)
    s1 = s2 = 0.0
    for s in seg:
        s0 = s + coeff * s1 - s2
        s2, s1 = s1, s0
    return s1 * s1 + s2 * s2 - coeff * s1 * s2

tail = samples[-int(rate * ch * 2):]
p440 = goertzel(tail, 440.0)
p1000 = goertzel(tail, 1000.0)
print(f"440Hz 功率 {p440:.3e} / 1000Hz {p1000:.3e} = {p440 / max(p1000, 1e-9):.0f} 倍")

ok = True
if sec < min_sec * 0.6:
    print(f"FAIL: 只有 {sec:.2f} 秒音频（跑了 {min_sec} 秒）"); ok = False
if peak < 2000:
    print(f"FAIL: 峰值只有 {peak}，几乎静音"); ok = False
if p440 < p1000 * 20:
    print("FAIL: 440 Hz 没有占主导，解出来的不是那个正弦波"); ok = False
sys.exit(0 if ok else 1)
PY

python "$(cygpath -m "$PY" 2>/dev/null || echo "$PY")" "$WAV_NATIVE" "$RUN_SECS"
RES=$?
if [ "$RES" -ne 0 ]; then echo "音频断言失败"; exit 1; fi

# 控制端统计行里必须真的有「收到 N 帧」并且 N > 0。
if ! grep -qE "音频：收到 [1-9][0-9]* 帧" "$WORK/client.log"; then
  echo "控制端统计里没有收到音频帧（grep 不到「音频：收到 N 帧」）"
  exit 1
fi
echo "通过：声音从被控端到了控制端，且解出来就是那个 440 Hz 正弦波。"
exit 0