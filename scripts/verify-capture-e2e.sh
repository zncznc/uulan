#!/usr/bin/env bash
# 验「鼠标捕获模式」端到端：Windows 控制端真窗口 + 真设备输入流 + 真机 Xvfb 被控端。
#
# 与 verify-input-app-e2e.sh 的分工：那条验的是 inject DSL（独立会话、按脚本步骤上线）；
# 本脚本验的是**窗口事件循环里的捕获路径**：
#   SendInput(相对移动) → Windows raw input → winit DeviceEvent::MouseMotion
#   → CaptureCtl（亚像素累积 + ±10000 拆分）→ C2H::MouseMoveRel（原样上线，不掺画幅换算）
#   → 被控端 mapper → XTestFakeRelativeMotionEvent → xdotool 独立判据。
# 注入的相对移动与硬件同源进 raw input（winit 不按 LLMHF_INJECTED 过滤）；
# 「硬件 vs 注入」的差异不在本判据范围，如实写进归档。
#
# 判据里最硬的一条是被控端 `events=3` **恰好等于**注入次数：
# 抓握时 winit 会 SetCursorPos 制造一条 WM_MOUSEMOVE —— 捕获模式下必须被守卫丢掉；
# 漏任何一条绝对坐标，对面指针就会跳到映射位置、事件数也会多出来。
#
# 用法：bash scripts/verify-capture-e2e.sh
# 环境变量：UULAN_CLIENT（本地控制端 exe）/ UULAN_SSH / UULAN_APP_PORT / UULAN_PYTHON
#           （UULAN_PYTHON 必须是**原生** Windows python：注入器路径已换好 D:/ 形式）
# 退出码：0 = 全部判据通过；1 = 有判据不达标
set -uo pipefail

SSH=${UULAN_SSH:-ssh -o ConnectTimeout=12 kylin}
SCP=${UULAN_SCP:-scp -q}
CLIENT=${UULAN_CLIENT:-/d/uulan-bin/uulan-client.exe}
PYTHON=${UULAN_PYTHON:-python}
SERVER=${UULAN_SERVER:-192.168.223.15}
PORT=${UULAN_APP_PORT:-21995}
SECS=${UULAN_CAPTURE_SECS:-18}
KEEP=${UULAN_KEEP_TESTBED:-0}
HERE=$(cd "$(dirname "$0")" && pwd)

pass=0; fail=0
ok()  { pass=$((pass+1)); echo "  PASS $1"; }
bad() { fail=$((fail+1)); echo "  FAIL $1"; }

tmpms=${TMPDIR:-/tmp}
mkdir -p "$tmpms"
tmpwin=$(cygpath -m "$tmpms" 2>/dev/null || echo "$tmpms")
PW="$tmpwin/uulan-capture-pw.txt"
LOG="$tmpwin/uulan-capture-client.log"
CLPID=""

cleanup() {
  rm -f "$PW"
  [ -z "$CLPID" ] || kill "$CLPID" 2>/dev/null
  [ "$KEEP" = "1" ] || $SSH "bash /tmp/remote-xvfb-testbed.sh down" >/dev/null 2>&1
}
trap cleanup EXIT

clean() { sed -E 's/\x1b\[[0-9;]*m//g' "$1" | tr -d '\r'; }   # 去 ANSI 副本再 grep（本仓库老坑）

echo "== 0) 参数闸门反例（不碰真机）：--capture 必须死在不开窗口的组合上 =="
"$CLIENT" --server "$SERVER:$PORT" --device 123456789 --password abcdefg1 \
  --capture --headless > "$tmpwin/gate1.out" 2>&1
R1=$?
"$CLIENT" --server "$SERVER:$PORT" --capture --devices > "$tmpwin/gate2.out" 2>&1
R2=$?
if [ "$R1" != "0" ] && grep -q -- "--capture" "$tmpwin/gate1.out"; then
  ok "--capture + --headless 当场报错（退出码 $R1，文案点名 --capture）"
