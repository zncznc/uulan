#!/bin/bash
# uulan 安装脚本（银河麒麟 V10 SP1 / 任何 systemd + X11 的 Linux）
#
# 两种装法，装的东西一样，只是位置与单元归属不同：
#
#   1) 系统装（root，生产默认）：二进制进 /opt/uulan，中继是**系统单元**，
#      被控端是**用户单元**。
#        sudo bash install.sh --user mis --server 192.168.223.15:21116
#
#   2) 用户态装（普通用户，无 root）：全部进 ~/.local/uulan，中继与被控端都是
#      **用户单元**，靠 `loginctl enable-linger` 开机自启。
#        bash install.sh --userland --server 192.168.223.15:21116
#      现场拿不到 sudo 口令时用这个；功能一样（中继也随这个用户跑）。
#
# 为什么被控端必须是**用户**单元：它要 x11grab 抓 :0、要 XTest 注入、要读
# ~/.Xauthority。系统单元跑在 root 下没有会话，X11 授权会拦住 → 抓出来是黑屏。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
USERLAND=0
PREFIX=""
UULAN_USER="${SUDO_USER:-$(id -un)}"
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
        --user)     UULAN_USER="$2"; shift 2 ;;
        --server)   SERVER="$2"; shift 2 ;;
        --bind)     BIND="$2"; shift 2 ;;
        --prefix)   PREFIX="$2"; shift 2 ;;
        --userland) USERLAND=1; shift ;;
        --no-server) DO_SERVER=0; shift ;;
        --no-host)   DO_HOST=0; shift ;;
        --no-start)  DO_START=0; shift ;;
        -h|--help)  sed -n '2,22p' "$0"; exit 0 ;;
        *) die "未知参数 $1（--help 看用法）" ;;
    esac
done

command -v systemctl >/dev/null || die "没有 systemd（这不是 systemd 系统？）"

if [ "$USERLAND" = 1 ]; then
    [ "$(id -u)" != 0 ] || die "用户态装请用普通用户跑（不要 sudo）"
    UULAN_USER="$(id -un)"
    PREFIX="${PREFIX:-$HOME/.local/uulan}"
else
    [ "$(id -u)" = 0 ] || die "系统装需要 root：sudo bash install.sh（没有 root 就用 --userland）"
    PREFIX="${PREFIX:-/opt/uulan}"
fi
id "$UULAN_USER" >/dev/null 2>&1 || die "用户不存在: $UULAN_USER"

USER_HOME="$(getent passwd "$UULAN_USER" | cut -d: -f6)"
USER_UID="$(id -u "$UULAN_USER")"
USER_UNIT_DIR="$USER_HOME/.config/systemd/user"
SYSTEM_UNIT_DIR=/etc/systemd/system
[ -d "$USER_HOME" ] || die "读不到 $UULAN_USER 的家目录"

# 二进制从哪来：--bin > 包内 ./bin > 源码树 target/release
if [ -z "$BIN_DIR" ]; then
    if [ -d "$HERE/bin" ]; then BIN_DIR="$HERE/bin"
    elif [ -d "$HERE/../target/release" ]; then BIN_DIR="$HERE/../target/release"
    else die "找不到二进制，用 --bin 指定"
    fi
fi
[ -x "$BIN_DIR/uulan-host" ] || die "$BIN_DIR 里没有 uulan-host"
if [ "$DO_SERVER" = 1 ]; then
    [ -x "$BIN_DIR/uulan-server" ] || die "$BIN_DIR 里没有 uulan-server"
fi

UNIT_SRC="$HERE/systemd"
[ -d "$UNIT_SRC" ] || UNIT_SRC="$HERE/../deploy/systemd"
[ -d "$UNIT_SRC" ] || die "找不到 systemd 单元模板（$HERE/systemd）"

# 让 systemctl --user 在 ssh / 脚本里也能用
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$USER_UID}"
if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ] && [ -S "$XDG_RUNTIME_DIR/bus" ]; then
    export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"
fi

# 以目标用户身份执行（系统装时 root 要降权；用户态装就是自己）
as_user() {
    if [ "$(id -u)" = 0 ] && [ "$UULAN_USER" != root ]; then
        runuser -u "$UULAN_USER" -- env \
            XDG_RUNTIME_DIR="/run/user/$USER_UID" \
            DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$USER_UID/bus" \
            HOME="$USER_HOME" "$@"
    else
        "$@"
    fi
}
user_ctl() { as_user systemctl --user "$@"; }

say "安装到 $PREFIX（$([ "$USERLAND" = 1 ] && echo 用户态 || echo 系统装)，运行身份 $UULAN_USER，中继 $SERVER）"
install -d -m 0755 "$PREFIX" "$PREFIX/bin" "$PREFIX/doc" "$PREFIX/state"
install -m 0755 "$BIN_DIR/uulan-host" "$PREFIX/bin/uulan-host"
if [ "$DO_SERVER" = 1 ]; then
    install -m 0755 "$BIN_DIR/uulan-server" "$PREFIX/bin/uulan-server"
fi
# 等 X 的小脚本：源码树里叫 deploy/uulan-wait-x.sh，离线包里被放到 bin/uulan-wait-x。
# 两种布局都要认，否则「打包能打、装包就断」——真机上第一次装就踩到了。
WAIT_X=""
for c in "$HERE/uulan-wait-x.sh" "$HERE/bin/uulan-wait-x" "$HERE/bin/uulan-wait-x.sh"; do
    if [ -f "$c" ]; then WAIT_X="$c"; break; fi
