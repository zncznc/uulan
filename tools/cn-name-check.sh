#!/usr/bin/env bash
# 一次性手工验收：中文文件名 + 空格的工作目录里上传/下载/删除。
# 不是常驻脚本，只为这一轮留下可复现的证据。
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# 必须落到工作区**外**：D:/default-workspace 下的 exe 写不了文件（skill 坑 2）
W="${UULAN_STAGE:-D:/uulan-bin}/cn-check-$(date +%H%M%S)"
PORT=${UULAN_CN_PORT:-21998}
mkdir -p "$W/data"
cp -f "$ROOT"/target/release/uulan-{server,host,client}.exe "$W"/
SPID=""; HPID=""
cleanup() { [ -n "$HPID" ] && kill "$HPID" 2>/dev/null; [ -n "$SPID" ] && kill "$SPID" 2>/dev/null; }
trap cleanup EXIT

RUST_LOG=warn "$W/uulan-server.exe" --bind "127.0.0.1:$PORT" --state "$W/devices.json" >"$W/server.log" 2>&1 &
SPID=$!
sleep 1
RUST_LOG=info "$W/uulan-host.exe" --server "127.0.0.1:$PORT" --config "$W/host.json" \
  --capture "lavfi:testsrc2=size=320x240:rate=5" --name cn >"$W/host.log" 2>&1 &
HPID=$!
sleep 3
DEV=$(grep -oE "设备 ID : [0-9]{6,}" "$W/host.log" | tail -1 | sed 's/.*: *//' | tr -d '[:space:]')
PW=$(sed -n 's/.*"password": *"\([^"]*\)".*/\1/p' "$W/host.json" | head -1)
[ -n "$DEV" ] || { echo "没拿到设备 ID"; exit 2; }

cli() { "$W/uulan-client.exe" --server "127.0.0.1:$PORT" --device "$DEV" --password "$PW" --name cn-cli "$@" 2>&1 | tail -2; }

mkdir -p "$W/data/中文 子目录"
printf 'hello 中文内容\n' >"$W/源文件 报告.txt"
echo "── put ──"; cli --put "$W/源文件 报告.txt" "$W/data/中文 子目录/带空格 的文件.txt"
echo "── ls ──"; cli --ls "$W/data/中文 子目录"
echo "── get ──"; cli --get "$W/data/中文 子目录/带空格 的文件.txt" "$W/下载 副本.txt"
cmp "$W/源文件 报告.txt" "$W/下载 副本.txt" && echo "CN-ROUNDTRIP-OK（逐字节一致）" || echo "CN-ROUNDTRIP-FAIL"
echo "── rm ──"; cli --rm "$W/data/中文 子目录/带空格 的文件.txt"
[ -e "$W/data/中文 子目录/带空格 的文件.txt" ] && echo "CN-RM-FAIL" || echo "CN-RM-OK"
echo "工作目录：$W"