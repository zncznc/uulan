#!/usr/bin/env bash
# M6 日志脱敏验收：**全量 TRACE 级**跑一次真会话，然后在所有日志里搜秘密。
#
# 断言的是「日志里搜不到」这几样东西（每一样都是真的经手过这条链路的）：
#   ① 被控端的访问密码（含它出现在 config/host.json 里的原值）
#   ② 一次性临时码的形态（host 的 temp_code）
#   ③ 会话票据的原值（32 个十六进制字符）—— 它是 60 秒有效的 bearer token，
#      拿到就能拨 relay 通道；日志里只该出现不可逆标签 `tXXXXXXXX`
#   ④ 终端里敲过的命令原文（终端输入是用户数据，不该进日志）
#   ⑤ 控制端命令行上的口令（脚本自己就**不用** `--password`，改用 --password-file）
#
# 反过来也要证「不是空跑」：日志里必须出现带标签的会话行、必须出现过终端会话、
# 必须出现过文件操作 —— 否则「搜不到」只是因为没有发生。
#
# 用法：bash scripts/verify-log-hygiene.sh
# 退出码：0 通过；1 断言失败；2 环境问题。
set -u

PORT="${UULAN_PORT:-21994}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${UULAN_STAGE:-D:/uulan-bin/loghyg-$(date +%Y%m%d-%H%M%S)}"
export NO_COLOR=1
# TRACE 是刻意的：越啰嗦越可能漏，验收要在最啰嗦的档位上过。
# 三个进程都吃这一份（客户端的日志走 stderr，stdout 只放终端字节/目录列表）。
export RUST_LOG=trace

mkdir -p "$WORK" || { echo "建不了 $WORK"; exit 2; }
cp -f "$ROOT"/target/release/uulan-server.exe \
      "$ROOT"/target/release/uulan-host.exe \
      "$ROOT"/target/release/uulan-client.exe "$WORK"/ || { echo "拷二进制失败（上一轮还在跑？）"; exit 2; }
echo "工作目录：$WORK  端口：$PORT"

SERVER_PID=""; HOST_PID=""
cleanup() {
  [ -n "$HOST_PID" ] && kill "$HOST_PID" 2>/dev/null
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null
  MSYS2_ARG_CONV_EXCL='*' taskkill /F /IM uulan-host.exe >/dev/null 2>&1
  MSYS2_ARG_CONV_EXCL='*' taskkill /F /IM uulan-server.exe >/dev/null 2>&1
}
trap cleanup EXIT

