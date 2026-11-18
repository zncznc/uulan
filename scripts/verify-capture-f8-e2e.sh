#!/usr/bin/env bash
# 捕获模式的 **F8 真键盘路径** 验收（Windows 控制端，窗口模式）
#
# 为什么单独一条：2026-11-17 夜批把捕获模式两端接线做完并真机验了 11/11，但 F8 这一格
# 只钉了单元测试的开关语义 —— 夜里没人按键盘，也保证不了窗口前台焦点。
# 这里用 SendInput 注入 F8（与硬件同源进消息队列）把它补上。
#
# 判据（四条，都不靠"看起来对"）：
#   ① 起手 `--capture` 就进捕获：日志「捕获模式已开启」+ 标题带 `· 捕获模式（F8 退出）`
#   ② 按一次 F8 → 日志「捕获模式已关闭：光标回到本地」+ 标题里的后缀消失
#   ③ 再按一次 F8 → 「捕获模式已开启」出现第 2 次 + 标题后缀回来
#   ④ 全程没有「指针锁定失败」（那意味着压根没抓住，后面几条就都是假的）
# 用法：
#   bash scripts/verify-capture-f8-e2e.sh --device <ID> --password-file <文件> [--server host:port]
# 注意：会短暂弹出控制端窗口并**锁住本地指针**（这就是被测行为），几秒后自动收工。
set -u

DEVICE="" PWFILE="" SERVER=${UULAN_SERVER:-192.168.223.15:21116}
CLIENT=${UULAN_CLIENT:-target/release/uulan-client.exe}
while [ $# -gt 0 ]; do
  case $1 in
    --device) DEVICE=$2; shift 2 ;;
    --password-file) PWFILE=$2; shift 2 ;;
    --server) SERVER=$2; shift 2 ;;
    *) echo "未知参数：$1"; exit 2 ;;
  esac
done
[ -n "$DEVICE" ] && [ -n "$PWFILE" ] || { echo "用法：--device <ID> --password-file <文件>"; exit 2; }
[ -x "$CLIENT" ] || { echo "FAIL 找不到控制端 $CLIENT"; exit 2; }
command -v python >/dev/null || { echo "FAIL 需要 python（SendInput 注入器用它）"; exit 2; }

TS=$(date +%H%M%S)
LOG="target/logs/capture-f8-$TS.log"
mkdir -p target/logs
pass=0 fail=0
ok()  { echo "  PASS $1"; pass=$((pass+1)); }
bad() { echo "  FAIL $1"; fail=$((fail+1)); }
cnt() { grep -ac "$1" "$LOG" 2>/dev/null || true; }
# 窗口标题（捕获态会带「· 捕获模式（F8 退出）」后缀）
title_of() {
  # 必须显式设 UTF-8 输出编码：默认 OEM 代码页会把中文标题吐成乱码，于是「标题里有没有
  # 捕获标记」永远判错 —— 和 2026-11-12 剪贴板那次同一个坑（判据坏了，不是功能坏了）。
  powershell.exe -NoProfile -Command "[Console]::OutputEncoding=[Text.Encoding]::UTF8; (Get-Process uulan-client -ErrorAction SilentlyContinue | Where-Object {\$_.MainWindowTitle} | Select-Object -First 1).MainWindowTitle" 2>/dev/null | tr -d '\r'
}

echo "== 0) 起一个带 --capture 的窗口会话（这会短暂锁住本地指针）=="
RUST_LOG=info NO_COLOR=1 "$CLIENT" --server "$SERVER" --device "$DEVICE" \
  --password-file "$PWFILE" --capture --name f8-验收 > "$LOG" 2>&1 &
CPID=$!
sleep 9
if ! kill -0 "$CPID" 2>/dev/null; then
  echo "  FAIL 控制端没起来（日志末尾）："; tail -6 "$LOG" | sed 's/^/    /'
  echo; echo "结果：$pass 通过 / $fail 失败"; exit 1
fi
T1=$(title_of)
echo "  标题：[$T1]"

# 置前 + 按 F8；置前失败就等于把键送给了别的窗口，必须当成环境问题退出而不是记失败
FO=$(python scripts/capture-sendkey.py --focus "uulan" 2>&1)
printf '  %s\n' "$FO"
printf '%s' "$FO" | grep -q "FOCUS OK" || { echo "  FAIL 窗口置前失败，F8 送达不到被测窗口（环境问题，不当功能失败）"; kill "$CPID" 2>/dev/null; echo; echo "结果：$pass 通过 / $fail 失败"; exit 2; }

echo
echo "== 1) 起手就该在捕获态 =="
[ "$(cnt '捕获模式已开启')" -ge 1 ] && ok "① 日志有「捕获模式已开启」" || bad "① 日志没有「捕获模式已开启」"
printf '%s' "$T1" | grep -q "捕获模式（F8 退出）" && ok "① 标题带「· 捕获模式（F8 退出）」" || bad "① 标题没带捕获标记：[$T1]"

echo
echo "== 2) 按一次 F8 =="
python scripts/capture-sendkey.py 0x77 2>&1 | sed 's/^/  /'
sleep 2.5
T2=$(title_of)
[ "$(cnt '捕获模式已关闭')" -ge 1 ] && ok "② 日志有「捕获模式已关闭：光标回到本地」" || bad "② 日志没有「捕获模式已关闭」"
printf '%s' "$T2" | grep -q "捕获模式" && bad "② 标题还带捕获标记：[$T2]" || ok "② 标题后缀消失（[$T2]）"

echo
echo "== 3) 再按一次 F8（切回捕获）=="
python scripts/capture-sendkey.py 0x77 2>&1 | sed 's/^/  /'
sleep 2.5
T3=$(title_of)
[ "$(cnt '捕获模式已开启')" -ge 2 ] && ok "③ 「捕获模式已开启」出现第 2 次（真的是开关不是一次性）" || bad "③ 第二次开启没发生"
printf '%s' "$T3" | grep -q "捕获模式（F8 退出）" && ok "③ 标题后缀回来了（[$T3]）" || bad "③ 标题没回来：[$T3]"

echo
echo "== 4) 反面对照：全程不许出现抓握失败 =="
[ "$(cnt '指针锁定失败')" -eq 0 ] && ok "④ 没有「指针锁定失败」" || bad "④ 出现抓握失败，上面几条不成立"

echo
kill "$CPID" 2>/dev/null; sleep 1; kill -9 "$CPID" 2>/dev/null
echo "结果：$pass 通过 / $fail 失败"
echo "（日志 $LOG）"
[ "$fail" = "0" ]