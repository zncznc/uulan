#!/usr/bin/env bash
# 面板 → 终端窗口 end-to-end（**在真机麒麟上跑**，无参数）
#
# 验的是「开一个远端终端」这条缝：面板 spawn 出来的到底是不是一个**用户能看见、能打字**的
# 终端，敲进去的字节是不是真的到了远端 shell，关窗后面板的结论对不对。
#
# 为什么判据是「远端写一个文件」而不是截图 OCR：这台测试台里远端就是本机，写下的文件能被
# 直接读回来 —— 于是「键到底有没有到远端 shell」是一个**机器可查**的量，而不是靠肉眼看窗口。
# （2026-11-22 首次人工验收时就是这么判的，这里把它固化成脚本。）
#
# 用法：bash scripts/verify-gui-term-e2e.sh
# 前置：· 这台机器就是被控端本身（有 ~/.config/uulan/host.json，正式服务在跑或不在跑都行）；
#       · ~/.local/uulan/bin/uulan-gui 与 uulan-client 都是 HEAD 构建；
#       · 必须在图形会话里（DISPLAY=:0），xdotool / ffmpeg / 一个终端模拟器可用。
# 退出码：0 = 全过；1 = 断言失败（产品问题）；2 = 环境给不出判据（别记成产品缺陷）。
#
# ⚠ 布局依赖：按钮位置按 1100x780 窗口量出来的窗口内相对坐标（开始远程控制 110,355；
#    开一个远端终端 276,355）。改了这两个按钮的排版就得重新量一次。
set -u
export DISPLAY="${DISPLAY:-:0}"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
BIN="${UULAN_BIN:-$HOME/.local/uulan/bin}"
GUI="$BIN/uulan-gui"
CLI="$BIN/uulan-client"
CFG="$HOME/.config/uulan"
RID="$(date +%Y%m%d-%H%M%S)"
MARK="/tmp/uulan-gui-term-$RID.txt"
LOG="$CFG/gui-actions.log"

PASS=0; FAIL=0; SKIP=0
ok()   { echo "  [ OK ] $*"; PASS=$((PASS+1)); }
bad()  { echo "  [FAIL] $*"; FAIL=$((FAIL+1)); }
skip() { echo "  [SKIP] $*"; SKIP=$((SKIP+1)); }
env_out() { echo "  [环境] $*"; echo "退出码 2：这台机器此刻给不出这条判据，别把它记成产品缺陷"; exit 2; }

echo "== 0. 前置 =="
[ -x "$GUI" ] || env_out "找不到可执行的面板 $GUI"
[ -x "$CLI" ] || env_out "找不到可执行的控制端 $CLI（面板要 spawn 它）"
[ -f "$CFG/host.json" ] || env_out "这台机器不是被控端（缺 $CFG/host.json）"
command -v xdotool >/dev/null || env_out "没有 xdotool（判据要用它点按钮/敲键盘）"
command -v ffmpeg >/dev/null || env_out "没有 ffmpeg（截图留证用）"
TERM_BIN=""
for t in x-terminal-emulator ukui-terminal mate-terminal gnome-terminal xfce4-terminal konsole lxterminal xterm; do
  if command -v "$t" >/dev/null; then TERM_BIN="$t"; break; fi
