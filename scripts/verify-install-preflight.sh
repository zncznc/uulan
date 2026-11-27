#!/bin/bash
# 安装脚本的「预检」验收：所有输入与冲突必须在动磁盘之前验完。
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

cat > "$SB/fakebin/getent" <<EOF
#!/bin/sh
if [ "\${1:-}" = passwd ]; then
    echo "\${2:-x}:x:\$(id -u):\$(id -g)::$SB/home:/bin/sh"
    exit 0
fi
exit 1
EOF
cat > "$SB/fakebin/systemctl" <<'EOF'
#!/bin/sh
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
if [ "${FAKE_PORT_BUSY:-0}" = 1 ]; then
    echo "State Recv-Q Send-Q Local Address:Port Peer Address:Port Process"
    echo "LISTEN 0 128 127.0.0.1:21199 0.0.0.0:*"
fi
exit 0
EOF
chmod +x "$SB/fakebin/getent" "$SB/fakebin/systemctl" "$SB/fakebin/loginctl" "$SB/fakebin/ss"

mkbin() {
    local d="$1"; shift
    mkdir -p "$d"
    for n in "$@"; do
        printf '#!/bin/sh\nexit 0\n' > "$d/$n"
        chmod +x "$d/$n"
    done
}

mkpkg() {
    local d="$1" wx="$2"
    mkdir -p "$d"
    cp "$ROOT/deploy/install.sh" "$d/install.sh"
    mkdir -p "$d/systemd" && cp "$ROOT/deploy/systemd/"*.service "$d/systemd/"
    if [ "$wx" = yes ]; then
        mkdir -p "$d/bin"
        cp "$ROOT/deploy/lanlink-wait-x.sh" "$d/bin/lanlink-wait-x"
        chmod +x "$d/bin/lanlink-wait-x"
    fi
}

run() {
    local mode="$1" d="$2" p="$3" b="$4"; shift 4
    local args=(--prefix "$p" --bin "$b" --server 127.0.0.1:21199 --bind 127.0.0.1:21199 --no-start)
    if [ "$mode" = sys ]; then
        env LANLINK_FAKE_ROOT=1 "LANLINK_SYSTEM_UNIT_DIR=$SB/sysunit" \
            PATH="$SB/fakebin:$PATH" "$@" \
            bash "$d/install.sh" --user "$(id -un)" "${args[@]}" >"$SB/last.log" 2>&1
    else
        env PATH="$SB/fakebin:$PATH" "$@" \
            bash "$d/install.sh" --userland "${args[@]}" >"$SB/last.log" 2>&1
    fi
    echo $?
}

echo "== 预检必须挡在动磁盘之前（输入缺失类）=="

# 1) 缺 lanlink-host
mkpkg "$SB/pkg1" no
mkbin "$SB/b1" lanlink-server
rc="$(run user "$SB/pkg1" "$SB/p1" "$SB/b1")"
[ "$rc" != 0 ] && ok "缺 lanlink-host → 退出码 $rc" || bad "缺 lanlink-host 竟然成功了"
[ ! -d "$SB/p1" ] && ok "缺 lanlink-host → prefix 未被创建" || bad "prefix 被创建了（半装前缀）"

# 2) 缺 lanlink-server（要求装中继时）
mkbin "$SB/b2" lanlink-host
rc="$(run user "$SB/pkg1" "$SB/p2" "$SB/b2")"
[ "$rc" != 0 ] && ok "缺 lanlink-server → 退出码 $rc" || bad "缺 lanlink-server 竟然成功了"
[ ! -d "$SB/p2" ] && ok "缺 lanlink-server → prefix 未被创建" || bad "prefix 被创建了（半装前缀）"

# 3) 缺 lanlink-wait-x
mkbin "$SB/b3" lanlink-host lanlink-server
rc="$(run user "$SB/pkg1" "$SB/p3" "$SB/b3")"
grep -q "找不到 lanlink-wait-x" "$SB/last.log" \
    && ok "缺 lanlink-wait-x → 报错明确（找不到 lanlink-wait-x）" \
    || bad "缺 lanlink-wait-x 的错误信息不对：$(tail -2 "$SB/last.log" | tr '\n' ' ')"
