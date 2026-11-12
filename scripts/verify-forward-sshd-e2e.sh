#!/usr/bin/env bash
# 验 `--forward` 过真麒麟 sshd 的实际表现 —— 补 DESIGN.md「只能在真麒麟上验」两格：
#   ① 连被控端本机 sshd 的**长连接**（先有节奏心跳，再完全静默 IDLE 秒，最后同一条
#      连接还要能说话：这中间 TCP 一个字节都不走，最能暴露「半开没察觉」）；
#   ② 内网真实链路（非回环）下的**吞吐与完整性**：经隧道 scp 64 MiB，逐字节 sha256。
# 本机 loopback 测试（portmap_path.rs 4 条 + verify-portmap-e2e.sh）钉的是语义，
# 这两格钉的是「真链路上跑得起来」——DESIGN 一直写着「只有真机能回答」，从没跑过。
#
# 判据全部是独立可查的量：隧道内外 hostname 对照、退出码原样传回、
# 心跳条数与 FINAL 标记、传输耗时与两端 sha256、客户端收尾统计（连接数 / 上下行字节）。
#
# 用法：
#   UULAN_DEVICE=<ID> UULAN_PWFILE=<口令文件> bash scripts/verify-forward-sshd-e2e.sh
# 环境变量：UULAN_CLIENT / UULAN_SERVER / UULAN_SSH / UULAN_WORK / UULAN_LP（本地端口，默认 22922）
#           UULAN_TICKS（心跳条数，默认 18）/ UULAN_TICK_EVERY（间隔秒，默认 30）
#           UULAN_IDLE（静默秒数，默认 120）/ UULAN_MB（传输 MiB，默认 64）
# 退出码：0 = 全部断言通过；1 = 不达标；2 = 环境做不了（缺参 / sshd 没在听 / 端口被占）
set -uo pipefail

CLIENT=${UULAN_CLIENT:-/d/uulan-bin/uulan-client.exe}
SERVER=${UULAN_SERVER:-192.168.223.15:21116}
SSH=${UULAN_SSH:-ssh -o ConnectTimeout=12 kylin}
WORK=${UULAN_WORK:-/d/uulan-bin}
LP=${UULAN_LP:-22922}
TICKS=${UULAN_TICKS:-18}
TICK_EVERY=${UULAN_TICK_EVERY:-30}
IDLE=${UULAN_IDLE:-120}
MB=${UULAN_MB:-64}
DEV=${UULAN_DEVICE:-}
PWF=${UULAN_PWFILE:-}

[ -n "$DEV" ] || { echo "FAIL 需要 UULAN_DEVICE=<设备 ID>"; exit 2; }
[ -n "$PWF" ] && [ -s "$PWF" ] || { echo "FAIL 需要 UULAN_PWFILE=<口令文件>"; exit 2; }
[ -x "$CLIENT" ] || { echo "FAIL 找不到控制端 $CLIENT（先 cargo build --release 再拷出工作区）"; exit 2; }
mkdir -p "$WORK"

# 原生 exe 只认 C:/ D:/；MSYS 工具只认 /c/ /d/ —— 两套都备着。
PWN=$(cygpath -m "$PWF" 2>/dev/null || echo "$PWF")
WORKN=$(cygpath -m "$WORK" 2>/dev/null || echo "$WORK")

pass=0; fail=0
ok()  { pass=$((pass+1)); echo "  PASS $1"; }
bad() { fail=$((fail+1)); echo "  FAIL $1"; }
chk() { if [ "$2" = "1" ]; then ok "$1"; else bad "$1（实际：$3）"; fi; }

TS=$(date +%Y%m%d_%H%M%S)
LOG="$WORK/fwd-$TS-cli.log"
HB="$WORK/fwd-$TS-heartbeat.txt"
KEYF="$HOME/.ssh/id_ed25519"
KH="$WORK/fwd-known-hosts"
SSHT="ssh -p $LP -i $KEYF -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=$KH -o ConnectTimeout=15 -o BatchMode=yes"
REMOTE=mis@127.0.0.1

FWD_PID=""
cleanup() {
  # 转发进程一定要收掉：占着本地端口与控制端会话，下一轮 cp 新 exe 会 busy。
  [ -n "$FWD_PID" ] && kill "$FWD_PID" 2>/dev/null
  return 0
}
trap cleanup EXIT

