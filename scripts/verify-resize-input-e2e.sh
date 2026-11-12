#!/usr/bin/env bash
# 验「同一条会话里：被控端换分辨率 → 键鼠落点跟着换」（分辨率跟随留下的最后一格未验项）。
#
# 为什么以前验不了：`--inject` 是**独立会话**，坐标按握手时报的固定尺寸算一次算完整条
# 脚本；而「换分辨率之后落点跟不跟得上」这件事只发生在**正在看画面的那条会话**里
# （被控端 `InputLoop::sync_screen()` 在换算坐标之前把 Mapper 换成新尺寸；控制端这边
# 新增的 `--inject-live` 每一步现取画幅）。本机有全链路测试覆盖这条路
# （resize_path.rs / resize_host.rs + inject.rs 的现场注入单测），真机上一直没有**落点**证据
# —— 这条脚本补的就是那一句。
#
# 判据是同机上的 `xdotool getmouselocation`（独立裁判，不是我们自己的日志）：
#
#   P1 换代之前  move:中心(A)   → 指针落在中心(A)（换代前的基线落点）
#   ── 看到 P1 落点之后立刻把帧缓冲换成 B；换代一定发生在 P1 与 P2 之间（按日志行号核）──
#   P2 换代之后  move:中心(B)   → 落在中心(B)，**且那一行的「画幅」必须是 B**
#   P3 换代之后  movn:0.9,0.9   → 落在 0.9×(B-1)。这条只经过被控端 Mapper
#                                 （归一化直接进协议），单独钉被控端那一半
#
#   ⚠ 为什么 P2 单独不算证据（2026-11-12 第一版就是这么错的）：`move:X,Y` 两端用同一个
#   尺寸时永远落在 (X,Y) 上 —— 两端都按旧尺寸算也会「对」。所以 P2 必须与它自己那行的
#   「画幅=」一起看（控制端那一半），加上 P3（被控端那一半），两条合起来才封住两端。
#
#   外加 click:left：判据是被控端 `键鼠事件统计` 的 events 与控制端发出条数一致、
#        inject_errors=0、dropped_locked=0
#   外加 换代之后的存帧 vs 真机截图逐像素相关 ≥0.99（顺带把本轮的画面回归做掉）
#
# 采样是**事件驱动**的：脚本盯着控制端日志里的「注入换算」行，一出现就立刻 ssh 读指针位置，
# 而不是猜时间点 —— 真机上 ssh 往返约 50ms，屏幕没人动，读回来就是那一步的结果。
#
# 用法：
#   bash scripts/verify-resize-input-e2e.sh --device <ID> --password-file <文件> [--alt 1280x720]
# 环境变量：UULAN_CLIENT / UULAN_SSH / UULAN_SERVER / UULAN_DISPLAY / UULAN_ALT / UULAN_WORK
#           UULAN_SECONDS（会话时长，默认 50）/ UULAN_TOL（落点容差 px，默认 3）
# 退出码：0 = 全部断言通过；1 = 不达标；2 = 环境做不了（改不了分辨率 / 解不开锁屏 / 缺参数）
set -uo pipefail

CLIENT=${UULAN_CLIENT:-/d/uulan-bin/uulan-client.exe}
SSH=${UULAN_SSH:-ssh -o ConnectTimeout=12 kylin}
SCP=${UULAN_SCP:-scp -q}
SERVER=${UULAN_SERVER:-192.168.223.15:21116}
DISP=${UULAN_DISPLAY:-:0}
WORK=${UULAN_WORK:-/d/uulan-bin}
ALT=${UULAN_ALT:-1280x720}
HERE=$(cd "$(dirname "$0")" && pwd)
SECS=${UULAN_SECONDS:-50}
TOL=${UULAN_TOL:-3}
# 时间线（毫秒）：P1 在 4s、P2 在 14s、P3 在 17.5s、点击在 19s。
# P1→P2 之间那 10 秒是给「编排方改帧缓冲 + 被控端 1 秒探针 + 编码器重启 + VideoCfg 回来」用的。
SCRIPT_TMPL='wait:4000; move:PA; wait:10000; move:PB; wait:3500; movn:0.9,0.9; wait:1500; click:left; wait:1000'

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