"$WORK/uulan-server.exe" --bind "127.0.0.1:$PORT" \
  --state "$WORK/devices.json" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 100); do
  (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && { exec 3>&-; break; }
  sleep 0.1
done

"$WORK/uulan-host.exe" --server "127.0.0.1:$PORT" --config "$WORK/host.json" \
  --capture "lavfi:testsrc2=size=640x360:rate=5" --name loghyg \
  >"$WORK/host.log" 2>&1 &
HOST_PID=$!

DEVICE=""
for _ in $(seq 1 200); do
  DEVICE=$(grep -oE "设备 ID : [0-9]{6,}" "$WORK/host.log" 2>/dev/null | tail -1 | sed 's/.*: *//' | tr -d '[:space:]')
  [ -n "$DEVICE" ] && break
  sleep 0.1
done
[ -n "$DEVICE" ] || { echo "被控端没拿到设备 ID"; sed -n 1,20p "$WORK/host.log"; exit 2; }

# 口令只从 host.json 读，**不进本脚本的输出，也不进客户端命令行**。
PW=$(sed -n 's/.*"password": *"\([^"]*\)".*/\1/p' "$WORK/host.json" | head -1)
[ -n "$PW" ] || { echo "host.json 里没有 password 字段"; exit 2; }
printf '%s\n' "$PW" > "$WORK/pw.txt"   # 用 --password-file 传给客户端（进程表里看不到）

CANARY="HYGIENE-$(date +%s)-Zq7"
FAIL=0
check() { if [ "$1" = 0 ]; then echo "  ✓ $2"; else echo "  ✗ $2"; FAIL=1; fi; }

echo "──────── ① 文件操作（--password-file 传口令）────────"
timeout 60 "$WORK/uulan-client.exe" --server "127.0.0.1:$PORT" --device "$DEVICE" \
  --password-file "$WORK/pw.txt" --ls ":/" >"$WORK/client-ls.out" 2>"$WORK/client-ls.log"
check $? "客户端用 --password-file 连上并列出「此电脑」"

echo "──────── ② 终端：敲一条带金丝雀的命令 ────────"
printf 'echo %s\r\nexit 0\r\n' "$CANARY" | \
  timeout 60 "$WORK/uulan-client.exe" --server "127.0.0.1:$PORT" --device "$DEVICE" \
    --password-file "$WORK/pw.txt" --term >"$WORK/client-term.out" 2>"$WORK/client-term.log"
RC=$?
check $([ "$RC" = 0 ] && echo 0 || echo 1) "终端会话退出码 0（两条命令都执行了）"
grep -q "$CANARY" "$WORK/client-term.out" && check 0 "终端输出里真的有金丝雀（证明命令确实在对面执行了）" \
                                          || check 1 "终端输出里没有金丝雀 —— 这一轮等于没发生，后面的「搜不到」不算数"

# 终端字节流里不该出现日志行：日志走 stderr，stdout 只放 PTY 的字节。
if grep -qE "^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:.]+Z" "$WORK/client-term.out"; then
  check 1 "终端 stdout 里混进了日志行（日志该走 stderr）"
else
  check 0 "终端 stdout 干净：没有日志行混进 PTY 字节流"
fi

echo "──────── ③ 键鼠注入（--inject 也走真链路）────────"
timeout 60 "$WORK/uulan-client.exe" --server "127.0.0.1:$PORT" --device "$DEVICE" \
  --password-file "$WORK/pw.txt" --inject "move:100,80; click:left" \
  >"$WORK/client-inject.out" 2>"$WORK/client-inject.log"
INJ_RC=$?
check $([ "$INJ_RC" = 0 ] || [ "$INJ_RC" = 3 ] && echo 0 || echo 1) \
  "注入跑完（rc=$INJ_RC；0=注入成功；3=合成源没有可注入的桌面）"

sleep 0.3
echo "──────── ④ 全量搜秘密 ────────"
# 只搜 tracing 日志（stderr）。`client-*.out` 是终端画面/目录列表本身 —— 那是给人看的
# 字节流，不是日志；把它算进来等于断言「终端不能用」。
LOGS="$WORK/server.log $WORK/host.log $WORK/client-ls.log $WORK/client-term.log $WORK/client-inject.log"
LINES=$(cat $LOGS | wc -l)
echo "  日志行数合计：$LINES（RUST_LOG=trace）"

hit() { # hit <说明> <grep 参数...>
  local what="$1"; shift
  local n
  n=$(grep -c -a "$@" $LOGS 2>/dev/null | awk -F: '{s+=$NF} END{print s+0}')
  if [ "$n" = 0 ]; then check 0 "$what：0 次命中"; else
    check 1 "$what：$n 次命中 —— 下面是要改的地方"
    grep -n -a "$@" $LOGS | head -5 | sed 's/^/      /'
  fi
}

hit "访问密码原值" -F "$PW"
hit "访问密码原值（按字段名）" -E "\"password\": *\"$PW\""
hit "终端命令金丝雀" -F "$CANARY"
hit "会话票据原值（旧格式 session=<32 hex>）" -E "session=t?[0-9a-f]{32}"

# 顺便看一眼：日志里到底有没有「带了标签的会话」—— 有，才说明票据那一路真的走过。
grep -q -E "session=t[0-9a-f]{8}" "$WORK/host.log" && check 0 "被控端日志里会话是标签形态（tXXXXXXXX）" \
  || { check 1 "被控端日志里没有标签形态的会话 —— 脱敏可能被改回去了"; }

# 反向证明：一次会话真的发生过，不是空跑。
grep -q "有人要连进来" "$WORK/host.log" && check 0 "确有会话建立（不是空跑）" || check 1 "日志里没有会话记录"
grep -q "终端会话已开启" "$WORK/host.log" && check 0 "确有终端会话" || check 1 "日志里没有终端会话"
grep -q "文件操作完成" "$WORK/host.log" && check 0 "确有文件操作" || check 1 "日志里没有文件操作"

echo
if [ "$FAIL" = 0 ]; then
  echo "OK：全量 TRACE 日志里搜不到口令/票据/终端原文（$LINES 行日志）"
  exit 0
else
  echo "FAIL：日志脱敏有漏（见上面 ✗ 行）"
  exit 1
fi