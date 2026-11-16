#!/usr/bin/env bash
# 验「多屏选屏」：`--capture x11::<显示>@<显示器号>` 只抓/只落那一块屏（抓取区域 + 注入原点）。
#
# 为什么不能直接拿现网验「两块屏」：麒麟真机 `xrandr` 里所有输出都是 disconnected（裸帧缓冲），
# 显示器热插拔在无人值守下做不了。所以分三段：
#
#   A) **抓取偏移与注入原点（机理）**：真 Xvfb :97，帧缓冲 2048x768（= 两块 1024x768 拼成的
#      扩展桌面），用 `xrandr --query` 的 PATH shim 列出「VIRTUAL1 +0+0 / VIRTUAL2 +1024+0」。
#      被测的 x11grab `+X,Y` 偏移、XTest 落点、pick_region 降级规则全是**真实代码**，
#      只有布局枚举这一小步是合成数据 —— 报告里要照实这么写。
#      判据：
#        · 被控端「初始抓取区域已确定 x=1024 w=1024」（region 挑中了 1 号块）
#        · 控制端存帧 vs `:97+1024,0` 截图逐像素相关 ≥0.99（真抓到了右半）
#        · 控制端存帧 vs `:97+0,0` 截图相关 **低于** 0.98（反面对照：按老行为抓左半会一样过）
#        · 同会话 `movn:0.5,0.5` → `xdotool getmouselocation` 落 (1536,384)±3
#          （= 区域内 (512,384) + 原点 (1024,0)；独立裁判，不是我们自己的日志）
#        · 反面对照：落点离「没加原点」的 (512,384) >100px
#   B) **号超界退回第一块**（副屏被拔掉的样子）：同测试台 `@7`（表里只有 0/1）→ region 应为
#      monitors[0]：x=0 且**宽 1024**（与「不指定 = 整帧缓冲 2048」分得开），落点 (512,384)。
#   C) **空表降级（现网真实形态）**：真 :0 + **真 xrandr** + `@1` → 列不出已连接输出 →
#      抓取区域 = 整帧缓冲、会话不断；`--diagnose` 打 `[WARN] 显示器 … 按整帧缓冲抓`、
#      退出码 0（WARN 不算 FAIL —— 同现场自检那条纪律）。C 不注入键鼠（:0 可能锁着，
#      锁屏门禁会丢输入；而且不该动用户桌面的指针）。
#
# 全程用测试台自己的 relay(:$RPORT)、独立 host.json 与设备身份，
# 绝不动 ~/.config/uulan/host.json 与正式服务。口令经 ssh 标记过滤落本地 0600 文件，绝不上屏。
#
# 用法（Windows 侧编排，真机侧全部走 ssh）：
#   bash scripts/verify-monitor-select-e2e.sh
# 环境变量：UULAN_CLIENT（默认 /d/uulan-bin/uulan-client.exe）/ UULAN_SSH / UULAN_SCP
#           UULAN_BIN（真机侧二进制目录，默认 ~/.local/uulan/src/target/release）
#           UULAN_RELAY（默认 192.168.223.15:21997）/ UULAN_WORK（默认 /d/uulan-bin）
#           UULAN_TOL（落点容差 px，默认 3）
# 退出码：0 = 全过；1 = 有断言不达标；2 = 环境做不了（连不上真机 / 缺工具 / 缺参数）
set -uo pipefail

CLIENT=${UULAN_CLIENT:-/d/uulan-bin/uulan-client.exe}
SSH=${UULAN_SSH:-ssh -o ConnectTimeout=12 kylin}
SCP=${UULAN_SCP:-scp -q}
RELAY=${UULAN_RELAY:-192.168.223.15:21997}
RPORT=21997
WORK=${UULAN_WORK:-/d/uulan-bin}
HERE=$(cd "$(dirname "$0")" && pwd)
TS=$(date +%Y%m%d_%H%M%S)
RDIR="/tmp/uulan-monitor.$TS"          # 真机侧独立测试台目录（带时间戳；旧的不删）
XD=2048; YD=768; MW=1024               # Xvfb :97 帧缓冲与「每块屏」尺寸
RIGHT_X=$((XD - MW))                   # 右半块原点 = +1024
TOL=${UULAN_TOL:-3}
# BIN 是要**拼进远端命令串**的，所以默认值里的家目录必须留给远端 shell 展开。
# 写成 ~ 会被本地 bash 先展开成 MSYS 家目录（/c/Users/xxx），远端路径当场作废 ——
# 表现为「真机上没有当前二进制」的假 FAIL，只有手动传 UULAN_BIN 时才跑得动。
BIN=${UULAN_BIN:-\$HOME/.local/uulan/src/target/release}

