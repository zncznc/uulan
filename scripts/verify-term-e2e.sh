#!/usr/bin/env bash
# M4 终端（PTY）验收（真二进制、真进程、真 PTY）：
#   真 server + 真被控端（lavfi 合成画面 + 生产 PTY 后端）+ 真控制端 `--term`。
#
# **这条脚本用的 PTY 是真的**：Windows 上走 ConPTY（`portable-pty` 的 native 后端），
# 麒麟上走 `/dev/ptmx`。所以在本机跑通说明「起 shell / 读输出 / 写输入 / 传退出码」
# 这套管线对真进程是成立的；麒麟上要额外看的是 Linux 独有的 termios / SIGHUP 行为（见
# docs/DEPLOY-KYLIN.md §10）。
#
# 断言的量都是机器可查的：输出里有命令的回显、客户端退出码等于远端 shell 的退出码。
#
# 用法：
#   bash scripts/verify-term-e2e.sh
#   UULAN_PORT=21997 UULAN_STAGE=D:/uulan-bin-term bash scripts/verify-term-e2e.sh
#
# 退出码：0 = 通过；1 = 断言失败；2 = 环境问题（端口占着 / 进程没起来）。
set -u

PORT="${UULAN_PORT:-21997}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${UULAN_STAGE:-D:/uulan-bin/term-$(date +%Y%m%d-%H%M%S)}"
DATA="$WORK/data"
NO_COLOR=1
export NO_COLOR

mkdir -p "$WORK" "$DATA" || { echo "建不了 $WORK"; exit 2; }
cp -f "$ROOT"/target/release/uulan-server.exe \
      "$ROOT"/target/release/uulan-host.exe \
      "$ROOT"/target/release/uulan-client.exe "$WORK"/ || { echo "拷二进制失败（上一轮还在跑？）"; exit 2; }
echo "工作目录：$WORK  端口：$PORT"

SERVER_PID=""; HOST_PID=""
cleanup() {
  [ -n "$HOST_PID" ] && kill "$HOST_PID" 2>/dev/null
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null
  # MSYS 的路径转换是关的：`//F` 会被原样传给 taskkill 而报参数错，必须显式关掉转换。
  MSYS2_ARG_CONV_EXCL='*' taskkill /F /IM uulan-host.exe >/dev/null 2>&1
  MSYS2_ARG_CONV_EXCL='*' taskkill /F /IM uulan-server.exe >/dev/null 2>&1
}
trap cleanup EXIT

RUST_LOG=info "$WORK/uulan-server.exe" --bind "127.0.0.1:$PORT" \
  --state "$WORK/devices.json" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!

for _ in $(seq 1 100); do
  (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && { exec 3>&-; break; }
  sleep 0.1
done
grep -q "" "$WORK/server.log" 2>/dev/null || { echo "服务器没起来"; exit 2; }

RUST_LOG=info "$WORK/uulan-host.exe" --server "127.0.0.1:$PORT" --config "$WORK/host.json" \
  --capture "lavfi:testsrc2=size=640x360:rate=5" --name e2e-term \
  >"$WORK/host.log" 2>&1 &
HOST_PID=$!

DEVICE=""
for _ in $(seq 1 200); do
  DEVICE=$(grep -oE "设备 ID : [0-9]{6,}" "$WORK/host.log" 2>/dev/null | tail -1 | sed 's/.*: *//' | tr -d '[:space:]')
  [ -n "$DEVICE" ] && break
  sleep 0.1
done
if [ -z "$DEVICE" ]; then
  echo "被控端没拿到设备 ID，日志（已滤掉密码行）："
  grep -vE "访问密码" "$WORK/host.log" | sed -n 1,20p
  exit 2
fi

# 密码从 host.json 读，**不回显**。
PW=$(sed -n 's/.*"password": *"\([^"]*\)".*/\1/p' "$WORK/host.json" | head -1)
[ -n "$PW" ] || { echo "host.json 里没有 password 字段"; exit 2; }

FAIL=0
check() { if [ "$1" = 0 ]; then echo "  ✓ $2"; else echo "  ✗ $2"; FAIL=1; fi; }
strip() { sed -E 's/\x1b\[[0-9;]*m//g' "$1"; }

echo "──────── ① 跑一条命令并原样传回退出码 ────────"
# 注意 `2>` 与 `>` 分开：stdout 只该有 PTY 的字节（日志走 stderr）。
printf 'echo uulan-term-ok\r\nexit 7\r\n' | \
  timeout 60 "$WORK/uulan-client.exe" --server "127.0.0.1:$PORT" --device "$DEVICE" \
    --password "$PW" --name e2e-term-cli --term \
    >"$WORK/term.out" 2>"$WORK/term.err"
CODE=$?
echo "    客户端退出码：$CODE"

strip "$WORK/term.out" | tr -d '\r' | sed -n 1,12p | sed 's/^/    /'

if strip "$WORK/term.out" | grep -q "uulan-term-ok"; then
  check 0 "远端 shell 真的执行了命令（输出里见到 uulan-term-ok）"
else
  check 1 "输出里没有 uulan-term-ok"
fi
if [ "$CODE" = 7 ]; then
  check 0 "客户端退出码 = 远端 shell 的退出码（7）"
else
  check 1 "退出码应当是 7，实际 $CODE（129 = 会话被我们提前关掉，说明 EOT 收尾没走对）"
fi

echo "──────── ② 终端后端是 portable-pty ────────"
if strip "$WORK/host.log" | grep -q '终端服务已就绪 backend="portable-pty"'; then
  check 0 "被控端日志：backend=\"portable-pty\""
else
  check 1 "被控端日志里没有「终端服务已就绪 backend=\"portable-pty\"」"
  strip "$WORK/host.log" | grep -E "终端" | sed -n 1,5p | sed 's/^/    /'
fi

echo "──────── ③ 被控端日志：会话开出来又结束 ────────"
strip "$WORK/host.log" | grep -E "终端会话已开启|终端会话结束" | sed -n 1,6p | sed 's/^/    /'
if strip "$WORK/host.log" | grep -q "终端会话已开启"; then
  check 0 "被控端日志里有「终端会话已开启」"
else
  check 1 "被控端日志里没有「终端会话已开启」"
fi
if strip "$WORK/host.log" | grep -q "终端会话结束"; then
  check 0 "被控端日志里有「终端会话结束」（退出事件到过控制端）"
else
  check 1 "被控端日志里没有「终端会话结束」"
fi

echo "──────── ④ 控制端 stderr 不该有 panic ────────"
if grep -qE "panicked|RUST_BACKTRACE" "$WORK/term.err"; then
  check 1 "控制端 stderr 里有 panic"
  sed -n 1,10p "$WORK/term.err" | sed 's/^/    /'
else
  check 0 "控制端 stderr 干净"
fi

echo "──────── 结果 ────────"
if [ "$FAIL" = 0 ]; then
  echo "OK：终端全链路通过（真 PTY + 真进程 + 真协议）"
else
  echo "FAIL：见上面的 ✗ 项；日志在 $WORK"
fi
exit "$FAIL"