#!/usr/bin/env bash
# 真二进制验证：画面为什么停了，被控端必须亲口说给控制端（H2C::VideoError）
#
# 与 `cargo test` 里的端到端测试互补：这里跑的是**真正发布出去的那三个 exe**，而且是
# **跨进程**的 —— 命令行解析、配置落盘、进程间真网络、真加密视频通道、headless 统计行。
#
# 两个用例把「算不算故障」的口径两头都钉住：
#   A 有界合成源（`@12`，12 帧放满）   → 「按计划结束」，failed=false → 故障 0 条会话
#   B 不设帧数上限的合成源（自然到头） → 「意外结束」，  failed=true  → 故障 1 条会话
# 少了 B 就只有好话没有反例；而把正常结束记成故障，会让每一轮无人值守验收凭空变红
# （同音频状态「Ended 不算 Failed」那条纪律）。
#
# 用法：bash scripts/verify-video-end-e2e.sh
# 环境变量：UULAN_STAGE=<msys 目录>  UULAN_PORT=<中继端口>
# 退出码：0 = 全部断言通过；非 0 = 有失败（并列出哪几条）。
#
# ⚠ 二进制必须在**工作区之外**跑：这台开发机上 exe 位于 D:\default-workspace 之下就
#   写不了任何文件（连被控端建自己的配置都会失败），那是目录问题不是代码问题。

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
STAGE="${UULAN_STAGE:-/d/uulan-bin/video-end}"
BASE_PORT="${UULAN_PORT:-21993}"
TS="$(date +%Y%m%d_%H%M%S)"
FAILURES=0
PASSES=0
BAD=""

say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
die() { printf '\n\033[31m[失败] %s\033[0m\n' "$*" >&2; exit 1; }

# ck <说明> <能在 eval 里跑的判定式>
ck() {
  local desc="$1"; shift
  if eval "$*"; then
    printf '  [通过] %s\n' "$desc"
    PASSES=$((PASSES + 1))
  else
    printf '  [FAIL] %s\n' "$desc"
    FAILURES=$((FAILURES + 1))
    BAD="$BAD
  x $desc"
  fi
}

for b in uulan-server uulan-host uulan-client; do
  [ -x "$ROOT/target/release/$b.exe" ] || die "找不到 target/release/$b.exe —— 先 cargo build --release"
done
command -v ffmpeg >/dev/null 2>&1 || die "本机找不到 ffmpeg"

say "0/3 把二进制拷到工作区之外"
mkdir -p "$STAGE"
STAGE_N="$(cd "$STAGE" && pwd -W)" || die "建不出 $STAGE"
for b in uulan-server uulan-host uulan-client; do
  cp -f "$ROOT/target/release/$b.exe" "$STAGE/" || die "拷贝 $b 失败（上一轮的进程还占着？）"
done
echo "  就位：$STAGE_N"

