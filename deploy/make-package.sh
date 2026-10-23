#!/bin/bash
# 打「离线安装包」：内网里没有 apt 源、没有公网，所以安装包就是
# 「二进制 + systemd 单元 + 安装脚本 + 文档」的 tar.gz，解包后 sudo bash install.sh。
#
# 用法（**在被控端同款机器上跑**，原生编译；不要交叉编译）：
#   bash deploy/make-package.sh                    # 版本号默认用今天日期
#   bash deploy/make-package.sh --version 0.3.0 --out /tmp
#   bash deploy/make-package.sh --with-client      # 连控制端 uulan-client 一起打进去
#
# 产物：/tmp/uulan-<version>-x86_64-linux.tar.gz
#       （解包即得 uulan-<version>-x86_64-linux/{bin,systemd,install.sh,uninstall.sh,doc}）
#
# 为什么要 --with-client 而不是默认带上：控制端要 winit/softbuffer 的 X11 后端，
# 在麒麟 V10 SP1 上必须靠 `x11-dlopen` 特性绕开缺失的 libx11-dev（见 Cargo.toml 注释），
# 别的发行版/精简系统上可能编不出来；编不出来不该连被控端的包一起打不出来。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
VERSION="$(date +%Y%m%d)"
OUT=/tmp
WITH_CLIENT=0

while [ $# -gt 0 ]; do
    case "$1" in
        --version) VERSION="$2"; shift 2 ;;
        --out)     OUT="$2"; shift 2 ;;
        --with-client) WITH_CLIENT=1; shift ;;
        -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
        *) echo "未知参数 $1" >&2; exit 2 ;;
    esac
done

NAME="uulan-$VERSION-x86_64-linux"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

PKGS="-p uulan-server -p uulan-host"
[ "$WITH_CLIENT" = 1 ] && PKGS="$PKGS -p uulan-client"
echo "==> 编译 $PKGS（原生，release）"
( cd "$ROOT" && cargo build --release $PKGS )

echo "==> 组装 $NAME"
install -d "$STAGE/$NAME/bin" "$STAGE/$NAME/systemd" "$STAGE/$NAME/doc"
install -m 0755 "$ROOT/target/release/uulan-server" "$STAGE/$NAME/bin/"
install -m 0755 "$ROOT/target/release/uulan-host"   "$STAGE/$NAME/bin/"
if [ "$WITH_CLIENT" = 1 ]; then
    [ -x "$ROOT/target/release/uulan-client" ] || { echo "打包失败：没有编出 uulan-client" >&2; exit 1; }
    install -m 0755 "$ROOT/target/release/uulan-client" "$STAGE/$NAME/bin/"
fi
install -m 0755 "$HERE/uulan-wait-x.sh"             "$STAGE/$NAME/bin/uulan-wait-x"
install -m 0644 "$HERE/systemd/"*.service           "$STAGE/$NAME/systemd/"
install -m 0755 "$HERE/install.sh"                  "$STAGE/$NAME/install.sh"
install -m 0755 "$HERE/uninstall.sh"                "$STAGE/$NAME/uninstall.sh"
for f in DESIGN.md DEPLOY-KYLIN.md; do
    [ -f "$ROOT/docs/$f" ] && install -m 0644 "$ROOT/docs/$f" "$STAGE/$NAME/doc/";
done
install -m 0644 "$HERE/README.md" "$STAGE/$NAME/README.md"

[ -f "$STAGE/$NAME/bin/uulan-host" ] || { echo "打包失败：没有二进制" >&2; exit 1; }

mkdir -p "$OUT"
TARBALL="$OUT/$NAME.tar.gz"
tar -C "$STAGE" -czf "$TARBALL" "$NAME"
echo "==> $TARBALL"
echo "    大小 $(du -h "$TARBALL" | cut -f1)；sha256："
sha256sum "$TARBALL" | sed 's/^/    /'
echo
echo "对方机器上：tar xzf $NAME.tar.gz && cd $NAME && sudo bash install.sh"