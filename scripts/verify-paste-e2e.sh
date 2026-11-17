#!/usr/bin/env bash
# 真机粘贴验收：Windows 剪贴板 →（uulan 同步）→ 麒麟 X11 剪贴板 → pluma 里 Ctrl+V → Ctrl+S → 读文件
#
# 覆盖 DESIGN 里长期挂着的一条「未验」：粘贴（Ctrl+V）没在真机应用里打过。
# 这条同时吃两端：剪贴板同步 + 修饰键组合注入。
#
# 用法：
#   bash scripts/verify-paste-e2e.sh --device 805747189 --password-file D:/uulan-bin/host.pw \
#        [--ssh mis@192.168.223.15] [--server 192.168.223.15:21116]
#
# 前提：屏幕**解锁**且 pluma 能拿到前台焦点（锁屏时键鼠被门禁挡下，那是策略不是故障，
# 所以这条在锁屏态必须拒绝开跑，而不是硬跑出一个假失败）。
set -u

DEVICE="" PWFILE="" SERVER=${UULAN_SERVER:-192.168.223.15:21116}
SSH_TARGET=${UULAN_SSH_TARGET:-mis@192.168.223.15}
CLIENT=${UULAN_CLIENT:-target/release/uulan-client.exe}
HOSTBIN=${UULAN_HOSTBIN:-\$HOME/.local/uulan/src/target/release}   # 留给远端 shell 展开
while [ $# -gt 0 ]; do
  case $1 in
    --device) DEVICE=$2; shift 2 ;;
    --password-file) PWFILE=$2; shift 2 ;;
    --ssh) SSH_TARGET=$2; shift 2 ;;
    --server) SERVER=$2; shift 2 ;;
    *) echo "未知参数：$1"; exit 2 ;;
  esac
done
[ -n "$DEVICE" ] && [ -n "$PWFILE" ] || { echo "用法：--device <ID> --password-file <文件>"; exit 2; }
SSH="ssh -o BatchMode=yes -o ConnectTimeout=12 $SSH_TARGET"

pass=0 fail=0
ok()  { echo "  PASS $1"; pass=$((pass+1)); }
bad() { echo "  FAIL $1"; fail=$((fail+1)); }
rs()  { $SSH "export DISPLAY=:0 XAUTHORITY=\$HOME/.Xauthority; $1" 2>/dev/null | tr -d '\r'; }

TS=$(date +%H%M%S)
MARK1="uulan-sync-$TS-同步中文标记"
MARK2="uulan-paste-$TS-粘贴中文标记"
F="/tmp/uulan-paste-$TS.txt"
LOG1="target/logs/paste-sync-$TS.log"
LOG2="target/logs/paste-paste-$TS.log"
mkdir -p target/logs

echo "== 0) 环境：剪贴板读得到、屏幕是解锁态 =="
CL=$(powershell.exe -NoProfile -Command '[Console]::OutputEncoding=[Text.Encoding]::UTF8; Get-Clipboard -Raw' 2>/dev/null | tr -d '\r')
if [ -z "$CL" ]; then
  echo "  本机拿不到 Windows 剪贴板（无人值守会话的老问题）。**不要据此判定粘贴不通**，退出 2。"
  exit 2
fi
ok "Windows 剪贴板可用（读回 ${#CL} 字符）"

DIAG=$(rs "$HOSTBIN/uulan-host --diagnose --config /tmp/uulan-paste-q.json --capture x11 2>/dev/null")
DLINE=$(printf '%s' "$DIAG" | grep -a '显示 ' | head -1)
printf '%s' "$DLINE" | grep -q '锁屏 否' || { echo "  屏幕还锁着：[$DLINE] —— 键鼠会被门禁挡下，退出 2（不硬跑）。"; exit 2; }
ok "真机是解锁态（被控端自己说的）：[$DLINE]"

echo
echo "== 1) 起点：文件里只有 BEFORE，pluma 打开它并拿到前台 =="
rs "printf 'BEFORE-$TS\n' > $F"
rs "setsid nohup pluma $F >/dev/null 2>&1 </dev/null & sleep 5; echo OPENED" | grep -q OPENED || bad "pluma 没起来"
WID=$(rs "xdotool search --name 'uulan-paste-$TS' | tail -1")
[ -n "$WID" ] || WID=$(rs "xdotool search --class pluma | tail -1")
if [ -n "$WID" ]; then
  rs "xdotool windowactivate --sync $WID 2>/dev/null; xdotool windowfocus $WID 2>/dev/null; sleep 1.5" >/dev/null
fi
FOCUS=$(rs "xdotool getwindowfocus getwindowname")
printf '%s' "$FOCUS" | grep -q "uulan-paste-$TS" || { bad "pluma 不在前台，焦点是 [$FOCUS]"; echo; echo "结果：$pass 通过 / $fail 失败"; exit 1; }
ok "pluma 在前台（[$FOCUS]）"
rs "cat $F" | grep -q "BEFORE-$TS" || bad "起点文件内容不对"

