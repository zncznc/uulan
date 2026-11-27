#!/bin/bash
# lanlink 安装脚本（银河麒麟 V10 SP1 / 任何 systemd + X11 的 Linux）
#
# 两种装法，装的东西一样，只是位置与单元归属不同：
#
#   1) 系统装（root，生产默认）：二进制进 /opt/lanlink，中继是**系统单元**，
#      被控端是**用户单元**。
#        sudo bash install.sh --user mis --server 192.168.223.15:21116
#
#   2) 用户态装（普通用户，无 root）：全部进 ~/.local/lanlink，中继与被控端都是
#      **用户单元**，靠 `loginctl enable-linger` 开机自启。
#        bash install.sh --userland --server 192.168.223.15:21116
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
USERLAND=0
PREFIX=""
LANLINK_USER="${SUDO_USER:-$(id -un)}"
SERVER=127.0.0.1:21116
BIND=0.0.0.0:21116
BIN_DIR=""
DO_SERVER=1
DO_HOST=1
DO_START=1

die() { echo "错误: $*" >&2; exit 1; }
say() { echo "==> $*"; }

while [ $# -gt 0 ]; do
    case "$1" in
        --bin)      BIN_DIR="$2"; shift 2 ;;
        --user)     LANLINK_USER="$2"; shift 2 ;;
        --server)   SERVER="$2"; shift 2 ;;
        --bind)     BIND="$2"; shift 2 ;;
        --prefix)   PREFIX="$2"; shift 2 ;;
        --userland) USERLAND=1; shift ;;
        --no-server) DO_SERVER=0; shift ;;
        --no-host)   DO_HOST=0; shift ;;
        --no-start)  DO_START=0; shift ;;
        -h|--help)  sed -n '2,14p' "$0"; exit 0 ;;
        *) die "未知参数 $1（--help 看用法）" ;;
    esac
done

command -v systemctl >/dev/null || die "没有 systemd（这不是 systemd 系统？）"

if [ "$USERLAND" = 1 ]; then
    [ "$(id -u)" != 0 ] || die "用户态装请用普通用户跑（不要 sudo）"
    LANLINK_USER="$(id -un)"
    PREFIX="${PREFIX:-$HOME/.local/lanlink}"
else
    [ "${LANLINK_FAKE_ROOT:-${UULAN_FAKE_ROOT:-}}" = 1 ] || [ "$(id -u)" = 0 ] || die "系统装需要 root：sudo bash install.sh（没有 root 就用 --userland）"
    PREFIX="${PREFIX:-/opt/lanlink}"
fi
id "$LANLINK_USER" >/dev/null 2>&1 || die "用户不存在: $LANLINK_USER"

USER_HOME="$(getent passwd "$LANLINK_USER" | cut -d: -f6)"
USER_UID="$(id -u "$LANLINK_USER")"
USER_UNIT_DIR="$USER_HOME/.config/systemd/user"
SYSTEM_UNIT_DIR="${LANLINK_SYSTEM_UNIT_DIR:-${UULAN_SYSTEM_UNIT_DIR:-/etc/systemd/system}}"
[ -d "$USER_HOME" ] || die "读不到 $LANLINK_USER 的家目录"

# 二进制从哪来：--bin > 包内 ./bin > 源码树 target/release
if [ -z "$BIN_DIR" ]; then
    if [ -d "$HERE/bin" ]; then BIN_DIR="$HERE/bin"
    elif [ -d "$HERE/../target/release" ]; then BIN_DIR="$HERE/../target/release"
    else die "找不到二进制，用 --bin 指定"
    fi
fi
[ -x "$BIN_DIR/lanlink-host" ] || die "$BIN_DIR 里没有 lanlink-host"
if [ "$DO_SERVER" = 1 ]; then
    [ -x "$BIN_DIR/lanlink-server" ] || die "$BIN_DIR 里没有 lanlink-server"
fi

UNIT_SRC="$HERE/systemd"
[ -d "$UNIT_SRC" ] || UNIT_SRC="$HERE/../deploy/systemd"
[ -d "$UNIT_SRC" ] || die "找不到 systemd 单元模板（$HERE/systemd）"

