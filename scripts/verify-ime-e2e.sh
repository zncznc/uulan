#!/usr/bin/env bash
# IME 上屏端到端验收（Windows 控制端 → 麒麟 pluma）
#
# 这条长期挂着「未验：真实 IME 上屏的中文」。本轮把它拆开看清楚：
#   · 代码通路**早就有**，而且写得很讲究（crates/uulan-client/src/input.rs 的 key_action：
#     非 ASCII 文本 → 只发字符不发物理键，否则会打出「拼音字母 + 汉字」；
#     有 shortcut 时物理键优先；松开永不再发字符）—— 还有 5 条单测钉着；
#   · 真正没验的是 **OS 层送进来的 IME 提交** 能不能走通这条路。
#
# 为什么 SendInput(KEYEVENTF_UNICODE) 能当判据：微软拼音/搜狗上屏汉字时，进应用的是
# WM_CHAR（Unicode 包、**没有扫描码**）；用同样的标志注入，落在 winit 上就是
# physical_key=Unidentified + text=该字符 —— 正是被测那条分支。
# 不覆盖的是「第三方 IME 自己的候选窗行为」，那不属于 uulan，如实写进归档。
#
# 用法：bash scripts/verify-ime-e2e.sh --device <ID> --password-file <文件> [--ssh user@host]
# 退出码：0 通过 / 1 断言失败 / 2 环境问题（锁屏、没桌面、客户端起不来）。
set -u
DEVICE="" PWFILE="" SERVER=${UULAN_SERVER:-192.168.223.15:21116}
SSH_TARGET=${UULAN_SSH_TARGET:-mis@192.168.223.15}
CLIENT=${UULAN_CLIENT:-D:/uulan-bin/uulan-client.exe}
HOSTBIN=${UULAN_HOSTBIN:-\$HOME/.local/uulan/src/target/release}
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
PY=$(command -v python3 2>/dev/null || command -v python)
TS=$(date +%H%M%S)
F="/tmp/uulan-ime-$TS.txt"
LOG="target/logs/ime-$TS.log"
CJK="中文测试IME上屏"
ASCII="ab"   # 用物理键 VK A/B 打出来（走另一条分支）
mkdir -p target/logs
pass=0 fail=0
ok()  { echo "  PASS $1"; pass=$((pass+1)); }
bad() { echo "  FAIL $1"; fail=$((fail+1)); }
rs()  { $SSH "export DISPLAY=:0 XAUTHORITY=\$HOME/.Xauthority; $1" 2>/dev/null | tr -d '\r'; }

echo "──────── ⓿ 前置：屏幕必须是解锁态 ────────"
D=$(rs "$HOSTBIN/uulan-host --diagnose --config /tmp/uulan-ime-q.json --capture x11 2>/dev/null" | grep -a '显示 ' | head -1)
printf '%s' "$D" | grep -q '锁屏 否' || { echo "  屏幕还锁着：[$D] —— 键鼠会被门禁挡下，退出 2（别硬跑）。"; exit 2; }
ok "被控端解锁态：[$D]"

echo "──────── ① 麒麟开 pluma 并拿到前台 ────────"
rs "printf 'BEFORE-$TS\n' > $F"
rs "setsid nohup pluma $F >/dev/null 2>&1 </dev/null & sleep 5; echo OPENED" | grep -q OPENED || bad "pluma 没起来"
WID=$(rs "xdotool search --name 'uulan-ime-$TS' | tail -1")
[ -n "$WID" ] || WID=$(rs "xdotool search --class pluma | tail -1")
rs "xdotool windowactivate --sync $WID 2>/dev/null; xdotool windowfocus $WID 2>/dev/null; sleep 1" >/dev/null

echo "──────── ② 起**窗口模式**控制端（IME 路径只在窗口模式存在）────────"
RUST_LOG=info NO_COLOR=1 "$CLIENT" --server "$SERVER" --device "$DEVICE" \
  --password-file "$PWFILE" --no-clipboard > "$LOG" 2>&1 &
CPID=$!
# 「窗口已就绪」只是**窗口建好了**，不等于会话连上了 —— 上一版就在这儿栽过：
# 焦点拿到的是「画面 0x0 · 0 fps · 正在连接」的窗口，注进去的键没有一个有人接。
for _ in $(seq 1 80); do grep -aq "已连上" "$LOG" && grep -aq "窗口已就绪" "$LOG" && break; sleep 0.5; done
grep -aq "已连上" "$LOG" && ok "① 会话真的连上了（不是只开了窗口）" || { bad "会话没连上"; tail -6 "$LOG" | sed 's/^/    /'; kill "$CPID" 2>/dev/null; exit 2; }

