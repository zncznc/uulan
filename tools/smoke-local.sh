#!/usr/bin/env bash
# 本机冒烟：用 **release 二进制** 跑通「中继 + 被控端 + 控制端」完整链路。
#
# 与 `cargo test` 里的端到端测试互补：
#   测试验证的是库代码；这个脚本验证的是**真正发布出去的那三个可执行文件** ——
#   命令行参数解析、配置文件的生成与落盘、进程间真实网络通信、日志输出。
#
# 被控端用 lavfi 合成源，所以不需要 X11；换到真机上把 --capture 改成 x11 即可。
#
# 用法：  bash tools/smoke-local.sh
# 退出码：0 成功；非 0 失败（并打印诊断）

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release"
PORT="${UULAN_SMOKE_PORT:-21999}"
DIR="$ROOT/target/smoke"
FRAMES=20

say()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
fail() { printf '\n\033[31m[失败] %s\033[0m\n' "$*" >&2; exit 1; }

# git-bash 给的是 `/d/...` 形式的 MSYS 路径，而 server/host/client 是**原生 Windows
# 程序**，不认这种路径（会直接报「拒绝访问」）。凡是作为参数/环境变量交给它们的路径，
# 一律先转成 `D:/...` 形式。重定向（`> file`）由 bash 自己处理，保持原样即可。
native() { if command -v cygpath >/dev/null 2>&1; then cygpath -m "$1"; else printf '%s' "$1"; fi }

cleanup() {
  [ -n "${HOST_PID:-}" ] && kill "$HOST_PID" 2>/dev/null
  [ -n "${SRV_PID:-}"  ] && kill "$SRV_PID"  2>/dev/null
  wait 2>/dev/null
}
trap cleanup EXIT

for b in uulan-server uulan-host uulan-client; do
  [ -x "$BIN/$b.exe" ] || [ -x "$BIN/$b" ] || fail "找不到 $BIN/$b —— 先 cargo build --release"
done

rm -rf "$DIR"; mkdir -p "$DIR"
DIR_N="$(native "$DIR")"

say "1/5 启动中继（127.0.0.1:$PORT）"
"$BIN/uulan-server.exe" --bind "127.0.0.1:$PORT" --state "$DIR_N/devices.json" >"$DIR/server.log" 2>&1 &
SRV_PID=$!
for _ in $(seq 1 50); do
  grep -q "已就绪" "$DIR/server.log" 2>/dev/null && break
  sleep 0.2
done
grep -q "已就绪" "$DIR/server.log" || { cat "$DIR/server.log"; fail "中继没起来"; }
echo "  中继已就绪"

say "2/5 启动被控端（lavfi 合成源，无需 X11）"
UULAN_CONFIG="$DIR_N/host.json" "$BIN/uulan-host.exe" \
  --server "127.0.0.1:$PORT" \
  --capture "lavfi:testsrc2=size=320x240:rate=25@$FRAMES" \
  --name "冒烟被控端" >"$DIR/host.log" 2>&1 &
HOST_PID=$!

for _ in $(seq 1 60); do
  grep -q "已上线" "$DIR/host.log" 2>/dev/null && break
  sleep 0.2
done
grep -q "已上线" "$DIR/host.log" || { cat "$DIR/host.log"; fail "被控端没上线"; }

DEVICE=$(grep -oE '设备 ID [0-9]{9}' "$DIR/host.log" | tail -1 | grep -oE '[0-9]{9}')
PASSWORD=$(grep -oE '访问密码: [A-Za-z0-9]+' "$DIR/host.log" | tail -1 | awk '{print $2}')
[ -n "$DEVICE" ]   || { cat "$DIR/host.log"; fail "没解析到设备 ID"; }
[ -n "$PASSWORD" ] || { cat "$DIR/host.log"; fail "没解析到访问密码"; }
echo "  设备 ID=$DEVICE  密码=$PASSWORD"
# 密码绝不能出现在日志里，顺便断言一下
if grep -q "$PASSWORD" "$DIR/server.log"; then fail "中继日志里出现了访问密码 —— privacy 规则被破坏"; fi
echo "  已确认中继日志中不含密码"

say "3/5 控制端连接并解码（--json）"
"$BIN/uulan-client.exe" \
  --server "127.0.0.1:$PORT" \
  --device "$DEVICE" \
  --password "$PASSWORD" \
  --name "冒烟控制端" \
  --frames "$FRAMES" \
  --json >"$DIR/client.json" 2>"$DIR/client.err"
CLIENT_RC=$?

say "4/5 校验结果"
cat "$DIR/client.json"
echo
[ "$CLIENT_RC" -eq 0 ] || { cat "$DIR/client.err"; fail "控制端退出码 $CLIENT_RC"; }

python - "$DIR/client.json" "$FRAMES" <<'PY'
import json, sys
path, want = sys.argv[1], int(sys.argv[2])
d = json.load(open(path, encoding="utf-8"))
errs = []
if d["video"]["w"] != 320 or d["video"]["h"] != 240:
    errs.append(f"画面尺寸不对: {d['video']}")
if d["stats"]["decoded"] < want:
    errs.append(f"只解出 {d['stats']['decoded']} 帧，应有 {want} 帧")
if d["stats"]["aus"] < want:
    errs.append(f"只收到 {d['stats']['aus']} 个 AU，应有 {want} 个")
if d["stats"]["keyframes"] < 1:
    errs.append("没有关键帧 —— 控制端接上会黑屏")
if d["stats"]["dropped"] != 0:
    errs.append(f"环回不该丢帧，丢了 {d['stats']['dropped']} 个")
if d["stats"]["first_frame_ms"] <= 0 or d["stats"]["first_frame_ms"] > 20000:
    errs.append(f"首帧延迟异常: {d['stats']['first_frame_ms']}ms")
if d["stats"]["bytes"] <= 0:
    errs.append("压缩字节数是 0")
if errs:
    print("\n".join("  ✗ " + e for e in errs), file=sys.stderr)
    sys.exit(1)
print(f"  ✓ {d['stats']['decoded']} 帧 / {d['stats']['keyframes']} 关键帧 / "
      f"首帧 {d['stats']['first_frame_ms']}ms / {d['stats']['bytes']} 字节 / "
      f"{d['bitrate_kbps']:.0f} kbps")
PY
[ $? -eq 0 ] || fail "断言未通过"

say "5/5 错误路径抽查（错密码必须被拒）"
set +e
"$BIN/uulan-client.exe" --server "127.0.0.1:$PORT" --device "$DEVICE" \
  --password "wrongPass9" --frames 1 >"$DIR/bad.out" 2>&1
BAD_RC=$?
set -e
[ "$BAD_RC" -ne 0 ] || fail "错密码竟然连上了"
grep -q "密码" "$DIR/bad.out" || fail "错密码的错误信息没说明原因: $(cat "$DIR/bad.out")"
echo "  ✓ 错密码被拒，提示：$(head -1 "$DIR/bad.out")"

printf '\n\033[32m冒烟通过：三个 release 二进制跑通了 中继 → 被控端 → 控制端 → 解码 全链路\033[0m\n'
echo "日志留在 $DIR/"
