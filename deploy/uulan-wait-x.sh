#!/bin/sh
# 等 X 显示就绪。
#
# 为什么需要它：被控端的 systemd 用户单元在登录时就启动，而 Xorg 可能还没建好
# socket / 授权文件。那时 xdpyinfo 失败，ffmpeg 的 x11grab 必然抓不到画面，
# 表现是「服务起来了但一连就是黑屏」。
#
# 用法：uulan-wait-x [超时秒数，默认 120]
# 就绪 → 退出 0（systemd 继续 ExecStart）
# 超时 → 退出 1（systemd 按 Restart=always 重新走一遍这个脚本，
#        于是「先开机、后登录」也能在登录后自动追上）
set -u

timeout="${1:-120}"
disp="${DISPLAY:-:0}"
i=0

# 不走 xdpyinfo 的替代路径：xset q 同样会打开显示，麒麟自带。
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