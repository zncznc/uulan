#!/usr/bin/env bash
# M6 鉴权锁定策略的端到端验收：**连续错几次真的会锁，而且锁定期间正确口令也进不去**。
#
# 锁定策略（crates/uulan-host/src/session.rs）：连续失败 5 次开始锁，首次 60 秒，
# 之后每多错一次翻倍（封顶 300 秒）；鉴权成功清零。计数是**跨会话共享**的
# —— 否则攻击者每次失败重连就能重置计数，锁定形同虚设。
#
# 为什么必须端到端验：「锁定」是安全控制，只靠单测（`AuthState` 的纯逻辑）说明不了
# 「真链路里第 6 次会被拒」。这个脚本用真 server + 真被控端 + 真二进制跑一遍。
# 两边都能跑：Windows git-bash 用 .exe 后缀与 D:/ 暂存盘，麒麟上按 uname 退回 $TMPDIR。
#
# 断言：
#   ① 前 4 次错口令：都是「密码不正确」（还没到阈值）
#   ② 第 5 次错口令：仍然「密码不正确」，但被控端日志出现锁定的日志
#   ③ 锁定期间用**正确**口令：被拒，理由是「已锁定，请等 N 秒」
#   ④ 等锁定过期后再用正确口令：进得去
#   ⑤ 被控端日志里确有「鉴权处于锁定期，拒绝」
#
# 用法：bash scripts/verify-auth-lockout-e2e.sh
# 退出码：0 通过；1 断言失败；2 环境问题。
set -u

PORT="${UULAN_PORT:-21993}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# 两边都能跑：Windows git-bash 用 .exe 后缀与 D:/ 暂存盘；麒麟上没有 D: 盘，退回 $TMPDIR。
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) EXE=".exe"; WIN_TAIL="1"; DEFAULT_WORK="D:/uulan-bin" ;;
  *) EXE=""; WIN_TAIL=""; DEFAULT_WORK="${TMPDIR:-/tmp}" ;;
esac
WORK="${UULAN_STAGE:-$DEFAULT_WORK/authlock-$(date +%Y%m%d-%H%M%S)}"
export NO_COLOR=1
RUST_LOG=info
export RUST_LOG

mkdir -p "$WORK" || { echo "建不了 $WORK"; exit 2; }
cp -f "$ROOT"/target/release/uulan-server$EXE \
      "$ROOT"/target/release/uulan-host$EXE \
      "$ROOT"/target/release/uulan-client$EXE "$WORK"/ || { echo "拷二进制失败（上一轮还在跑？）"; exit 2; }
echo "工作目录：$WORK  端口：$PORT  平台：$(uname -s)"

SERVER_PID=""; HOST_PID=""
cleanup() {
  [ -n "$HOST_PID" ] && kill "$HOST_PID" 2>/dev/null
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null
  if [ -n "$WIN_TAIL" ]; then
    MSYS2_ARG_CONV_EXCL='*' taskkill /F /IM uulan-host.exe >/dev/null 2>&1
    MSYS2_ARG_CONV_EXCL='*' taskkill /F /IM uulan-server.exe >/dev/null 2>&1
  fi
}
trap cleanup EXIT