echo
echo "== 2) 第一段会话（证「同步」这半条）：连上之后改剪贴板，会话进行中用探针读回 =="
# 三个必须先知道的坑（都是本脚本自己踩出来的，不是产品问题）：
#  ① 连接之前就设好的值不外推：控制端在会话开头取基线，只有**会话中**的变更才推送，
#     所以设剪贴板必须在连上之后。
#  ② X11 的 CLIPBOARD 由被控端的持有线程在会话期间拥有，会话一结束就没人持有、
#     读回来是 none —— 任何读剪贴板的动作必须在会话进行中做。
#  ③ x11_clip_probe **读之前必先写**（不给参数就写 "PROBE-WRITE"）。拿它当纯读探针
#     会把剪贴板污染掉，随后的 Ctrl+V 就粘出 "PROBE-WRITE"，看起来像粘贴坏了。
#     所以：探针只用在这一段，并且让它写**同一个标记**，污染变无害；粘贴单独一段。
"$CLIENT" --server "$SERVER" --device "$DEVICE" --password-file "$PWFILE" \
  --headless --seconds 14 > "$LOG1" 2>&1 &
P1=$!
sleep 6
powershell.exe -NoProfile -Command "Set-Clipboard -Value '$MARK1'" >/dev/null 2>&1
echo "  （会话第 6 秒把同步标记写进 Windows 剪贴板）"
sleep 4
PROBE="$HOSTBIN/examples/x11_clip_probe"
if ! rs "test -x $PROBE && echo HAVE" | grep -q HAVE; then
  rs "export PATH=\$HOME/.cargo/bin:\$PATH; cd ~/.local/uulan/src && cargo build --release --example x11_clip_probe -p uulan-clip >/dev/null 2>&1" \
    && ok "真机上补编了剪贴板探针" || echo "  （探针编不出来，只能靠落盘内容判）"
fi
# 探针先打一行 INFO 再打 GET-BEFORE，所以必须按内容挑行，不能 head -2 —— 上一版就是
# 被这个截断骗成「没同步」。
HOSTCLIP=$(rs "timeout 20 $PROBE '$MARK1' 1 2>/dev/null" | grep -a "GET-BEFORE" | head -1)
printf '  探针读回（会话进行中）：%s\n' "${HOSTCLIP:-<空>}"
printf '%s' "$HOSTCLIP" | grep -q "$MARK1" && ok "① 被控端 X11 剪贴板里有本轮同步标记（同步确实发生，中文逐字对）" \
  || bad "① 被控端剪贴板不是同步标记：[${HOSTCLIP:-<空>}]"
wait "$P1" 2>/dev/null || true
STAT=$(grep -oE "发送 [0-9]+ / 接收 [0-9]+" "$LOG1" | tail -1)
printf '  控制端统计：%s\n' "${STAT:-<没有这行>}"
[ -n "$STAT" ] && ok "控制端有剪贴板统计（$STAT）" || bad "控制端没有剪贴板统计（这条链根本没跑）"

echo
echo "== 3) 第二段会话（证「粘贴」这半条）：会话中换一个**新**标记，再发 Ctrl+V、Ctrl+S =="
# 单独一段的两个理由：上一段的探针已经把剪贴板占过一遍；而且基线在会话开头取，
# 标记必须在「这一段会话」里变更才会被推过去。
# 必须用 --inject-live：`--inject` 是快车道、不跑剪贴板泵，拿它做粘贴验收等于
# 自己把被测的那半关掉。
# keydown/keyup 夹住 Ctrl，中间 tap 一个键 = 组合键（VK：17=Ctrl 86=V 83=S）
rs "xdotool windowactivate --sync $WID 2>/dev/null; xdotool windowfocus $WID 2>/dev/null" >/dev/null
INJ="wait:11000; keydown:17; tap:86; keyup:17; wait:2500; keydown:17; tap:83; keyup:17; wait:3500"
"$CLIENT" --server "$SERVER" --device "$DEVICE" --password-file "$PWFILE" \
  --headless --seconds 20 --inject-live "$INJ" > "$LOG2" 2>&1 &
P2=$!
sleep 6
powershell.exe -NoProfile -Command "Set-Clipboard -Value '$MARK2'" >/dev/null 2>&1
echo "  （会话第 6 秒写入粘贴标记，第 11 秒发 Ctrl+V）"
wait "$P2" 2>/dev/null || true
printf '  控制端统计：%s\n' "$(tr -d '\r' < "$LOG2" | grep -a "已发送" | tail -1 | sed 's/^ *//')"
ok "粘贴段会话跑完"

echo
echo "== 4) 落盘判据 =="
CONTENT=$(rs "cat $F")
printf '  文件内容：%s\n' "$(printf '%s' "$CONTENT" | head -3 | tr '\n' '|')"
printf '%s' "$CONTENT" | grep -q "$MARK2" && ok "② 文件里出现粘贴标记（Ctrl+V 真打进应用、中文逐字没坏）" \
  || bad "② 文件里没有粘贴标记"
printf '%s' "$CONTENT" | grep -q "PROBE-WRITE" && bad "③ 有探针污染残留（两段没隔离干净）" \
  || ok "③ 文件里没有探针写入的污染串"

echo
echo "结果：$pass 通过 / $fail 失败"
echo "（文件 $F 留在真机 /tmp 供复看；pluma 窗口未关）"
[ "$fail" = "0" ]
