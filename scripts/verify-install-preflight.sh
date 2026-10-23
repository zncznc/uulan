#!/bin/bash
# 安装脚本的「预检」验收：所有输入必须在动磁盘之前验完。
#
# 为什么要有这个脚本：`deploy/install.sh` 过去把「找 uulan-wait-x」放在
# `install -d $PREFIX` 和两个二进制落盘**之后**。真机上第一次从离线包装就断在那里，
# 留下一个半装前缀（bin/ 里只有 host+server、没有 wait-x，服务起不来），
# 而安装输出看起来只是「中途报了个错」。
#
# 这份测试把它变成可重复的断言：缺任何一个输入 → 退出码非 0 **且 prefix 目录压根没被创建**。
# 全部在临时目录里跑（假的 getent/systemctl/loginctl + 假二进制），
# 不碰本机真实的 systemd 用户单元、不碰真实安装。
#
# 用法：bash scripts/verify-install-preflight.sh
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
SB="$(mktemp -d)"
trap 'rm -rf "$SB"' EXIT
mkdir -p "$SB/home" "$SB/fakebin"

PASS=0
FAIL=0
ok()   { PASS=$((PASS+1)); echo "  ✓ $*"; }
bad()  { FAIL=$((FAIL+1)); echo "  ✗ $*"; }

# ---- 隔离设施：getent / systemctl / loginctl 全走假的，家目录指向沙箱 ----
cat > "$SB/fakebin/getent" <<EOF
#!/bin/sh
# 只回答 "getent passwd <user>"，且把家目录指到沙箱，让单元文件写不出沙箱。
if [ "\${1:-}" = passwd ]; then
    echo "\${2:-x}:x:\$(id -u):\$(id -g)::$SB/home:/bin/sh"
    exit 0
fi
exit 1
EOF
cat > "$SB/fakebin/systemctl" <<'EOF'
#!/bin/sh
# daemon-reload / enable / restart / is-active 一律成功；不真的动 systemd。
case "$*" in
    *is-active*) exit 0 ;;
    *) exit 0 ;;
esac
EOF
cat > "$SB/fakebin/loginctl" <<'EOF'
#!/bin/sh
exit 0
EOF
chmod +x "$SB/fakebin/getent" "$SB/fakebin/systemctl" "$SB/fakebin/loginctl"

# 假二进制：install.sh 只要求「存在且可执行」，这里不需要真的跑起来
mkbin() { # mkbin <dir> <名字...>
    local d="$1"; shift
    mkdir -p "$d"
    for n in "$@"; do
        printf '#!/bin/sh\nexit 0\n' > "$d/$n"
        chmod +x "$d/$n"
    done
}

# 组装一个「包」：install.sh + systemd 模板（+ 可选 bin/wait-x）
mkpkg() { # mkpkg <dir> <带wait-x:yes|no>
    local d="$1" wx="$2"
    mkdir -p "$d"
    cp "$ROOT/deploy/install.sh" "$d/install.sh"
    mkdir -p "$d/systemd" && cp "$ROOT/deploy/systemd/"*.service "$d/systemd/"
    if [ "$wx" = yes ]; then
        mkdir -p "$d/bin"
        cp "$ROOT/deploy/uulan-wait-x.sh" "$d/bin/uulan-wait-x"
        chmod +x "$d/bin/uulan-wait-x"
    fi
}

run_install() { # run_install <包目录> <prefix> <bin目录> -> 打印退出码
    PATH="$SB/fakebin:$PATH" bash "$1/install.sh" --userland \
        --prefix "$2" --bin "$3" --server 127.0.0.1:21199 --no-start \
        >"$SB/last.log" 2>&1
    echo $?
}

echo "== 预检必须挡在动磁盘之前 =="

# 1) 缺 uulan-host
mkpkg "$SB/pkg1" no
mkbin "$SB/b1" uulan-server
rc="$(run_install "$SB/pkg1" "$SB/p1" "$SB/b1")"
[ "$rc" != 0 ] && ok "缺 uulan-host → 退出码 $rc" || bad "缺 uulan-host 竟然成功了"
[ ! -d "$SB/p1" ] && ok "缺 uulan-host → prefix 未被创建" || bad "prefix 被创建了（半装前缀）"