echo "== 0) 前置：真机 sshd 在听 127.0.0.1:22、本地端口空闲 =="
LISTEN=$($SSH "ss -tln 2>/dev/null | awk '{print \$4}' | grep -c '127.0.0.1:22\|:22$'" 2>/dev/null | tr -d '\r' | tail -1)
[ "${LISTEN:-0}" -ge 1 ] && ok "真机 sshd 在听 22（命中 $LISTEN 行）" || { echo "FAIL 真机 sshd 没在听 22"; exit 2; }
netstat -ano 2>/dev/null | grep -q "127.0.0.1:$LP .*LISTENING" && { echo "FAIL 本地 $LP 已被占用，换 UULAN_LP"; exit 2; }

# 直连对照（绕过隧道）：隧道内外必须看到同一个 sshd，否则后面的话都不算数。
DIRECT=$($SSH "hostname" 2>/dev/null | tr -d '\r' | tail -1)
echo "  直连对照：$DIRECT"

echo "== 1) 起转发会话（--forward $LP:127.0.0.1:22，--seconds 900）=="
: > "$LOG"
( NO_COLOR=1 "$CLIENT" --server "$SERVER" --device "$DEV" --password-file "$PWN" \
    --forward "$LP:127.0.0.1:22" --seconds 900 > "$LOG" 2>&1; echo $? > "$LOG.rc" ) &
FWD_PID=$!
for _ in $(seq 1 60); do grep -qa '转发已就绪' "$LOG" 2>/dev/null && break; sleep 0.5; done
grep -qa '转发已就绪' "$LOG" 2>/dev/null || { echo "FAIL 转发没就绪（见 $LOG）"; exit 1; }
grep -qa "端口映射：本机 127.0.0.1:$LP" "$LOG" 2>/dev/null && ok "转发就绪并已报映射行" || bad "少了映射行"

echo "== 2) 同 sshd 对照 + 退出码经隧道原样传回 =="
TUNNEL=$($SSHT -o BatchMode=yes $REMOTE "hostname" 2>/dev/null | tr -d '\r' | tail -1)
chk "隧道里 hostname == 直连（$TUNNEL）" "$([ -n "$TUNNEL" ] && [ "$TUNNEL" = "$DIRECT" ] && echo 1)" "$TUNNEL"
$SSHT $REMOTE "exit 9" >/dev/null 2>&1; RC9=$?
chk "远端 exit 9 → 本地退出码 9（半关闭在真链路映射正确）" "$([ "$RC9" = "9" ] && echo 1)" "rc=$RC9"

echo "== 3) 长连接：${TICKS} 拍心跳（每 ${TICK_EVERY}s）+ 完全静默 ${IDLE}s + 同连接收尾 =="
T0=$(date +%s)
# 同一条 TCP 连接跑完整段：心跳是「有人在用」的形态，静默段一个字节都不走，
# FINAL 必须也能出来 —— 否则就是「半开没察觉」那种要命形态。
$SSHT $REMOTE "for i in \$(seq 1 $TICKS); do echo tick \$i \$(date +%s); sleep $TICK_EVERY; done; echo IDLE_BEGIN \$(date +%s); sleep $IDLE; echo FINAL \$(hostname) \$(date +%s)" > "$HB" 2>/dev/null &
HB_PID=$!

