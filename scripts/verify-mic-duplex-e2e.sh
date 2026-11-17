#!/usr/bin/env bash
# 双向同会话（--audio 下行 与 --mic 上行 同时）+ 麒麟真麦克风输入源观察 —— 冒烟
# 用法（真机）：bash verify-mic-duplex-e2e.sh
# 判据口径：
#   * 上行声学的硬判据仍是 monitor 录回 440（合成源轮）；
#   * 下行只判「同会话收满时长 + 画面统计不回归」——测试台没有真声可采，WAV 内容就是 monitor 底；
#   * 真麦轮判「本机采到非静音」= 被控端收尾账 peak>0（夜里 office = 底噪，数值只判非零）；
#   * 画面活性判控制端统计行（解出帧/丢帧），不新开 x11grab。
set -u
BIN="${UULAN_BIN:-$HOME/.local/uulan/src/target/release}"
PORT="${UULAN_PORT:-21998}"
TS=$(date +%Y%m%d-%H%M%S)
WORK="/tmp/uulan-duplex.$TS"
NO_COLOR=1; export NO_COLOR
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
PASS=0; FAIL=0
ok()  { echo "  [PASS] $*"; PASS=$((PASS+1)); }
bad() { echo "  [FAIL] $*"; FAIL=$((FAIL+1)); }
env_out() { echo "  [环境] $*"; echo "退出码 2：这台机器给不出判据"; exit 2; }
mkdir -p "$WORK" || exit 2
clean_ansi() { sed -E 's/\x1b\[[0-9;]*m//g' "$1"; }

SRV="$BIN/uulan-server"; HOST="$BIN/uulan-host"; CLI="$BIN/uulan-client"
for f in "$SRV" "$HOST" "$CLI"; do [ -x "$f" ] || env_out "缺 $f"; done
for c in ffmpeg parecord python3 pgrep; do command -v "$c" >/dev/null || env_out "缺 $c"; done

# 复用麦克风验收里**修正后**的 check440（先混 (L+R)/2 再按 48k 探 440）
sed -n "/cat > \"\$WORK\/check440.py\"/,/^PY$/p" \
  ~/.local/uulan/src/scripts/verify-mic-realkylin-e2e.sh | sed '1d;$d' > "$WORK/check440.py"
[ -s "$WORK/check440.py" ] || env_out "拿不到修正版 check440（麦克风验收脚本被挪了？）"

SRVPID=""; HOSTPID=""; RECPID=""
cleanup() {
  [ -n "$RECPID" ] && kill "$RECPID" 2>/dev/null
  [ -n "$HOSTPID" ] && kill "$HOSTPID" 2>/dev/null
  [ -n "$SRVPID" ] && kill "$SRVPID" 2>/dev/null
  return 0
}
trap cleanup EXIT

