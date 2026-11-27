#!/bin/bash
# 在麒麟桌面用户里安装「被控端信息面板」——**装成可点击的图标，不是开机自启**。
#
# 用法（在麒麟上，以桌面登录的那个用户身份，不要 sudo）：
#   bash install-kylin-gui.sh            # 优先用包里的 bin/lanlink-gui；没有才现场编译
#   bash install-kylin-gui.sh --bin /path/to/lanlink-gui
#
# 装完之后：桌面出现「lanlink 远程协助」图标，开始菜单里也有一项。
set -e

BIN_SRC=""
while [ $# -gt 0 ]; do
  case "$1" in
    --bin) BIN_SRC="$2"; shift 2 ;;
    *) echo "未知参数 $1"; exit 2 ;;
  esac
done

if [ "$(id -u)" = "0" ]; then
  echo "别用 root 跑这个：桌面图标与自启项都属于当前登录用户。" >&2
  exit 1
fi

SRC_DIR="${SRC_DIR:-$HOME/.local/lanlink/src}"
DEST="$HOME/.local/lanlink/bin"
mkdir -p "$DEST"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [ -z "$BIN_SRC" ]; then
  for c in "$HERE/bin/lanlink-gui" "$HERE/lanlink-gui"; do
    [ -x "$c" ] && { BIN_SRC="$c"; break; }
  done
fi

if [ -z "$BIN_SRC" ]; then
  command -v cargo >/dev/null 2>&1 || { echo "既没在包里找到 lanlink-gui，也没有 cargo；装不了。" >&2; exit 1; }
  [ -d "$SRC_DIR" ] || { echo "找不到源码目录 $SRC_DIR（可用 SRC_DIR=... 指定）" >&2; exit 1; }
  echo "现场编译 lanlink-gui（海光 CPU 上约 20 秒～几分钟）…"
  ( cd "$SRC_DIR" && cargo build --release -p uulan-gui )
  BIN_SRC="$SRC_DIR/target/release/lanlink-gui"
fi

[ -f "$BIN_SRC" ] || { echo "二进制不存在：$BIN_SRC" >&2; exit 1; }
if [ "$(readlink -f "$BIN_SRC")" != "$(readlink -f "$DEST/lanlink-gui" 2>/dev/null || echo '')" ]; then
  install -m 0755 "$BIN_SRC" "$DEST/lanlink-gui"
fi
echo "已安装：$DEST/lanlink-gui"
# 若同目录有 lanlink-client / ffmpeg，同步放一份到同目录
for extra in lanlink-client ffmpeg; do
  SRC_EXTRA="$(dirname "$BIN_SRC")/$extra"
  if [ -x "$SRC_EXTRA" ] && [ "$(readlink -f "$SRC_EXTRA")" != "$(readlink -f "$DEST/$extra" 2>/dev/null || echo '')" ]; then
    install -m 0755 "$SRC_EXTRA" "$DEST/$extra"
    echo "已同步：$DEST/$extra"
  fi
done

# ---- 菜单项 ----
APPDIR="$HOME/.local/share/applications"
mkdir -p "$APPDIR"
cat > "$APPDIR/lanlink-gui.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=lanlink 远程协助
Name[zh_CN]=lanlink 远程协助
Comment=内网远程协助与控制面板
Comment[zh_CN]=内网远程协助与控制面板
Exec=$DEST/lanlink-gui
Terminal=false
Categories=Network;RemoteAccess;
StartupNotify=true
EOF
chmod 0644 "$APPDIR/lanlink-gui.desktop"
echo "开始菜单项：$APPDIR/lanlink-gui.desktop"

# ---- 桌面图标 ----
if command -v xdg-user-dir >/dev/null 2>&1; then
  DESK="$(xdg-user-dir DESKTOP)"
else
  DESK="$HOME/Desktop"
fi
[ -d "$DESK" ] || DESK="$(ls -d "$HOME"/桌面 "$HOME"/Desktop 2>/dev/null | head -1)"
[ -n "$DESK" ] && [ -d "$DESK" ] || { echo "找不到桌面目录，跳过桌面图标（菜单项已装好）。"; DESK=""; }

if [ -n "$DESK" ]; then
  ICON="$DESK/lanlink-远程协助.desktop"
  cp "$APPDIR/lanlink-gui.desktop" "$ICON"
  chmod 0755 "$ICON"
  gio set "$ICON" metadata::trusted true 2>/dev/null || true
  echo "桌面图标：$ICON"
fi

if [ -f "$HOME/.config/autostart/lanlink-gui.desktop" ]; then
  mv "$HOME/.config/autostart/lanlink-gui.desktop" "$HOME/.config/autostart/lanlink-gui.desktop.disabled"
  echo "已取消开机自启（改名为 .disabled）——这一屏只在需要时打开。"
fi

echo
echo "下一步：双击桌面上的「lanlink 远程协助」，切到第二页「本机（被控端）信息」，"
echo "把上面的设备 ID 和一次性临时验证码念给对方即可。"
