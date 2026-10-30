#!/bin/bash
# 真机侧：给「键鼠进应用」造一个可重复的测试台（Xvfb :99 + xev 判据窗 + 独立 relay/host）。
#
# 为什么不打物理 :0：UKUI 锁屏时 uulan 按策略拒发键鼠（XTest 会绕过锁屏抓取），
# 而那台机器上没人守着的时候屏幕一直是锁着的。:99 是一个真的 X 服务器（Xvfb），
# 采集 / 注入 / 锁屏探测全都跟着 `--capture x11::99.0` 走，所以这条链路是真实的，
# 只是桌面内容不是用户的那一个。物理 :0 那条仍由 scripts/verify-input-e2e.sh 负责。
#
# 用法（在真机上）：bash remote-xvfb-testbed.sh up|down|events|status
# 环境变量：UULAN_BIN（默认 $HOME/.local/uulan/bin）、UULAN_APP_PORT（默认 21995）
set -u

BIN="${UULAN_BIN:-$HOME/.local/uulan/bin}"
PORT="${UULAN_APP_PORT:-21995}"
DIR="${UULAN_APP_DIR:-/tmp/uulan-input-app}"
DISP="${UULAN_APP_DISPLAY:-:99}"
GEOM="${UULAN_APP_GEOM:-420x300+100+100}"
mkdir -p "$DIR"

stop_all() {
  for f in host.pid server.pid xev.pid xvfb.pid; do
    if [ -f "$DIR/$f" ]; then
      pid=$(cat "$DIR/$f" 2>/dev/null)
      [ -n "$pid" ] && kill "$pid" 2>/dev/null
      rm -f "$DIR/$f"
    fi
  done
  sleep 0.8
}

wait_display() {
  for _ in $(seq 1 30); do
    DISPLAY="$DISP" xdpyinfo > /dev/null 2>&1 && return 0
    sleep 0.3
  done
  return 1
}

case "${1:-status}" in
  up)
    stop_all
    setsid nohup Xvfb "$DISP" -screen 0 1280x800x24 -nolisten tcp > "$DIR/xvfb.log" 2>&1 < /dev/null &
    echo $! > "$DIR/xvfb.pid"
    if ! wait_display; then
      echo "FAIL Xvfb $DISP 起不来"; tail -5 "$DIR/xvfb.log"; exit 1
    fi

    # 判据窗：xev 只把收到的事件原样打印，不插字、不改应用状态，与 uulan 完全无关。
    : > "$DIR/xev.log"
    setsid nohup env DISPLAY="$DISP" xev -geometry "$GEOM" >> "$DIR/xev.log" 2>&1 < /dev/null &
    echo $! > "$DIR/xev.pid"
    sleep 1.5

    setsid nohup "$BIN/uulan-server" --bind "0.0.0.0:$PORT" --state "$DIR/devices.json" > "$DIR/server.log" 2>&1 < /dev/null &
    echo $! > "$DIR/server.pid"
    sleep 1.5

    # 独立配置：绝不动 ~/.config/uulan/host.json（那是真机正式服务的身份与口令）
    # NO_COLOR=1：tracing 给字段名上色（`events\x1b[0m\x1b[2m=\x1b[0m17`），
    # 不上色的日志才能用 grep -E 'events=[0-9]+' 抓（本仓库第 4 次踩这个坑）。
    setsid nohup env DISPLAY="$DISP" NO_COLOR=1 "$BIN/uulan-host" \
      --config "$DIR/host.json" --capture "x11::$DISP.0" \
      --server "127.0.0.1:$PORT" --name Xvfb99 > "$DIR/host.log" 2>&1 < /dev/null &
    echo $! > "$DIR/host.pid"
    sleep 3

    python3 - "$DIR/host.json" "$DISP" "$DIR/xev.log" <<'PY'
import json, subprocess, sys
cfg, disp, xevlog = sys.argv[1], sys.argv[2], sys.argv[3]
d = json.load(open(cfg))
wid = subprocess.run(["xdotool", "search", "--name", "Event Tester"],
                     capture_output=True, text=True,
                     env={"DISPLAY": disp, "PATH": "/usr/bin:/bin"}).stdout.split()
