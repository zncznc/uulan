#!/usr/bin/env bash
# 真机验「往应用里打字 / 拖拽」——用真机自起的 xev 当独立判据（xev 只打印收到的事件，不插字、不改应用状态）。
#
# 为什么需要它：:0 被 UKUI 锁屏时，uulan 按策略拒发键鼠（XTest 会绕过锁屏抓取，往锁屏里打字有风险），
# 所以这项验收必须在一个**持续解锁**的窗口里跑。锁屏时本脚本直接说明并退出 2（= 未验，不是失败）。
#
# 用法：bash scripts/verify-input-e2e.sh
# 可用环境变量覆盖：UULAN_CLIENT / UULAN_SERVER / UULAN_SSH / UULAN_PW_FILE / UULAN_KEEP_XEV
# 退出码：0 = 打字与拖拽都验过；2 = 被控端锁屏，未验；1 = 跑了但判据不达标（如实失败）
set -uo pipefail

CLIENT=${UULAN_CLIENT:-/d/uulan-bin/uulan-client.exe}
SERVER=${UULAN_SERVER:-192.168.223.15:21116}
SSH=${UULAN_SSH:-ssh -o ConnectTimeout=12 kylin}
# 原生 Windows 程序只认 C:/... 形式的路径（MSYS 的 /c/... 会报「系统找不到指定的文件 (os error 2)」）
PW=${UULAN_PW_FILE:-C:/Users/chaoc/AppData/Local/hermes/cache/scratch/uulan-pw.txt}
HOSTJSON=/home/mis/.config/uulan/host.json
KEEP_XEV=${UULAN_KEEP_XEV:-0}

pass=0; fail=0
ok()   { pass=$((pass+1)); echo "  PASS $1"; }
bad()  { fail=$((fail+1)); echo "  FAIL $1"; }

cleanup() {
  [ "$KEEP_XEV" = "1" ] || $SSH "pkill -f 'xev -geometry' >/dev/null 2>&1; rm -f /tmp/xev.log" >/dev/null 2>&1
  rm -f "$PW"
}
trap cleanup EXIT

echo "== 0) 前置：被控端是否锁屏 =="
FOCUS=$($SSH "DISPLAY=:0 xdotool getwindowfocus getwindowname 2>/dev/null" 2>/dev/null | tr -d '\r' | tail -1)
if printf '%s' "$FOCUS" | grep -qi 'screensaver'; then
  echo "  当前焦点窗口 = $FOCUS"
  echo "  被控端处于锁屏：按策略拒发键鼠 → 本项未验（不是失败）。请解锁屏幕后重跑。"
  exit 2
fi
echo "  焦点窗口 = ${FOCUS:-（取不到）} → 视为已解锁，继续"

echo "== 1) 取设备 ID 与口令（口令只落文件、不上屏）=="
DEV=${UULAN_DEVICE:-$($SSH "python3 -c \"import json;print(json.load(open('$HOSTJSON'))['device_id'])\"" 2>/dev/null | tr -d '\r')}
$SSH "python3 -c \"import json;print(json.load(open('$HOSTJSON'))['password'])\"" 2>/dev/null | tr -d '\r' > "$PW"
echo "  设备 ID = $DEV ；口令文件 $(wc -c < "$PW" | tr -d ' ') 字节"

echo "== 2) 真机起 xev（判据窗）=="
$SSH "DISPLAY=:0 nohup xev -geometry 420x320+120+120 > /tmp/xev.log 2>&1 < /dev/null & sleep 1.5; echo 已启动" >/dev/null 2>&1
WID=$($SSH "DISPLAY=:0 xdotool search --name 'Event Tester' 2>/dev/null | tail -1" | tr -d '\r')
GEO=$($SSH "DISPLAY=:0 xdotool getwindowgeometry $WID 2>/dev/null" | tr -d '\r')
PX=$(echo "$GEO" | grep -o 'Position: [0-9]*' | grep -o '[0-9]*$')
PY=$(echo "$GEO" | grep -o 'Position: [0-9]*,[0-9]*' | cut -d, -f2 | tr -dc '0-9')
W=$(echo "$GEO"  | grep -o 'Geometry: [0-9]*' | grep -o '[0-9]*$')
H=$(echo "$GEO"  | grep -o 'Geometry: [0-9]*x[0-9]*' | cut -dx -f2 | tr -dc '0-9')
PX=${PX:-120}; PY=${PY:-158}; W=${W:-420}; H=${H:-320}
CX=$(( PX + W/2 )); CY=$(( PY + H/2 )); DX=$(( CX + 90 )); DY=$(( CY + 60 ))
echo "  xev 窗口 $WID 位于 $PX,$PY ${W}x${H} → 点 ($CX,$CY)、拖到 ($DX,$DY)"
$SSH "DISPLAY=:0 xdotool windowactivate --sync $WID" >/dev/null 2>&1

INJECT="move:$CX,$CY; click:left; type:hello; move:$CX,$CY; down:left; move:$DX,$DY; up:left"
echo "== 3) 沿真实链路注入：$INJECT =="
"$CLIENT" --server "$SERVER" --device "$DEV" --password-file "$PW" --inject "$INJECT" 2>&1 | sed -n '1,8p'
RC=${PIPESTATUS[0]}
echo "  客户端退出码 = $RC"

echo "== 4) 真机侧判据（xev 收到的事件）=="
KEYS=$($SSH "grep -cE 'KeyPress' /tmp/xev.log" | tr -dc '0-9')
PRESS=$($SSH "grep -cE 'ButtonPress' /tmp/xev.log" | tr -dc '0-9')
REL=$($SSH "grep -cE 'ButtonRelease' /tmp/xev.log" | tr -dc '0-9')
MOTION=$($SSH "grep -cE 'MotionNotify' /tmp/xev.log" | tr -dc '0-9')
echo "  KeyPress=$KEYS ButtonPress=$PRESS ButtonRelease=$REL MotionNotify=$MOTION"
$SSH "grep -E 'KeyPress' /tmp/xev.log | sed -E 's/^.*keysym //' | tr '\n' ' '" 2>/dev/null; echo

echo "== 5) 断言 =="
[ "${RC:-1}" = "0" ] || bad "客户端退出码应为 0（实为 ${RC:-?}）"
[ "${KEYS:-0}" -ge 5 ] && ok "键盘进到应用：KeyPress $KEYS 次（hello 5 个键）" || bad "键盘事件不足（$KEYS）"
[ "${PRESS:-0}" -ge 1 ] && ok "鼠标按下进到应用（ButtonPress $PRESS）" || bad "没有 ButtonPress"
[ "${REL:-0}"   -ge 1 ] && ok "鼠标松开进到应用（ButtonRelease $REL）" || bad "没有 ButtonRelease"
[ "${MOTION:-0}" -ge 1 ] && ok "拖拽过程有移动事件（MotionNotify $MOTION）" || bad "没有 MotionNotify"

echo
echo "== 结果: $pass 通过 / $fail 失败 =="
[ "$fail" = "0" ] && exit 0 || exit 1