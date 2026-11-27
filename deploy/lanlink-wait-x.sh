#!/bin/sh
# 等 X 显示就绪。
#
# 用法：lanlink-wait-x [超时秒数，默认 120]
# 就绪 → 退出 0（systemd 继续 ExecStart）
# 超时 → 退出 1（systemd 按 Restart=always 重新走一遍这个脚本）
set -u

timeout="${1:-120}"
disp="${DISPLAY:-:0}"
i=0

probe() {
    if command -v xdpyinfo >/dev/null 2>&1; then
        xdpyinfo >/dev/null 2>&1
    else
        xset q >/dev/null 2>&1
    fi
}

while [ "$i" -lt "$timeout" ]; do
    if probe; then
        echo "X $disp 已就绪（等了 ${i}s）"
        exit 0
    fi
    sleep 1
    i=$((i + 1))
done

echo "等 X $disp 超时（${timeout}s）；交回 systemd 稍后重试" >&2
exit 1
