#!/bin/bash
# 安装脚本的「预检」验收：所有输入与冲突必须在动磁盘之前验完。
#
# 为什么要有这个脚本：`deploy/install.sh` 过去把「找 uulan-wait-x」放在
# `install -d $PREFIX` 和两个二进制落盘**之后**。真机上第一次从离线包装就断在那里，
# 留下一个半装前缀（bin/ 里只有 host+server、没有 wait-x，服务起不来），
# 而安装输出看起来只是「中途报了个错」。
#
# 2026-11-12 加了第二类预检：系统装与用户态装各有一套中继单元、抢同一个端口。
# 白天那次 root 装靠人工先 `systemctl --user disable --now` 才没撞车；撞了的话
# 失败要到 restart 才暴露，那时二进制与单元都已落盘 —— 同样是「先落盘、后失败」。
# 这份脚本把两类都钉成可重复的断言：
#   缺输入 / 对端没让位 / 端口被占 → 退出码非 0 **且 prefix 目录压根没被创建**；
#   「自己的中继在跑」就地升级 → 必须放行（否则预检自己变成新 bug）。
# 全部在临时目录里跑（假的 getent/systemctl/loginctl/ss + 假二进制 + 假系统单元目录），
# 不碰本机真实的 systemd 单元、不碰真实安装。
#
# 用法：bash scripts/verify-install-preflight.sh
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
SB="$(mktemp -d)"
trap 'rm -rf "$SB"' EXIT
mkdir -p "$SB/home" "$SB/fakebin" "$SB/sysunit"

PASS=0
FAIL=0
ok()   { PASS=$((PASS+1)); echo "  ✓ $*"; }
bad()  { FAIL=$((FAIL+1)); echo "  ✗ $*"; }
msys() { echo "$(uname -s)" | grep -qiE 'msys|mingw|cygwin'; }

# ---- 隔离设施：getent / systemctl / loginctl / ss 全走假的，家目录指向沙箱 ----
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
# 默认把所有单元问成「不存在/没在跑」，用环境变量逐个点亮：
#   FAKE_SYS_SRV_ACTIVE=0    系统单元在运行          FAKE_SYS_SRV_ENABLED=0   系统单元开自启
#   FAKE_USER_SRV_ACTIVE=0   用户单元在运行          （用户单元的 enabled 用文件符号链接判）
# daemon-reload / enable / restart 一律成功；不真的动 systemd。
user=0
for a in "$@"; do [ "$a" = "--user" ] && user=1; done
case "$*" in
    *is-active*)
        if [ "$user" = 1 ]; then exit "${FAKE_USER_SRV_ACTIVE:-3}"
        else exit "${FAKE_SYS_SRV_ACTIVE:-3}"; fi ;;
    *is-enabled*)
        if [ "$user" = 1 ]; then exit "${FAKE_USER_SRV_ENABLED:-1}"
        else exit "${FAKE_SYS_SRV_ENABLED:-1}"; fi ;;
    *) exit 0 ;;
esac
EOF
cat > "$SB/fakebin/loginctl" <<'EOF'
#!/bin/sh
exit 0
EOF
cat > "$SB/fakebin/ss" <<'EOF'
#!/bin/sh
# 只有 FAKE_PORT_BUSY=1 时回答「21199 有人在听」，且只回 --bind 用的那个端口。
if [ "${FAKE_PORT_BUSY:-0}" = 1 ]; then
    echo "State Recv-Q Send-Q Local Address:Port Peer Address:Port Process"
    echo "LISTEN 0 128 127.0.0.1:21199 0.0.0.0:*"
fi
exit 0
EOF
chmod +x "$SB/fakebin/getent" "$SB/fakebin/systemctl" "$SB/fakebin/loginctl" "$SB/fakebin/ss"

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

run() { # run <mode:user|sys> <包目录> <prefix> <bin目录> [VAR=V ...] -> 打印退出码；日志在 $SB/last.log
    local mode="$1" d="$2" p="$3" b="$4"; shift 4
    local args=(--prefix "$p" --bin "$b" --server 127.0.0.1:21199 --bind 127.0.0.1:21199 --no-start)
    if [ "$mode" = sys ]; then
        # 系统装分支在沙箱里跑：UULAN_FAKE_ROOT 只放行 root 检查，系统单元目录也指进沙箱。
        # 真非 root 时这两处本来就写不进 /etc，这个开关不会让任何东西越权。
        env UULAN_FAKE_ROOT=1 "UULAN_SYSTEM_UNIT_DIR=$SB/sysunit" \
            PATH="$SB/fakebin:$PATH" "$@" \
            bash "$d/install.sh" --user "$(id -un)" "${args[@]}" >"$SB/last.log" 2>&1
    else
        env PATH="$SB/fakebin:$PATH" "$@" \
            bash "$d/install.sh" --userland "${args[@]}" >"$SB/last.log" 2>&1
    fi
    echo $?
}

echo "== 预检必须挡在动磁盘之前（输入缺失类）=="

