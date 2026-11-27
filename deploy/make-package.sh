#!/bin/bash
# 打「离线安装包」：内网里没有 apt 源、没有公网，所以安装包就是
# 「二进制 + systemd 单元 + 安装脚本 + 文档」的 tar.gz，解包后 sudo bash install.sh。
#
# 用法（**在被控端同款机器上跑**，原生编译；不要交叉编译）：
#   bash deploy/make-package.sh                    # 默认版本 0.2.0
#   bash deploy/make-package.sh --version 0.2.0 --out /tmp
#   bash deploy/make-package.sh --with-client      # 连控制端 lanlink-client 一起打进去
#   bash deploy/make-package.sh --with-gui         # 连图形面板 lanlink-gui 一起打进去
#   bash deploy/make-package.sh --with-ffmpeg /tmp/ffmpeg-static
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
VERSION="0.2.0"
OUT=/tmp
WITH_CLIENT=0
WITH_GUI=0
FFMPEG_BIN=""

while [ $# -gt 0 ]; do
    case "$1" in
        --version) VERSION="$2"; shift 2 ;;
        --out)     OUT="$2"; shift 2 ;;
        --with-client) WITH_CLIENT=1; shift ;;
        --with-gui)    WITH_GUI=1; shift ;;
        --with-ffmpeg) FFMPEG_BIN="$2"; shift 2 ;;
        -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
        *) echo "未知参数 $1" >&2; exit 2 ;;
    esac
done

NAME="lanlink-$VERSION-x86_64-linux"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

PKGS="-p uulan-server -p uulan-host"
[ "$WITH_CLIENT" = 1 ] && PKGS="$PKGS -p uulan-client"
[ "$WITH_GUI" = 1 ] && PKGS="$PKGS -p uulan-gui"
echo "==> 编译 $PKGS（原生，release）"
( cd "$ROOT" && cargo build --release $PKGS )

echo "==> 组装 $NAME"
install -d "$STAGE/$NAME/bin" "$STAGE/$NAME/systemd" "$STAGE/$NAME/doc"
install -m 0755 "$ROOT/target/release/lanlink-server" "$STAGE/$NAME/bin/"
install -m 0755 "$ROOT/target/release/lanlink-host"   "$STAGE/$NAME/bin/"
if [ "$WITH_CLIENT" = 1 ]; then
    [ -x "$ROOT/target/release/lanlink-client" ] || { echo "打包失败：没有编出 lanlink-client" >&2; exit 1; }
    install -m 0755 "$ROOT/target/release/lanlink-client" "$STAGE/$NAME/bin/"
fi
install -m 0755 "$HERE/lanlink-wait-x.sh"             "$STAGE/$NAME/bin/lanlink-wait-x"
if [ -n "$FFMPEG_BIN" ]; then
    [ -x "$FFMPEG_BIN" ] || { echo "打包失败：找不到可执行的 ffmpeg：$FFMPEG_BIN" >&2; exit 1; }
    install -m 0755 "$FFMPEG_BIN" "$STAGE/$NAME/bin/ffmpeg"
fi
if [ "$WITH_GUI" = 1 ]; then
    [ -x "$ROOT/target/release/lanlink-gui" ] || { echo "打包失败：没有编出 lanlink-gui" >&2; exit 1; }
    install -m 0755 "$ROOT/target/release/lanlink-gui" "$STAGE/$NAME/bin/"
    install -m 0644 "$ROOT/scripts/install-kylin-gui.sh" "$STAGE/$NAME/install-gui.sh"
fi
install -m 0644 "$HERE/systemd/"*.service           "$STAGE/$NAME/systemd/"
install -m 0755 "$HERE/install.sh"                  "$STAGE/$NAME/install.sh"
install -m 0755 "$HERE/uninstall.sh"                "$STAGE/$NAME/uninstall.sh"
for f in DESIGN.md DEPLOY-KYLIN.md; do
    [ -f "$ROOT/docs/$f" ] && install -m 0644 "$ROOT/docs/$f" "$STAGE/$NAME/doc/";
done
install -m 0644 "$HERE/README.md" "$STAGE/$NAME/README.md"

[ -f "$STAGE/$NAME/bin/lanlink-host" ] || { echo "打包失败：没有二进制" >&2; exit 1; }

mkdir -p "$OUT"
TARBALL="$OUT/$NAME.tar.gz"
tar -C "$STAGE" -czf "$TARBALL" "$NAME"
echo "==> $TARBALL"
echo "    大小 $(du -h "$TARBALL" | cut -f1)；sha256："
sha256sum "$TARBALL" | sed 's/^/    /'
echo
echo "对方机器上："
echo "  tar xzf $NAME.tar.gz && cd $NAME && sudo bash install.sh --user <桌面用户> --server <中继IP:端口>"
if [ "$WITH_GUI" = 1 ]; then
    echo "  再切到那个桌面用户：bash install-gui.sh        # 装桌面图标「lanlink 远程协助」"
fi