echo "== 4) 同隧道并发大流量：64 MiB 经 scp（SFTP 同一条新连接），sha256 逐字节 =="
RFILE="/tmp/uulan-fwd-$TS.bin"
HS=$($SSH "dd if=/dev/urandom of=$RFILE bs=1M count=$MB status=none && sha256sum $RFILE | cut -d' ' -f1" 2>/dev/null | tr -d '\r' | tail -1)
[ -n "$HS" ] && ok "真机生成 $MB MiB 测试文件（sha 前缀 ${HS:0:12}）" || { echo "FAIL 真机造文件失败"; exit 1; }
LFILE="$WORK/uulan-fwd-$TS.bin"
LFILEN="$WORKN/uulan-fwd-$TS.bin"
S0=$(date +%s%3N)
scp -q -P $LP -i "$KEYF" -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile="$KH" "$REMOTE:$RFILE" "$LFILE" 2>/dev/null
SCPRC=$?
S1=$(date +%s%3N)
# 原生 python 只认 D:/ 形式（喂 /d/ 会得到空哈希 —— 2026-11-13 第一轮就是这么红的）
LOCAL_HS=$(python -c "import hashlib,sys;print(hashlib.sha256(open(sys.argv[1],'rb').read()).hexdigest())" "$LFILEN" 2>/dev/null | tr -d '\r')
DS=$(( (S1 - S0) / 1000 )); MS=$(( (S1 - S0) % 1000 ))
SIZE=$(stat -c %s "$LFILE" 2>/dev/null)
chk "scp 退出码 0" "$([ "$SCPRC" = "0" ] && echo 1)" "rc=$SCPRC"
chk "字节数 = $((MB * 1048576))" "$([ "$SIZE" = "$((MB * 1048576))" ] && echo 1)" "$SIZE"
chk "sha256 与真机一致" "$([ -n "$LOCAL_HS" ] && [ "$HS" = "$LOCAL_HS" ] && echo 1)" "${LOCAL_HS:0:12}..."
RATE=$(awk -v m="$MB" -v s="$((S1 - S0))" 'BEGIN{printf "%.1f", m*1000/s}')
echo "  经隧道传输 ${MB} MiB 用时 ${DS}.${MS}s ≈ ${RATE} MiB/s（并发在心跳流进行期间）"

wait "$HB_PID"; HB_RC=$?
T1=$(date +%s)
NT=$(grep -ac '^tick' "$HB" 2>/dev/null)
FIN=$(grep -ac '^FINAL' "$HB" 2>/dev/null)
chk "心跳收齐 $TICKS 拍（且退出码 0）" "$([ "$NT" = "$TICKS" ] && [ "$HB_RC" = "0" ] && echo 1)" "ticks=$NT rc=$HB_RC"
chk "静默 ${IDLE}s 后**同一条连接**还能说话（FINAL 出现）" "$([ "$FIN" = "1" ] && echo 1)" "FINAL=$FIN"
chk "长连接总时长 ≥ $((TICKS * TICK_EVERY + IDLE))s" "$([ $((T1 - T0)) -ge $((TICKS * TICK_EVERY + IDLE - 5)) ] && echo 1)" "$((T1 - T0))s"
FINHOST=$(grep -a '^FINAL' "$HB" | awk '{print $2}')
chk "FINAL 里的 hostname 仍是 $FINHOST" "$([ "$FINHOST" = "$DIRECT" ] && echo 1)" "$FINHOST"

echo "== 5) 收尾：转发会话自己报的账 =="
# 统计行是客户端 --seconds 到点自收时才打的 —— 提前 kill 会把子壳连带统计一起掐掉
# （2026-11-13 第一轮就是这么空的）。等到它自己收工，最多再等 400 秒。
for _ in $(seq 1 400); do grep -qa '端口映射结束' "$LOG" 2>/dev/null && break; sleep 1; done
wait "$FWD_PID" 2>/dev/null; FWD_PID=""
STAT=$(grep -a '端口映射结束' "$LOG" | tail -1)
CN=$(printf '%s' "$STAT" | sed -E 's/.*连接 ([0-9]+).*/\1/')
UP=$(printf '%s' "$STAT" | sed -E 's/.*上行 ([0-9]+).*/\1/')
DOWN=$(printf '%s' "$STAT" | sed -E 's/.*下行 ([0-9]+).*/\1/')
# 连接数：hostname(1) + exit9(1) + 心跳(1) + scp(1) = 4
chk "客户端统计连接数 = 4（hostname/exit9/心跳/scp）" "$([ "${CN:-0}" = "4" ] && echo 1)" "$STAT"
chk "下行字节 ≥ ${MB} MiB 的量级（down=${DOWN:-?}）" "$([ "${DOWN:-0}" -ge $((MB * 800000)) ] && echo 1)" "$DOWN"
chk "上行有实际流量（心跳+scp 请求，up=${UP:-?}）" "$([ "${UP:-0}" -gt 100000 ] && echo 1)" "$UP"

echo
echo "结果：$pass 通过 / $fail 失败"
echo "（真机测试文件留在 $RFILE 未删，供复查；本地副本 $LFILE）"
[ "$fail" -eq 0 ] || exit 1
exit 0