[ "$rc" != 0 ] && ok "缺 lanlink-wait-x → 退出码 $rc" || bad "缺 lanlink-wait-x 竟然成功了"
[ ! -d "$SB/p3" ] && ok "缺 lanlink-wait-x → prefix 未被创建" \
    || bad "prefix 被创建了"

# 4) 缺 systemd 单元模板
mkbin "$SB/b4" lanlink-host lanlink-server
mkdir -p "$SB/pkg4" && cp "$ROOT/deploy/install.sh" "$SB/pkg4/install.sh"
rc="$(run user "$SB/pkg4" "$SB/p4" "$SB/b4")"
[ "$rc" != 0 ] && ok "缺单元模板 → 退出码 $rc" || bad "缺单元模板竟然成功了"
[ ! -d "$SB/p4" ] && ok "缺单元模板 → prefix 未被创建" || bad "prefix 被创建了（半装前缀）"

echo
echo "== 预检必须挡在动磁盘之前（对端中继 / 端口占用类）=="

mkpkg "$SB/pkgA" yes
mkbin "$SB/bA" lanlink-host lanlink-server

# 5) 用户态装，系统单元正在运行
rc="$(run user "$SB/pkgA" "$SB/p5" "$SB/bA" FAKE_SYS_SRV_ACTIVE=0)"
grep -q "对端中继没让位" "$SB/last.log" \
    && ok "用户态装遇系统单元在跑 → 报错明确（对端中继没让位）" \
    || bad "该拦没拦：$(tail -3 "$SB/last.log" | tr '\n' ' ')"
[ "$rc" != 0 ] && ok "对端系统单元在跑 → 退出码 $rc" || bad "对端在跑竟然装下去了"
[ ! -d "$SB/p5" ] && ok "对端系统单元在跑 → prefix 未被创建" || bad "prefix 被创建了（半装前缀）"

# 6) 用户态装，系统单元只是开着自启
rc="$(run user "$SB/pkgA" "$SB/p6" "$SB/bA" FAKE_SYS_SRV_ENABLED=0)"
grep -q "开着开机自启" "$SB/last.log" \
    && ok "对端只开自启也要拦" \
    || bad "自启没拦住：$(tail -3 "$SB/last.log" | tr '\n' ' ')"
[ "$rc" != 0 ] && ok "对端系统单元开自启 → 退出码 $rc" || bad "开自启竟然装下去了"
[ ! -d "$SB/p6" ] && ok "对端系统单元开自启 → prefix 未被创建" || bad "prefix 被创建了"

# 7) 端口被陌生进程占
rc="$(run user "$SB/pkgA" "$SB/p7" "$SB/bA" FAKE_PORT_BUSY=1)"
grep -q "已被占用" "$SB/last.log" \
    && ok "端口被占（非本形态单元）→ 报错明确（已被占用）" \
    || bad "端口占用没拦住：$(tail -3 "$SB/last.log" | tr '\n' ' ')"
[ "$rc" != 0 ] && ok "端口被占 → 退出码 $rc" || bad "端口被占竟然装下去了"
[ ! -d "$SB/p7" ] && ok "端口被占 → prefix 未被创建" || bad "prefix 被创建了"

# 8) 本形态自己的中继在跑 + 端口被自己占 → 就地升级放行
rc="$(run user "$SB/pkgA" "$SB/p8" "$SB/bA" FAKE_USER_SRV_ACTIVE=0 FAKE_PORT_BUSY=1)"
if grep -qE "对端中继没让位|已被占用" "$SB/last.log"; then
    bad "就地升级被预检误伤：$(grep -m1 -E '对端中继没让位|已被占用' "$SB/last.log")"
elif [ "$rc" = 0 ]; then
    ok "自己的中继在跑 + 端口被自己占 → 放行（就地升级不被预检挡死）"
elif msys; then
    ok "SKIP（MSYS 目录 chmod 不可靠）：放行判据已验，装完的动作留到 Linux 上验"
else
    bad "放行之后安装应成功，退出码 $rc：$(tail -3 "$SB/last.log" | tr '\n' ' ')"
