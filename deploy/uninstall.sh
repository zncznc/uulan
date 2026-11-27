#!/bin/bash
# lanlink 卸载脚本
#
#   sudo bash uninstall.sh              # 系统装：停服务、删单元，保留 /opt/lanlink 与配置
#   sudo bash uninstall.sh --purge      # 连二进制、状态、被控端配置一起删
#   bash uninstall.sh --userland        # 用户态装（无 root）
set -euo pipefail

USERLAND=0
PREFIX=""
LANLINK_USER="${SUDO_USER:-$(id -un)}"
PURGE=0
while [ $# -gt 0 ]; do
    case "$1" in
        --purge)    PURGE=1; shift ;;
        --user)     LANLINK_USER="$2"; shift 2 ;;
        --userland) USERLAND=1; LANLINK_USER="$(id -un)"; shift ;;
        --prefix)   PREFIX="$2"; shift 2 ;;
        -h|--help)  sed -n '2,10p' "$0"; exit 0 ;;
        *) echo "未知参数 $1" >&2; exit 2 ;;
    esac
done

if [ "$USERLAND" = 1 ]; then
    [ "$(id -u)" != 0 ] || { echo "用户态卸载请用普通用户跑" >&2; exit 1; }
    PREFIX="${PREFIX:-$HOME/.local/lanlink}"
else
    [ "$(id -u)" = 0 ] || { echo "需要 root（没有 root 就用 --userland）" >&2; exit 1; }
    PREFIX="${PREFIX:-/opt/lanlink}"
fi

USER_HOME="$(getent passwd "$LANLINK_USER" | cut -d: -f6)"
USER_UID="$(id -u "$LANLINK_USER")"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$USER_UID}"
[ -S "$XDG_RUNTIME_DIR/bus" ] && export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"
as_user() {
    if [ "$(id -u)" = 0 ] && [ "$LANLINK_USER" != root ]; then
        runuser -u "$LANLINK_USER" -- env \
            XDG_RUNTIME_DIR="/run/user/$USER_UID" \
            DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$USER_UID/bus" \
            HOME="$USER_HOME" "$@"
    else
        "$@"
    fi
}

echo "==> 停止被控端（用户单元）"
as_user systemctl --user disable --now lanlink-host.service 2>/dev/null || true
rm -f "$USER_HOME/.config/systemd/user/lanlink-host.service"

if [ "$USERLAND" = 1 ]; then
    echo "==> 停止中继（用户单元）"
    as_user systemctl --user disable --now lanlink-server.service 2>/dev/null || true
    rm -f "$USER_HOME/.config/systemd/user/lanlink-server.service"
    as_user systemctl --user daemon-reload 2>/dev/null || true
else
    echo "==> 停止中继（系统单元）"
    systemctl disable --now lanlink-server.service 2>/dev/null || true
    rm -f /etc/systemd/system/lanlink-server.service
    systemctl daemon-reload
fi

pkill -x lanlink-host 2>/dev/null || true
pkill -x lanlink-server 2>/dev/null || true

if [ "$PURGE" = 1 ]; then
    echo "==> 删除 $USER_HOME/.config/lanlink 与 $PREFIX"
    rm -rf "$USER_HOME/.config/lanlink" "$PREFIX"
    loginctl disable-linger "$LANLINK_USER" 2>/dev/null || true
fi

echo "完成。$([ "$PURGE" = 1 ] && echo '（已彻底清除）' || echo '（配置与二进制保留，重新安装会复用设备 ID 与密码）')"
