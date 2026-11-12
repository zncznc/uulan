#!/usr/bin/env bash
# 验「窗口在解码器换代那一刻有没有黑屏一拍」（M2.5 遗留格，真机 + 真服务，无人值守可跑）。
#
# 为什么以前算未验：这条是「观感」，帧计数（painted）看不出一拍全黑 —— 黑帧也是被计进去的。
# 现在的办法把观感变成机器可查：
#   1. Windows 侧用 tools/win_window_capture.py 以 20 ms 分辨率 PrintWindow 抓控制端窗口客户区；
#   2. 真机上 `xrandr --fb 800x600` 造一次换代（比窗口小 → fit_rect 从不缩小 → 黑边占比必须跳变，
#      这一跳同时证明「抓取通路是活的」与「窗口真的跟着远端画幅走」）；
#   3. tools/analyze_window_blackframe.py 判「黑闪 run」：连续采样点全黑且前后正常 —— 33 ms 的
#      一拍黑必然落进 20 ms 的采样窗口；
#   4. 恢复原分辨率，黑边必须落回基线。
#
# 断言（12 条）：旧控制端进程数 0 / fb 改得动 / 抓到 ≥150 帧 / 基线亮度有效 / 黑边跳变（活性）/
#   恢复回落 / 黑闪 0 / 「窗口已就绪」恰好 1 次 / 换代重建日志 800x600 方向 / 原尺寸方向 /
#   会话没掉线（painted≥300 且无「不再重连」）/ 退出码 0 且分辨率已恢复。
#
# 用法：
#   bash scripts/verify-window-blackframe-e2e.sh --device <ID> --password-file <文件>
# 环境变量：UULAN_CLIENT / UULAN_SSH / UULAN_SERVER / UULAN_DISPLAY / UULAN_ALT / UULAN_WORK / UULAN_PY
# 退出码：0 全过；1 有断言失败（含「确实存在黑闪」——那是真回归）；2 环境做不了（抓不到窗口/改不动 fb），
#   此时**不得**据此判定功能好坏。
set -uo pipefail

CLIENT=${UULAN_CLIENT:-/d/uulan-bin/uulan-client.exe}
SSH=${UULAN_SSH:-ssh -o ConnectTimeout=12 kylin}
SERVER=${UULAN_SERVER:-192.168.223.15:21116}
DISP=${UULAN_DISPLAY:-:0}
ALT=${UULAN_ALT:-800x600}
WORK=${UULAN_WORK:-/d/uulan-bin}
PY=${UULAN_PY:-python}
SECS=${UULAN_SECONDS:-46}
HERE=$(cd "$(dirname "$0")/.." && pwd)
TS=$(date +%Y%m%d_%H%M%S)

DEV=""; PWF=""
while [ $# -gt 0 ]; do
  case "$1" in
    --device) DEV=$2; shift 2 ;;
    --password-file) PWF=$2; shift 2 ;;
    *) echo "未知参数：$1" 2>&1; exit 2 ;;
  esac
done
DEV=${DEV:-${UULAN_DEVICE:-}}
[ -n "$DEV" ] || { echo "FAIL 需要 --device <设备 ID>"; exit 2; }
[ -n "$PWF" ] && [ -s "$PWF" ] || { echo "FAIL 需要 --password-file <文件>"; exit 2; }
[ -x "$CLIENT" ] || { echo "FAIL 找不到控制端 $CLIENT（先 cargo build --release，再拷出工作区）"; exit 2; }

pass=0; fail=0; envfail=0
ok()  { pass=$((pass+1)); echo "  PASS $1"; }
bad() { fail=$((fail+1)); echo "  FAIL $1"; }
chk() { if [ "$2" = "1" ]; then ok "$1"; else bad "$1（实际：$3）"; fi; }
env() { envfail=$((envfail+1)); echo "  ENV  $1"; }