run_case() {
  local name="$1" cap="$2" port="$3"
  local dir="$STAGE/$name.$TS"
  local dir_n="$STAGE_N/$name.$TS"
  mkdir -p "$dir" || die "$name 建不出工作目录"
  local SRV="" HOST=""

  say "$name：起中继 + 被控端（capture=$cap）"
  "$STAGE/uulan-server.exe" --bind "127.0.0.1:$port" --state "$dir_n/devices.json" \
    >"$dir/server.log" 2>&1 &
  SRV=$!
  for _ in $(seq 1 50); do grep -q "已就绪" "$dir/server.log" 2>/dev/null && break; sleep 0.2; done
  grep -q "已就绪" "$dir/server.log" || { cat "$dir/server.log"; die "$name 中继没起来"; }

  UULAN_CONFIG="$dir_n/host.json" "$STAGE/uulan-host.exe" \
    --server "127.0.0.1:$port" --capture "$cap" --name "验证被控端" \
    >"$dir/host.log" 2>&1 &
  HOST=$!
  for _ in $(seq 1 60); do grep -q "已上线" "$dir/host.log" 2>/dev/null && break; sleep 0.2; done
  grep -q "已上线" "$dir/host.log" || { cat "$dir/host.log"; die "$name 被控端没上线"; }

  local DEVICE
  # 设备 ID 与访问口令：都从**被控端落盘的 host.json**读。
  # 不能着从日志 grep：非 tty 时口令被刻意隐藏（日志里只剩「（已隐藏）」）
  # 而 banner 那行「设备 ID : 」的冤格式分隔符号数量不可控。
  DEVICE=$(python -c "import json,sys;print(json.load(open(sys.argv[1]))['device_id'])" \
    "$dir_n/host.json" 2>/dev/null | tr -d '[:space:]')
  [ -n "$DEVICE" ] || { cat "$dir/host.log"; die "$name host.json 里读不到 device_id"; }
  # 口令只落进 0600 文件喂控制端，**绝不上屏**（本仓库的硬规矩）。
  # 用 stdout.buffer 写：单个，不带换行 —— 控制端读的是原值。
  python -c "import json,sys;sys.stdout.buffer.write(json.load(open(sys.argv[1]))['password'].encode())" \
    "$dir_n/host.json" >"$dir/pw" 2>/dev/null
  chmod 600 "$dir/pw"
  [ -s "$dir/pw" ] || die "$name 拿不到 host.json 里的 password"
  echo "  设备 ID=$DEVICE（口令已落到 0600 文件，不打印）"

  say "$name：控制端 headless（--reconnect 0，画面一停就退，好看清那一句）"
  "$STAGE/uulan-client.exe" --server "127.0.0.1:$port" --device "$DEVICE" \
    --password-file "$dir_n/pw" --name "验证控制端" --headless --seconds 15 --reconnect 0 \
    >"$dir/client.log" 2>"$dir/client.err"
  local CRC=$?
  echo "  控制端退出码 $CRC（画面被动结束且不许重连 → 4 是预期）"
  # 被控端 stderr 的 tracing 给字段名与等号上色（painted=N 同款坑），
  # failed=false / failed=true 这类断言必须读去色版，否则会假红。
  sed -E 's/\x1b\[[0-9;]*m//g' "$dir/host.log" >"$dir/host.clean"
  sed -n '/────── 统计/,$p' "$dir/client.log" | sed 's/^/    /'

  ck "$name 被控端收尾前把原因发了出去" \
     "grep -q '编码源收尾：已把原因发给控制端' '$dir/host.clean' && grep -q '视频源结束' '$dir/host.clean'"
  ck "$name 控制端统计行拿到的是对面那句话（不是未说明）" \
     "grep -q '画面结束' '$dir/client.log' && ! grep -q '画面结束   : 被控端未说明' '$dir/client.log'"
  ck "$name 画面是被动结束的（不是本机 --seconds 到点）" \
     "! grep -q '画面结束   : 本机按时限收工' '$dir/client.log'"
  ck "$name 没有被误判成连接半开" \
     "! grep -q '半开' '$dir/client.log'"

  if [ "$name" = "A-放满" ]; then
    ck "A 有界源放满 = 不是故障（failed=false）" \
       "grep -q 'failed=false' '$dir/host.clean' && grep -q '按计划结束' '$dir/host.clean'"
    ck "A 控制端统计：故障 0 条会话（正常结束不许变红）" \
       "grep -q '被控端报编码器故障 0 条会话' '$dir/client.log'"
  else
    ck "B 不设上限的源自然到头 = 故障（failed=true）" \
       "grep -q 'failed=true' '$dir/host.clean' && grep -q '意外结束' '$dir/host.clean'"
    ck "B 控制端统计：故障 1 条会话（这条必须数得出来）" \
       "grep -q '被控端报编码器故障 1 条会话' '$dir/client.log'"
  fi

  local PW
  PW="$(cat "$dir/pw")"
  ck "$name 三份日志里都搜不到访问口令" \
     "! grep -rqF '$PW' '$dir/server.log' '$dir/host.clean' '$dir/client.log' '$dir/client.err'"

  [ -n "$HOST" ] && kill "$HOST" 2>/dev/null
  [ -n "$SRV" ] && kill "$SRV" 2>/dev/null
  wait 2>/dev/null
  sleep 1
}

say "1/3 用例 A：有界合成源（12 帧放满 = 正常收工）"
run_case "A-放满" "lavfi:testsrc2=size=320x240:rate=25@12" "$BASE_PORT"

say "2/3 用例 B：不设帧数上限（自然到头 = 按设计就是意外）"
run_case "B-没放满" "lavfi:testsrc2=size=320x240:rate=25:duration=2" "$((BASE_PORT + 1))"

say "3/3 汇总"
if [ "$FAILURES" -eq 0 ]; then
  printf '\n\033[32m全部通过：%d 条断言\033[0m\n' "$PASSES"
  exit 0
fi
printf '\n\033[31m%d 条断言失败（通过 %d 条）：%s\n\033[0m\n' "$FAILURES" "$PASSES" "$BAD"
exit 1