fi

# 9) 系统装，被目标用户的用户单元自启符号链接挡
mkdir -p "$SB/home/.config/systemd/user/default.target.wants"
: > "$SB/home/.config/systemd/user/default.target.wants/lanlink-server.service"
rc="$(run sys "$SB/pkgA" "$SB/p9" "$SB/bA")"
grep -q "用户单元 lanlink-server.service 开着开机自启" "$SB/last.log" \
    && ok "系统装遇用户单元自启（查文件不走总线）→ 拦下" \
    || bad "系统装没拦住自启的用户单元：$(tail -3 "$SB/last.log" | tr '\n' ' ')"
[ "$rc" != 0 ] && ok "对端用户单元开自启 → 退出码 $rc" || bad "对端没让位竟然装下去了"
[ ! -d "$SB/p9" ] && ok "对端用户单元开自启 → prefix 未被创建" || bad "prefix 被创建了"
rm -f "$SB/home/.config/systemd/user/default.target.wants/lanlink-server.service"

# 10) 系统装，对端用户单元正在跑
rc="$(run sys "$SB/pkgA" "$SB/p10" "$SB/bA" FAKE_USER_SRV_ACTIVE=0)"
grep -q "用户单元 lanlink-server.service 正在运行" "$SB/last.log" \
    && ok "系统装遇用户单元在跑 → 拦下" || bad "没拦住：$(tail -3 "$SB/last.log" | tr '\n' ' ')"
[ "$rc" != 0 ] && ok "对端用户单元在跑 → 退出码 $rc" || bad "对端在跑竟然装下去了"
[ ! -d "$SB/p10" ] && ok "对端用户单元在跑 → prefix 未被创建" || bad "prefix 被创建了"

echo
echo "== 正常路径仍要装得下去（用户态与系统装两条都要）=="
mkpkg "$SB/pkg5" yes
mkbin "$SB/b5" lanlink-host lanlink-server
cp "$ROOT/deploy/lanlink-wait-x.sh" "$SB/b5/lanlink-wait-x"
chmod +x "$SB/b5/lanlink-wait-x"
rc="$(run user "$SB/pkg5" "$SB/p5ok" "$SB/b5")"
if [ "$rc" = 0 ]; then
    ok "齐全时用户态安装成功（退出码 0）"
elif msys; then
    ok "SKIP（MSYS 目录 chmod 不可靠）：正常路径需在 Linux 上验"
else
    bad "齐全时用户态安装失败：$(tail -3 "$SB/last.log" | tr '\n' ' ')"
fi
for f in lanlink-host lanlink-server lanlink-wait-x; do
    [ -x "$SB/p5ok/bin/$f" ] && ok "已落盘 $f" || bad "缺 $f"
done
[ -f "$SB/home/.config/systemd/user/lanlink-server.service" ] \
    && ok "中继单元写到了「目标用户」的家目录（沙箱内）" \
    || bad "中继单元没写出来"
grep -q "安装到 $SB/p5ok" "$SB/last.log" && ok "安装摘要里写出了 prefix" || bad "安装摘要缺 prefix"

# 11) 系统装正常路径
rc="$(run sys "$SB/pkgA" "$SB/p11" "$SB/bA")"
if [ "$rc" = 0 ]; then
    ok "齐全时系统装成功（退出码 0）"
    [ -f "$SB/sysunit/lanlink-server.service" ] \
        && ok "中继（系统单元）落进系统单元目录（沙箱内）" \
        || bad "系统单元目录里没有中继单元"
    grep -q "^ExecStart=" "$SB/sysunit/lanlink-server.service" \
        && ok "系统单元模板展开后可用" || bad "系统单元内容不对"
elif msys; then
    ok "SKIP（MSYS 目录 chmod 不可靠）：系统装正常路径留到 Linux 上验"
else
    bad "齐全时系统装失败：$(tail -3 "$SB/last.log" | tr '\n' ' ')"
fi

echo
echo "== 结果：$PASS 通过 / $FAIL 失败 =="
[ "$FAIL" -eq 0 ]