"$SRV" --bind "127.0.0.1:$PORT" --state "$WORK/devices.json" >"$WORK/server.log" 2>&1 &
SRVPID=$!
for _ in $(seq 1 100); do
  (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && { exec 3>&-; break; }
  sleep 0.1
done
RUST_LOG=info "$HOST" --server "127.0.0.1:$PORT" --config "$WORK/host.json" \
  --capture "lavfi:testsrc2=size=640x480:rate=15" --name duplex-host >"$WORK/host.log" 2>&1 &
HOSTPID=$!
DEVICE=""
for _ in $(seq 1 200); do
  DEVICE=$(grep -oE "设备 ID : [0-9]{6,}" "$WORK/host.log" 2>/dev/null | tail -1 | sed 's/.*: *//' | tr -d '[:space:]')
  [ -n "$DEVICE" ] && break
  sleep 0.1
done
[ -n "$DEVICE" ] || { grep -vE "访问密码" "$WORK/host.log" | head -12; exit 2; }
sed -n 's/.*"password": *"\([^"]*\)".*/\1/p' "$WORK/host.json" | head -1 > "$WORK/pw.txt"
[ -s "$WORK/pw.txt" ] || exit 2
chmod 600 "$WORK/pw.txt"
SINK=$(pactl list sinks short | awk 'NR==1{print $2}')
MON="$SINK.monitor"
pactl list sources short | grep -qF "$MON" || env_out "没有 $MON"
MICSRC=$(pactl list sources short | awk '$2 ~ /^alsa_input\./{print $2; exit}')
[ -n "$MICSRC" ] || env_out "这台机器没有任何 alsa_input 源 —— 真麦观察给不出"
ok "测试台起来了（设备 $DEVICE；真麦源 $MICSRC；口令不上屏）"

start_rec() { # $1=输出
  RECPID=""
  parecord -d "$MON" --format=s16le --rate=48000 --channels=2 --file-format=wav "$1" \
    >"$WORK/record.err" 2>&1 &
  RECPID=$!
}
stop_rec() { [ -n "$RECPID" ] && kill "$RECPID" 2>/dev/null; RECPID=""; sleep 0.5; }

echo "== 1. 对照组：会话开始前 monitor 必须安静 =="
start_rec "$WORK/baseline.wav"; sleep 3; stop_rec
[ -s "$WORK/baseline.wav" ] || env_out "parecord 录不到 $MON"
python3 "$WORK/check440.py" "$WORK/baseline.wav" quiet \
  && ok "对照组干净" || bad "对照组就测出 440 —— 环境在响，本轮判据作废"

echo "== 2. 合成源双向同会话（--audio + --mic 同时，10 秒）=="
start_rec "$WORK/loop.wav"
sleep 1
RUST_LOG=info timeout 45 "$CLI" --server "127.0.0.1:$PORT" --device "$DEVICE" \
  --password-file "$WORK/pw.txt" --headless --seconds 10 \
  --audio --audio-save "$WORK/down.wav" \
  --mic --mic-source "lavfi:sine=frequency=440:sample_rate=48000" \
  >"$WORK/client1.log" 2>&1
CRC=$?
stop_rec
[ "$CRC" = 0 ] || bad "双向会话控制端退出码 $CRC（应 0；参数互斥或挂死都会红在这里）"
CA=$(clean_ansi "$WORK/client1.log" | grep -a "音频状态" | tail -1)
CM=$(clean_ansi "$WORK/client1.log" | grep -a "麦克风状态" | tail -1)
CV=$(clean_ansi "$WORK/client1.log" | grep -aE "解出帧|解码丢帧" | tr -s ' ')
echo "    $CA"; echo "    $CM"; echo "    $CV"
echo "$CA" | grep -q "正在推流" && ok "下行同会话到 Running" || bad "下行没起来：$CA"
echo "$CM" | grep -q "正在推流" && ok "上行同会话到「被控端正在推流」" || bad "上行没起来：$CM"
L=$(echo "$CV" | grep -oE "解出帧 *: [0-9]+" | grep -oE "[0-9]+")
D=$(echo "$CV" | grep -oE "解码丢帧 *: [0-9]+" | grep -oE "[0-9]+")
{ [ "${L:-0}" -ge 100 ] && [ "${D:-9}" = "0" ]; } \
  && ok "同会话画面照出 $L 帧 / 丢帧 0（两条音频通道没拖垮视频）" \
  || bad "画面异常：解出 ${L:-?} 帧 / 丢帧 ${D:-?}"
python3 "$WORK/check440.py" "$WORK/loop.wav" tone \
  && ok "上行 440 真到了空气里（同会话 monitor 录回主导）" || bad "monitor 录不回 440 —— 上行被下行挤掉了？"
python3 - "$WORK/down.wav" <<'PY'
import struct, sys
try:
    b = open(sys.argv[1], "rb").read()
except Exception:
    print("  下行 WAV 不存在"); sys.exit(2)
if len(b) < 44 or b[:4] != b"RIFF":
    print("  下行 WAV 不是 WAV（%d 字节）" % len(b)); sys.exit(2)
rate = struct.unpack_from("<I", b, 24)[0]
bits = struct.unpack_from("<H", b, 34)[0]
ch = struct.unpack_from("<H", b, 22)[0]
sec = (len(b) - 44) / (rate * ch * (bits / 8))
print("  下行 WAV %.2f 秒" % sec)
sys.exit(0 if sec >= 4.0 else 1)
PY
[ "$?" = "0" ] && ok "下行通道同会话收满时长（≥4 秒）" || bad "下行 WAV 太短 —— 双向互踩？"

echo "== 3. 收尾账目：双向会话的 on_end =="
H=$(clean_ansi "$WORK/host.log" | grep -a "麦克风转接收尾" | tail -1)
echo "    $H"
echo "$H" | grep -q "bytes=" && ok "上行收尾账落盘（on_end 在双向会话里照样说得出话）" \
  || bad "双向会话里收尾账丢了 —— on_end 回归"
sleep 1
clean_ansi "$WORK/host.log" | grep -aE "音频" | tail -2 | sed 's/^/    host: /'
NG=$(clean_ansi "$WORK/client1.log" | grep -acE "error|失败" || true)
echo "    （控制端日志含 error/失败 的行数 = $NG，只作观察）"

echo "== 4. 真麦克风输入源轮（--mic-source pulse:<alsa_input>；采到非静音即观察成立）=="
start_rec "$WORK/realmic.wav"
sleep 1
RUST_LOG=info timeout 45 "$CLI" --server "127.0.0.1:$PORT" --device "$DEVICE" \
  --password-file "$WORK/pw.txt" --headless --seconds 8 \
  --mic --mic-source "pulse:$MICSRC" >"$WORK/client2.log" 2>&1
CRC2=$?
stop_rec
sleep 2
[ "$CRC2" = 0 ] || bad "真麦会话退出码 $CRC2（应 0）"
M2=$(clean_ansi "$WORK/client2.log" | grep -a "麦克风状态" | tail -1)
U2=$(clean_ansi "$WORK/client2.log" | grep -aE "麦克风 *: 上行" | tail -1)
echo "    $M2"
echo "    $U2"
echo "$M2" | grep -q "正在推流" && ok "真麦（$MICSRC）采集起来了" || bad "真麦没起来：$M2"
H2=$(clean_ansi "$WORK/host.log" | grep -a "麦克风转接收尾" | tail -1)
echo "    $H2"
PK=$(echo "$H2" | sed -n 's/.*peak=\([0-9]*\).*/\1/p')
B2=$(echo "$H2" | sed -n 's/.*bytes=\([0-9]*\).*/\1/p')
[ -n "$PK" ] && [ "$PK" -gt 0 ] \
  && ok "真麦采到非静音：被控端解出 peak=$PK / bytes=$B2（夜里 office=底噪，判据只认非零）" \
  || bad "真麦轮 peak=${PK:-?}（收尾账没更新或真静音流——两种都得查）"
python3 "$WORK/check440.py" "$WORK/realmic.wav" quiet \
  && ok "真麦轮 monitor 无 440 主导（没把环境音误判成 tone）" \
  || bad "真麦轮 monitor 测出 440 主导 —— 上一轮在响？还是判据器又被碰了？"

echo "== 5. 子进程泄漏：全部会话结束后 ffmpeg 必须归零 =="
sleep 2
NF=$(pgrep -c ffmpeg 2>/dev/null || true)
[ "${NF:-0}" = "0" ] && ok "ffmpeg 子进程 0 个" || bad "还挂着 $NF 个 ffmpeg（编码/播放子进程泄漏）"

echo
echo "== 结果：$PASS 通过 / $FAIL 失败（工作目录 $WORK）=="
[ "$FAIL" = 0 ] || exit 1
exit 0
