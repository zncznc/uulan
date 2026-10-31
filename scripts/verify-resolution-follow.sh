#!/usr/bin/env bash
# 验「被控端换分辨率 → 画面自动跟上」（M2.5 最后一格，真机 + 真服务，无人值守可跑）。
#
# 为什么需要它：这条链路以前只有本机全链路测试（注入假探针），真机上从没跑过。
# 真机上要造「有人在用的时候改了分辨率」，最干净的手法是 `xrandr --fb WxH`
# —— 没有连着的输出时（麒麟这台就是 VGA/HDMI 都 disconnected）它直接改帧缓冲尺寸，
# 等价于用户把分辨率调小/调大，而且可逆（trap 里恢复）。
#
# 断言的顺序就是「一次真实的分辨率变化」该有的样子：
#   1. 探到真机当前分辨率
#   2. 会话 A 开局就报对尺寸（**2026-11-01 修的那个 bug**：进程启动时的缓存过期时这里会是旧值）
#   3. 会话 A 中途拉大：控制端日志出现「解码器已重建」，会话**没断**（建立 1 条 / 接回 0 次）
#   4. 会话 A 收工时画幅与存出的帧都是新尺寸，且与真机截图逐像素相关 ≥ 0.99
#   5. 会话 B 反过来缩回：同样三条（开局报对、重建、画幅/存帧/相关）
#   6. 被控端 journal 里两次「分辨率已变」都有记录（证明确实是被控端跟上，不是控制端自己猜的）
#   7. 退出时分辨率已恢复原值
#
# 用法：
#   bash scripts/verify-resolution-follow.sh --device <ID> --password-file <文件> [--alt 1280x720]
# 环境变量：UULAN_CLIENT / UULAN_SSH / UULAN_SERVER / UULAN_DISPLAY / UULAN_ALT / UULAN_WORK
# 退出码：0 = 全部断言通过；1 = 不达标；2 = 环境做不了（改不了分辨率 / 没设备 ID）
set -uo pipefail

CLIENT=${UULAN_CLIENT:-/d/uulan-bin/uulan-client.exe}
SSH=${UULAN_SSH:-ssh -o ConnectTimeout=12 kylin}
SCP=${UULAN_SCP:-scp -q}
SERVER=${UULAN_SERVER:-192.168.223.15:21116}
DISP=${UULAN_DISPLAY:-:0}
WORK=${UULAN_WORK:-/d/uulan-bin}
ALT=${UULAN_ALT:-1280x720}
HERE=$(cd "$(dirname "$0")" && pwd)
SECS=${UULAN_SECONDS:-30}
CHANGE_AT=${UULAN_CHANGE_AT:-11}
SHOT_AT=${UULAN_SHOT_AT:-17}

DEV=""; PWF=""
while [ $# -gt 0 ]; do
  case "$1" in
    --device) DEV=$2; shift 2 ;;
    --password-file) PWF=$2; shift 2 ;;
    --alt) ALT=$2; shift 2 ;;
    *) echo "未知参数：$1"; exit 2 ;;
  esac
done
DEV=${DEV:-${UULAN_DEVICE:-}}
PWF=${PWF:-${UULAN_PASSWORD_FILE:-}}
[ -n "$DEV" ] || { echo "FAIL 需要 --device <设备 ID>"; exit 2; }
[ -n "$PWF" ] && [ -s "$PWF" ] || { echo "FAIL 需要 --password-file <文件>（首行口令）"; exit 2; }
[ -x "$CLIENT" ] || { echo "FAIL 找不到控制端 $CLIENT（先 cargo build --release，再拷出工作区）"; exit 2; }
mkdir -p "$WORK"

pass=0; fail=0
ok()  { pass=$((pass+1)); echo "  PASS $1"; }
bad() { fail=$((fail+1)); echo "  FAIL $1"; }
chk() { if [ "$2" = "1" ]; then ok "$1"; else bad "$1（实际：$3）"; fi; }

# 原生程序只认 C:/ D:/ 形式，MSYS 工具反过来只认 /d/ —— 两套都备着。
PWN=$(cygpath -m "$PWF" 2>/dev/null || echo "$PWF")
WORKN=$(cygpath -m "$WORK" 2>/dev/null || echo "$WORK")
HEREN=$(cygpath -m "$HERE" 2>/dev/null || echo "$HERE")   # 同上：python 只认 D:/... 形式

ORIG=""
CLI_PID=""
cleanup() {
  [ -n "${CLI_PID:-}" ] && kill "$CLI_PID" 2>/dev/null
  # 分辨率一定要恢复：这是用户真实桌面，不能留在改过的尺寸上。
  if [ -n "$ORIG" ]; then
    $SSH "DISPLAY=$DISP xrandr --fb $ORIG" >/dev/null 2>&1
  fi
  rm -f "$WORK"/uulan-res-pw.txt 2>/dev/null
}
trap cleanup EXIT

