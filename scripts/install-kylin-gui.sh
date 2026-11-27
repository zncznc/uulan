#!/bin/bash
# 在麒麟桌面用户里安装「被控端信息面板」——**装成可点击的图标，不是开机自启**。
#
# 为什么不做成自启：对面那个人是「需要远程协助时才点开它」，把码念给你。开机就挂一个
# 窗口既扰民，又让那屏验证码长时间暴露在屏幕上（暴露越久，越容易被旁人看到）。
#
# 用法（在麒麟上，以桌面登录的那个用户身份，不要 sudo）：
#   bash install-kylin-gui.sh            # 优先用包里的 bin/uulan-gui；没有才现场编译
#   bash install-kylin-gui.sh --bin /path/to/uulan-gui   # 用现成的二进制
#
# 装完之后：桌面出现「uulan 远程协助」图标，开始菜单里也有一项。
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

SRC_DIR="${SRC_DIR:-$HOME/.local/uulan/src}"
DEST="$HOME/.local/uulan/bin"
mkdir -p "$DEST"

# 离线包里就带着二进制：从包目录里取，**不需要 cargo、不需要源码**。
# 这一步必须放在编译分支之前 —— 内网里没有一台机器有 cargo。
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [ -z "$BIN_SRC" ]; then
  for c in "$HERE/bin/uulan-gui" "$HERE/uulan-gui"; do
    [ -x "$c" ] && { BIN_SRC="$c"; break; }
  done
fi

if [ -z "$BIN_SRC" ]; then
  command -v cargo >/dev/null 2>&1 || { echo "既没在包里找到 uulan-gui，也没有 cargo；装不了。" >&2; exit 1; }
  [ -d "$SRC_DIR" ] || { echo "找不到源码目录 $SRC_DIR（可用 SRC_DIR=... 指定）" >&2; exit 1; }
  echo "现场编译 uulan-gui（海光 CPU 上约 20 秒～几分钟）…"
  ( cd "$SRC_DIR" && cargo build --release -p uulan-gui )
  BIN_SRC="$SRC_DIR/target/release/uulan-gui"
fi

[ -f "$BIN_SRC" ] || { echo "二进制不存在：$BIN_SRC" >&2; exit 1; }
install -m 0755 "$BIN_SRC" "$DEST/uulan-gui"
echo "已安装：$DEST/uulan-gui"
# 若同目录有 uulan-client / ffmpeg，同步放一份到同目录，确保从桌面图标打开面板能直接发起远控并解码
for extra in uulan-client ffmpeg; do
  SRC_EXTRA="$(dirname "$BIN_SRC")/$extra"
  if [ -x "$SRC_EXTRA" ] && [ "$(readlink -f "$SRC_EXTRA")" != "$(readlink -f "$DEST/$extra" 2>/dev/null || echo '')" ]; then
    install -m 0755 "$SRC_EXTRA" "$DEST/$extra"
    echo "已同步：$DEST/$extra"
  fi
done

# ---- 菜单项 ----
APPDIR="$HOME/.local/share/applications"
mkdir -p "$APPDIR"
cat > "$APPDIR/uulan-gui.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=uulan 远程协助
Name[zh_CN]=uulan 远程协助
Comment=显示本机设备 ID 与一次性验证码，便于对方连接
Comment[zh_CN]=显示本机设备 ID 与一次性验证码，便于对方连接
Exec=$DEST/uulan-gui
Terminal=false
Categories=Network;RemoteAccess;
StartupNotify=true
EOF
chmod 0644 "$APPDIR/uulan-gui.desktop"
echo "开始菜单项：$APPDIR/uulan-gui.desktop"

# ---- 桌面图标 ----
# 中文环境的桌面目录是「桌面」不是 Desktop，必须问 xdg；没有 xdg-user-dir 再退到 \$HOME/Desktop。
if command -v xdg-user-dir >/dev/null 2>&1; then
  DESK="$(xdg-user-dir DESKTOP)"
else
  DESK="$HOME/Desktop"
fi
[ -d "$DESK" ] || DESK="$(ls -d "$HOME"/桌面 "$HOME"/Desktop 2>/dev/null | head -1)"
[ -n "$DESK" ] && [ -d "$DESK" ] || { echo "找不到桌面目录，跳过桌面图标（菜单项已装好）。"; DESK=""; }

if [ -n "$DESK" ]; then
  ICON="$DESK/uulan-远程协助.desktop"
  cp "$APPDIR/uulan-gui.desktop" "$ICON"
  chmod 0755 "$ICON"
  # UKUI/nautilus 只信任被显式标记为可信任的桌面启动器，否则双击会弹「不受信任」。
  gio set "$ICON" metadata::trusted true 2>/dev/null || true
  echo "桌面图标：$ICON"
fi

# ---- 顺手把自启关掉（这是用户要的「需要时点开」语义）----
if [ -f "$HOME/.config/autostart/uulan-gui.desktop" ]; then
  mv "$HOME/.config/autostart/uulan-gui.desktop" "$HOME/.config/autostart/uulan-gui.desktop.disabled"
  echo "已取消开机自启（改名为 .disabled）——这一屏只在需要时打开。"
fi

echo
echo "下一步：双击桌面上的「uulan 远程协助」，切到第二页「本机（被控端）信息」，"
echo "把上面的设备 ID 和一次性临时验证码念给对方即可。"
