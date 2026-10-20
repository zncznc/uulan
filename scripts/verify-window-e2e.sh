#!/usr/bin/env bash
# 全链路窗口验收：真 server + 真被控端 + 真控制端窗口，跑 N 秒后自动关窗，
# 断言**控制端自己报的呈现帧数**（painted）> 0 —— 不是「解出多少帧」，
# 解出的帧可能一帧都没画上去（blit 写错、surface 没 present）。
#
# 为什么要拷出去跑：位于 D:/default-workspace/ 之下的 exe 在这台开发机上
# **写不了任何文件**（被控端要写自己的 host.json，会直接失败）。把三个 exe
# 拷到工作区外（默认 D:/uulan-bin/run-*）再执行，与代码无关。
#
# 用法：
#   bash scripts/verify-window-e2e.sh              # 8 秒
#   UULAN_SECONDS=15 bash scripts/verify-window-e2e.sh
#   UULAN_PORT=21995 UULAN_STAGE=D:/uulan-bin2 bash scripts/verify-window-e2e.sh
#
# 退出码：0 = 窗口真的呈现了帧；1 = 断言失败；2 = 环境问题（端口占着 / 进程没起来）。
set -u

PORT="${UULAN_PORT:-21990}"
RUN_SECS="${UULAN_SECONDS:-8}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${UULAN_STAGE:-D:/uulan-bin/run-$(date +%Y%m%d-%H%M%S)}"
NO_COLOR=1
export NO_COLOR

mkdir -p "$WORK" || { echo "建不了 $WORK"; exit 2; }
cp -f "$ROOT"/target/release/uulan-server.exe \
      "$ROOT"/target/release/uulan-host.exe \
      "$ROOT"/target/release/uulan-client.exe "$WORK"/ || { echo "拷二进制失败（是不是上一轮还在跑？）"; exit 2; }
echo "工作目录：$WORK  端口：$PORT  时长：${RUN_SECS}s"

SERVER_PID=""; HOST_PID=""
cleanup() {
  [ -n "$HOST_PID" ] && kill "$HOST_PID" 2>/dev/null
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null
}
trap cleanup EXIT

RUST_LOG=info "$WORK/uulan-server.exe" --bind "127.0.0.1:$PORT" \
  --state "$WORK/devices.json" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!

# 等端口起来（最多 10 秒）
for _ in $(seq 1 100); do
  (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && { exec 3>&-; break; }
  sleep 0.1
done
if ! grep -q "" "$WORK/server.log" 2>/dev/null; then echo "服务器没起来"; exit 2; fi

RUST_LOG=info "$WORK/uulan-host.exe" --server "127.0.0.1:$PORT" --config "$WORK/host.json" \
  --capture "lavfi:testsrc2=size=1280x720:rate=30" --name e2e-host --once \
  >"$WORK/host.log" 2>&1 &
HOST_PID=$!

DEVICE=""
for _ in $(seq 1 200); do
  # 取**最后一条**「设备 ID : <数字>」：横幅里还有一条「（首次连上中继后分配）」。
  DEVICE=$(grep -oE "设备 ID : [0-9]{6,}" "$WORK/host.log" 2>/dev/null | tail -1 | sed 's/.*: *//' | tr -d '[:space:]')
  [ -n "$DEVICE" ] && break
  sleep 0.1
done
# 失败时只打非敏感行：横幅里有访问密码，绝不能进日志/终端。
if [ -z "$DEVICE" ]; then
  echo "被控端没拿到设备 ID，日志（已滤掉密码行）："
  grep -vE "访问密码" "$WORK/host.log" | sed -n 1,20p
  exit 2
fi

# 密码从 host.json 读，**不回显**（日志里也不打印）。
PW=$(sed -n 's/.*"password": *"\([^"]*\)".*/\1/p' "$WORK/host.json" | head -1)
[ -n "$PW" ] || { echo "host.json 里没有 password 字段"; exit 2; }

RUST_LOG=info "$WORK/uulan-client.exe" --server "127.0.0.1:$PORT" --device "$DEVICE" \
  --password "$PW" --name e2e-client --seconds "$RUN_SECS" >"$WORK/client.log" 2>&1
CLIENT_RC=$?

# tracing 落盘的行里带 ANSI（RUST_LOG=info 时字段名会被染色），grep 前先剥一层。
strip() { sed -E 's/\x1b\[[0-9;]*m//g' "$1"; }

echo "──────── 控制端 ────────"
strip "$WORK/client.log" | grep -E "窗口已就绪|窗口已关闭|输入上报|剪贴板同步|收到 AU" || true

echo "──────── 被控端 ────────"
strip "$WORK/host.log" | grep -E "剪贴板后端|键鼠注入后端|剪贴板同步统计|键鼠事件统计" || true

PAINTED=$(strip "$WORK/client.log" | grep -oE 'painted=[0-9]+' | tail -1 | cut -d= -f2)
if [ -z "${PAINTED:-}" ]; then
  echo "FAIL：控制端没有报告呈现帧数（painted=0）—— 窗口可能没画、或根本没起来"
  exit 1
fi
if [ "$PAINTED" -le 0 ]; then
  echo "FAIL：painted=$PAINTED"
  exit 1
fi
echo "OK：窗口真的呈现了 $PAINTED 帧（client 退出码 $CLIENT_RC）"
exit 0