# 原生程序只认 C:/ D:/；MSYS 只认 /c/ /d/ —— 两套都备着。
PWN=$(cygpath -m "$PWF" 2>/dev/null || echo "$PWF")
HEREN=$(cygpath -m "$HERE" 2>/dev/null || echo "$HERE")   # python 只认 D:/... 形式的脚本路径
WORKN=$(cygpath -m "$WORK" 2>/dev/null || echo "$WORK")
CSVN="$WORKN/bf-$TS.csv"; CSV=$WORK/bf-$TS.csv
EVN="$WORKN/bf-$TS.ev"; EV=$WORK/bf-$TS.ev
LOG=$WORK/bf-cli-$TS.log
CAPLOG=$WORK/bf-cap-$TS.log
ANLOG=$WORK/bf-an-$TS.log

ORIG=""; CLI_PID=""; CAP_PID=""
cleanup() {
  [ -n "${CAP_PID:-}" ] && kill "$CAP_PID" 2>/dev/null
  if [ -n "${CLI_PID:-}" ] && kill -0 "$CLI_PID" 2>/dev/null; then
    taskkill /PID "$CLI_PID" >/dev/null 2>&1 || true
  fi
  # 用户真实桌面：分辨率一定要恢复。
  [ -n "$ORIG" ] && $SSH "DISPLAY=$DISP xrandr --fb $ORIG" >/dev/null 2>&1
}
trap cleanup EXIT

dim() { $SSH "DISPLAY=$DISP xdpyinfo 2>/dev/null | awk '/dimensions:/{print \$2}'" 2>/dev/null | tr -d '\r' | tail -1; }
set_fb() { $SSH "DISPLAY=$DISP xrandr --fb $1 >/dev/null 2>&1; sleep 0.8; DISPLAY=$DISP xdpyinfo 2>/dev/null | awk '/dimensions:/{print \$2}'" 2>/dev/null | tr -d '\r' | tail -1; }
strip() { sed -E 's/\x1b\[[0-9;]*m//g' "$1" | tr -d '\r'; }

# 0. 别留旧控制端进程（窗口标题匹配会撞车）
OLD=$(tasklist 2>/dev/null | grep -ciE '^uulan-client' || true)
[ "$OLD" = "0" ] || { env "已有 $OLD 个 uulan-client 进程在跑，先收掉再验"; exit 2; }