# 原生程序只认 C:/ D:/，MSYS 工具只认 /d/ —— 两套路径都备着。
WORKN=$(cygpath -m "$WORK" 2>/dev/null || echo "$WORK")
HEREN=$(cygpath -m "$HERE" 2>/dev/null || echo "$HERE")
mkdir -p "$WORK"

pass=0; fail=0; skip=0
ok()  { pass=$((pass+1)); echo "  PASS $1"; }
bad() { fail=$((fail+1)); echo "  FAIL $1"; }
skp() { skip=$((skip+1)); echo "  SKIP $1"; }
chk() { if [ "$2" = "1" ]; then ok "$1"; else bad "$1（实际：$3）"; fi; }
isnear() {
  local got=$1 want=$2 tol=${3:-$TOL}
  if [ -z "$got" ] || [ -z "$want" ]; then echo 0; return; fi
  set -- $got; local a=$1 b=$2
  set -- $want; local x=$1 y=$2
  local dx=$((a - x)); local dy=$((b - y))
  if [ "${dx#-}" -le "$tol" ] && [ "${dy#-}" -le "$tol" ]; then echo 1; else echo 0; fi
}
isfar() {
  local got=$1 want=$2 min=${3:-100}
  if [ -z "$got" ] || [ -z "$want" ]; then echo 0; return; fi
  set -- $got; local a=$1 b=$2
  set -- $want; local x=$1 y=$2
  local dx=$((a - x)); local dy=$((b - y))
  if [ $((dx * dx + dy * dy)) -gt $((min * min)) ]; then echo 1; else echo 0; fi
}
# ssh 的 stdout 混着 motd（本仓库第 3 次踩）：要「一行结果」的都在远端管道里数完，
# 要落盘的都用 DEVID=/PW= 标记过滤，绝不按行号猜。
rssh() { $SSH "$1" 2>/dev/null | tr -d '\r'; }
rlast() { rssh "$1" | tail -1; }
ppm_size() { python -c "
import sys
f=open(sys.argv[1],'rb')
assert f.readline().strip()==b'P6'
l=f.readline().split()
while len(l)<2: l+=f.readline().split()
print('%sx%s'%(l[0].decode(),l[1].decode()))" "$1" 2>/dev/null | tr -d '\r'; }
corr_of() { python "$HEREN/compare-ppm.py" "$1" "$2" 2>/dev/null | tr -d '\r' | awk '/相关系数/{print $3}'; }

[ -x "$CLIENT" ] || { echo "FAIL 找不到控制端 $CLIENT（先 cargo build --release，再拷出工作区）"; exit 2; }

echo "== 0) 真机连通与测试台（Xvfb :97 ${XD}x${YD} + xrandr shim + 双 xev 标记）=="
rlast "echo READY" | grep -q READY || { echo "FAIL 连不上真机（ssh kylin）"; exit 2; }
for c in Xvfb xev xdotool ffmpeg python3; do
  rlast "command -v $c" | grep -q "^/" || { echo "  SKIP 真机缺 $c，这条验做不了"; exit 2; }
done
rlast "test -x $BIN/uulan-host && test -x $BIN/uulan-server && echo HAVE" | grep -q HAVE || {
  echo "FAIL 真机上没有 $BIN 下的当前二进制（先从 HEAD 部署并 cargo build --release -p uulan-server -p uulan-host）"; exit 2; }

# 测试台的进程特征（供开跑前清扫与收尾判据共用）。
# 方括号写法 [X]vfb 让模式**不匹配执行这条命令的 shell 自己** —— 否则 pgrep/grep 会
# 把自己数进去，收尾永远报「残留 1~3 条」的假失败（2026-11-16 撞过）。
BENCH="pgrep -f '[/]tmp/uulan-monitor\.'; pgrep -f '[X]vfb :97'; pgrep -f '[x]ev -geometry 300x200'"

# 开跑前扫陈旧测试台：上一轮若中途死掉（脚本 bug / 超时 / Ctrl-C），Xvfb :97 和两个 xev
# 会留在真机上，新一轮 Xvfb 因显示号被占起不来，故障会被误报成「被控端 A 没起来」。
STALE=$(rlast "{ $BENCH; } | sort -u | wc -l")
if [ "${STALE:-0}" != "0" ]; then
  echo "  提示：真机有 ${STALE} 个上一轮的测试台残留，先清掉（只按测试台特征匹配，不碰生产服务）"
  rlast "{ $BENCH; } | sort -u | xargs -r kill 2>/dev/null; sleep 2; { $BENCH; } | sort -u | xargs -r kill -9 2>/dev/null; echo SWEPT" >/dev/null
fi

rssh "mkdir -p $RDIR/bin $RDIR/cfg" >/dev/null
# 布局 shim：真 Xvfb 的 xrandr 不列已连接输出，这里造「两块屏并排」（仅此一步是合成数据）。
$SSH "cat > $RDIR/bin/xrandr && chmod +x $RDIR/bin/xrandr" <<SHIM
#!/bin/sh
echo "Screen 0: minimum 1 x 1, current ${XD} x ${YD}, maximum ${XD} x ${YD}"
echo "VIRTUAL1 connected primary ${MW}x${YD}+0+0 (normal left inverted right x axis y axis) 0mm x 0mm"
echo "   ${MW}x${YD}     60.00*+"
echo "VIRTUAL2 connected ${MW}x${YD}+${RIGHT_X}+0 (normal left inverted right x axis y axis) 0mm x 0mm"
echo "   ${MW}x${YD}     60.00*"
SHIM
NCONN=$(rlast "$RDIR/bin/xrandr --query | grep -c ' connected '")
chk "shim 列出 2 块已连接输出" "$([ "${NCONN:-0}" = "2" ] && echo 1)" "${NCONN:-0}"

rlast "setsid nohup Xvfb :97 -screen 0 ${XD}x${YD}x24 -nolisten tcp > $RDIR/xvfb.log 2>&1 < /dev/null & echo \$! > $RDIR/xvfb.pid; echo OK" >/dev/null
FB97=""
for i in $(seq 1 30); do
  FB97=$(rlast "DISPLAY=:97 xdpyinfo 2>/dev/null | awk '/dimensions:/{print \$2}'")
  [ -n "$FB97" ] && break; sleep 0.3
done
[ "$FB97" = "${XD}x${YD}" ] || { echo "FAIL Xvfb :97 帧缓冲没起来（读到 ${FB97:-空}）"; exit 2; }
ok "Xvfb :97 帧缓冲 $FB97"
# 两块 xev 标记窗各占一半（互不重叠）：左右半区像素上有分别，反面断言才成立。
rlast "setsid nohup env DISPLAY=:97 xev -geometry 300x200+150+250 > $RDIR/xev-l.log 2>&1 < /dev/null & echo \$! > $RDIR/xev-l.pid; echo OK" >/dev/null
rlast "setsid nohup env DISPLAY=:97 xev -geometry 300x200+1250+250 > $RDIR/xev-r.log 2>&1 < /dev/null & echo \$! > $RDIR/xev-r.pid; echo OK" >/dev/null
sleep 1.5
NW=$(rlast "DISPLAY=:97 xdotool search --name 'Event Tester' 2>/dev/null | grep -c .")
chk "两块 xev 标记窗都在" "$([ "${NW:-0}" -ge 2 ] && echo 1)" "${NW:-0}"
rlast "setsid nohup $BIN/uulan-server --bind 0.0.0.0:$RPORT --state $RDIR/cfg/devices.json > $RDIR/server.log 2>&1 < /dev/null & echo \$! > $RDIR/server.pid; echo OK" >/dev/null
sleep 1.5

kill_pid() { local p; p=$(rlast "cat $RDIR/$1.pid 2>/dev/null"); [ -n "$p" ] && rssh "kill $p 2>/dev/null" >/dev/null; }

# start_host <名字> <capture spec> <要不要 shim:1/0> → 打印 device_id（空 = 没起来）
start_host() {
  local nm=$1 spec=$2 shim=$3 pre=""
  [ "$shim" = "1" ] && pre="PATH=$RDIR/bin:\$PATH "
  rlast "setsid nohup env $pre NO_COLOR=1 $BIN/uulan-host --config $RDIR/cfg/$nm.json --capture '$spec' --server 127.0.0.1:$RPORT --name MonSel-$nm > $RDIR/$nm.log 2>&1 < /dev/null & echo \$! > $RDIR/$nm.pid; echo OK" >/dev/null
  local dev=""
  for i in $(seq 1 24); do
    dev=$($SSH "python3 -c \"import json;print('DEVID='+json.load(open('$RDIR/cfg/$nm.json'))['device_id'])\"" 2>/dev/null | tr -d '\r' | grep -a 'DEVID=' | head -1 | sed 's/.*DEVID=//')
    [ -n "$dev" ] && break; sleep 0.5
  done
  echo "$dev"
}

# 测试台自己的口令 → 本地 0600 文件（标记过滤，绝不上屏；拿不到打印空串）
grab_pw() {
  # 必须分两条：bash 先把整条 `local` 的词表展开完再赋值，所以
  # `local nm=$1 f="…$nm…"` 里的 $nm 取的是**外层**同名变量（这里没定义，set -u 直接炸）。
  local nm=$1 pw=""
  local f="$WORK/ms-$nm.pw"
  for i in $(seq 1 10); do
    pw=$($SSH "python3 -c \"import json;print('PW='+json.load(open('$RDIR/cfg/$nm.json'))['password'])\"" 2>/dev/null | tr -d '\r' | grep -a 'PW=' | head -1 | sed 's/.*PW=//')
    [ -n "$pw" ] && break; sleep 0.5
  done
  [ -n "$pw" ] || { echo ""; return; }
  printf '%s\n' "$pw" > "$f"
  chmod 600 "$f" 2>/dev/null
  cygpath -m "$f" 2>/dev/null || echo "$f"
}

# 会话编排：headless + 存帧 (+ 现场注入)。等「注入换算」行出现后 0.45s 读指针 ——
# 场景是静态的（Xvfb 上没有别的东西动指针），读回来就是那一步的结果。
POINTER=""; CLI_LOG=""; RC=1
run_session() { # run_session <device> <pwf> <存帧名> <注入脚本|-> [秒]
  local dev=$1 pwf=$2 fr=$3 inj=$4 secs=${5:-12}
  CLI_LOG="$WORK/ms-cli-$fr.log"; : > "$CLI_LOG"
  POINTER=""
  local extra=()
  [ "$inj" != "-" ] && extra=(--inject-live "$inj")
  NO_COLOR=1 RUST_LOG=info "$CLIENT" --server "$RELAY" --device "$dev" --password-file "$pwf" \
    --headless --seconds "$secs" --no-clipboard --save-frame "$WORKN/ms-$fr.ppm" \
    "${extra[@]}" > "$CLI_LOG" 2>&1 &
  local pid=$!
  if [ "$inj" != "-" ]; then
    for _ in $(seq 1 70); do
      grep -qa '注入换算 :' "$CLI_LOG" 2>/dev/null && break
      kill -0 $pid 2>/dev/null || break
      sleep 0.4
    done
    sleep 0.45
    POINTER=$(rssh "DISPLAY=:97 xdotool getmouselocation --shell" | awk -F= '/^X=/{x=$2}/^Y=/{y=$2}END{if(x!=""&&y!="")print x" "y}')
  fi
  wait $pid
  RC=$?
}

# shot <名字> <x11grab spec 例 :97+1024,0> <WxH> → 本地文件路径（拿不到打印空串）
shot() {
  local nm=$1 spec=$2 wh=$3
  rlast "ffmpeg -loglevel error -f x11grab -video_size $wh -i $spec -frames:v 1 -y $RDIR/shot-$nm.ppm; echo RC=\$?" | grep -q 'RC=0' || { echo ""; return; }
  $SCP "kylin:$RDIR/shot-$nm.ppm" "$WORK/ms-shot-$nm.ppm" >/dev/null 2>&1 || { echo ""; return; }
  echo "$WORKN/ms-shot-$nm.ppm"
}

echo
echo "== A) @1 抓右半块：画面与落点都按原点 (1024,0) 走 =="
DEV_A=$(start_host A "x11::97@1" 1); PWA=$(grab_pw A)
if [ -z "$DEV_A" ] || [ -z "$PWA" ]; then echo "FAIL 测试被控端 A 没起来"; rlast "tail -5 $RDIR/A.log"; exit 1; fi
REG=$(rlast "grep -a '初始抓取区域已确定' $RDIR/A.log" | sed -E 's/.*x=([-0-9]+) y=([-0-9]+) w=([0-9]+) h=([0-9]+).*/\1 \2 \3 \4/')
chk "A 抓取区域 = $RIGHT_X 0 $MW $YD（挑中 1 号块）" "$([ "$REG" = "$RIGHT_X 0 $MW $YD" ] && echo 1)" "[$REG]"
run_session "$DEV_A" "$PWA" A "wait:4500; movn:0.5,0.5; wait:1500" 14
chk "A 会话 rc=0（--seconds 正常收工）" "$([ "$RC" = "0" ] && echo 1)" "rc=$RC"
chk "A 落点 ≈ (1536,384) = 区域内(512,384)+原点(1024,0)，独立裁判 xdotool" "$(isnear "$POINTER" "1536 384")" "[$POINTER]"
chk "A 落点**不是**没加原点的 (512,384)（反面对照 >100px）" "$(isfar "$POINTER" "512 384" 100)" "[$POINTER]"
AMW=$(tr -d '\r' < "$CLI_LOG" | grep -a '注入换算 :' | tail -1 | sed -E 's/.*画幅=([0-9]+x[0-9]+).*/\1/')
chk "A 注入换算自报画幅 = ${MW}x${YD}（控制端那半按区域尺寸走）" "$([ "$AMW" = "${MW}x${YD}" ] && echo 1)" "[$AMW]"
FS=$(ppm_size "$WORKN/ms-A.ppm")
chk "A 存帧尺寸 = ${MW}x${YD}" "$([ "$FS" = "${MW}x${YD}" ] && echo 1)" "${FS:-无}"
SHOTR=$(shot RA ":97+${RIGHT_X},0" "${MW}x${YD}")
SHOTL=$(shot LA ":97+0,0" "${MW}x${YD}")
if [ -n "$SHOTR" ] && [ -n "$SHOTL" ] && [ -s "$WORK/ms-A.ppm" ]; then
  CR=$(corr_of "$WORKN/ms-A.ppm" "$SHOTR"); CL=$(corr_of "$WORKN/ms-A.ppm" "$SHOTL")
  chk "A 存帧 vs 右半截图 相关 ≥0.99（=$CR）" "$(awk -v c="${CR:-0}" 'BEGIN{print (c+0>=0.99)?1:0}')" "${CR:-无}"
  chk "A 存帧 vs 左半截图 相关 <0.98（反面对照 =$CL）" "$(awk -v c="${CL:-1}" 'BEGIN{print (c+0<0.98)?1:0}')" "${CL:-无}"
else
  bad "A 截图拿不到（逐像素比对没做成）"
fi
sleep 2
EVA=$(rlast "grep -a '键鼠事件统计' $RDIR/A.log")
chk "A 被控端 inject_errors=0" "$(printf '%s' "$EVA" | grep -aq 'inject_errors=0' && echo 1)" "[$EVA]"

echo
echo "== B) 号超界（副屏被拔掉的样子）：@7 → 退回 0 号块，会话不断 =="
DEV_B=$(start_host B "x11::97@7" 1); PWB=$(grab_pw B)
if [ -z "$DEV_B" ] || [ -z "$PWB" ]; then echo "FAIL 测试被控端 B 没起来"; rlast "tail -5 $RDIR/B.log"; exit 1; fi
REGB=$(rlast "grep -a '初始抓取区域已确定' $RDIR/B.log" | sed -E 's/.*x=([-0-9]+) y=([-0-9]+) w=([0-9]+) h=([0-9]+).*/\1 \2 \3 \4/')
chk "B 抓取区域退回 0 0 $MW $YD（第一块；与「不指定=整帧缓冲」宽 2048 分得开）" "$([ "$REGB" = "0 0 $MW $YD" ] && echo 1)" "[$REGB]"
run_session "$DEV_B" "$PWB" B "wait:4000; movn:0.5,0.5; wait:1000" 12
chk "B 落点 ≈ (512,384)（退回主屏，原点归 0）" "$(isnear "$POINTER" "512 384")" "[$POINTER]"
FSB=$(ppm_size "$WORKN/ms-B.ppm")
chk "B 会话不断线、出画面（存帧 ${FSB:-无}）" "$([ "$FSB" = "${MW}x${YD}" ] && echo 1)" "${FSB:-无}"

echo
echo "== C) 空表降级（现网真实形态）：真 :0 + 真 xrandr + @1 =="
FB0=$(rlast "DISPLAY=:0 xdpyinfo 2>/dev/null | awk '/dimensions:/{print \$2}'")
CONN0=$(rlast "DISPLAY=:0 xrandr --query 2>/dev/null | grep -c ' connected '")
if [ -z "$FB0" ]; then
  bad "读不到真 :0 的帧缓冲（xdpyinfo 失败）—— C 段没法钉"
elif [ "${CONN0:-0}" = "0" ]; then
  # 裸帧缓冲（机器上没挂显示器）：空表 → 按整帧缓冲抓
  WANT='按整帧缓冲抓'
else
  # 挂了真显示器：@1 越界 → 退回第一块。这条同样要钉住。
  # 2026-11-16 实测（VGA-0 1920x1080 已连接）：`[WARN] 显示器1 不存在（已连接只有 1 块）—— 退回第一块`。
  WANT='显示器1 不存在'
fi
if [ -n "$FB0" ]; then
  rssh "DISPLAY=:0 NO_COLOR=1 $BIN/uulan-host --diagnose --config $RDIR/cfg/C.json --capture 'x11::0@1' > $RDIR/diagC.txt 2>&1; echo RC=\$?" | grep -q 'RC=0' \
    && ok "C --diagnose 退出码 0（WARN 不算 FAIL）" || bad "C --diagnose 退出码非 0"
  WLINE=$(rssh "grep -a '^\\[WARN\\] 显示器' $RDIR/diagC.txt" | head -1)
  chk "C --diagnose 显示器件降级为「$WANT」（已连接 ${CONN0:-?} 块）" "$(printf '%s' "$WLINE" | grep -q "$WANT" && echo 1)" "[$WLINE]"
  # 判据坑：`grep -c` 数到 0 个匹配时退出码是 1，`|| echo 9` 会把**健康的 0 个 FAIL**
  # 变成哨兵 9 —— 于是「一切正常」被报成失败（2026-11-16 撞过）。这里显式区分
  # 「文件在不在」和「数到几个」，不再用兜底哨兵冒充数据。
  FAILN=$(rlast "if test -f $RDIR/diagC.txt; then grep -ac '\[FAIL\]' $RDIR/diagC.txt; else echo ERR; fi")
  chk "C --diagnose 没有任何 [FAIL] 项" "$([ "${FAILN:-ERR}" = "0" ] && echo 1)" "FAIL×${FAILN:-?}"
  DEV_C=$(start_host C "x11::0@1" 0); PWC=$(grab_pw C)
  if [ -n "$DEV_C" ] && [ -n "$PWC" ]; then
    run_session "$DEV_C" "$PWC" C "-" 8
    FSC=$(ppm_size "$WORKN/ms-C.ppm")
    chk "C 会话不断：存帧尺寸 = 整帧缓冲 $FB0" "$([ "$FSC" = "$FB0" ] && echo 1)" "${FSC:-无}（帧缓冲 $FB0）"
    SHOTC=$(shot C ":0+0,0" "$FB0")
    if [ -n "$SHOTC" ] && [ -s "$WORK/ms-C.ppm" ]; then
      CC=$(corr_of "$WORKN/ms-C.ppm" "$SHOTC")
      chk "C 存帧 vs :0 截图 相关 ≥0.98（现网形态抓的就是这台机器）" "$(awk -v c="${CC:-0}" 'BEGIN{print (c+0>=0.98)?1:0}')" "${CC:-无}"
    else
      bad "C 截图/存帧没拿到"
    fi
    kill_pid C
  else
    bad "C 测试被控端没起来"
  fi
fi

echo
echo "== 收尾：停测试台（kill 各 pid，目录留底不删）；正式服务与 :0 配置全程未动 =="
kill_pid A; kill_pid B
for f in xvfb xev-l xev-r server; do kill_pid $f; done
sleep 1
LEFT=$(rlast "{ $BENCH; } | sort -u | wc -l")
chk "测试台进程清干净（真机无 monitor / Xvfb:97 / xev 残留，生产服务不在判据里）" "$([ "${LEFT:-9}" = "0" ] && echo 1)" "残留 ${LEFT:-?} 条"

echo
echo "结果：$pass 通过 / $fail 失败 / $skip 跳过"
echo "测试台留底：kylin:$RDIR（日志/配置/截图都在，可回看）"
[ "$fail" -eq 0 ] || exit 1
exit 0