done
[ -n "$TERM_BIN" ] || env_out "这台机器一个终端模拟器都没有，面板该走「红字告知」那条路 —— 换一台有终端的机器跑"
# 判「终端窗口起来了」不能靠 pgrep -x <终端名>：x-terminal-emulator 是 alternatives 的壳
# （readlink 指到 mate-terminal.wrapper），而内核 comm 只留 15 字符（`mate-terminal.w`），
# 按全名精确匹配必然落空 —— 第一版就吃了这个假红。判据改成**控制端进程的 argv**：
# 只有「终端把控制端拉起来」才会出现 `uulan-client … --term`。
term_client_pid() { pgrep -f "uulan-client.*--term" 2>/dev/null | grep -v "^$$\$" | head -1; }
term_client_pids() { pgrep -f "uulan-client.*--term" 2>/dev/null | tr '\n' ' '; }
term_host_pid()   { pgrep -f "terminal.*-e .*uulan-client.*--term|-x .*uulan-client.*--term" 2>/dev/null | head -1; }
echo "      面板=$GUI 终端=$TERM_BIN 标记=$MARK"
# 上一轮遗留的 --term 进程会让「这次真的拉起来了」变成假绿：先清掉，清不掉就别跑。
STALE=$(term_client_pids)
if [ -n "${STALE// /}" ]; then
  echo "      清理上一轮遗留的 --term 进程：$STALE"
  for p in $STALE; do kill "$p" 2>/dev/null; done
  sleep 2
  STALE2=$(term_client_pids)
  [ -z "${STALE2// /}" ] || env_out "遗留的 --term 进程杀不掉（$STALE2），这台机器此刻给不出干净的判据"
fi

BK=""
restore() {
  # 收尾：停面板 → 还原 gui.json → 重启面板（留一台在跑，与验收前一致）
  for p in $(pgrep -x uulan-gui); do kill "$p" 2>/dev/null; done
  sleep 1
  if [ -n "${BK:-}" ] && [ -f "$BK" ]; then cp -a "$BK" "$CFG/gui.json"; fi
  # 备份用完就删：每跑一轮留一个 gui.json.bak-<RID> 会在配置目录里越堆越多。
  [ -n "${BK:-}" ] && python3 -c "import os,sys; p=sys.argv[1]; os.remove(p) if os.path.exists(p) else None" "$BK" 2>/dev/null
  # 面板按设计会把口令写进 gui-current.pw（0600）。台架预置的是生产口令，收尾一并删掉：
  # gui.json 已还原成不含口令的默认，面板下次真要口令时会自己重写。
  python3 -c "import os;p=os.path.expanduser('~/.config/uulan/gui-current.pw');os.remove(p) if os.path.exists(p) else None" 2>/dev/null
  setsid nohup "$GUI" >/tmp/uulan-gui-after-$RID.log 2>&1 < /dev/null &
  rm -f "$MARK" 2>/dev/null
}
trap restore EXIT

echo "== 1. 预置表单（口令从 host.json 读，不上屏）=="
[ -f "$CFG/gui.json" ] && { BK="$CFG/gui.json.bak-$RID"; cp -a "$CFG/gui.json" "$BK"; }
python3 - <<'PY' || env_out "写不了 gui.json（权限？）"
import json, os
cfg = os.path.expanduser('~/.config/uulan')
host = json.load(open(os.path.join(cfg, 'host.json')))
g = {"relay": "127.0.0.1:21116", "device": host.get('device_id', ''), "direct": "",
     "remember_pw": True, "password": host['password']}
p = os.path.join(cfg, 'gui.json')
open(p, 'w').write(json.dumps(g)); os.chmod(p, 0o600)
PY
echo "      表单已预置（设备 ID + 记住口令；口令不进 argv）"

echo "== 2. 起面板 =="
for p in $(pgrep -x uulan-gui); do kill "$p" 2>/dev/null; done
sleep 1
pgrep -x uulan-gui >/dev/null && env_out "旧面板杀不掉（按 PID 查 comm 截断问题）"
shots="$HOME/.cache/uulan-verify"; mkdir -p "$shots"
setsid nohup "$GUI" >/tmp/uulan-gui-$RID.log 2>&1 < /dev/null &
sleep 6
PID=$(pgrep -x uulan-gui | head -1)
[ -n "${PID:-}" ] || env_out "面板没起来（看 /tmp/uulan-gui-$RID.log）"
WID=$(xdotool search --pid "$PID" 2>/dev/null | head -1)
[ -n "${WID:-}" ] || env_out "面板进程在，但找不到窗口（GL 起不来？）"
ok "面板起来了（PID $PID，窗口 $WID）"
xdotool windowsize "$WID" 1100 780; sleep 2
eval "$(xdotool getwindowgeometry --shell "$WID")"
echo "      窗口几何 $X,$Y ${WIDTH}x${HEIGHT}"

