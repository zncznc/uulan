#!/usr/bin/env bash
# 验「键鼠真的进了应用」（打字 + 拖拽）—— 真机上的 Xvfb :99 + xev 当独立判据。
#
# 与 scripts/verify-input-e2e.sh 的分工：
#   - verify-input-e2e.sh 打**物理 :0**：那才是用户的桌面，但 :0 被 UKUI 锁屏挡着时
#     按策略不发键鼠，脚本只能退出 2（= 未验）。要人在机器跟前解锁且保持解锁。
#   - 本脚本打**真机上的一个 Xvfb :99 虚拟桌面**：X 服务器、XTest 注入、被控端采集与
#     锁屏探测全都是真的，只是桌面内容不是用户那一个。因此**锁不锁屏都能跑**，
#     可以放进无人值守的夜批。跳过的只有「UKUI 真实桌面上的窗口管理器行为」。
#
# 判据（xev 与 uulan 无关，它只打印收到的事件）：
#   点一下 → ButtonPress(1)；type:hello → 5 个 KeyPress，keysym 依次 h e l l o；
#   按住左键拖 90,60 → ButtonPress → MotionNotify(state 0x100) → ButtonRelease。
#
# 用法：bash scripts/verify-input-app-e2e.sh
# 环境变量：UULAN_CLIENT（本地控制端 exe）/ UULAN_SSH / UULAN_HOST_BIN（远端前缀）/ UULAN_APP_PORT
# 退出码：0 = 全部判据通过；1 = 有判据不达标
set -uo pipefail

SSH=${UULAN_SSH:-ssh -o ConnectTimeout=12 kylin}
SCP=${UULAN_SCP:-scp -q}
CLIENT=${UULAN_CLIENT:-/d/uulan-bin/uulan-client.exe}
SERVER=${UULAN_SERVER:-192.168.223.15}
PORT=${UULAN_APP_PORT:-21995}
GEOM=${UULAN_APP_GEOM:-420x300+100+100}
KEEP=${UULAN_KEEP_TESTBED:-0}
HERE=$(cd "$(dirname "$0")" && pwd)

pass=0; fail=0
ok()  { pass=$((pass+1)); echo "  PASS $1"; }
bad() { fail=$((fail+1)); echo "  FAIL $1"; }

tmpms=${TMPDIR:-/tmp}
mkdir -p "$tmpms"
tmpwin=$(cygpath -m "$tmpms" 2>/dev/null || echo "$tmpms")
PW="$tmpwin/uulan-app-pw.txt"

cleanup() {
  rm -f "$PW"
  [ "$KEEP" = "1" ] || $SSH "bash /tmp/remote-xvfb-testbed.sh down" >/dev/null 2>&1
}
trap cleanup EXIT

echo "== 0) 前置：把测试台脚本送到真机、确认控制端 exe 在 =="
[ -x "$CLIENT" ] || { echo "FAIL 找不到控制端 $CLIENT"; exit 1; }
$SCP "$HERE/remote-xvfb-testbed.sh" "kylin:/tmp/remote-xvfb-testbed.sh" || { echo "FAIL scp 测试台脚本失败"; exit 1; }

echo "== 1) 真机上起 Xvfb :99 + xev 判据窗 + 独立 relay/host（端口 $PORT）=="
UP=$($SSH "bash /tmp/remote-xvfb-testbed.sh up" 2>&1)
echo "$UP" | sed 's/^/  /'
DEV=$(printf '%s\n' "$UP" | awk '/^DEVICE /{print $2}' | tail -1)
WID=$(printf '%s\n' "$UP" | awk '/^WID /{print $2}' | tail -1)
PX=$(printf '%s\n' "$UP" | grep -o 'Position: [0-9]*' | grep -o '[0-9]*$')
PY=$(printf '%s\n' "$UP" | grep -o 'Position: [0-9]*,[0-9]*' | cut -d, -f2 | tr -dc '0-9')
W=$(printf '%s\n' "$UP"  | grep -o 'Geometry: [0-9]*' | grep -o '[0-9]*$')
H=$(printf '%s\n' "$UP"  | grep -o 'Geometry: [0-9]*x[0-9]*' | cut -dx -f2 | tr -dc '0-9')
PX=${PX:-100}; PY=${PY:-100}; W=${W:-420}; H=${H:-300}
[ -n "${DEV:-}" ] && [ -n "${WID:-}" ] || { echo "FAIL 测试台没起来（没拿到设备 ID / 窗口 ID）"; exit 1; }
CX=$(( PX + W/2 )); CY=$(( PY + H/2 )); DX=$(( CX + 90 )); DY=$(( CY + 60 ))
echo "  设备 ID=$DEV  xev 窗口 $WID 在 $PX,$PY ${W}x${H} → 点 ($CX,$CY)、拖到 ($DX,$DY)"

echo "== 2) 取测试台的访问口令（只落文件，不上屏）=="
$SSH "python3 -c \"import json;print(json.load(open('/tmp/uulan-input-app/host.json'))['password'])\"" \
  2>/dev/null | tr -d '\r' | tail -1 > "$PW"
[ -s "$PW" ] || { echo "FAIL 没拿到口令"; exit 1; }
echo "  口令文件 $(wc -c < "$PW" | tr -d ' ') 字节（内容不上屏）"