WAIT_X=""
for c in "$HERE/lanlink-wait-x.sh" "$HERE/bin/lanlink-wait-x" "$HERE/bin/lanlink-wait-x.sh"; do
    if [ -f "$c" ]; then WAIT_X="$c"; break; fi
done
[ -n "$WAIT_X" ] || die "找不到 lanlink-wait-x（源码树应在 deploy/，安装包应在 bin/）"

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$USER_UID}"
if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ] && [ -S "$XDG_RUNTIME_DIR/bus" ]; then
    export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"
fi

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
user_ctl() { as_user systemctl --user "$@"; }

# ---------- 预检：对端中继与目标端口 ----------
if [ "$DO_SERVER" = 1 ]; then
    RELAY_PORT="${BIND##*:}"
    [ "$RELAY_PORT" != "$BIND" ] || die "--bind 里没带端口: $BIND"
    PEER=""
    if [ "$USERLAND" = 1 ]; then
        if systemctl is-active --quiet lanlink-server.service 2>/dev/null; then
            PEER="系统单元 lanlink-server.service 正在运行"
        elif systemctl is-enabled --quiet lanlink-server.service 2>/dev/null; then
            PEER="系统单元 lanlink-server.service 开着开机自启（下次开机就会抢端口 $RELAY_PORT）"
        fi
        OWN_ACTIVE=0
        user_ctl is-active --quiet lanlink-server.service 2>/dev/null && OWN_ACTIVE=1
    else
        if user_ctl is-active --quiet lanlink-server.service 2>/dev/null; then
            PEER="用户 $LANLINK_USER 的用户单元 lanlink-server.service 正在运行"
        elif [ -e "$USER_HOME/.config/systemd/user/default.target.wants/lanlink-server.service" ]; then
            PEER="用户 $LANLINK_USER 的用户单元 lanlink-server.service 开着开机自启（下次开机就会抢端口 $RELAY_PORT）"
        fi
        OWN_ACTIVE=0
        systemctl is-active --quiet lanlink-server.service 2>/dev/null && OWN_ACTIVE=1
    fi
    if [ -n "$PEER" ]; then
        if [ "$USERLAND" = 1 ]; then
            FIX="sudo systemctl disable --now lanlink-server.service"
        else
            FIX="sudo -u $LANLINK_USER XDG_RUNTIME_DIR=/run/user/$USER_UID systemctl --user disable --now lanlink-server.service"
        fi
        die "对端中继没让位：$PEER。
     本次安装要装的是另一套中继单元，两边都监听 $RELAY_PORT，装到一半必炸。
     请先让位再重跑（要 disable 而不是只 stop：只 stop 的话下次开机又抢回来）：
         $FIX
     这次不需要装中继的话，也可以加 --no-server 只装被控端。"
    fi
    if [ "$OWN_ACTIVE" = 0 ] && command -v ss >/dev/null 2>&1; then
        if ss -ltn 2>/dev/null | awk '{print $4}' | grep -qE "[:.]${RELAY_PORT}\$"; then
            die "端口 $RELAY_PORT 已被占用，而占用者不是本次要重装的那套中继。
     先看是谁在听：ss -ltnp | grep :$RELAY_PORT
     处理掉再重跑，或 --bind 换一个端口。"
        fi
    fi
fi

say "安装到 $PREFIX（$([ "$USERLAND" = 1 ] && echo 用户态 || echo 系统装)，运行身份 $LANLINK_USER，中继 $SERVER）"
install -d -m 0755 "$PREFIX" "$PREFIX/bin" "$PREFIX/doc" "$PREFIX/state"
install -m 0755 "$BIN_DIR/lanlink-host" "$PREFIX/bin/lanlink-host"
if [ "$DO_SERVER" = 1 ]; then
    install -m 0755 "$BIN_DIR/lanlink-server" "$PREFIX/bin/lanlink-server"