if not wid:
    print("FAIL xev 窗口没找到"); sys.exit(1)
wid = wid[-1]
geo = subprocess.run(["xdotool", "getwindowgeometry", wid], capture_output=True, text=True,
                     env={"DISPLAY": disp, "PATH": "/usr/bin:/bin"}).stdout
print("DEVICE", d["device_id"])
print("WID", wid)
for line in geo.splitlines():
    line = line.strip()
    if line.startswith("Position:") or line.startswith("Geometry:"):
        print(line)
PY
    echo "--- host 自报 ---"
    grep -E "已用原有设备 ID 上线|中继分配了新的设备 ID|采集源|等待控制端连接" "$DIR/host.log" | tail -4 | sed 's/^/  /'
    ;;
  down)
    stop_all
    echo "已停止测试台（$DIR）"
    ;;
  events)
    L="$DIR/xev.log"
    [ -f "$L" ] || { echo "FAIL 没有 xev 日志（测试台没起？）"; exit 1; }
    echo "KEYPRESS $(grep -c 'KeyPress' "$L")"
    echo "KEYRELEASE $(grep -c 'KeyRelease' "$L")"
    echo "BUTTONPRESS $(grep -c 'ButtonPress' "$L")"
    echo "BUTTONRELEASE $(grep -c 'ButtonRelease' "$L")"
    echo "MOTION $(grep -c 'MotionNotify' "$L")"
    echo "DRAGMOTION $(grep -c 'state 0x100' "$L")"   # Button1Mask：按下左键期间的移动
    # 只看 KeyPress 那几行：KeyRelease 的 keysym 一模一样，混进来会变成 "hheelllloo"
    echo "KEYSYMS $(grep -A4 '^KeyPress event' "$L" | grep -oE 'keysym 0x[0-9a-f]+, [A-Za-z0-9_]+' | awk -F', ' '{printf "%s ", $2}')"
    # 非 ASCII：走动态注册的 Unicode keycode（0x01000000+码点），xev 里是 U4E2D 这种名字
    echo "UNICODE $(grep -A4 '^KeyPress event' "$L" | grep -oE 'keysym 0x1[0-9a-f]+, [A-Za-z0-9_]+' | awk -F', ' '{printf "%s ", $2}')"
    # XLookupString 把码点翻成了 UTF-8 字节（中的 e4 b8 ad / 文的 e6 96 87）——这才是「应用真的拿到了字」
    echo "UTF8ZHONG $(grep -c 'bytes: (e4 b8 ad)' "$L")"
    echo "UTF8WEN $(grep -c 'bytes: (e6 96 87)' "$L")"
    echo "POINTER $(DISPLAY="$DISP" xdotool getmouselocation 2>/dev/null)"
    # 会话统计是被控端在收尾时写的，客户端刚退出时可能还没落盘 —— 最多再等 4 秒。
    # 先剥 ANSI 再 grep：字段名可能带颜色（`events\x1b[0m\x1b[2m=\x1b[0m17`）。
    STAT=""
    for _ in $(seq 1 20); do
      STAT=$(sed -E 's/\x1b\[[0-9;]*m//g' "$DIR/host.log" \
        | grep -oE '键鼠事件统计 events=[0-9]+ inject_errors=[0-9]+ dropped_locked=[0-9]+' | tail -1)
      [ -n "$STAT" ] && break
      sleep 0.2
    done
    echo "HOSTSTAT $STAT"
    ;;
  status)
    for f in xvfb server host xev; do
      pid=$(cat "$DIR/$f.pid" 2>/dev/null)
      if [ -n "${pid:-}" ] && kill -0 "$pid" 2>/dev/null; then echo "$f 运行中 pid=$pid"; else echo "$f 未运行"; fi
    done
    ;;
  *)
    echo "用法: bash remote-xvfb-testbed.sh up|down|events|status"
    exit 1
    ;;
esac