# isnear "gx gy" "x y" [容差] → 1/0（读不到位置一律 0，绝不把「没采到样」当成通过）
isnear() {
  local got=$1 want=$2 tol=${3:-$TOL}
  if [ -z "$got" ] || [ -z "$want" ]; then echo 0; return; fi
  set -- $got; local a=$1 b=$2
  set -- $want; local x=$1 y=$2
  local dx=$((a - x)); local dy=$((b - y))
  if [ "${dx#-}" -le "$tol" ] && [ "${dy#-}" -le "$tol" ]; then echo 1; else echo 0; fi
}
# isfar "gx gy" "x y" "最小距离" → 1/0（反面对照：不许落在「按旧尺寸 A 算」的那个点上）
isfar() {
  local got=$1 want=$2 min=${3:-100}
  if [ -z "$got" ] || [ -z "$want" ]; then echo 0; return; fi
  set -- $got; local a=$1 b=$2
  set -- $want; local x=$1 y=$2
  local dx=$((a - x)); local dy=$((b - y))
  if [ $((dx * dx + dy * dy)) -gt $((min * min)) ]; then echo 1; else echo 0; fi
}

# 原生程序只认 C:/ D:/ 形式，MSYS 工具反过来只认 /d/ —— 两套都备着。
PWN=$(cygpath -m "$PWF" 2>/dev/null || echo "$PWF")
WORKN=$(cygpath -m "$WORK" 2>/dev/null || echo "$WORK")
HEREN=$(cygpath -m "$HERE" 2>/dev/null || echo "$HERE")

ORIG=""; CLI_PID=""; UNLOCKED=0
LOG="$WORK/ri-cli.log"

cleanup() {
  [ -n "${CLI_PID:-}" ] && kill "$CLI_PID" 2>/dev/null
  # 分辨率一定要恢复：这是用户真实桌面。这里**不**清 ORIG，恢复失败时下面第 5 节还会再试并如实报错。
  if [ -n "$ORIG" ]; then
    $SSH "DISPLAY=$DISP xrandr --fb $ORIG" >/dev/null 2>&1
  fi
  # 屏保策略要还原：验期间关掉只是这几分钟，不能把人家的桌面永久设成不锁屏。
  if [ "${UNLOCKED:-0}" = "1" ]; then
    $SSH "gsettings set org.ukui.screensaver idle-activation-enabled true" >/dev/null 2>&1
  fi
}
trap cleanup EXIT

dim() { $SSH "DISPLAY=$DISP xdpyinfo 2>/dev/null | awk '/dimensions:/{print \$2}'" 2>/dev/null | tr -d '\r' | tail -1; }
set_fb() { $SSH "DISPLAY=$DISP xrandr --fb $1 >/dev/null 2>&1; sleep 1.2; DISPLAY=$DISP xdpyinfo 2>/dev/null | awk '/dimensions:/{print \$2}'" 2>/dev/null | tr -d '\r' | tail -1; }
pointer() { $SSH "DISPLAY=$DISP xdotool getmouselocation" 2>/dev/null | tr -d '\r' | tail -1 | sed -E 's/.*x:([0-9]+) y:([0-9]+).*/\1 \2/'; }
ppm_size() { python -c "
import sys
f=open(sys.argv[1],'rb')
assert f.readline().strip()==b'P6'
l=f.readline().split()
while len(l)<2: l+=f.readline().split()
print('%sx%s'%(l[0].decode(),l[1].decode()))" "$1" 2>/dev/null | tr -d '\r'; }
stat_line() { tr -d '\r' < "$1" | grep -aE "^ *$2" | tail -1; }

echo "== 0) 前置：分辨率 / 锁屏 / 改得动帧缓冲 =="
ORIG=$(dim)
[ -n "$ORIG" ] || { echo "FAIL 连不上真机或读不到分辨率（$DISP）"; exit 2; }
if [ "$ALT" = "$ORIG" ]; then ALT="1024x768"; [ "$ALT" = "$ORIG" ] && ALT="800x600"; fi
ok "读到现在分辨率 $ORIG（备选 $ALT）"

FOC=$($SSH "DISPLAY=$DISP xdotool getwindowfocus getwindowname" 2>/dev/null | tr -d '\r' | tail -1)
case "$FOC" in
  *-screensaver-dialog|*ukui-screensaver*)
    # 锁屏就自己解（用户已授权）：屏保进程与 ssh 会话同属 mis，不需要 root。
    # 取 PID 再 kill，不用 pkill -x —— comm 被截断成 15 字符时 -x 会落空（2026-10-31 踩过）。
    PIDS=$($SSH "ps -u mis -o pid,args" 2>/dev/null | tr -d '\r' | grep screensaver | awk '{print $1}' | tr '\n' ' ')
    for p in $PIDS; do $SSH "kill $p" >/dev/null 2>&1; done
    $SSH "gsettings set org.ukui.screensaver idle-activation-enabled false" >/dev/null 2>&1
    UNLOCKED=1
    sleep 2
    FOC=$($SSH "DISPLAY=$DISP xdotool getwindowfocus getwindowname" 2>/dev/null | tr -d '\r' | tail -1)
    ;;
