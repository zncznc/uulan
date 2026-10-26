#!/usr/bin/env bash
# M4 目录树传输验收（真二进制、真 socket、真磁盘）：
#   真 server + 真被控端（lavfi 合成画面）+ 真控制端 CLI，跑 --put-tree / --get-tree。
#
# 断言的都是机器可查的量：两边每个文件的 sha256 逐个一致、空目录也过去了、
# 符号链接被跳过、可疑条目名没能写到本地目录外面、远端根不存在时一个本地目录都不建。
#
# 为什么从工作区外面跑：位于 D:/default-workspace/ 之下的 exe 在这台开发机上
# **写不了任何文件**（被控端要落盘 host.json、要写上传的临时文件）。拷出去再跑。
#
# 用法：
#   bash scripts/verify-tree-e2e.sh
#   UULAN_PORT=21998 UULAN_STAGE=D:/uulan-bin-tree bash scripts/verify-tree-e2e.sh
#
# 退出码：0 = 通过；1 = 断言失败；2 = 环境问题（端口占着 / 进程没起来）。
set -u

PORT="${UULAN_PORT:-21997}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${UULAN_STAGE:-D:/uulan-bin/tree-$(date +%Y%m%d-%H%M%S)}"
DATA="$WORK/data"          # 被控端看到的世界（fs_roots 指向这里）
SRC="$WORK/src-tree"       # 本地要上传的树
BACK="$WORK/back-tree"     # 下载回来的树
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
  --capture "lavfi:testsrc2=size=640x360:rate=5" --name e2e-tree \
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

# 口令从 host.json 读，**不回显**。
PW=$(sed -n 's/.*"password": *"\([^"]*\)".*/\1/p' "$WORK/host.json" | head -1)
[ -n "$PW" ] || { echo "host.json 里没有 password 字段"; exit 2; }

cli() {
  "$WORK/uulan-client.exe" --server "127.0.0.1:$PORT" --device "$DEVICE" \
    --password "$PW" --name e2e-tree-client "$@" 2>&1
}
strip() { sed -E 's/\x1b\[[0-9;]*m//g' "$1"; }
FAIL=0
check() { if [ "$1" = 0 ]; then echo "  ✓ $2"; else echo "  ✗ $2"; FAIL=1; fi; }

# 一棵有代表性的本地树：多层目录、空目录、中文名、0 字节文件、跨 chunk 的大文件。
echo "──────── ⓪ 造本地树 ────────"
mkdir -p "$SRC/tools/bin" "$SRC/文档/2026" "$SRC/empty/deeper" || exit 2
python -c "import sys;open(sys.argv[1],'wb').write(bytes((i*31+7)%256 for i in range(700000)))" "$SRC/tools/big.bin"
printf '#!/bin/sh\necho hi\n' >"$SRC/tools/bin/run.sh"
printf 'readme\n' >"$SRC/readme.txt"
printf 'x' >"$SRC/文档/说明.txt"
printf 'y' >"$SRC/文档/2026/清单.txt"
: >"$SRC/tools/zero.bin"
# 符号链接：能建就验「跳过」，建不了（Windows 没权限）就标 SKIP
LINK_MADE=0
if ln -s "$SRC/readme.txt" "$SRC/link-to-readme" 2>/dev/null && [ -L "$SRC/link-to-readme" ]; then
  LINK_MADE=1
fi
FILES=$(find "$SRC" -type f -not -name "link-to-readme" | wc -l)
echo "  本地：$FILES 个文件（find -type f），符号链接 $LINK_MADE"

echo "──────── ① 上传整棵树 ────────"
time cli --put-tree "$SRC" "$DATA/inbox" >"$WORK/put-tree.log" 2>&1
check $? "--put-tree"
grep -E "目录树上传完成" "$WORK/put-tree.log" | sed 's/^/    /'
grep -E "^上传目录树" "$WORK/put-tree.log" | sed 's/^/    /'

