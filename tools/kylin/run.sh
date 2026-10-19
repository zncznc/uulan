#!/bin/bash
# uulan 在麒麟真机上的运行脚本。
#
# 部署位置：~/.local/uulan/run.sh（见 docs/DEPLOY-KYLIN.md）
# 用法：
#   bash ~/.local/uulan/run.sh server          # 启动中继
#   bash ~/.local/uulan/run.sh host [额外参数]  # 启动被控端（真实 X11 抓屏）
#   bash ~/.local/uulan/run.sh stop            # 停掉两个
#   bash ~/.local/uulan/run.sh status          # 看状态和日志尾巴
#
# 注意：清理进程必须用 `pkill -x`（按进程名精确匹配）。
# 用 `pkill -f uulan-server` 会匹配到「命令行里含这个字符串」的调用者自己，
# 把自己一起杀掉 —— 表现为命令毫无输出地消失，极难排查。

set -u

REL="$HOME/.local/uulan/src/target/release"
export DISPLAY=:0
export XAUTHORITY="$HOME/.Xauthority"

case "${1:-status}" in
  server)
    pkill -x uulan-server 2>/dev/null; sleep 0.6
    setsid nohup "$REL/uulan-server" --bind 0.0.0.0:21116 \
      --state "$HOME/.local/uulan/devices.json" > "$HOME/server.log" 2>&1 < /dev/null &
    sleep 2
    cat "$HOME/server.log"
    ;;
  host)
    pkill -x uulan-host 2>/dev/null; sleep 0.6
    shift || true
    setsid nohup "$REL/uulan-host" --server 192.168.223.15:21116 "$@" \
      > "$HOME/host.log" 2>&1 < /dev/null &
    sleep 4
    cat "$HOME/host.log"
    ;;
  stop)
    pkill -x uulan-host 2>/dev/null
    pkill -x uulan-server 2>/dev/null
    sleep 0.5
    echo "已停止"
    ;;
  *)
    echo "进程："
    ps -eo pid,etime,comm --no-headers | grep -E '^ *[0-9]+ +[0-9:]+ +uulan' || echo "  (无)"
    echo "监听："
    ss -lnt 2>/dev/null | grep 21116 || echo "  (21116 未监听)"
    echo "--- server.log ---"; tail -5 "$HOME/server.log" 2>/dev/null || echo "(无)"
    echo "--- host.log ---";   tail -12 "$HOME/host.log" 2>/dev/null || echo "(无)"
    ;;
esac