dim() { $SSH "DISPLAY=$DISP xdpyinfo 2>/dev/null | awk '/dimensions:/{print \$2}'" 2>/dev/null | tr -d '\r' | tail -1; }
set_fb() { $SSH "DISPLAY=$DISP xrandr --fb $1 >/dev/null 2>&1; sleep 0.8; DISPLAY=$DISP xdpyinfo 2>/dev/null | awk '/dimensions:/{print \$2}'" 2>/dev/null | tr -d '\r' | tail -1; }
ppm_size() { python -c "
import sys
f=open(sys.argv[1],'rb')
assert f.readline().strip()==b'P6'
l=f.readline().split()
while len(l)<2: l+=f.readline().split()
print('%sx%s'%(l[0].decode(),l[1].decode()))" "$1" 2>/dev/null; }
stat_line() { tr -d '\r' < "$1" | grep -aE "^ *$2" | tail -1; }

echo "== 0) 前置：真机当前分辨率 =="
ORIG=$(dim)
[ -n "$ORIG" ] || { echo "FAIL 连不上真机或读不到分辨率（$DISP）"; exit 2; }
echo "  原始分辨率 $ORIG，本次备选 $ALT"
if [ "$ALT" = "$ORIG" ]; then ALT="1024x768"; [ "$ALT" = "$ORIG" ] && ALT="800x600"; fi
ok "读到现在分辨率 $ORIG"

echo "== 0b) 先确认这台机器改得动帧缓冲（改不动这条验证没意义）=="
PROBE=$(set_fb "$ALT")
if [ "$PROBE" != "$ALT" ]; then
  echo "  SKIP 这台机器的 xrandr --fb 改不动（有连着的输出？拿到 $PROBE）—— 分辨率跟随无法在它上面验"
  ORIG=""
  exit 2
fi
BACK=$(set_fb "$ORIG")
[ "$BACK" = "$ORIG" ] && ok "xrandr --fb 可改可恢复（$ORIG ↔ $ALT）" || { bad "改回原分辨率失败（$BACK）"; exit 1; }

run_session() {   # $1=起始尺寸（当前） $2=中途目标尺寸 $3=日志 $4=存帧 $5=截图标记
  local from=$1 to=$2 log=$3 frame=$4 shot=$5
  : > "$log"
  # RUST_LOG=info：不带它「解码器已重建」这条 INFO 根本不会打出来（第一版因此假红）。
  # NO_COLOR：tracing 会把字段名染色，grep `screen_w=1280` 抓不到。
  ( NO_COLOR=1 RUST_LOG=info "$CLIENT" --server "$SERVER" --device "$DEV" --password-file "$PWN" \
      --headless --seconds "$SECS" --save-frame "$frame" > "$log" 2>&1
    echo $? > "$log.rc" ) &
  CLI_PID=$!
  # 等真出画面再动分辨率，否则测的是「连之前就改好」，等于什么都没验
  for _ in $(seq 1 40); do grep -q '^第 [0-9]* 帧' "$log" 2>/dev/null && break; sleep 0.5; done
  sleep "$CHANGE_AT"
  GOT=$(set_fb "$to")
  echo "  中途改分辨率：$from -> $GOT"
  sleep $((SHOT_AT - CHANGE_AT))
  # 截图：远端文件名带本次会话的标记，取不回来一眼能看出是哪一张；失败不静默。
  SHOTLOG=$($SSH "DISPLAY=$DISP ffmpeg -loglevel error -f x11grab -video_size $GOT -i $DISP -frames:v 1 -y /tmp/uulan-res-shot-$shot.ppm && echo SHOT_OK" 2>&1 | tr -d '\r')
  $SCP "kylin:/tmp/uulan-res-shot-$shot.ppm" "$WORK/uulan-res-shot-$shot.ppm" >/dev/null 2>&1
  echo "  截图 $GOT：$(printf '%s' "$SHOTLOG" | tail -1)"
  wait "$CLI_PID" 2>/dev/null; CLI_PID=""
  echo "  会话收工（rc=$(cat "$log.rc" 2>/dev/null | tr -d '\r')）"
}

echo "== 1) 会话 A：$ORIG -> $ALT（会话中途拉大）=="
run_session "$ORIG" "$ALT" "$WORK/res-a.log" "$WORKN/uulan-res-a.ppm" a
A_RC=$(tr -d '\r' < "$WORK/res-a.log.rc" 2>/dev/null)
A_BANNER=$(tr -d '\r' < "$WORK/res-a.log" | grep -ao '屏幕 [0-9]*x[0-9]*' | head -1 | sed 's/屏幕 //')
chk "会话 A 开局就报对尺寸（$ORIG）" "$([ "$A_BANNER" = "$ORIG" ] && echo 1)" "$A_BANNER"
chk "会话 A 日志里出现「解码器已重建」 $ORIG -> $ALT" \
    "$(tr -d '\r' < "$WORK/res-a.log" | grep -aq "解码器已重建 screen_w=${ALT%x*} screen_h=${ALT#*x}" && echo 1)" "无"
A_SIZE=$(stat_line "$WORK/res-a.log" '当前画幅' | grep -oE '[0-9]+x[0-9]+' | head -1)
chk "会话 A 收工画幅 = $ALT" "$([ "$A_SIZE" = "$ALT" ] && echo 1)" "$A_SIZE"
A_FRAME=$(ppm_size "$WORKN/uulan-res-a.ppm")
chk "会话 A 存出的帧是 $ALT" "$([ "$A_FRAME" = "$ALT" ] && echo 1)" "$A_FRAME"
A_SESS=$(stat_line "$WORK/res-a.log" '会话 *:')
chk "会话 A 换分辨率没断线（建立 1 / 接回 0）" \
    "$(printf '%s' "$A_SESS" | grep -aq '建立 1 条 / 断线接回 0 次' && echo 1)" "$A_SESS"
A_DROP=$(stat_line "$WORK/res-a.log" '解码丢帧 *:' | grep -oE '[0-9]+' | head -1)
chk "会话 A 丢帧 ≤ 5（换解码器边界上允许丢几帧）" "$(awk -v d="${A_DROP:-99}" 'BEGIN{print (d+0<=5)?1:0}')" "丢帧 ${A_DROP:-?}"
chk "会话 A 退出码 0" "$([ "$A_RC" = "0" ] && echo 1)" "rc=$A_RC"
CORR=$(python "$HEREN/compare-ppm.py" "$WORKN/uulan-res-a.ppm" "$WORKN/uulan-res-shot-a.ppm" 2>/dev/null | tr -d '\r')
C1=$(printf '%s' "$CORR" | awk '/相关系数/{print $3}')
chk "会话 A 画面与真机截图相关 ≥ 0.99（$C1）" "$(awk -v c="${C1:-0}" 'BEGIN{print (c+0>=0.99)?1:0}')" "${C1:-无}"

echo "== 2) 会话 B：$ALT -> $ORIG（会话中途缩回）=="
run_session "$ALT" "$ORIG" "$WORK/res-b.log" "$WORKN/uulan-res-b.ppm" b
B_RC=$(tr -d '\r' < "$WORK/res-b.log.rc" 2>/dev/null)
B_BANNER=$(tr -d '\r' < "$WORK/res-b.log" | grep -ao '屏幕 [0-9]*x[0-9]*' | head -1 | sed 's/屏幕 //')
# 这一条是 2026-11-01 修掉的 bug：进程启动时缓存的分辨率过期时，以前这里会报旧值，
# 编码器还会按旧尺寸抓一块不存在的区域当场退出（journal: 视频源结束 frames=0）。
chk "会话 B 开局就报对尺寸（$ALT，不是缓存里的旧值）" "$([ "$B_BANNER" = "$ALT" ] && echo 1)" "$B_BANNER"
chk "会话 B 日志里出现「解码器已重建」 $ALT -> $ORIG" \
    "$(tr -d '\r' < "$WORK/res-b.log" | grep -aq "解码器已重建 screen_w=${ORIG%x*} screen_h=${ORIG#*x}" && echo 1)" "无"
B_SIZE=$(stat_line "$WORK/res-b.log" '当前画幅' | grep -oE '[0-9]+x[0-9]+' | head -1)
chk "会话 B 收工画幅 = $ORIG" "$([ "$B_SIZE" = "$ORIG" ] && echo 1)" "$B_SIZE"
B_FRAME=$(ppm_size "$WORKN/uulan-res-b.ppm")
chk "会话 B 存出的帧是 $ORIG" "$([ "$B_FRAME" = "$ORIG" ] && echo 1)" "$B_FRAME"
CORR=$(python "$HEREN/compare-ppm.py" "$WORKN/uulan-res-b.ppm" "$WORKN/uulan-res-shot-b.ppm" 2>/dev/null | tr -d '\r')
C2=$(printf '%s' "$CORR" | awk '/相关系数/{print $3}')
chk "会话 B 画面与真机截图相关 ≥ 0.99（$C2）" "$(awk -v c="${C2:-0}" 'BEGIN{print (c+0>=0.99)?1:0}')" "${C2:-无}"
chk "会话 B 退出码 0" "$([ "$B_RC" = "0" ] && echo 1)" "rc=$B_RC"

echo "== 3) 被控端 journal：两次「分辨率已变」都要有 =="
J=$($SSH "journalctl --user -u uulan-host --since '-6 min' --no-pager 2>/dev/null | grep -aE '分辨率已变|分辨率变了'" 2>/dev/null | tr -d '\r')
N1=$(printf '%s\n' "$J" | grep -c "to_w=${ALT%x*}")
N2=$(printf '%s\n' "$J" | grep -c "to_w=${ORIG%x*}")
chk "被控端记录到「换成 $ALT」" "$([ "${N1:-0}" -ge 1 ] && echo 1)" "命中 $N1"
chk "被控端记录到「换回 $ORIG」" "$([ "${N2:-0}" -ge 1 ] && echo 1)" "命中 $N2"

echo "== 4) 收尾：分辨率恢复 =="
NOW=$(dim)
chk "分辨率已恢复 $ORIG" "$([ "$NOW" = "$ORIG" ] && echo 1)" "$NOW"; ORIG=""

echo
echo "结果：$pass 通过 / $fail 失败"
[ "$fail" -eq 0 ] || exit 1
exit 0