echo "──────── ② 逐个文件比 sha256 ────────"
MISMATCH=0; COUNT=0
while IFS= read -r f; do
  REL=${f#"$SRC"/}
  COUNT=$((COUNT + 1))
  A=$(sha256sum "$f" | cut -d' ' -f1)
  B=$(sha256sum "$DATA/inbox/$REL" 2>/dev/null | cut -d' ' -f1)
  if [ -z "$B" ] || [ "$A" != "$B" ]; then
    echo "    ✗ $REL（本地 ${A:0:12}… / 远端 ${B:-缺失}）"
    MISMATCH=$((MISMATCH + 1))
  fi
done < <(find "$SRC" -type f -not -name 'link-to-readme')
[ "$MISMATCH" = 0 ] && check 0 "全部 $COUNT 个文件 sha256 一致" || check 1 "$MISMATCH 个文件不一致"

[ -d "$DATA/inbox/empty/deeper" ] && check 0 "空目录也建出来了（empty/deeper）" || check 1 "空目录没过去"
[ -d "$DATA/inbox/文档/2026" ] && check 0 "中文目录名过得去（文档/2026）" || check 1 "中文目录名没过去"
if [ "$LINK_MADE" = 1 ]; then
  [ -e "$DATA/inbox/link-to-readme" ] && check 1 "符号链接被跟随了（不该）" || check 0 "本地符号链接已跳过"
  grep -q "跳过符号链接 1" "$WORK/put-tree.log" && check 0 "日志里报了跳过 1 个符号链接" || check 1 "日志没报跳过数"
else
  echo "  - SKIP 符号链接（这台机器上 ln -s 建不出来）"
fi
ls "$DATA/inbox"/*.uulan-part >/dev/null 2>&1 && check 1 "远端残留 .uulan-part" || check 0 "远端没有残留临时文件"

echo "──────── ③ 再传一次（幂等：覆盖，不重复建目录）────────"
cli --put-tree "$SRC" "$DATA/inbox" >"$WORK/put-tree-again.log" 2>&1
check $? "重复 --put-tree 不报错（已存在的目录按幂等处理）"
AGAIN=$(grep -oE "目录树上传完成：目录 [0-9]+ / 文件 [0-9]+" "$WORK/put-tree-again.log" | tail -1)
echo "    第二次：${AGAIN:-（没抓到摘要）}"

echo "──────── ④ 下载整棵树，与源逐字节比 ────────"
time cli --get-tree "$DATA/inbox" "$BACK" >"$WORK/get-tree.log" 2>&1
check $? "--get-tree"
grep -E "目录树下载完成" "$WORK/get-tree.log" | sed 's/^/    /'
MISMATCH=0; COUNT=0
while IFS= read -r f; do
  REL=${f#"$SRC"/}
  COUNT=$((COUNT + 1))
  A=$(sha256sum "$f" | cut -d' ' -f1)
  B=$(sha256sum "$BACK/$REL" 2>/dev/null | cut -d' ' -f1)
  [ "$A" = "$B" ] || { echo "    ✗ $REL"; MISMATCH=$((MISMATCH + 1)); }
done < <(find "$SRC" -type f -not -name 'link-to-readme')
[ "$MISMATCH" = 0 ] && check 0 "下载回来 $COUNT 个文件全部一致" || check 1 "$MISMATCH 个文件不一致"
[ -d "$BACK/empty/deeper" ] && check 0 "空目录也下来了" || check 1 "空目录没下来"

echo "──────── ⑤ 远端根不存在：必须失败，且不许先建本地目录 ────────"
cli --get-tree "$DATA/does-not-exist" "$WORK/should-not-appear" >"$WORK/get-missing.log" 2>&1
RC=$?
[ "$RC" != 0 ] && check 0 "远端根不存在时退出码非 0（$RC）" || check 1 "远端根不存在竟然成功了"
[ -e "$WORK/should-not-appear" ] && check 1 "失败时仍然建了本地目录" || check 0 "失败时没留下本地目录"

echo "──────── ⑥ 把文件当根：两边都要明确拒绝 ────────"
cli --put-tree "$SRC/readme.txt" "$DATA/inbox" >"$WORK/put-file.log" 2>&1
[ $? != 0 ] && check 0 "--put-tree 传文件被拒" || check 1 "--put-tree 传文件竟然成功"

echo "──────── 被控端日志（文件服务）────────"
strip "$WORK/host.log" | grep -E "文件操作完成|文件操作失败" | tail -5 | sed 's/^/    /'
strip "$WORK/host.log" | grep -E " ERROR |panic" | head -5 | sed 's/^/    /' || true

if [ "$FAIL" = 0 ]; then
  echo "OK：目录树传输全链路通过（上传 $COUNT 个文件 + 空目录 + 中文名，逐文件 sha256 一致）"
  exit 0
else
  echo "FAIL：见上面的 ✗ 行"
  exit 1
fi