else
  bad "--capture + --headless 没拦住（rc=$R1）：$(head -1 "$tmpwin/gate1.out")"
fi
if [ "$R2" != "0" ] && grep -q -- "--capture" "$tmpwin/gate2.out"; then
  ok "--capture + --devices 当场报错（退出码 $R2 —— 这条是提前返回分支，闸门必须在门口）"
else
  bad "--capture + --devices 没拦住（rc=$R2）：$(head -1 "$tmpwin/gate2.out")"
fi

echo "== 1) 真机上起 Xvfb :99 测试台（独立 relay/host，端口 $PORT）=="
[ -x "$CLIENT" ] || { echo "FAIL 找不到控制端 $CLIENT"; exit 1; }
$SCP "$HERE/remote-xvfb-testbed.sh" "kylin:/tmp/remote-xvfb-testbed.sh" || { echo "FAIL scp 测试台脚本失败"; exit 1; }
UP=$($SSH "bash /tmp/remote-xvfb-testbed.sh up" 2>&1)
echo "$UP" | sed 's/^/  /'
DEV=$(printf '%s\n' "$UP" | awk '/^DEVICE /{print $2}' | tail -1)
[ -n "${DEV:-}" ] || { echo "FAIL 测试台没起来（没拿到设备 ID）"; exit 1; }
$SSH "python3 -c \"import json;print(json.load(open('/tmp/uulan-input-app/host.json'))['password'])\"" \
  2>/dev/null | tr -d '\r' | tail -1 > "$PW"
[ -s "$PW" ] || { echo "FAIL 没拿到口令"; exit 1; }

echo "== 2) 独立判据基线：把 :99 指针钉到 (500,400) =="
$SSH 'DISPLAY=:99 xdotool mousemove 500 400' >/dev/null 2>&1
P0=$($SSH 'DISPLAY=:99 xdotool getmouselocation' 2>/dev/null | tr -d '\r' | tail -1)
X0=$(printf '%s' "$P0" | grep -oE 'x:[0-9]+' | tr -dc '0-9')
Y0=$(printf '%s' "$P0" | grep -oE 'y:[0-9]+' | tr -dc '0-9')
if [ "${X0:-0}" = "500" ] && [ "${Y0:-0}" = "400" ]; then
  ok "判据指针基线 ($X0,$Y0)"
else
  bad "基线没摆对：$P0"; exit 1
fi

echo "== 3) 窗口模式 + --capture 起控制端（$SECS 秒，日志 $LOG）=="
RUST_LOG=info NO_COLOR=1 "$CLIENT" --server "$SERVER:$PORT" --device "$DEV" \
  --password-file "$PW" --capture --seconds "$SECS" > "$LOG" 2>&1 &
CLPID=$!
CAP=0
for _ in $(seq 1 25); do
  if grep -q "捕获模式已开启" <(clean "$LOG"); then CAP=1; break; fi
  kill -0 "$CLPID" 2>/dev/null || break
  sleep 0.4
done
if [ "$CAP" = "1" ]; then
  ok "捕获模式真的开了（日志有「捕获模式已开启」= 抓握成功，不是带病运行）"
else
  bad "10 秒内没等到「捕获模式已开启」——抓握失败或窗口没起来"
  clean "$LOG" | tail -8 | sed 's/^/    /'
  wait "$CLPID" 2>/dev/null; exit 1
fi
sleep 2   # 会话彻底就绪（首帧、SystemState 都到齐）再注入