esac
case "$FOC" in
  ""|*screensaver*) echo "  SKIP 焦点还是屏保（$FOC）—— 这条验要能注入键鼠，锁屏时按策略不发"
                    ORIG=""; exit 2 ;;
  *) ok "可注入：焦点窗口「$FOC」不是屏保" ;;
esac

PROBE=$(set_fb "$ALT")
if [ "$PROBE" != "$ALT" ]; then
  echo "  SKIP 这台机器的 xrandr --fb 改不动（拿到 $PROBE）—— 换代落点无法在它上面验"
  ORIG=""; exit 2
fi
BACK=$(set_fb "$ORIG")
[ "$BACK" = "$ORIG" ] && ok "xrandr --fb 可改可恢复（$ORIG ↔ $ALT）" || { bad "改回原分辨率失败（$BACK）"; exit 1; }

# 中心点：A 与 B 必须不一样，否则「落在中心」分不出用的哪个尺寸。
AC=$(( ${ORIG%x*} / 2 )); AH=$(( ${ORIG#*x} / 2 ))
BC=$(( ${ALT%x*} / 2 )); BH=$(( ${ALT#*x} / 2 ))
if [ "$AC" = "$BC" ] && [ "$AH" = "$BH" ]; then echo "  SKIP A/B 中心相同（$ORIG vs $ALT）"; exit 2; fi
N9W=$(( ( ${ALT%x*} - 1 ) * 9 / 10 )); N9H=$(( ( ${ALT#*x} - 1 ) * 9 / 10 ))
SCRIPT=$(printf '%s' "$SCRIPT_TMPL" | sed "s/PA/$AC,$AH/; s/PB/$BC,$BH/")
echo "  注入脚本：$SCRIPT"
echo "  期望落点：P1=$AC,$AH（A 中心） P2=$BC,$BH（B 中心） P3=$N9W,$N9H（0.9×(B-1)）"

# 把指针先挪到角落，免得「落在中心」这条断言其实是在原地踏步。
$SSH "DISPLAY=$DISP xdotool mousemove 120 90" >/dev/null 2>&1
echo "  起始指针：$(pointer)"

echo "== 1) 起会话（画面 + 现场注入同时跑）=="
: > "$LOG"
( NO_COLOR=1 RUST_LOG=info "$CLIENT" --server "$SERVER" --device "$DEV" --password-file "$PWN" \
    --headless --seconds "$SECS" --no-clipboard --inject-live "$SCRIPT" \
    --save-frame "$WORKN/uulan-ri-frame.ppm" > "$LOG" 2>&1
  echo $? > "$LOG.rc" ) &
CLI_PID=$!
for _ in $(seq 1 60); do grep -qa '^第 [0-9]* 帧' "$LOG" 2>/dev/null && break; sleep 0.5; done
grep -qa '^第 [0-9]* 帧' "$LOG" 2>/dev/null || { echo "FAIL 没出画面（见 $LOG）"; exit 1; }
ok "会话已出画面"

# 事件驱动采样：盯「注入换算」行。**看到 P1 落点之后才改帧缓冲**，
# 于是换代必然发生在 P1 之后、P2 之前（下面按日志行号再核一遍，不靠约定）。
seen=0; S1=""; S2=""; S3=""; RESIZE_GOT=""
END=$(( $(date +%s) + SECS + 25 ))
while [ "$(date +%s)" -lt "$END" ]; do
  N=$(tr -d '\r' < "$LOG" 2>/dev/null | grep -ac '注入换算 :')
  if [ "${N:-0}" -gt "$seen" ]; then
    seen=$N
    sleep 0.35
    P=$(pointer)
    case "$seen" in
      1) S1="$P"
         RESIZE_GOT=$(set_fb "$ALT")
         echo "  （P1 已落点，现在把帧缓冲换成 $ALT → xdpyinfo 读到 $RESIZE_GOT）" ;;
      2) S2="$P" ;;
      3) S3="$P" ;;
    esac
    echo "  采样 #$seen：指针=[$P]   ← $(tr -d '\r' < "$LOG" | grep -a '注入换算 :' | tail -1)"
  fi
  if [ "$seen" -ge 3 ]; then
    while kill -0 "$CLI_PID" 2>/dev/null; do sleep 1; done
    break
  fi
  kill -0 "$CLI_PID" 2>/dev/null || break
  sleep 0.25
done
wait "$CLI_PID" 2>/dev/null; CLI_PID=""
RC=$(tr -d '\r' < "$LOG.rc" 2>/dev/null)
echo "  会话收工（rc=$RC）"

echo "== 2) 断言：落点 =="
chk "P1（换代前 move:$AC,$AH）落在 A 的中心" "$(isnear "$S1" "$AC $AH")" "[$S1] 期望 $AC,$AH"
chk "P2（换代后 move:$BC,$BH）落在 B 的中心" "$(isnear "$S2" "$BC $BH")" "[$S2] 期望 $BC,$BH"
# 反面对照：被控端 Mapper 没跟上（还按 A 算）时，P2 会落在 A 的中心附近。
chk "P2 不是「被控端按旧尺寸 A 算」的结果（离 A 中心 >100px）" "$(isfar "$S2" "$AC $AH" 100)" "[$S2] A 中心 $AC,$AH"
# 控制端那一半的独立证据：P2 那一行自己报的画幅必须是 B（同尺寸时 move 落点分不出新旧）。
SZS=$(tr -d '\r' < "$LOG" | grep -a '注入换算 :' | sed -E 's/.*画幅=([0-9]+x[0-9]+).*/\1/' | tr '\n' ' ')
chk "三条「注入换算」用的画幅是 $ORIG → $ALT → $ALT" \
    "$([ "$SZS" = "$ORIG $ALT $ALT " ] && echo 1)" "[$SZS]"
P1LINE=$(tr -d '\r' < "$LOG" | grep -an '注入换算 :' | sed -n 1p | cut -d: -f1)
P2LINE=$(tr -d '\r' < "$LOG" | grep -an '注入换算 :' | sed -n 2p | cut -d: -f1)
RBLINE=$(tr -d '\r' < "$LOG" | grep -an '解码器已重建' | sed -n 1p | cut -d: -f1)
chk "换代确实发生在 P1 与 P2 之间（日志行号 $P1LINE < ${RBLINE:-?} < $P2LINE）" \
    "$([ -n "$P1LINE" ] && [ -n "$P2LINE" ] && [ -n "$RBLINE" ] && [ "$RBLINE" -gt "$P1LINE" ] && [ "$RBLINE" -lt "$P2LINE" ] && echo 1)" \
    "P1=$P1LINE 重建=$RBLINE P2=$P2LINE"
chk "P3（换代后 movn:0.9,0.9）落在 0.9×(B-1)=$N9W,$N9H —— 只经过被控端 Mapper" \
    "$(isnear "$S3" "$N9W $N9H")" "[$S3] 期望 $N9W,$N9H"
chk "控制端确实重建过解码器（$ALT）" \
    "$(tr -d '\r' < "$LOG" | grep -aq "解码器已重建 screen_w=${ALT%x*} screen_h=${ALT#*x}" && echo 1)" "无该行"
chk "收工画幅 = $ALT" \
    "$([ "$(stat_line "$LOG" '当前画幅' | grep -oE '[0-9]+x[0-9]+' | head -1)" = "$ALT" ] && echo 1)" \
    "$(stat_line "$LOG" '当前画幅')"
chk "收工统计里有「现场注入」且发送数 > 0" \
    "$(stat_line "$LOG" '现场注入 *:' | grep -aqE '发送 [1-9][0-9]* 条' && echo 1)" "$(stat_line "$LOG" '现场注入 *:')"
chk "会话没断（建立 1 条 / 接回 0 次）" \
    "$(stat_line "$LOG" '会话 *:' | grep -aq '建立 1 条 / 断线接回 0 次' && echo 1)" "$(stat_line "$LOG" '会话 *:')"
chk "退出码 0" "$([ "$RC" = "0" ] && echo 1)" "rc=$RC"

echo "== 3) 断言：被控端那一侧 =="
sleep 3
J=$($SSH "journalctl --user -u uulan-host --since '-5 min' --no-pager -o cat 2>/dev/null" 2>/dev/null | tr -d '\r' | sed -E 's/\x1b\[[0-9;]*m//g')
EV=$(printf '%s\n' "$J" | grep -a '键鼠事件统计' | tail -1)
EVN=$(printf '%s' "$EV" | sed -E 's/.*events=([0-9]+).*/\1/')
SENT=$(tr -d '\r' < "$LOG" | grep -a '已发送' | head -1 | sed -E 's/.*已发送 ([0-9]+) 条.*/\1/')
# 2026-11-12 第一版在这里写了「events ≥ 12」——那是猜的：一条 move 就是 1 个事件，
# 三条移动 + 一次点击（按下+松开）= 5，被控端如实报了 5。判据改成「与控制端发出条数相等」。
chk "被控端收到的事件数 = 控制端发出的条数（sent=$SENT events=${EVN:-?}）" \
    "$([ -n "$SENT" ] && [ "$EVN" = "$SENT" ] && echo 1)" "events=$EVN sent=$SENT"
chk "被控端注入零错误（inject_errors=0）" "$(printf '%s' "$EV" | grep -aq 'inject_errors=0' && echo 1)" "$EV"
chk "锁屏没丢键鼠（dropped_locked=0）" "$(printf '%s' "$EV" | grep -aq 'dropped_locked=0' && echo 1)" "$EV"
chk "被控端记录到「换成 $ALT」" "$(printf '%s\n' "$J" | grep -aq "to_w=${ALT%x*}" && echo 1)" "journal 没命中"

echo "== 4) 换代后的画面与真机截图逐像素比对 =="
SHOT=$($SSH "DISPLAY=$DISP ffmpeg -loglevel error -f x11grab -video_size $ALT -i $DISP -frames:v 1 -y /tmp/uulan-ri-shot.ppm && echo SHOT_OK" 2>&1 | tr -d '\r' | tail -1)
echo "  截图（$ALT）：$SHOT"
$SCP "kylin:/tmp/uulan-ri-shot.ppm" "$WORK/uulan-ri-shot.ppm" >/dev/null 2>&1
FS=$(ppm_size "$WORKN/uulan-ri-frame.ppm")
chk "存帧尺寸 = $ALT" "$([ "$FS" = "$ALT" ] && echo 1)" "$FS"
CORR=$(python "$HEREN/compare-ppm.py" "$WORKN/uulan-ri-frame.ppm" "$WORKN/uulan-ri-shot.ppm" 2>/dev/null | tr -d '\r')
C1=$(printf '%s' "$CORR" | awk '/相关系数/{print $3}')
chk "相关系数 ≥ 0.99（$C1）" "$(awk -v c="${C1:-0}" 'BEGIN{print (c+0>=0.99)?1:0}')" "${C1:-无}"
printf '  比对原文：%s\n' "$(printf '%s' "$CORR" | tr '\n' ' ')"
$SSH "rm -f /tmp/uulan-ri-shot.ppm" >/dev/null 2>&1

echo "== 5) 收尾：分辨率与屏保策略恢复 =="
# 先真恢复，再断言恢复了什么（第一版把恢复留在 EXIT trap 里，于是这一条必然读到旧值）。
NOW=$(set_fb "$ORIG")
chk "分辨率已恢复 $ORIG" "$([ "$NOW" = "$ORIG" ] && echo 1)" "$NOW"
[ "$NOW" = "$ORIG" ] && ORIG=""   # 恢复成功才清，失败时 trap 还会再兜一次
if [ "$UNLOCKED" = "1" ]; then
  $SSH "gsettings set org.ukui.screensaver idle-activation-enabled true" >/dev/null 2>&1
  GS=$($SSH "gsettings get org.ukui.screensaver idle-activation-enabled" 2>/dev/null | tr -d '\r' | tail -1)
  chk "屏保策略已改回（$GS）" "$([ "$GS" = "true" ] && echo 1)" "$GS"
fi

echo
echo "结果：$pass 通过 / $fail 失败"
[ "$fail" -eq 0 ] || exit 1
exit 0