# 1. 探当前分辨率；ALT 必须比它小（黑边几何才成立）
ORIG=$(dim)
[ -n "$ORIG" ] || { env "读不到真机分辨率"; exit 2; }
OW=${ORIG%x*}; OH=${ORIG#*x}; AW=${ALT%x*}; AH=${ALT#*x}
if [ "$AW" -lt "$OW" ] && [ "$AH" -lt "$OH" ]; then ok "真机 $ORIG，ALT $ALT 比它小（黑边判据可用）"; else env "ALT $ALT 不比 $ORIG 小，判据几何不成立"; exit 2; fi
PROBE=$(set_fb "$ALT"); if [ "$PROBE" = "$ALT" ]; then ok "xrandr --fb 改得动（先试改 $ALT 成功）"; set_fb "$ORIG" >/dev/null; else env "这台机器 xrandr --fb 改不动（拿到 $PROBE）"; exit 2; fi

# 2. 起窗口模式控制端（不 --headless、不 --save-frame；换代期间画面照常流）
: > "$EV"
RUST_LOG=info NO_COLOR=1 "$CLIENT" --server "$SERVER" --device "$DEV" --password-file "$PWN" \
  --seconds "$SECS" > "$LOG" 2>&1 &
CLI_PID=$!
READY=0
for _ in $(seq 1 30); do grep -aq "窗口已就绪" "$LOG" && { READY=1; break; }; sleep 0.5; done
if [ "$READY" = "1" ]; then ok "窗口起来了（日志出现「窗口已就绪」）"; else bad "15 秒没等到「窗口已就绪」（见 $LOG）"; exit 1; fi
sleep 3   # 首帧与几秒稳定画面

# 3. 开抓（20 ms 分辨率，34 秒覆盖两次换代）
$PY "$HEREN/tools/win_window_capture.py" --out "$CSVN" --seconds 34 --interval-ms 20 --match uulan \
  > "$CAPLOG" 2>&1 &
CAP_PID=$!
sleep 6
echo "shrink $(date +%s%3N)" >> "$EV"
G=$(set_fb "$ALT")
[ "$G" = "$ALT" ] && ok "第一次换代生效（$G）" || bad "第一次换代没生效（$G）"
sleep 13
echo "restore $(date +%s%3N)" >> "$EV"
G=$(set_fb "$ORIG")
[ "$G" = "$ORIG" ] && ok "第二次换代生效并恢复（$G）" || bad "恢复原分辨率失败（$G）"

# 4. 等抓取与控制端各自收工
wait "$CAP_PID"; CAPRC=$?
CAPN=$(grep -oE 'n=[0-9]+' "$CAPLOG" | head -1 | cut -d= -f2)
CAPN=${CAPN:-0}
[ "$CAPRC" = "2" ] && { env "抓不到窗口：$(cat "$CAPLOG")"; exit 2; }
if [ "$CAPN" -ge 150 ]; then ok "窗口抓到 $CAPN 帧（≥150，20 ms 采样在跑）"; else bad "只抓到 $CAPN 帧"; fi
for _ in $(seq 1 80); do kill -0 "$CLI_PID" 2>/dev/null || break; sleep 0.5; done
kill -0 "$CLI_PID" 2>/dev/null && { bad "控制端过了 --seconds 还没退出"; } 
wait "$CLI_PID" 2>/dev/null; CLIRC=$?

# 5. 判读 CSV
$PY "$HEREN/tools/analyze_window_blackframe.py" --csv "$CSVN" --events "$EVN" > "$ANLOG" 2>&1
ANRC=$?
BL=$(grep -c 'RESULT baseline=PASS' "$ANLOG" || true)
LV=$(grep -c 'RESULT liveness=PASS' "$ANLOG" || true)
RS=$(grep -c 'RESULT restore=PASS' "$ANLOG" || true)
FL=$(grep -c 'RESULT flash=PASS' "$ANLOG" || true)
if [ "$ANRC" = "2" ]; then env "判读器按环境不成立收场：$(tr -d '\r' < "$ANLOG" | tail -2)"; exit 2; fi
chk "基线亮度有效（抓取没被挡）" "$BL" "$(grep 'RESULT baseline' "$ANLOG" | head -1)"
chk "黑边随换代跳变（活性判据，窗口确实在跟远端走）" "$LV" "$(grep 'RESULT liveness' "$ANLOG" | head -1)"
chk "恢复后黑边回落基线" "$RS" "$(grep 'RESULT restore' "$ANLOG" | head -1)"
chk "黑屏 run 计数为 0（控制端空白；对面直播来的真黑帧单列 INFO）" "$FL" "$(grep 'RESULT flash' "$ANLOG" | head -1)"

# 6. 日志侧的换代与会话完整性
S=$(strip "$LOG")
N_READY=$(printf '%s\n' "$S" | grep -ac "窗口已就绪")
[ "$N_READY" = "1" ] && ok "「窗口已就绪」恰好 1 次（窗口没重建）" || bad "「窗口已就绪」出现 $N_READY 次"
chk "解码器按 800x600 重建过" "$(printf '%s\n' "$S" | grep -acq "解码器已重建 screen_w=$AW screen_h=$AH" && echo 1)" "无"
chk "解码器按 $ORIG 重建回去" "$(printf '%s\n' "$S" | grep -acq "解码器已重建 screen_w=$OW screen_h=$OH" && echo 1)" "无"
PAINTED=$(printf '%s\n' "$S" | grep -oE 'painted=[0-9]+' | tail -1 | cut -d= -f2); PAINTED=${PAINTED:-0}
if [ "$PAINTED" -ge 300 ]; then ok "窗口呈现 painted=$PAINTED（≥300）"; else bad "painted=$PAINTED 太少"; fi
chk "会话没掉线重连（没有「不再重连」）" "$(printf '%s\n' "$S" | grep -acq "不再重连" || echo 1)" "命中"
[ "$CLIRC" = "0" ] && ok "控制端退出码 0" || bad "控制端退出码 $CLIRC"

echo "----"
echo "断言：$pass 通过 / $fail 失败 / $envfail 环境；证据：$CSV $EV $LOG $ANLOG"
[ "$fail" = "0" ] && [ "$envfail" = "0" ] || exit 1
exit 0