fi
install -m 0755 "$WAIT_X" "$PREFIX/bin/lanlink-wait-x"
HAVE_CLIENT=0
if [ -x "$BIN_DIR/lanlink-client" ]; then
    install -m 0755 "$BIN_DIR/lanlink-client" "$PREFIX/bin/lanlink-client"
    HAVE_CLIENT=1
fi
HAVE_GUI=0
if [ -x "$BIN_DIR/lanlink-gui" ]; then
    install -m 0755 "$BIN_DIR/lanlink-gui" "$PREFIX/bin/lanlink-gui"
    HAVE_GUI=1
fi
HAVE_FFMPEG=0
if [ -x "$BIN_DIR/ffmpeg" ]; then
    install -m 0755 "$BIN_DIR/ffmpeg" "$PREFIX/bin/ffmpeg"
    HAVE_FFMPEG=1
fi
if [ -d "$HERE/doc" ]; then
    cp -f "$HERE/doc/"*.md "$PREFIX/doc/" 2>/dev/null || true
fi

# ---------- 中继 ----------
if [ "$DO_SERVER" = 1 ]; then
    if [ "$USERLAND" = 1 ]; then
        say "安装 lanlink-server.service（用户单元）"
        install -d -m 0755 "$USER_UNIT_DIR"
        sed -e "s|@LANLINK_PREFIX@|$PREFIX|g" -e "s|@LANLINK_BIND@|$BIND|g" \
            "$UNIT_SRC/lanlink-server.user.service" > "$USER_UNIT_DIR/lanlink-server.service"
        user_ctl daemon-reload
        user_ctl enable lanlink-server.service >/dev/null
        [ "$DO_START" = 1 ] && user_ctl restart lanlink-server.service
    else
        say "安装 lanlink-server.service（系统单元）"
        sed -e "s|@LANLINK_PREFIX@|$PREFIX|g" \
            -e "s|@LANLINK_USER@|$LANLINK_USER|g" \
            -e "s|@LANLINK_GROUP@|$LANLINK_USER|g" \
            -e "s|@LANLINK_BIND@|$BIND|g" \
            "$UNIT_SRC/lanlink-server.service" > "$SYSTEM_UNIT_DIR/lanlink-server.service"
        chown "$LANLINK_USER" "$PREFIX/state"
        systemctl daemon-reload
        systemctl enable lanlink-server.service >/dev/null
        [ "$DO_START" = 1 ] && systemctl restart lanlink-server.service
    fi
    if [ "$DO_START" = 1 ]; then
        sleep 1
        if [ "$USERLAND" = 1 ]; then
            user_ctl is-active --quiet lanlink-server.service \
                || { user_ctl status --no-pager lanlink-server.service || true; die "中继没起来"; }
        else
            systemctl is-active --quiet lanlink-server.service \
                || { journalctl -u lanlink-server -n 20 --no-pager; die "中继没起来"; }
        fi
        say "中继已启动，监听 $BIND"
    fi
fi

