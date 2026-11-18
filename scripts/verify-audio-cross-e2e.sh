#!/usr/bin/env bash
# 跨机下行「首包延迟与 Windows 播放末端段」（Windows 控制端 ← 麒麟被控端）
#
# 为什么不是声学判据：这台 Windows 的 dshow 录音枚举为空（11-16/11-18 两轮真机批实测），
# 「从信宿 monitor 把声音录回来」那招只在麒麟侧成立。本台架改用**自报锚点 + 对表法**：
#   · 机内两段 = 共享 pump 自报的「实时播放首包已播完 dur_ms=…」与「播放末端统计
#     首包解码=Xms」（同一台机器内的差值，根本不需要对表）；
#   · 跨机总段 = 被控端「音频已开始（Opus/Ogg）」（麒麟钟）→ 控制端「首包已播完」（本机钟），
#     两机时钟差用 scripts/clock-offset.py（常驻 ssh 往返，局域网 ±0.01 s 量级）。
# 它钉住的是「第一块音频什么时候在播放器上放完」；那之后的「出声到耳朵」已由
# 11-18 麒麟 monitor 台架量到 1.2 s 量级，两条合起来才是完整的延迟图。
#
# 用法（开发机 git-bash，开发机需与真机同二层）：
#   UULAN_CLIENT=D:/uulan-bin/uulan-client.exe bash scripts/verify-audio-cross-e2e.sh
# 可覆盖：KYLIN_TARGET=kylin KYLIN_IP=192.168.223.15 UULAN_CROSS_PORT=21995 UULAN_CROSS_SECS=18
# 退出码：0 = 全过；1 = 断言失败（产品问题）；2 = 台架/环境给不出时间基或物料（别记成产品缺陷）。
set -u

KY="${KYLIN_TARGET:-kylin}"
KY_IP="${KYLIN_IP:-192.168.223.15}"
PORT="${UULAN_CROSS_PORT:-21995}"
RUN_SECS="${UULAN_CROSS_SECS:-18}"
CLI="${UULAN_CLIENT:?要 UULAN_CLIENT=<工作区外的 Windows 控制端 exe，D:/ 形式>}"
# 原生 python 只认 D:/ 形式（坑 4 第 N+1 次复发）：一律 cygpath -m 换算。
REPO="$(cygpath -m "$(cd "$(dirname "$0")/.." && pwd)")"
TS=$(date +%Y%m%d-%H%M%S)
WORKW="D:/uulan-bin/cross.$TS"     # 本地物料一律 D:/ 原生形式（原生 exe/python 都只认它）
WORKK="/tmp/uulan-cross.$TS"       # 远端工作目录
NO_COLOR=1
export NO_COLOR

PASS=0; FAIL=0; SKIP=0
ok()   { echo "  [PASS] $*"; PASS=$((PASS+1)); }
bad()  { echo "  [FAIL] $*"; FAIL=$((FAIL+1)); }
skip() { echo "  [SKIP] $*"; SKIP=$((SKIP+1)); }
env_out() { echo "  [环境] $*"; echo "退出码 2：这台机器此刻给不出这条判据，别把它记成产品缺陷"; exit 2; }

mkdir -p "$WORKW" || env_out "建不了 $WORKW"
calc() { awk "BEGIN{printf \"%.4f\", $1}"; }   # 算式放程序文本里（mawk -v 截断坑，11-18 归档第 3 条）

# 日志时间戳 → epoch（tracing 默认 UTC ISO8601；取第一条命中）。两边日志都先过一层脱色。
log_epoch() { # $1=文件 $2=字样
  local ts
  ts=$(sed -E 's/\x1b\[[0-9;]*m//g' "$1" 2>/dev/null \
      | grep -aF "$2" | head -1 | sed -E 's/^([0-9T:.Z+-]+) .*/\1/')
  [ -n "$ts" ] || { echo ""; return; }
  date -d "$ts" +%s.%N 2>/dev/null || echo ""
}