echo "== 3. 点「开一个远端终端」=="
LOG_BEFORE=$(wc -l < "$LOG" 2>/dev/null || echo 0)
PRE_PIDS=" $(term_client_pids) "
xdotool mousemove $((X + 276)) $((Y + 355)) click 1
T_CLICK=$(date +%s)
CPID=""; TPID=""
for _ in $(seq 1 20); do
  sleep 1
  for q in $(term_client_pids); do
    case "$PRE_PIDS" in *" $q "*) continue ;; esac
    CPID="$q"; break
  done
  [ -n "${CPID:-}" ] && break
done
if [ -n "${CPID:-}" ]; then
  ok "终端把控制端拉起来了（控制端 PID $CPID）"
  pgrep -af "uulan-client.*--term" | grep -q -- "--password-file" \
    && ok "argv 里有口令文件、没有明文口令（与桌面模式同一条纪律）" \
    || skip "argv 形状不像本面板拼的（人工起的会话？）"
  TPID=$(term_host_pid)
  [ -n "${TPID:-}" ] && ok "终端宿主进程也在（PID $TPID）" || skip "宿主进程名匹配不到（壳的命名差异），不影响主判据"
else
  bad "点了 20 秒也没见到「终端 -e 控制端」这条链（这就是 2026-11-22 修掉的那个「死按钮」）"
fi
NEWLOG=$(tail -n +$((LOG_BEFORE + 1)) "$LOG" 2>/dev/null | grep "执行：")
echo "      面板新行：$(echo "$NEWLOG" | tr '\n' ' ')"
echo "$NEWLOG" | grep -qE "-e uulan-client|-x .*uulan-client" \
  && ok "面板的「最近的动作」记的是「终端 -e 控制端 …」" \
  || bad "面板动作行不像终端宿主：${NEWLOG:-（没有新行）}"

echo "== 4. 在终端里敲哨兵（判据 = 远端 shell 真的写下了文件）=="
TWID=""
[ -n "${CPID:-}" ] && TWID=$(xdotool search --pid "$CPID" 2>/dev/null | tail -1)
if [ -n "${TWID:-}" ]; then
  xdotool windowactivate "$TWID"; sleep 1
  xdotool type --delay 40 "echo UULAN-GUI-TERM-OK > $MARK"
  xdotool key Return
  for _ in $(seq 1 15); do sleep 1; [ -f "$MARK" ] && break; done
  if [ -f "$MARK" ]; then
    ok "远端 shell 收到了按键（$MARK 已生成）"
  else
    bad "终端窗口在，但按键没到远端 shell（$MARK 不存在）"
  fi
  DIM=$(xdpyinfo 2>/dev/null | awk '/dimensions/{print $2}' | head -1); DIM=${DIM:-1920x1080}
  ffmpeg -hide_banner -loglevel error -f x11grab -video_size "$DIM" -i "$DISPLAY" -frames:v 1 -y "$shots/term-$RID.png" 2>/dev/null
  [ -f "$shots/term-$RID.png" ] && echo "      截图：$shots/term-$RID.png"
else
  skip "拿不到终端窗口 id，跳过敲键判据"
fi

echo "== 5. 关终端（exit 5）与面板结论 =="
LINES_BEFORE=$(wc -l < "$LOG" 2>/dev/null || echo 0)
# **会话必须活过 10 秒**：面板对「不到 10 秒秒退」走的是「只弹红字、不写动作日志」那条分支
# （那是给「点了没反应」用的提示），于是动作日志里不会出现收尾行 —— 第一版就是这么假红的。
while [ $(( $(date +%s) - T_CLICK )) -lt 12 ]; do sleep 1; done
if [ -n "${TWID:-}" ]; then
  xdotool windowactivate "$TWID"; sleep 1
  xdotool type --delay 40 'exit 5'
  xdotool key Return