# 1) 缺 uulan-host
mkpkg "$SB/pkg1" no
mkbin "$SB/b1" uulan-server
rc="$(run user "$SB/pkg1" "$SB/p1" "$SB/b1")"
[ "$rc" != 0 ] && ok "缺 uulan-host → 退出码 $rc" || bad "缺 uulan-host 竟然成功了"
[ ! -d "$SB/p1" ] && ok "缺 uulan-host → prefix 未被创建" || bad "prefix 被创建了（半装前缀）"

# 2) 缺 uulan-server（要求装中继时）
mkbin "$SB/b2" uulan-host
rc="$(run user "$SB/pkg1" "$SB/p2" "$SB/b2")"
[ "$rc" != 0 ] && ok "缺 uulan-server → 退出码 $rc" || bad "缺 uulan-server 竟然成功了"
[ ! -d "$SB/p2" ] && ok "缺 uulan-server → prefix 未被创建" || bad "prefix 被创建了（半装前缀）"

# 3) 缺 uulan-wait-x —— 这就是真机上踩到的那个半装前缀
mkbin "$SB/b3" uulan-host uulan-server
rc="$(run user "$SB/pkg1" "$SB/p3" "$SB/b3")"
grep -q "找不到 uulan-wait-x" "$SB/last.log" \
    && ok "缺 uulan-wait-x → 报错明确（找不到 uulan-wait-x）" \
    || bad "缺 uulan-wait-x 的错误信息不对：$(tail -2 "$SB/last.log" | tr '\n' ' ')"
[ "$rc" != 0 ] && ok "缺 uulan-wait-x → 退出码 $rc" || bad "缺 uulan-wait-x 竟然成功了"
[ ! -d "$SB/p3" ] && ok "缺 uulan-wait-x → prefix 未被创建（修掉的正是这一条）" \
    || bad "prefix 被创建了 —— 又回到「先落盘、后失败」"

# 4) 缺 systemd 单元模板
mkbin "$SB/b4" uulan-host uulan-server
mkdir -p "$SB/pkg4" && cp "$ROOT/deploy/install.sh" "$SB/pkg4/install.sh"
rc="$(run user "$SB/pkg4" "$SB/p4" "$SB/b4")"
[ "$rc" != 0 ] && ok "缺单元模板 → 退出码 $rc" || bad "缺单元模板竟然成功了"
[ ! -d "$SB/p4" ] && ok "缺单元模板 → prefix 未被创建" || bad "prefix 被创建了（半装前缀）"

echo
echo "== 预检必须挡在动磁盘之前（对端中继 / 端口占用类，2026-11-12 新增）=="

mkpkg "$SB/pkgA" yes
mkbin "$SB/bA" uulan-host uulan-server

# 5) 用户态装，系统单元正在运行 —— 白天真机的实况（装之前对端没让位）
rc="$(run user "$SB/pkgA" "$SB/p5" "$SB/bA" FAKE_SYS_SRV_ACTIVE=0)"
grep -q "对端中继没让位" "$SB/last.log" \
    && ok "用户态装遇系统单元在跑 → 报错明确（对端中继没让位）" \
    || bad "该拦没拦：$(tail -3 "$SB/last.log" | tr '\n' ' ')"
[ "$rc" != 0 ] && ok "对端系统单元在跑 → 退出码 $rc" || bad "对端在跑竟然装下去了"
[ ! -d "$SB/p5" ] && ok "对端系统单元在跑 → prefix 未被创建" || bad "prefix 被创建了（半装前缀）"

# 6) 用户态装，系统单元只是开着自启（没在跑）—— 下次开机就抢，也必须拦
rc="$(run user "$SB/pkgA" "$SB/p6" "$SB/bA" FAKE_SYS_SRV_ENABLED=0)"
grep -q "开着开机自启" "$SB/last.log" \
    && ok "对端只开自启也要拦（只 stop 没 disable 挡不住下次开机）" \
    || bad "自启没拦住：$(tail -3 "$SB/last.log" | tr '\n' ' ')"
[ "$rc" != 0 ] && ok "对端系统单元开自启 → 退出码 $rc" || bad "开自启竟然装下去了"
[ ! -d "$SB/p6" ] && ok "对端系统单元开自启 → prefix 未被创建" || bad "prefix 被创建了"

# 7) 两边单元都不在，端口被陌生进程占 —— 兜底检查也要挡
rc="$(run user "$SB/pkgA" "$SB/p7" "$SB/bA" FAKE_PORT_BUSY=1)"
grep -q "已被占用" "$SB/last.log" \
    && ok "端口被占（非本形态单元）→ 报错明确（已被占用）" \
    || bad "端口占用没拦住：$(tail -3 "$SB/last.log" | tr '\n' ' ')"
[ "$rc" != 0 ] && ok "端口被占 → 退出码 $rc" || bad "端口被占竟然装下去了"
[ ! -d "$SB/p7" ] && ok "端口被占 → prefix 未被创建" || bad "prefix 被创建了"