"$WORK/uulan-server$EXE" --bind "127.0.0.1:$PORT" --state "$WORK/devices.json" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 100); do
  (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && { exec 3>&-; break; }
  sleep 0.1
done

"$WORK/uulan-host$EXE" --server "127.0.0.1:$PORT" --config "$WORK/host.json" \
  --capture "lavfi:testsrc2=size=320x240:rate=5" --name e2e-authlock \
  >"$WORK/host.log" 2>&1 &
HOST_PID=$!

DEVICE=""
for _ in $(seq 1 200); do
  DEVICE=$(grep -oE "设备 ID : [0-9]{6,}" "$WORK/host.log" 2>/dev/null | tail -1 | sed 's/.*: *//' | tr -d '[:space:]')
  [ -n "$DEVICE" ] && break
  sleep 0.1
done
[ -n "$DEVICE" ] || { echo "被控端没拿到设备 ID"; sed -n 1,20p "$WORK/host.log"; exit 2; }

PW=$(sed -n 's/.*"password": *"\([^"]*\)".*/\1/p' "$WORK/host.json" | head -1)
[ -n "$PW" ] || { echo "host.json 里没有 password 字段"; exit 2; }
printf '%s\n' "$PW" >"$WORK/pw-good.txt"
printf '%s\n' "wrong-password-$RANDOM" >"$WORK/pw-bad.txt"   # 错口令也只落文件，别进 ps
chmod 600 "$WORK/pw-good.txt" "$WORK/pw-bad.txt" 2>/dev/null

FAIL=0
check() { if [ "$1" = 0 ]; then echo "  ✓ $2"; else echo "  ✗ $2"; FAIL=1; fi; }

attempt() { # attempt <口令文件> <输出前缀> → 打印客户端 stderr，返回码写在 $?
  timeout 40 "$WORK/uulan-client$EXE" --server "127.0.0.1:$PORT" --device "$DEVICE" \
    --password-file "$1" --headless --seconds 3 >"$WORK/$2.out" 2>"$WORK/$2.log"
}

echo "──────── ① 前 4 次错口令：还没到阈值 ────────"
for i in 1 2 3 4; do
  attempt "$WORK/pw-bad.txt" "bad$i"
  rc=$?
  msg=$(grep -a "密码不正确\|已锁定" "$WORK/bad$i.log" | tail -1)
  echo "    第 $i 次：rc=$rc  $msg"
  if [ "$rc" != 0 ] && ! printf '%s' "$msg" | grep -q "已锁定"; then check 0 "第 $i 次错口令被拒（还没锁）"; else check 1 "第 $i 次的结果不对"; fi
done

echo "──────── ② 第 5 次错口令：到达阈值（开始锁）────────"
attempt "$WORK/pw-bad.txt" "bad5"
echo "    第 5 次：rc=$?  $(grep -a '密码不正确\|已锁定' "$WORK/bad5.log" | tail -1)"

echo "──────── ③ 锁定期间用**正确**口令：必须被拒 ────────"
attempt "$WORK/pw-good.txt" "good-locked"
rc=$?
msg=$(grep -a "已锁定\|密码不正确" "$WORK/good-locked.log" | tail -1)
echo "    正确口令：rc=$rc  $msg"
if [ "$rc" != 0 ] && printf '%s' "$msg" | grep -q "已锁定"; then
  check 0 "锁定期间正确口令也被拒（理由是已锁定，不是密码不正确）"
else
  check 1 "锁定期间正确口令居然能用，或者理由不对（$msg）"
fi

echo "──────── ④ 被控端日志：确实因锁定拒绝过 ────────"
if grep -q "鉴权处于锁定期" "$WORK/host.log"; then
  check 0 "被控端日志里有「鉴权处于锁定期，拒绝」"
  grep -a "鉴权处于锁定期" "$WORK/host.log" | tail -2 | sed 's/^/    /'
else
  check 1 "被控端日志里没有锁定记录"
fi
grep -a "鉴权失败\|密码不正确" "$WORK/host.log" | tail -2 | sed 's/^/    /'

echo "──────── ⑤ 锁定过期后，正确口令要能进 ────────"
# 首次锁定 60 秒；多等 8 秒余量。
echo "    等 68 秒（首次锁 60 s）…"
sleep 68
attempt "$WORK/pw-good.txt" "good-after"
rc=$?
if [ "$rc" = 0 ]; then check 0 "锁定过期后正确口令连上了"; else
  check 1 "锁定过期后正确口令仍进不去（rc=$rc）"
  tail -5 "$WORK/good-after.log" | sed 's/^/    /'
fi

echo
if [ "$FAIL" = 0 ]; then
  echo "OK：鉴权锁定策略端到端成立（5 次后锁、锁定期间正确口令也拒、过期后恢复）"
else
  echo "FAIL：见上面的 ✗ 项；日志在 $WORK"
fi
exit "$FAIL"