# 2) 缺 uulan-server（要求装中继时）
mkbin "$SB/b2" uulan-host
rc="$(run_install "$SB/pkg1" "$SB/p2" "$SB/b2")"
[ "$rc" != 0 ] && ok "缺 uulan-server → 退出码 $rc" || bad "缺 uulan-server 竟然成功了"
[ ! -d "$SB/p2" ] && ok "缺 uulan-server → prefix 未被创建" || bad "prefix 被创建了（半装前缀）"

# 3) 缺 uulan-wait-x —— 这就是真机上踩到的那个半装前缀
mkbin "$SB/b3" uulan-host uulan-server
rc="$(run_install "$SB/pkg1" "$SB/p3" "$SB/b3")"
grep -q "找不到 uulan-wait-x" "$SB/last.log" \
    && ok "缺 uulan-wait-x → 报错明确（找不到 uulan-wait-x）" \
    || bad "缺 uulan-wait-x 的错误信息不对：$(tail -2 "$SB/last.log" | tr '\n' ' ')"
[ "$rc" != 0 ] && ok "缺 uulan-wait-x → 退出码 $rc" || bad "缺 uulan-wait-x 竟然成功了"
[ ! -d "$SB/p3" ] && ok "缺 uulan-wait-x → prefix 未被创建（修掉的正是这一条）" \
    || bad "prefix 被创建了 —— 又回到「先落盘、后失败」"

# 4) 缺 systemd 单元模板
mkbin "$SB/b4" uulan-host uulan-server
mkdir -p "$SB/pkg4" && cp "$ROOT/deploy/install.sh" "$SB/pkg4/install.sh"
rc="$(run_install "$SB/pkg4" "$SB/p4" "$SB/b4")"
[ "$rc" != 0 ] && ok "缺单元模板 → 退出码 $rc" || bad "缺单元模板竟然成功了"
[ ! -d "$SB/p4" ] && ok "缺单元模板 → prefix 未被创建" || bad "prefix 被创建了（半装前缀）"

echo
echo "== 正常路径仍要装得下去 =="
mkpkg "$SB/pkg5" yes
mkbin "$SB/b5" uulan-host uulan-server
cp "$ROOT/deploy/uulan-wait-x.sh" "$SB/b5/uulan-wait-x"
chmod +x "$SB/b5/uulan-wait-x"
rc="$(run_install "$SB/pkg5" "$SB/p5" "$SB/b5")"
if [ "$rc" = 0 ]; then
    ok "齐全时安装成功（退出码 0）"
elif echo "$(uname -s)" | grep -qiE 'msys|mingw|cygwin'; then
    # MSYS 上 `install -d -m 0700` 改目录权限不可靠（会报 cannot change permissions），
    # 这是测试宿主的问题、不是安装脚本的问题 —— 正常路径要在 Linux 上跑一遍才算数。
    ok "SKIP（MSYS 目录 chmod 不可靠）：正常路径需在 Linux 上验，本机只验了「预检挡住」那四条"
else
    bad "齐全时安装失败：$(tail -3 "$SB/last.log" | tr '\n' ' ')"
fi
for f in uulan-host uulan-server uulan-wait-x; do
    [ -x "$SB/p5/bin/$f" ] && ok "已落盘 $f" || bad "缺 $f"
done
[ -f "$SB/home/.config/systemd/user/uulan-server.service" ] \
    && ok "中继单元写到了「目标用户」的家目录（沙箱内）" \
    || bad "中继单元没写出来"
grep -q "安装到 $SB/p5" "$SB/last.log" && ok "安装摘要里写出了 prefix" || bad "安装摘要缺 prefix"

echo
echo "== 结果：$PASS 通过 / $FAIL 失败 =="
[ "$FAIL" -eq 0 ]