# 8) 反过来：本形态自己的中继在跑 + 端口被它占着 —— 这是「就地升级」，必须放行
rc="$(run user "$SB/pkgA" "$SB/p8" "$SB/bA" FAKE_USER_SRV_ACTIVE=0 FAKE_PORT_BUSY=1)"
if grep -qE "对端中继没让位|已被占用" "$SB/last.log"; then
    bad "就地升级被预检误伤（自己的单元/端口不该算对端）：$(grep -m1 -E '对端中继没让位|已被占用' "$SB/last.log")"
elif [ "$rc" = 0 ]; then
    ok "自己的中继在跑 + 端口被自己占 → 放行（就地升级不被预检挡死）"
elif msys; then
    ok "SKIP（MSYS 目录 chmod 不可靠）：放行判据已验（没报 对端/占用），装完的动作留到 Linux 上验"
else
    bad "放行之后安装应成功，退出码 $rc：$(tail -3 "$SB/last.log" | tr '\n' ' ')"
fi

# 9) 系统装，被目标用户的用户单元自启符号链接挡（白天真机的镜像场景）
mkdir -p "$SB/home/.config/systemd/user/default.target.wants"
: > "$SB/home/.config/systemd/user/default.target.wants/uulan-server.service"
rc="$(run sys "$SB/pkgA" "$SB/p9" "$SB/bA")"
grep -q "用户单元 uulan-server.service 开着开机自启" "$SB/last.log" \
    && ok "系统装遇用户单元自启（查文件不走总线）→ 拦下" \
    || bad "系统装没拦住自启的用户单元：$(tail -3 "$SB/last.log" | tr '\n' ' ')"
[ "$rc" != 0 ] && ok "对端用户单元开自启 → 退出码 $rc" || bad "对端没让位竟然装下去了"
[ ! -d "$SB/p9" ] && ok "对端用户单元开自启 → prefix 未被创建" || bad "prefix 被创建了"
rm -f "$SB/home/.config/systemd/user/default.target.wants/uulan-server.service"

# 10) 系统装，对端用户单元正在跑（bus 能连通时的另一条判据）
rc="$(run sys "$SB/pkgA" "$SB/p10" "$SB/bA" FAKE_USER_SRV_ACTIVE=0)"
grep -q "用户单元 uulan-server.service 正在运行" "$SB/last.log" \
    && ok "系统装遇用户单元在跑 → 拦下" || bad "没拦住：$(tail -3 "$SB/last.log" | tr '\n' ' ')"
[ "$rc" != 0 ] && ok "对端用户单元在跑 → 退出码 $rc" || bad "对端在跑竟然装下去了"
[ ! -d "$SB/p10" ] && ok "对端用户单元在跑 → prefix 未被创建" || bad "prefix 被创建了"

echo
echo "== 正常路径仍要装得下去（用户态与系统装两条都要）=="
mkpkg "$SB/pkg5" yes
mkbin "$SB/b5" uulan-host uulan-server
cp "$ROOT/deploy/uulan-wait-x.sh" "$SB/b5/uulan-wait-x"
chmod +x "$SB/b5/uulan-wait-x"
rc="$(run user "$SB/pkg5" "$SB/p5ok" "$SB/b5")"
if [ "$rc" = 0 ]; then
    ok "齐全时用户态安装成功（退出码 0）"
elif msys; then
    # MSYS 上 `install -d -m 0700` 改目录权限不可靠（会报 cannot change permissions），
    # 这是测试宿主的问题、不是安装脚本的问题 —— 正常路径要在 Linux 上跑一遍才算数。
    ok "SKIP（MSYS 目录 chmod 不可靠）：正常路径需在 Linux 上验，本机只验了「预检挡住」那几条"
else
    bad "齐全时用户态安装失败：$(tail -3 "$SB/last.log" | tr '\n' ' ')"
fi
for f in uulan-host uulan-server uulan-wait-x; do
    [ -x "$SB/p5ok/bin/$f" ] && ok "已落盘 $f" || bad "缺 $f"
done
[ -f "$SB/home/.config/systemd/user/uulan-server.service" ] \
    && ok "中继单元写到了「目标用户」的家目录（沙箱内）" \
    || bad "中继单元没写出来"
grep -q "安装到 $SB/p5ok" "$SB/last.log" && ok "安装摘要里写出了 prefix" || bad "安装摘要缺 prefix"

# 11) 系统装正常路径：单元进（假的）系统单元目录，中继模板展开后可用
rc="$(run sys "$SB/pkgA" "$SB/p11" "$SB/bA")"
if [ "$rc" = 0 ]; then
    ok "齐全时系统装成功（退出码 0）"
    [ -f "$SB/sysunit/uulan-server.service" ] \
        && ok "中继（系统单元）落进系统单元目录（沙箱内）" \
        || bad "系统单元目录里没有中继单元"
    grep -q "^ExecStart=" "$SB/sysunit/uulan-server.service" \
        && ok "系统单元模板展开后可用" || bad "系统单元内容不对"
elif msys; then
    ok "SKIP（MSYS 目录 chmod 不可靠）：系统装正常路径留到 Linux 上验"
else
    bad "齐全时系统装失败：$(tail -3 "$SB/last.log" | tr '\n' ' ')"
fi

echo
echo "== 结果：$PASS 通过 / $FAIL 失败 =="
[ "$FAIL" -eq 0 ]
