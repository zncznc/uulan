#!/usr/bin/env bash
# `uulan-host --diagnose` 的端到端验收：**自检说「能跑」的时候真的能跑，说「不能跑」的时候
# 手指头指向的那一项真的坏了**。
#
# 为什么必须端到端验：自检本身是给人看的判断器，它答错比不答更坏（现场会照着它排除故障）。
# 所以既要有「全过」的正例，也要有几种**故意弄坏一项**的反例，断言它指出的是那一项。
#
# 断言（需要 X11 的真机；在 Windows 上直接 SKIP 退出码 2）：
#   ① 正常配置（真 server + X11 采集源）：退出码 0，输出含「结论：可以跑」，
#      且含 `[ OK ] 显示` / `[ OK ] 键鼠注入` / `[ OK ] 中继` / `[ OK ] ffmpeg`
#   ② DISPLAY 指向一个不存在的显示：退出码 1 且 `[FAIL] 显示`
#   ③ --ffmpeg 指向不是 ffmpeg 的程序：退出码 1 且 `[FAIL] ffmpeg`
#   ④ 中继地址不可达：退出码 1 且 `[FAIL] 中继`
#   ⑤ 全程不打印口令：把 host.json 的 password 原值拿去 grep 输出，必须 0 命中
#
# 用法：bash scripts/verify-diagnose-e2e.sh [额外参数…]
#   UULAN_STAGE   暂存目录（默认 $TMPDIR/diag-<时间戳>）
#   UULAN_CONFIG  用哪份配置（默认暂存目录里新生成的一份，不碰现场的真配置）
# 退出码：0 通过；1 断言失败；2 环境问题（非 Linux / 没有 X）。
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${UULAN_STAGE:-${TMPDIR:-/tmp}/diag-$(date +%Y%m%d-%H%M%S)}"
PORT="${UULAN_PORT:-21992}"
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) EXE=".exe"; HOST_OS="win" ;;
  *) EXE=""; HOST_OS="unix" ;;
esac
export NO_COLOR=1

if [ "$HOST_OS" = "win" ]; then
  echo "SKIP：这台机器没有 X11，自检的「显示 / XTest」正例不成立（请在麒麟上跑）。"
  exit 2
fi
if [ -z "${DISPLAY:-}" ]; then
  echo "SKIP：没有 DISPLAY（自检正例需要一个图形会话）。"
  exit 2
fi

mkdir -p "$WORK" || { echo "建不了 $WORK"; exit 2; }
cp -f "$ROOT"/target/release/uulan-host$EXE "$ROOT"/target/release/uulan-server$EXE "$WORK"/ \
  || { echo "拷二进制失败（先 cargo build --release）"; exit 2; }
CFG="${UULAN_CONFIG:-$WORK/host.json}"
echo "工作目录：$WORK  配置：$CFG  端口：$PORT  DISPLAY=$DISPLAY"