FO=$(python scripts/capture-sendkey.py --focus "uulan" 2>&1)
printf '  %s\n' "$FO"
printf '%s' "$FO" | grep -q "FOCUS OK" || { bad "控制端窗口没置前（键会送给别的窗口）"; kill "$CPID" 2>/dev/null; echo; echo "结果：$pass 通过 / $fail 失败"; exit 1; }
# 焦点回到 pluma：控制端窗口在前台 ≠ 对面 pluma 有焦点，但对面应用才是收字的地方，
# 所以先把本地窗口置前（拿到 winit 事件），再靠对面那份焦点打进文档。
# 对面（麒麟）的 pluma 必须拿着键盘焦点；本地这扇 uulan 窗口必须是前台。
# 两边都要确认，否则「注进去没反应」就分不清是哪一侧没焦点。
TITLE=$(powershell.exe -NoProfile -Command "[Console]::OutputEncoding=[Text.Encoding]::UTF8; (Get-Process uulan-client -ErrorAction SilentlyContinue | Where-Object {\$_.MainWindowTitle} | Select-Object -First 1).MainWindowTitle" 2>/dev/null | tr -d '\r')
printf '  本地窗口标题：%s\n' "$TITLE"
printf '%s' "$TITLE" | grep -q "画面 0x0" && bad "本地窗口还在「画面 0x0」，注入无人接收" || ok "② 本地窗口已在收画面"
REMOTE_FOCUS=$(rs "xdotool getwindowfocus getwindowname")
printf '%s' "$REMOTE_FOCUS" | grep -q "uulan-ime-$TS" && ok "③ 对面 pluma 仍是键盘焦点（[$REMOTE_FOCUS]）" \
  || { rs "xdotool windowactivate --sync $WID; xdotool windowfocus $WID" >/dev/null; sleep 0.8;
       REMOTE_FOCUS=$(rs "xdotool getwindowfocus getwindowname")
       printf '%s' "$REMOTE_FOCUS" | grep -q "uulan-ime-$TS" && ok "③ 重新激活后对面焦点回来了（[$REMOTE_FOCUS]）" \
         || bad "③ 对面 pluma 拿不到键盘焦点（[$REMOTE_FOCUS]）"; }

echo "──────── ③ 注入 IME 提交形态的中文 ────────"
# 置前与注入必须在同一次调用里（分两次时，注入那一次已经不是前台了）
python scripts/capture-sendkey.py --focus uulan --unicode "$CJK" 2>&1 | sed 's/^/  /'
sleep 2
python scripts/capture-sendkey.py --focus uulan --down 17 >/dev/null 2>&1
python scripts/capture-sendkey.py --focus uulan 83 >/dev/null 2>&1
python scripts/capture-sendkey.py --focus uulan --up 17 >/dev/null 2>&1
sleep 1.5
C1=$(rs "cat $F")
printf '  文件（第一步）：%s\n' "$(printf '%s' "$C1" | head -2 | tr '\n' '|')"
printf '%s' "$C1" | grep -q "$CJK" && ok "① 中文经 Unicode 注入（IME 提交形态）真的落进了应用文档" \
  || bad "① 文件里没有中文串（实际：$(printf '%s' "$C1" | head -1)）"

echo "──────── ④ 反面对照：ASCII 物理键路径不能被带坏 ────────"
# 走**物理键**路径（VK 65=A、66=B），而不是 Unicode 注入 —— 这条对照要证明的是
# 「IME 那条分支没有把普通打字也一起改掉」，用 --unicode 注入就等于自己把考题改了。
python scripts/capture-sendkey.py --focus uulan 65 >/dev/null 2>&1
python scripts/capture-sendkey.py --focus uulan 66 >/dev/null 2>&1
sleep 1
python scripts/capture-sendkey.py --focus uulan --down 17 >/dev/null 2>&1
python scripts/capture-sendkey.py --focus uulan 83 >/dev/null 2>&1
python scripts/capture-sendkey.py --focus uulan --up 17 >/dev/null 2>&1
sleep 1.5
C2=$(rs "cat $F")
printf '  文件（第二步）：%s\n' "$(printf '%s' "$C2" | head -2 | tr '\n' '|')"
printf '%s' "$C2" | grep -q "$CJK" && printf '%s' "$C2" | grep -q "$ASCII" \
  && ok "② 中文与 ASCII 同时在文档里（两条分支并存、不互踩）" \
  || bad "② 第二步之后内容不对"
# 「不发物理键」这条纪律的直接证据：文件里不许出现任何拼音字母残骸
printf '%s' "$C2" | grep -qiE "zhong|wenshi|shangping" && bad "③ 出现拼音字母残骸（说明发的是物理键）" \
  || ok "③ 没有拼音字母残骸（只发字符这条纪律成立）"

echo "──────── ⑤ 会话健康 ────────"
grep -aq "指针锁定失败" "$LOG" && bad "④ 出现指针锁定失败" || ok "④ 没有抓握失败"
kill "$CPID" 2>/dev/null; sleep 1; kill -9 "$CPID" 2>/dev/null
echo
echo "结果：$pass 通过 / $fail 失败"
echo "（文件 $F 留在真机 /tmp；日志 $LOG）"
[ "$fail" = "0" ]