echo "== 4) SendInput 注入 3×(80,-50)（与硬件同源、走真实 OS 输入队列）=="
# UULAN_PYTHON 是**原生** Windows python —— 它只认 D:/... 不认 /d/...（本仓库坑 4，第 N 次踩）。
SEND_PY="$HERE/capture-sendinput.py"
SEND_PY_NATIVE=$(cygpath -m "$SEND_PY" 2>/dev/null || echo "$SEND_PY")
INJ=$($PYTHON "$SEND_PY_NATIVE" 80 -50 3 0.4 2>&1)
INJ_RC=$?
echo "  $INJ"
if [ "$INJ_RC" != "0" ]; then
  bad "注入器没跑成（rc=$INJ_RC）—— 后面三条判据无效，整轮作废重跑"
  wait "$CLPID" 2>/dev/null; CLPID=""
  exit 1
fi
sleep 1.5
P1=$($SSH 'DISPLAY=:99 xdotool getmouselocation' 2>/dev/null | tr -d '\r' | tail -1)
X1=$(printf '%s' "$P1" | grep -oE 'x:[0-9]+' | tr -dc '0-9')
Y1=$(printf '%s' "$P1" | grep -oE 'y:[0-9]+' | tr -dc '0-9')
echo "  指针独立判据：$P0 → $P1"
if [ -n "${X1:-}" ] && [ $((X1 - X0)) = 240 ] && [ $((Y1 - Y0)) = -150 ]; then
  ok "捕获移动原样到 X：增量 ($((X1-X0)),$((Y1-Y0))) = 3×(80,-50)，**一字不差也没有跳点**"
else
  bad "指针落点不对：(${X0:-?},${Y0:-?})→(${X1:-?},${Y1:-?})，期望增量 (240,-150)"
fi

echo "== 5) 控制端收尾 =="
wait "$CLPID"; RC=$?; CLPID=""
[ "$RC" = "0" ] && ok "控制端退出码 0（窗口自己到点收工）" || bad "控制端退出码 $RC"
PAINTED=$(clean "$LOG" | grep -oE 'painted=[0-9]+' | grep -oE '[0-9]+' | tail -1)
SENT=$(clean "$LOG" | grep -oE 'input_sent=[0-9]+' | grep -oE '[0-9]+' | tail -1)
if [ "${PAINTED:-0}" -gt 0 ]; then ok "窗口真的画了帧（painted=$PAINTED）"; else bad "painted 为空/0 —— 画面根本没进窗口"; fi
if [ "${SENT:-0}" -ge 3 ]; then ok "控制端上报计数 ≥3（input_sent=$SENT）"; else bad "input_sent=${SENT:-?} < 3"; fi
clean "$LOG" | grep -E "窗口已关闭|捕获模式" | sed 's/^/    /'

echo "== 6) 被控端统计：事件数必须恰好等于注入次数 =="
EV=$($SSH "bash /tmp/remote-xvfb-testbed.sh events" 2>&1)
HST=$(printf '%s\n' "$EV" | sed -n 's/^HOSTSTAT //p')
EVT=$(printf '%s' "$HST" | grep -oE 'events=[0-9]+' | grep -oE '[0-9]+')
ERR=$(printf '%s' "$HST" | grep -oE 'inject_errors=[0-9]+' | grep -oE '[0-9]+')
DRP=$(printf '%s' "$HST" | grep -oE 'dropped_locked=[0-9]+' | grep -oE '[0-9]+')
if [ "${EVT:-?}" = "3" ]; then
  ok "被控端 events=3 —— 抓握制造的 WM_MOUSEMOVE 全被守卫丢掉，没有绝对坐标混上线"
else
  bad "被控端 events=${EVT:-?} ≠ 3（多出来=绝对/重复泄漏，少了=半路丢事件）：$HST"
fi
[ "${ERR:-1}" = "0" ] && ok "注入零失败（inject_errors=0）" || bad "inject_errors=${ERR:-?}"
[ "${DRP:-1}" = "0" ] && ok "锁屏门禁没挡（dropped_locked=0）" || bad "dropped_locked=${DRP:-?}"

echo
echo "== 结果: $pass 通过 / $fail 失败 =="
[ "$fail" = "0" ] || exit 1