SERVER_PID=""
cleanup() { [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null; }
trap cleanup EXIT

"$WORK/uulan-server$EXE" --bind "127.0.0.1:$PORT" --state "$WORK/devices.json" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 100); do
  (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && { exec 3>&-; break; }
  sleep 0.1
done

FAIL=0
check() { if [ "$1" = 0 ]; then echo "  ✓ $2"; else echo "  ✗ $2"; FAIL=1; fi; }

# 只跑自检，不启动被控端。--capture x11 让它走真实那条路。
diag() { # diag <输出文件> [额外参数…]
  local out="$1"; shift
  "$WORK/uulan-host$EXE" --diagnose --config "$CFG" --server "127.0.0.1:$PORT" \
    --capture x11 "$@" >"$out" 2>&1
}

echo "──────── ① 正常配置：自检必须说「可以跑」────────"
diag "$WORK/ok.txt"
rc=$?
sed 's/^/    /' "$WORK/ok.txt"
if [ "$rc" = 0 ]; then check 0 "退出码 0"; else check 1 "退出码 $rc（应为 0）"; fi
for item in "身份" "配置" "可写" "显示" "键鼠注入" "ffmpeg" "中继" "本机"; do
  if grep -q "^\[ OK \] $item" "$WORK/ok.txt"; then check 0 "$item 通过"; else
    check 1 "$item 没通过（$(grep -E "^\[(FAIL|WARN)\] $item" "$WORK/ok.txt" | head -1)）"
  fi
done
if grep -q "结论：可以跑" "$WORK/ok.txt"; then check 0 "结论行是「可以跑」"; else check 1 "结论行不对"; fi

echo "──────── ② DISPLAY 打不开：必须指到「显示」这一项 ────────"
DISPLAY=:77 diag "$WORK/nodisp.txt"
rc=$?
if [ "$rc" = 1 ]; then check 0 "退出码 1"; else check 1 "退出码 $rc（应为 1）"; fi
if grep -q "^\[FAIL\] 显示" "$WORK/nodisp.txt"; then
  check 0 "指出了 [FAIL] 显示"
  grep -A1 "^\[FAIL\] 显示" "$WORK/nodisp.txt" | sed 's/^/    /'
else
  check 1 "没有指出 [FAIL] 显示"; sed 's/^/    /' "$WORK/nodisp.txt"
fi

echo "──────── ③ --ffmpeg 指错：必须指到「ffmpeg」这一项 ────────"
# /bin/false 成功退出但什么也不打印；不是 ffmpeg。
diag "$WORK/badff.txt" --ffmpeg /bin/false
rc=$?
if [ "$rc" = 1 ]; then check 0 "退出码 1"; else check 1 "退出码 $rc（应为 1）"; fi
if grep -q "^\[FAIL\] ffmpeg" "$WORK/badff.txt"; then
  check 0 "指出了 [FAIL] ffmpeg"
  grep -A1 "^\[FAIL\] ffmpeg" "$WORK/badff.txt" | sed 's/^/    /'
else
  check 1 "没有指出 [FAIL] ffmpeg"; sed 's/^/    /' "$WORK/badff.txt"
fi

echo "──────── ④ 中继不可达：必须指到「中继」这一项 ────────"
"$WORK/uulan-host$EXE" --diagnose --config "$CFG" --server "127.0.0.1:1" --capture x11 \
  >"$WORK/norelay.txt" 2>&1
rc=$?
if [ "$rc" = 1 ]; then check 0 "退出码 1"; else check 1 "退出码 $rc（应为 1）"; fi
if grep -q "^\[FAIL\] 中继" "$WORK/norelay.txt"; then
  check 0 "指出了 [FAIL] 中继"
  grep -A1 "^\[FAIL\] 中继" "$WORK/norelay.txt" | sed 's/^/    /'
else
  check 1 "没有指出 [FAIL] 中继"; sed 's/^/    /' "$WORK/norelay.txt"
fi

echo "──────── ⑤ 口令不出现在输出里 ────────"
PW=$(sed -n 's/.*"password": *"\([^"]*\)".*/\1/p' "$CFG" | head -1)
if [ -z "$PW" ]; then
  check 1 "读不到配置里的口令（自检应该已经生成了它）"
else
  hits=$(cat "$WORK"/ok.txt "$WORK"/nodisp.txt "$WORK"/badff.txt "$WORK"/norelay.txt | grep -c -F "$PW")
  if [ "$hits" = 0 ]; then check 0 "四份自检输出里口令命中 0 次"; else check 1 "口令在自检输出里出现 $hits 次"; fi
  if grep -q "设备 ID" "$WORK/ok.txt"; then check 0 "身份行里给的是设备 ID（可公开）"; else check 1 "身份行没写设备 ID"; fi
fi

echo
if [ "$FAIL" = 0 ]; then
  echo "OK：自检结论与故障定位都成立（正常 0 / 三项反例各自指对）"
else
  echo "FAIL：见上面的 ✗ 项；输出在 $WORK"
fi
exit "$FAIL"