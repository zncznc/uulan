#!/usr/bin/env bash
# 验「重连次数用尽 → 非 0 退出」这条路径（真机 + 真网络，无人值守可跑）。
#
# 为什么需要它：这条路径以前只在单测与本机 e2e 里走过；真机上「中继长时间不回来」既不好造、
# 又容易把机器留在坏状态。做法：用测试台自己的中继（真机上另起的 uulan-server，不是正式服务），
# 会话跑起来之后把它 kill 掉，客户端 `--reconnect 1` 只试一次 → 必然放弃。
#
# 断言（headless 也要能把失败说出口，否则夜批分不清「跑完了」和「画面早冻住」）：
#   1. 退出码 = 4（与窗口模式同一个码）
#   2. 输出里有「不再重连」和「会话没能接回」
#   3. 统计段照旧打印（说明是「有画面、后来断了」，不是「一次都没连上」）
#
# 用法：bash scripts/verify-reconnect-giveup-e2e.sh
# 环境变量：UULAN_CLIENT / UULAN_SSH / UULAN_APP_PORT
# 退出码：0 = 全部断言通过；1 = 不达标
set -uo pipefail

SSH=${UULAN_SSH:-ssh -o ConnectTimeout=12 kylin}
SCP=${UULAN_SCP:-scp -q}
CLIENT=${UULAN_CLIENT:-/d/uulan-bin/uulan-client.exe}
SERVER=${UULAN_SERVER:-192.168.223.15}
PORT=${UULAN_APP_PORT:-21995}
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${UULAN_OUT:-$TMPDIR/uulan-giveup.log}

pass=0; fail=0
ok()  { pass=$((pass+1)); echo "  PASS $1"; }
bad() { fail=$((fail+1)); echo "  FAIL $1"; }

tmpms=${TMPDIR:-/tmp}
mkdir -p "$tmpms"
tmpwin=$(cygpath -m "$tmpms" 2>/dev/null || echo "$tmpms")
PW="$tmpwin/uulan-giveup-pw.txt"

cleanup() {
  rm -f "$PW" "${RCFILE:-/nonexistent}"
  [ -n "${CLI_PID:-}" ] && kill "$CLI_PID" 2>/dev/null
  $SSH "bash /tmp/remote-xvfb-testbed.sh down" >/dev/null 2>&1
}
trap cleanup EXIT

[ -x "$CLIENT" ] || { echo "FAIL 找不到控制端 $CLIENT"; exit 1; }
$SCP "$HERE/remote-xvfb-testbed.sh" "kylin:/tmp/remote-xvfb-testbed.sh" || { echo "FAIL scp 测试台脚本失败"; exit 1; }

echo "== 1) 起测试台（真机 Xvfb :99 + 独立中继 $PORT）=="
UP=$($SSH "bash /tmp/remote-xvfb-testbed.sh up" 2>&1)
DEV=$(printf '%s\n' "$UP" | awk '/^DEVICE /{print $2}' | tail -1)
[ -n "${DEV:-}" ] || { echo "FAIL 测试台没起来"; printf '%s\n' "$UP" | tail -5; exit 1; }
$SSH "python3 -c \"import json;print(json.load(open('/tmp/uulan-input-app/host.json'))['password'])\"" \
  2>/dev/null | tr -d '\r' | tail -1 > "$PW"
[ -s "$PW" ] || { echo "FAIL 没拿到口令"; exit 1; }
echo "  设备 ID=$DEV（口令只落文件，不上屏）"

echo "== 2) 跑 headless 会话（--reconnect 1），等真出画面 =="
: > "$OUT"
RCFILE="$tmpms/uulan-giveup-rc.txt"
rm -f "$RCFILE"
# 退出码由包着客户端的那层子 shell 写文件：`wait` 只能等自己的子进程，另起一个 shell 去 wait 会
# 报 "pid ... is not a child of this shell"（第一版就这么踩了，RC 变成 127）。
(
  "$CLIENT" --server "$SERVER:$PORT" --device "$DEV" --password-file "$PW" \
    --headless --seconds 120 --reconnect 1 > "$OUT" 2>&1
  echo $? > "$RCFILE"
) &
CLI_PID=$!
for _ in $(seq 1 60); do
  grep -q '^第 [0-9]* 帧' "$OUT" && break
  sleep 0.5
done
grep -c '^第 [0-9]* 帧' "$OUT" | sed 's/^/  已出画面帧行数：/'

echo "== 3) 把测试台中继 kill 掉（客户端应只重试一次就放弃）=="
$SSH "kill \$(cat /tmp/uulan-input-app/server.pid)" >/dev/null 2>&1
$SSH "ss -lnt | grep -q $PORT && echo 'FAIL 中继还在监听' || echo '  中继已停'" 2>/dev/null | sed 's/^/  /'

RC=""
for _ in $(seq 1 120); do
  [ -s "$RCFILE" ] && break
  sleep 0.5
done
RC=$(tr -dc '0-9' < "$RCFILE")
CLI_PID=""
echo "  客户端退出码 = ${RC:-?}（期望 4）"

echo "== 4) 断言 =="
[ "$RC" = "4" ] && ok "放弃重连后退出码 4（脚本能分辨「没接回」）" || bad "退出码是 $RC，不是 4"
grep -q '不再重连' "$OUT" && ok "输出里有「不再重连」" || bad "输出里没有「不再重连」"
grep -q '会话没能接回' "$OUT" && ok "输出里有「会话没能接回：…」并带原因" || bad "没有「会话没能接回」"
grep -q '解出帧' "$OUT" && ok "统计段照旧打印（不是「一次都没连上」）" || bad "统计段缺失"
echo "  ---- 关键几行 ----"
grep -E '不再重连|会话没能接回|会话       :' "$OUT" | sed 's/^/  /'

echo
echo "== 结果: $pass 通过 / $fail 失败 =="
[ "$fail" = "0" ] && exit 0 || exit 1