fi
CLOSED=0
for _ in $(seq 1 20); do sleep 1; pgrep -f "uulan-client.*--term" > /dev/null || { CLOSED=1; break; }; done
[ "$CLOSED" = "1" ] && ok "终端里那条会话结束了（--term 进程退出）" || bad "exit 5 之后 --term 进程还在"
for _ in $(seq 1 15); do
  tail -n +$((LINES_BEFORE + 1)) "$LOG" 2>/dev/null | grep -q "远端终端窗口已关闭" && break
  sleep 1
done
NEW=$(tail -n +$((LINES_BEFORE + 1)) "$LOG" 2>/dev/null | tail -3)
echo "      面板新行：$(echo "$NEW" | tr '\n' ' ')"
echo "$NEW" | grep -q "远端终端窗口已关闭" && ok "面板记下了终端收尾" || bad "面板没有终端收尾那一句：$NEW"
echo "$NEW" | grep -q "终端程序退出码" && ok "措辞写清了退出码归属（终端程序 vs 远端 shell）" \
  || skip "这次退出码是 0，措辞里不带「终端程序退出码」（可接受）"

echo "== 6. 桌面模式回归（同一条 spawn 路径，改过就必须一起看）=="
DESK_OK=0
# 干净的窗口：上一阶段结束时面板可能挂着一条红字提示，那会把按钮行顶下去，
# 按固定相对坐标点就会落空（第二版就吃了这个假红）。
for p in $(pgrep -x uulan-gui); do kill "$p" 2>/dev/null; done
sleep 1
setsid nohup "$GUI" >/tmp/uulan-gui-desk-$RID.log 2>&1 < /dev/null &
sleep 6
PID=$(pgrep -x uulan-gui | head -1)
WID=$(xdotool search --pid "$PID" 2>/dev/null | head -1)
if [ -z "${WID:-}" ]; then
  skip "新面板没有窗口，跳过桌面模式回归"
else
  xdotool windowsize "$WID" 1100 780; sleep 2
  eval "$(xdotool getwindowgeometry --shell "$WID")"
  xdotool mousemove $((X + 110)) $((Y + 355)) click 1
  for _ in $(seq 1 25); do sleep 1; pgrep -x uulan-client >/dev/null && break; done
  if pgrep -x uulan-client >/dev/null; then
    ok "桌面模式起了控制端进程"
    # 画面统计是**会话收尾时**才落盘的（`窗口已关闭。收到 AU … 解出帧 …`），
    # 所以先让它跑够十几秒，再关窗，然后才读数 —— 中途读日志只会读到「已连上」。
    sleep 14
    CWID=$(xdotool search --pid "$(pgrep -x uulan-client | head -1)" 2>/dev/null | tail -1)
    if [ -n "${CWID:-}" ] && command -v wmctrl >/dev/null; then wmctrl -ic "$CWID"; fi
    for _ in $(seq 1 10); do sleep 1; pgrep -x uulan-client >/dev/null || break; done
    if pgrep -x uulan-client >/dev/null; then
      # 台架收尾：WM 关不掉就直接结束它（这一步验的不是关窗，别让它挡住汇总）
      for p in $(pgrep -x uulan-client); do kill "$p" 2>/dev/null; done
      sleep 2
    fi
    SESS=$(sed -E 's/\x1b\[[0-9;]*m//g' "$CFG/gui-session.log" 2>/dev/null | grep "窗口已关闭")
    D=$(echo "$SESS" | grep -oE "解出帧 [0-9]+" | tail -1 | awk '{print $2}')
    DR=$(echo "$SESS" | grep -oE "丢帧 [0-9]+" | tail -1 | awk '{print $2}')
    if [ -n "${D:-}" ] && [ "${D:-0}" -gt 50 ] && [ "${DR:-99}" -le 5 ]; then
      ok "桌面模式解出 $D 帧 / 丢帧 ${DR:-?}（收尾统计）"
    else
      bad "桌面模式统计不达标：${SESS:-（日志里没有收尾统计）}"
    fi
  else
    bad "桌面模式点了之后没有控制端进程"
  fi
fi

echo "== 汇总 =="
echo "  通过 $PASS / 失败 $FAIL / 跳过 $SKIP（标记文件 $MARK，收尾会删）"
[ "$FAIL" = "0" ] && exit 0 || exit 1