done
[ -n "$WAIT_X" ] || die "找不到 uulan-wait-x（源码树应在 deploy/，安装包应在 bin/）"
install -m 0755 "$WAIT_X" "$PREFIX/bin/uulan-wait-x"
if [ -d "$HERE/doc" ]; then
    cp -f "$HERE/doc/"*.md "$PREFIX/doc/" 2>/dev/null || true
fi

# ---------- 中继 ----------
if [ "$DO_SERVER" = 1 ]; then
    if [ "$USERLAND" = 1 ]; then
        say "安装 uulan-server.service（用户单元）"
        install -d -m 0755 "$USER_UNIT_DIR"
        sed -e "s|@UULAN_PREFIX@|$PREFIX|g" -e "s|@UULAN_BIND@|$BIND|g" \
            "$UNIT_SRC/uulan-server.user.service" > "$USER_UNIT_DIR/uulan-server.service"
        user_ctl daemon-reload
        user_ctl enable uulan-server.service >/dev/null
        [ "$DO_START" = 1 ] && user_ctl restart uulan-server.service
    else
        say "安装 uulan-server.service（系统单元）"
        sed -e "s|@UULAN_PREFIX@|$PREFIX|g" \
            -e "s|@UULAN_USER@|$UULAN_USER|g" \
            -e "s|@UULAN_GROUP@|$UULAN_USER|g" \
            -e "s|@UULAN_BIND@|$BIND|g" \
            "$UNIT_SRC/uulan-server.service" > "$SYSTEM_UNIT_DIR/uulan-server.service"
        chown "$UULAN_USER" "$PREFIX/state"
        systemctl daemon-reload
        systemctl enable uulan-server.service >/dev/null
        [ "$DO_START" = 1 ] && systemctl restart uulan-server.service
    fi
    if [ "$DO_START" = 1 ]; then
        sleep 1
        if [ "$USERLAND" = 1 ]; then
            user_ctl is-active --quiet uulan-server.service \
                || { user_ctl status --no-pager uulan-server.service || true; die "中继没起来"; }
        else
            systemctl is-active --quiet uulan-server.service \
                || { journalctl -u uulan-server -n 20 --no-pager; die "中继没起来"; }
        fi
        say "中继已启动，监听 $BIND"
    fi
fi

# ---------- 被控端（永远是用户单元） ----------
if [ "$DO_HOST" = 1 ]; then
    CFG_DIR="$USER_HOME/.config/uulan"
    CFG="$CFG_DIR/host.json"
    install -d -m 0700 "$CFG_DIR"
    [ "$(id -u)" = 0 ] && chown "$UULAN_USER" "$CFG_DIR" || true

    # 配置文件不存在就先让程序自己生成（随机 12 位密码 + 0600）。
    # 这里**故意不打印密码**：安装输出经常被贴进工单/日志。看密码用 `uulan-host --show`。
    if [ ! -f "$CFG" ]; then
        say "生成配置文件 $CFG（访问密码随机；用 uulan-host --show 查看）"
        as_user "$PREFIX/bin/uulan-host" --server "$SERVER" --show >/dev/null
    fi

    say "安装 uulan-host.service（用户单元）"
    install -d -m 0755 "$USER_UNIT_DIR"
    sed -e "s|@UULAN_PREFIX@|$PREFIX|g" -e "s|@UULAN_SERVER@|$SERVER|g" \
        "$UNIT_SRC/uulan-host.service" > "$USER_UNIT_DIR/uulan-host.service"
    [ "$(id -u)" = 0 ] && chown "$UULAN_USER" "$USER_UNIT_DIR/uulan-host.service" || true

    # 开机即启动（不等登录）。被控端自己等 X，所以「先开机后登录」也能追上。
    loginctl enable-linger "$UULAN_USER" \
        || echo "警告: enable-linger 失败，重启后要等这个用户登录才会自启" >&2

    if [ "$DO_START" = 1 ]; then
        user_ctl daemon-reload
        user_ctl enable uulan-host.service >/dev/null
        user_ctl restart uulan-host.service
        say "等待被控端上线（最多 20 秒）……"
        ok=0
        for _ in $(seq 1 20); do
            sleep 1
            if user_ctl is-active --quiet uulan-host.service; then ok=1; break; fi
        done
        if [ "$ok" = 1 ]; then
            say "被控端已启动"
        else
            user_ctl status --no-pager uulan-host.service || true
            die "被控端没起来（X 没就绪时会一直重试，见 journalctl --user -u uulan-host）"
        fi
    fi
fi

# ---------- 小结 ----------
DEVICE_ID="（稍后分配：连上中继后写入配置）"
if [ -f "$USER_HOME/.config/uulan/host.json" ]; then
    # 只读 device_id，绝不读密码字段
    DEVICE_ID="$(grep -o '"device_id"[^,}]*' "$USER_HOME/.config/uulan/host.json" \
        | head -1 | sed 's/.*: *"//; s/"$//')"
    [ -n "$DEVICE_ID" ] || DEVICE_ID="（稍后分配：连上中继后写入配置）"
fi

echo
echo "──────────────────────────────────────────────"
echo " 安装完成（$([ "$USERLAND" = 1 ] && echo 用户态 || echo 系统装)）"
if [ "$DO_SERVER" = 1 ]; then
    echo " 中继     : systemctl$([ "$USERLAND" = 1 ] && echo ' --user') status uulan-server"
fi
if [ "$DO_HOST" = 1 ]; then
    echo " 被控端   : systemctl --user status uulan-host"
    echo " 设备 ID  : $DEVICE_ID"
    echo " 访问密码 : uulan-host --show   （刻意不写进安装日志）"
    echo " 日志     : journalctl --user -u uulan-host -f"
fi
echo "──────────────────────────────────────────────"