# ── 远端台架脚本（落文件送过去再 bash，避开 ssh 嵌套引号被守卫判 malformed）──
cat > "$WORKW/remote.sh" <<'RSH'
#!/usr/bin/env bash
set -u
BIN="$HOME/.local/uulan/src/target/release"
WORK="$1"; PORT="$2"
mkdir -p "$WORK"
"$BIN/uulan-server" --bind "0.0.0.0:$PORT" --state "$WORK/devices.json" >"$WORK/server.log" 2>&1 &
echo $! > "$WORK/pids"
for _ in $(seq 1 100); do
  (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && { exec 3>&-; break; }
  sleep 0.1
done
RUST_LOG=info NO_COLOR=1 "$BIN/uulan-host" --server "127.0.0.1:$PORT" --config "$WORK/host.json" \
  --capture "lavfi:testsrc2=size=640x480:rate=15" \
  --audio-source "lavfi:sine=frequency=440:sample_rate=48000" \
  --name crossbench >"$WORK/host.log" 2>&1 &
echo $! >> "$WORK/pids"
dev=""
for _ in $(seq 1 300); do
  dev=$(grep -oE "设备 ID : [0-9]{6,}" "$WORK/host.log" 2>/dev/null | tail -1 | sed 's/.*: *//' | tr -d '[:space:]')
  [ -n "$dev" ] && break
  sleep 0.1
done
[ -n "$dev" ] || exit 1
echo "$dev" > "$WORK/dev"
# 口令只落 0600 文件（在远端就地抽取，绝不上屏）
sed -n 's/.*"password": *"\([^"]*\)".*/\1/p' "$WORK/host.json" | head -1 > "$WORK/pw"
chmod 600 "$WORK/host.json" "$WORK/pw"
[ -s "$WORK/pw" ] || exit 1
touch "$WORK/ready"
RSH
cat > "$WORKW/clean.sh" <<'CLS'
#!/usr/bin/env bash
while read -r pid; do kill "$pid" 2>/dev/null; done < "$1/pids"
CLS

cleanup() { ssh "$KY" "bash $WORKK/clean.sh $WORKK" 2>/dev/null; python -c "import os,sys;[os.remove(p) for p in sys.argv[1:] if os.path.exists(p)]" "$WORKW/pw.txt" "$WORKW/pw.raw" 2>/dev/null; return 0; }
trap cleanup EXIT

echo "== 0. 前置：真机可达 + 物料 =="
ssh -o ConnectTimeout=6 "$KY" true >/dev/null 2>&1 || env_out "$KY 不可达（先按失联判法分辨，别记产品失败）"
[ -x "$CLI" ] || [ -f "$CLI" ] || env_out "控制端 exe 不存在：$CLI（先 cargo build --release 再拷出工作区）"
ok "通道与物料在（$KY / $(basename "$CLI")）"
ssh "$KY" "mkdir -p $WORKK" || env_out "远端建不了 $WORKK"
scp -q "$WORKW/remote.sh" "$WORKW/clean.sh" "$KY:$WORKK/" || env_out "scp 台架脚本上不去"

echo "== 1. 起远端台架（中继绑 0.0.0.0:$PORT，被控端 lavfi 440Hz）=="
ssh "$KY" "bash $WORKK/remote.sh $WORKK $PORT" || env_out "远端台架起不来（看 $WORKK/host.log）"
R=1
for _ in $(seq 1 50); do
  R=$(ssh "$KY" "[ -f $WORKK/ready ] && echo YES || echo NO" 2>/dev/null | tail -1)
  [ "$R" = "YES" ] && break
  sleep 0.4
done
[ "$R" = "YES" ] || env_out "台架没就绪（$WORKK/host.log 前几行）"
DEV=$(ssh "$KY" "cat $WORKK/dev" 2>/dev/null | grep -oE "[0-9]{6,}" | head -1)
[ -n "$DEV" ] || env_out "拿不到设备 ID"
# 口令走 ssh 重定向进 0600 文件，不上屏、不进本仓库任何输出。
# 先落原始文件再抽合格行 —— banner/motd 混进来时绝不让垃圾行进 --password-file。
ssh "$KY" "cat $WORKK/pw" > "$WORKW/pw.raw" 2>/dev/null
grep -aoE '^[A-Za-z0-9]{8,16}$' "$WORKW/pw.raw" | head -1 > "$WORKW/pw.txt"
rm -f "$WORKW/pw.raw"
chmod 600 "$WORKW/pw.txt"
[ -s "$WORKW/pw.txt" ] || env_out "口令不合格式（传输被污染？）"
ok "台架就绪（设备 $DEV；口令已落 0600 文件）"

echo "== 2. 对表（常驻 ssh 往返，跑前/跑后各一轮）=="
O1=$(python "$REPO/scripts/clock-offset.py" "$KY" 25) || true
U1=$(echo "$O1" | python -c 'import json,sys;print(json.load(sys.stdin)["unc_s"])' 2>/dev/null) || env_out "对表脚本没出数：$O1"
OK1=$(echo "$O1" | python -c 'import json,sys;print(json.load(sys.stdin)["offset_s"])')
python -c "import sys; sys.exit(0 if float('$U1')<=0.05 else 1)" \
  && ok "对表不确定度 ${U1}s（≤0.05）：$O1" || env_out "对表不确定度 ${U1}s 太大，跨机数值给不出（$O1）"

echo "== 3. Windows 控制端连过去播（--audio → waveOut，${RUN_SECS}s）=="
RUST_LOG=info timeout 120 "$CLI" --server "$KY_IP:$PORT" --device "$DEV" \
  --password-file "$WORKW/pw.txt" --headless --seconds "$RUN_SECS" \
  --no-input --no-clipboard --audio > "$WORKW/client.log" 2>&1
CRC=$?
sed -E 's/\x1b\[[0-9;]*m//g' "$WORKW/client.log" > "$WORKW/client.plain"
[ "$CRC" -eq 0 ] && ok "控制端正常收工（退出码 0）" || bad "控制端退出码 $CRC"
ssh "$KY" "cat $WORKK/host.log" > "$WORKW/host.raw" 2>/dev/null
sed -E 's/\x1b\[[0-9;]*m//g' "$WORKW/host.raw" > "$WORKW/host.plain"

O2=$(python "$REPO/scripts/clock-offset.py" "$KY" 25) || true
OK2=$(echo "$O2" | python -c 'import json,sys;print(json.load(sys.stdin)["offset_s"])' 2>/dev/null) || OK2=""
if [ -n "$OK2" ]; then
  DRIFT=$(calc "$OK2 - $OK1"); ABS=${DRIFT#-}
  python -c "import sys; sys.exit(0 if float('$ABS')<=0.05 else 1)" \
    && ok "跑后复测漂移 ${DRIFT}s（≤0.05，两机钟稳）" || bad "对表漂移 ${DRIFT}s —— 本轮跨机数值存疑"
else
  bad "跑后对表没出数"
fi

echo "== 4. 锚点与判据 =="
grep -aF "打不开音频输出设备" "$WORKW/client.plain" >/dev/null \
  && bad "waveOut 没开成（只解码不播放）—— Windows 侧末端段本轮测不了" \
  || ok "waveOut 真开了（没有「打不开音频输出设备」）"
grep -aF "实时播放已接上 program=waveOut" "$WORKW/client.plain" >/dev/null \
  && ok "锚：实时播放已接上 program=waveOut" || bad "缺「实时播放已接上 program=waveOut」锚点"

T1=$(log_epoch "$WORKW/host.plain" "音频已开始（Opus/Ogg）")
T3=$(log_epoch "$WORKW/client.plain" "实时播放首包已播完")
[ -n "$T1" ] && ok "锚：被控端「音频已开始」" || bad "缺被控端「音频已开始（Opus/Ogg）」锚点"
[ -n "$T3" ] && ok "锚：控制端「实时播放首包已播完」" || bad "缺控制端「实时播放首包已播完」锚点"

DUR=$(grep -aF "实时播放首包已播完" "$WORKW/client.plain" | head -1 | grep -oE "dur_ms=[0-9]+" | head -1 | cut -d= -f2)
TAIL=$(grep -aF "播放末端统计" "$WORKW/client.plain" | head -1)
DEC=$(echo "$TAIL" | grep -oE "首包解码=[0-9]+" | head -1 | cut -d= -f2)
DONE=$(echo "$TAIL" | grep -oE "首包播完=[0-9]+" | head -1 | cut -d= -f2)
echo "  机内账：首包解码=${DEC:-?}ms 首包播完=${DONE:-?}ms（首块 dur=${DUR:-?}ms）"
[ -n "$DUR" ] || bad "统计行缺 dur_ms（判据器/埋点没对上）"
[ -n "$DEC" ] && [ -n "$DONE" ] && ok "播放末端统计齐（收工行可 grep）" || bad "缺「播放末端统计」行或数值解析不出"
if [ -n "$DUR" ]; then
  python -c "import sys; sys.exit(0 if 0<=int('$DUR')<=400 else 1)" \
    && ok "waveOut 首块放完 ${DUR}ms（≤400：一块 16 KiB ≈ 85ms + 轮询 5ms）" \
    || bad "waveOut 首块放完 ${DUR}ms 异常（设备挂了或被系统缓冲吞了）"
fi
if [ -n "$DEC" ]; then
  python -c "import sys; sys.exit(0 if 0<=int('$DEC')<=1500 else 1)" \
    && ok "本机解码首包 ${DEC}ms（≤1500）" || bad "本机解码首包 ${DEC}ms 超界"
fi

if [ -n "$T1" ] && [ -n "$T3" ]; then
  L=$(calc "$T3 + ($OK1) - $T1")
  SEG=$(calc "0 + $L")   # 展示用
  echo "  跨机首包延迟（音频已开始 → 首块在播放器放完）= ${L}s（对表 ±${U1}s）"
  if [ -n "$DONE" ]; then
    NET=$(calc "$L - $DONE/1000")
    echo "  分解：网络+管道+等被控端起编 = ${NET}s；本机末端段 = $DONE ms"
  fi
  python -c "import sys; sys.exit(0 if 0.0 < float('$L') < 4.0 else 1)" \
    && ok "跨机首包延迟在合理区间（0 < ${L} < 4 s）" || bad "跨机首包延迟 ${L}s 出界"
  python -c "import sys; sys.exit(0 if float('$SEG') > 0.03 else 1)" \
    && ok "跨机段非零可分辨（>30ms，不是对表噪声）" || skip "跨机段小于对表噪声，本轮分不出（判据器极限）"
else
  bad "锚点不齐，跨机延迟没算"
fi

# 收工摘要打的是**最后相位**（正常跑完是 Ended/收尾），Running 只在迁移那一刻出现 ——
# 判「到过 Running」必须 grep 迁移行，判「被控端真的在处理」看它回过的条数。
grep -aF "被控端正在推流" "$WORKW/client.plain" >/dev/null \
  && ok "音频到过「被控端正在推流」（AudioWatch 见过 running:true）" || bad "音频状态从没到 Running"
FAILHOST=$(grep -acF "起不来" "$WORKW/host.plain")
[ "${FAILHOST:-0}" -eq 0 ] && ok "被控端无「起不来」" || bad "被控端音频起不来（$FAILHOST 条）"
echo "== 3b. 上行跨机（--mic，Windows 侧 lavfi 880Hz → 麒麟 aplay，${RUN_SECS}s）=="
RUST_LOG=info timeout 120 "$CLI" --server "$KY_IP:$PORT" --device "$DEV" \
  --password-file "$WORKW/pw.txt" --headless --seconds "$RUN_SECS" \
  --no-input --no-clipboard \
  --mic --mic-source "lavfi:sine=frequency=880:sample_rate=48000" > "$WORKW/client-up.log" 2>&1
URC=$?
sed -E 's/\x1b\[[0-9;]*m//g' "$WORKW/client-up.log" > "$WORKW/client-up.plain"
[ "$URC" -eq 0 ] && ok "上行会话控制端正常收工（退出码 0）" || bad "上行会话控制端退出码 $URC"
ssh "$KY" "cat $WORKK/host.log" > "$WORKW/host2.raw" 2>/dev/null
sed -E 's/\x1b\[[0-9;]*m//g' "$WORKW/host2.raw" > "$WORKW/host2.plain"
# 锚点变量带 UP 前缀：U1 在上面已是对表不确定度（同名互相覆盖是台架的老事故族）。
UPA=$(log_epoch "$WORKW/client-up.plain" "麦克风转接已开始")
UPE=$(log_epoch "$WORKW/host2.plain" "实时播放首包已播完")
[ -n "$UPA" ] && ok "锚：控制端「麦克风转接已开始」" || bad "缺「麦克风转接已开始」锚点"
[ -n "$UPE" ] && ok "锚：被控端「实时播放首包已播完」（上行也有机内账了）" || bad "缺被控端首包播完锚点"
if [ -n "$UPA" ] && [ -n "$UPE" ]; then
  LU=$(calc "$UPE - $UPA - ($OK1)")
  UTAIL=$(grep -aF "播放末端统计" "$WORKW/host2.plain" | tail -1 | grep -oE "首包播完=[0-9]+" | cut -d= -f2)
  echo "  跨机上行为时（转接已开始 → 被控端首块在 aplay 上放完）= ${LU}s（对表 ±${U1}s）"
  [ -n "$UTAIL" ] && echo "  其中麒麟机内末端段 = ${UTAIL}ms（aplay 首块含唤醒）"
  python -c "import sys; sys.exit(0 if 0.0 < float('$LU') < 6.0 else 1)" \
    && ok "跨机上行为时合理（0 < ${LU} < 6 s）" || bad "跨机上行为时 ${LU}s 出界"
else
  bad "上行锚点不齐，跨机上行数值没算"
fi


echo
echo "结论：$PASS 通过 / $FAIL 失败 / $SKIP 跳过（本地 $WORKW 远端 $WORKK）"
rm -f "$WORKW/pw.txt" "$WORKW/pw" 2>/dev/null
[ "$FAIL" -eq 0 ] && exit 0 || exit 1