INJECT="move:$CX,$CY; click:left; type:hello; move:$CX,$CY; down:left; move:$DX,$DY; up:left"
echo "== 3) 真实链路注入（ASCII 打字 + 拖拽）：$INJECT =="
"$CLIENT" --server "$SERVER:$PORT" --device "$DEV" --password-file "$PW" --inject "$INJECT" 2>&1 \
  | grep -vE '^\s*$' | tail -6 | sed 's/^/  /'
RC=${PIPESTATUS[0]}
echo "  控制端退出码 = $RC"

# 第二阶段：非 ASCII（中文）走的是 KeyChar → 动态注册一个 Unicode keycode（0x01000000+码点）。
# 这条路径与物理键位无关，是最容易「单测绿、真机没反应」的那一条，所以单独验一次。
INJECT_CJK="move:$CX,$CY; type:中文"
echo "== 3b) 真实链路注入（中文，走动态 Unicode keycode）：$INJECT_CJK =="
"$CLIENT" --server "$SERVER:$PORT" --device "$DEV" --password-file "$PW" --inject "$INJECT_CJK" 2>&1 \
  | grep -vE '^\s*$' | tail -4 | sed 's/^/  /'
RC2=${PIPESTATUS[0]}
echo "  控制端退出码 = $RC2"

echo "== 4) 独立判据（真机 xev 收到的事件）=="
EV=$($SSH "bash /tmp/remote-xvfb-testbed.sh events" 2>&1)
echo "$EV" | sed 's/^/  /'
KEYS=$(printf '%s\n' "$EV"    | awk '/^KEYPRESS /{print $2}')
KREL=$(printf '%s\n' "$EV"    | awk '/^KEYRELEASE /{print $2}')
BP=$(printf '%s\n' "$EV"      | awk '/^BUTTONPRESS /{print $2}')
BR=$(printf '%s\n' "$EV"      | awk '/^BUTTONRELEASE /{print $2}')
MOT=$(printf '%s\n' "$EV"     | awk '/^MOTION /{print $2}')
DRAG=$(printf '%s\n' "$EV"    | awk '/^DRAGMOTION /{print $2}')
SYMS=$(printf '%s\n' "$EV"    | awk '/^KEYSYMS /{sub(/^KEYSYMS /,"");print}' | sed 's/ *$//')
UNI=$(printf '%s\n' "$EV"     | awk '/^UNICODE /{sub(/^UNICODE /,"");print}' | tr -d ' ')
ZH=$(printf '%s\n' "$EV"      | awk '/^UTF8ZHONG /{print $2}')
WEN=$(printf '%s\n' "$EV"     | awk '/^UTF8WEN /{print $2}')
HST=$(printf '%s\n' "$EV"     | sed -n 's/^HOSTSTAT //p')

echo "== 5) 断言 =="
[ "${RC:-1}" = "0" ] && ok "控制端退出码 0（ASCII 阶段）" || bad "控制端退出码 ${RC:-?}（ASCII 阶段）"
[ "${RC2:-1}" = "0" ] && ok "控制端退出码 0（中文阶段）" || bad "控制端退出码 ${RC2:-?}（中文阶段）"
[ "${KEYS:-0}" -ge 5 ] && ok "键盘进了应用：KeyPress $KEYS 次" || bad "KeyPress 不足（${KEYS:-0}）"
[ "${KREL:-0}" -ge 5 ] && ok "按键收尾成对：KeyRelease $KREL 次" || bad "KeyRelease 不足（${KREL:-0}）"
case "${SYMS:-}" in
  "h e l l o"*) ok "keysym 序列以 hello 开头（ASCII 真的打进去了）" ;;
  *) bad "ASCII keysym 序列是「${SYMS:-空}」，不是以 hello 开头" ;;
esac
case "${UNI:-}" in
  *U4E2D*U6587*) ok "中文到了 X：动态 keycode 上是 U4E2D / U6587（$UNI）" ;;
  *) bad "中文没到：Unicode keysym 是「${UNI:-空}」" ;;
esac
[ "${ZH:-0}" -ge 2 ] && [ "${WEN:-0}" -ge 2 ] && ok "XLookupString 把「中文」翻成 UTF-8（e4b8ad / e69687 各出现 $ZH/$WEN 次，按下+松开）" \
  || bad "UTF-8 查表结果不对（中 $ZH 次 / 文 $WEN 次，期望各 2 次）"
[ "${BP:-0}" -ge 2 ] && ok "两次按下都到了应用（ButtonPress $BP：点击 + 拖拽）" || bad "ButtonPress 不足（${BP:-0}）"
[ "${BR:-0}" -ge 2 ] && ok "两次松开都到了应用（ButtonRelease $BR）" || bad "ButtonRelease 不足（${BR:-0}）"
[ "${DRAG:-0}" -ge 1 ] && ok "拖拽期间有按住左键的移动（state 0x100 的 MotionNotify $DRAG）" || bad "没看到按住左键的移动（拖拽没成）"
case "${HST:-}" in
  *dropped_locked=0) ok "被控端没有因为锁屏丢事件（$HST）" ;;
  *) bad "被控端统计异常（${HST:-空}）" ;;
esac

echo
echo "== 结果: $pass 通过 / $fail 失败 =="
[ "$fail" = "0" ] && exit 0 || exit 1