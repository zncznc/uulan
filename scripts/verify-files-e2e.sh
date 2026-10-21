#!/usr/bin/env bash
# M4 文件传输验收（真二进制、真 socket、真磁盘）：
#   真 server + 真被控端（lavfi 合成画面）+ 真控制端 CLI，跑 --ls / --mkdir / --put / --get / --rm。
#
# 断言的都是机器可查的量：sha256 一致、字节数一致、远端文件存在/消失。
# 为什么不从工作区里直接跑：位于 D:/default-workspace/ 之下的 exe 在这台开发机上
# **写不了任何文件**（被控端要落盘 host.json、要写上传的临时文件）。拷出去再跑。
#
# 用法：
#   bash scripts/verify-files-e2e.sh
#   UULAN_PORT=21996 UULAN_STAGE=D:/uulan-bin-fs bash scripts/verify-files-e2e.sh
#   UULAN_MIB=32 bash scripts/verify-files-e2e.sh     # 换更大的文件
#
# 退出码：0 = 通过；1 = 断言失败；2 = 环境问题（端口占着 / 进程没起来）。
set -u

PORT="${UULAN_PORT:-21996}"
MIB="${UULAN_MIB:-6}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${UULAN_STAGE:-D:/uulan-bin/fs-$(date +%Y%m%d-%H%M%S)}"
DATA="$WORK/data"
NO_COLOR=1
export NO_COLOR

mkdir -p "$WORK" "$DATA" || { echo "建不了 $WORK"; exit 2; }
cp -f "$ROOT"/target/release/uulan-server.exe \
      "$ROOT"/target/release/uulan-host.exe \
      "$ROOT"/target/release/uulan-client.exe "$WORK"/ || { echo "拷二进制失败（上一轮还在跑？）"; exit 2; }
echo "工作目录：$WORK  端口：$PORT  文件：${MIB} MiB"

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
  --capture "lavfi:testsrc2=size=640x360:rate=5" --name e2e-fs \
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

cli() {
  "$WORK/uulan-client.exe" --server "127.0.0.1:$PORT" --device "$DEVICE" \
    --password "$PW" --name e2e-fs-client "$@" 2>&1
}

strip() { sed -E 's/\x1b\[[0-9;]*m//g' "$1"; }
FAIL=0
check() { if [ "$1" = 0 ]; then echo "  ✓ $2"; else echo "  ✗ $2"; FAIL=1; fi; }

echo "──────── ① 列「此电脑」────────"
cli --ls ":/" >"$WORK/ls-drives.log" 2>&1
check $? "--ls :/ 成功"
sed -n 1,6p "$WORK/ls-drives.log" | sed 's/^/    /'
if grep -q "共 0 项" "$WORK/ls-drives.log"; then echo "  ✗ 「此电脑」是空的"; FAIL=1; fi

echo "──────── ② 建目录 ────────"
cli --mkdir "$DATA/sub" >"$WORK/mkdir.log" 2>&1
check $? "--mkdir"
[ -d "$DATA/sub" ] && check 0 "被控端磁盘上真的建出来了" || check 1 "目录没建出来"

echo "──────── ③ 上传 ${MIB} MiB ────────"
SRC="$WORK/upload.bin"
python -c "import sys;open(sys.argv[1],'wb').write(bytes((i*37+11)%256 for i in range($MIB*1024*1024)))" "$SRC"
REMOTE="$DATA/sub/upload.bin"
time cli --put "$SRC" "$REMOTE" >"$WORK/put.log" 2>&1
check $? "--put"
sed -n 1,3p "$WORK/put.log" | sed 's/^/    /'
A=$(sha256sum "$SRC" | cut -d' ' -f1)
B=$(sha256sum "$REMOTE" 2>/dev/null | cut -d' ' -f1)
[ -n "$B" ] && [ "$A" = "$B" ] && check 0 "sha256 一致（${A:0:12}…）" || check 1 "sha256 不一致（本地 $A / 远端 ${B:-缺失}）"
ls "$DATA/sub"/*.uulan-part >/dev/null 2>&1 && check 1 "残留了 .uulan-part 临时文件" || check 0 "没有残留临时文件"

echo "──────── ④ 下载回来 ────────"
BACK="$WORK/roundtrip.bin"
time cli --get "$REMOTE" "$BACK" >"$WORK/get.log" 2>&1
check $? "--get"
sed -n 1,3p "$WORK/get.log" | sed 's/^/    /'
C=$(sha256sum "$BACK" 2>/dev/null | cut -d' ' -f1)
[ "$A" = "$C" ] && check 0 "下载内容与源文件一致" || check 1 "下载内容不一致（${C:-缺失}）"

echo "──────── ⑤ 列目录 ────────"
cli --ls "$DATA/sub" >"$WORK/ls.log" 2>&1
check $? "--ls 远端目录"
grep -q "upload.bin" "$WORK/ls.log" && check 0 "列表里能看到 upload.bin" || check 1 "列表里没有 upload.bin"

echo "──────── ⑥ 删除 ────────"
cli --rm "$REMOTE" >"$WORK/rm.log" 2>&1
check $? "--rm"
[ -e "$REMOTE" ] && check 1 "文件还在" || check 0 "远端文件已删除"

echo "──────── 被控端日志（文件服务）────────"
strip "$WORK/host.log" | grep -E "文件服务已就绪|文件操作完成|文件操作失败" | tail -8 | sed 's/^/    /'
echo "──────── 中继/被控端错误 ────────"
strip "$WORK/host.log" | grep -E " ERROR |panic" | head -5 | sed 's/^/    /' || true

if [ "$FAIL" = 0 ]; then
  echo "OK：文件传输全链路通过（${MIB} MiB，sha256 往返一致）"
  exit 0
else
  echo "FAIL：见上面的 ✗ 行"
  exit 1
fi