# ---------- 被控端（永远是用户单元） ----------
if [ "$DO_HOST" = 1 ]; then
    CFG_DIR="$USER_HOME/.config/lanlink"
    CFG="$CFG_DIR/host.json"
    install -d -m 0700 "$CFG_DIR"
    [ "$(id -u)" = 0 ] && chown "$LANLINK_USER" "$CFG_DIR" || true

    if [ ! -f "$CFG" ]; then
        say "生成配置文件 $CFG（默认启用系统账户认证：仅允许 $LANLINK_USER；临时码备路照常可用）"
        as_user "$PREFIX/bin/lanlink-host" --server "$SERVER" --show >/dev/null
        as_user "$PREFIX/bin/lanlink-host" --pam --pam-allow "$LANLINK_USER" --auth os >/dev/null
    fi

    say "安装 lanlink-host.service（用户单元）"
    if [ "$(id -u)" = 0 ] && [ -d "$USER_HOME/.config/systemd" ]; then
        chown -R "$LANLINK_USER:$LANLINK_USER" "$USER_HOME/.config/systemd" || true
    fi
    as_user install -d -m 0755 "$USER_UNIT_DIR" "$USER_UNIT_DIR/default.target.wants"
    sed -e "s|@LANLINK_PREFIX@|$PREFIX|g" -e "s|@LANLINK_SERVER@|$SERVER|g" \
        "$UNIT_SRC/lanlink-host.service" > "$USER_UNIT_DIR/lanlink-host.service"
    if [ "$(id -u)" = 0 ]; then
        chown -R "$LANLINK_USER:$LANLINK_USER" "$USER_HOME/.config/systemd" || true
    fi
    if [ -L "$USER_UNIT_DIR/default.target.wants/lanlink-host.service" ] && [ ! -e "$USER_UNIT_DIR/default.target.wants/lanlink-host.service" ]; then
        rm -f "$USER_UNIT_DIR/default.target.wants/lanlink-host.service"
    fi

    loginctl enable-linger "$LANLINK_USER" \
        || echo "警告: enable-linger 失败，重启后要等这个用户登录才会自启" >&2

    if [ "$DO_START" = 1 ]; then
        user_ctl daemon-reload
        user_ctl enable lanlink-host.service >/dev/null
        user_ctl restart lanlink-host.service
        say "等待被控端上线（最多 20 秒）……"
        ok=0
        for _ in $(seq 1 20); do
            sleep 1
            if user_ctl is-active --quiet lanlink-host.service; then ok=1; break; fi
        done
        if [ "$ok" = 1 ]; then
            say "被控端已启动"
        else
            user_ctl status --no-pager lanlink-host.service || true
            die "被控端没起来（X 没就绪时会一直重试，见 journalctl --user -u lanlink-host）"
        fi
    fi
fi

# ---------- 图形面板与桌面图标（包里带 lanlink-gui 就自动装上） ----------
if [ "$HAVE_GUI" = 1 ]; then
    GUI_SCRIPT=""
    for c in "$HERE/install-gui.sh" "$HERE/../scripts/install-kylin-gui.sh"; do
        [ -f "$c" ] && { GUI_SCRIPT="$c"; break; }
    done
    if [ -n "$GUI_SCRIPT" ]; then
        say "安装图形面板与桌面图标「lanlink 远程协助」"
        as_user bash "$GUI_SCRIPT" --bin "$PREFIX/bin/lanlink-gui" \
            || echo "警告: 桌面图标安装未完全成功，可切到 $LANLINK_USER 手动跑 bash install-gui.sh" >&2
    fi
fi

# ---------- 小结 ----------
DEVICE_ID="（稍后分配：连上中继后写入配置）"
if [ -f "$USER_HOME/.config/lanlink/host.json" ]; then
    DEVICE_ID="$(grep -o '"device_id"[^,}]*' "$USER_HOME/.config/lanlink/host.json" \
        | head -1 | sed 's/.*: *"//; s/"$//')"
    [ -n "$DEVICE_ID" ] || DEVICE_ID="（稍后分配：连上中继后写入配置）"
fi

echo
echo "──────────────────────────────────────────────"
echo " 安装完成（$([ "$USERLAND" = 1 ] && echo 用户态 || echo 系统装)）"
if [ "$DO_SERVER" = 1 ]; then
    echo " 中继     : systemctl$([ "$USERLAND" = 1 ] && echo ' --user') status lanlink-server"
fi
if [ "$DO_HOST" = 1 ]; then
    echo " 被控端   : systemctl --user status lanlink-host"
    echo " 设备 ID  : $DEVICE_ID"
    echo " 访问密码 : lanlink-host --show   （刻意不写进安装日志）"
    echo " 日志     : journalctl --user -u lanlink-host -f"
    echo " 自检     : $PREFIX/bin/lanlink-host --diagnose   （显示/XTest/ffmpeg/中继，退出码 0 = 能跑）"
fi
if [ "$HAVE_CLIENT" = 1 ]; then
    echo " 控制端   : $PREFIX/bin/lanlink-client --server $SERVER --device $DEVICE_ID --password-file <文件>"
fi
echo "──────────────────────────────────────────────"
