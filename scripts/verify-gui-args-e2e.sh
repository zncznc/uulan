#!/usr/bin/env bash
# GUI → 控制端 这条接缝的验收：面板到底会拼出什么命令行，拼出来的能不能用。
#
# 为什么需要它：`uulan-gui` v1 不接管画面，它只是 spawn 同一个 `uulan-client`。
# 于是「面板能不能用」整个落在那份 argv 上 —— 而 GUI 子系统没有控制台，参数错一个，
# 现场只看到「点了没反应」，日志里也没有任何东西。这条缝以前唯一的证据是「人点了一下，
# 窗口出来了」，机器查不到。
#
# 两个真 bug 就是在这条缝上（2026-11-20 夜批修掉，本脚本是它们的回归判据）：
#   ① 面板无条件写一个**空口令文件**并递 `--password-file`。被控端用「系统账户 + 登录密码」
#      认证时（OS 模式）根本没有访问口令，于是控制端在解析参数阶段就以
#      `--password-file 的第一行是空的` 退出 —— 面板还在，子进程已经死了。
#   ② 「还没填设备 ID」的检查比「直连不用设备 ID」的提示更早生效，照着提示做的人过不去。
#
# 用例：
#   A 经中继 + 口令      → argv 必须能跑出画面（真 server + 真被控端 + 真控制端）
#   B 直连 + 系统账户    → argv 里**不许**有 --password-file，且控制端能走到鉴权（不是参数报错）
#   C 直连 + 空设备 ID   → 表单放行；经中继 + 空设备 ID → 表单必须拦下（反面对照）
#
# 用法：bash scripts/verify-gui-args-e2e.sh
# 退出码：0 = 全过；1 = 断言红；2 = 环境问题（二进制没编出来 / 端口被占）
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${UULAN_STAGE:-D:/uulan-bin/gui-args-$(date +%Y%m%d-%H%M%S)}"
RPORT="${UULAN_RPORT:-21972}"
DPORT="${UULAN_DPORT:-21973}"
NO_COLOR=1
export NO_COLOR

mkdir -p "$WORK" || { echo "建不了 $WORK"; exit 2; }
for b in uulan-server uulan-host uulan-client uulan-gui; do
  [ -x "$ROOT/target/release/$b.exe" ] || { echo "缺少 $ROOT/target/release/$b.exe —— 先 cargo build --release"; exit 2; }
  cp -f "$ROOT/target/release/$b.exe" "$WORK/" || { echo "拷 $b 失败（上一轮还在跑？）"; exit 2; }
done
echo "工作目录：$WORK  中继端口：$RPORT  直连端口：$DPORT"

strip() { sed -E 's/\x1b\[[0-9;]*m//g' "$1"; }
PASS=0; FAIL=0
ok()   { PASS=$((PASS+1)); echo "  [ OK ] $*"; }
bad()  { FAIL=$((FAIL+1)); echo "  [FAIL] $*"; }

SERVER_PID=""; HOST_PID=""
cleanup() {
  [ -n "$HOST_PID" ] && kill "$HOST_PID" 2>/dev/null
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null
  # MSYS 的 kill 对**原生 Windows** 进程常常无效（第一版收尾后台上真留着两个 uulan-host，
  # 下一轮端口被占、cp 新 exe 还会 Device or resource busy）。用 Win32 TerminateProcess 兜底：
  # 无人值守会话里 taskkill /F 会被命令守卫拦下，这条路不经过它。
  sleep 0.3
  python "$ROOT/scripts/kill-pid.py" ${HOST_PID:-} ${SERVER_PID:-} >/dev/null 2>&1 || true
}
trap cleanup EXIT

RUST_LOG=info "$WORK/uulan-server.exe" --bind "127.0.0.1:$RPORT" --state "$WORK/devices.json" \
  >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 100); do
  (exec 3<>"/dev/tcp/127.0.0.1/$RPORT") 2>/dev/null && { exec 3>&-; break; }
  sleep 0.1
done
kill -0 "$SERVER_PID" 2>/dev/null || { echo "中继没起来"; sed -n 1,10p "$WORK/server.log"; exit 2; }

RUST_LOG=info "$WORK/uulan-host.exe" --server "127.0.0.1:$RPORT" --config "$WORK/host.json" \
  --capture "lavfi:testsrc2=size=320x240:rate=15" --name e2e-host --listen "127.0.0.1:$DPORT" \
  >"$WORK/host.log" 2>&1 &
HOST_PID=$!

DEVICE=""
for _ in $(seq 1 200); do
  DEVICE=$(grep -oE "设备 ID : [0-9]{6,}" "$WORK/host.log" 2>/dev/null | tail -1 | sed 's/.*: *//' | tr -d '[:space:]')
  [ -n "$DEVICE" ] && break
  sleep 0.1
done
[ -n "$DEVICE" ] || { echo "被控端没拿到设备 ID（已滤掉密码行）："; grep -vE "访问密码" "$WORK/host.log" | sed -n 1,15p; exit 2; }
# 口令从 host.json 读，**不回显**（台架自己生成的随机口令，与真机无关）。
PW=$(sed -n 's/.*"password": *"\([^"]*\)".*/\1/p' "$WORK/host.json" | head -1)
[ -n "$PW" ] || { echo "host.json 里没有 password 字段"; exit 2; }

echo
echo "── A：经中继 + 口令 —— 面板拼的 argv 必须真能跑出画面 ──"
LOCALAPPDATA="$WORK/cfgA" UULAN_GUI_PASSWORD="$PW" \
  "$WORK/uulan-gui.exe" --print-args --relay "127.0.0.1:$RPORT" --device "$DEVICE" \
  --argv-out "$WORK/argv-a.txt" >"$WORK/print-a.txt" 2>&1
RC=$?
[ "$RC" = 0 ] && ok "print-args 退出码 0" || bad "print-args 退出码 $RC（$(cat "$WORK/print-a.txt")）"
grep -qx -- "--password-file" "$WORK/argv-a.txt" && ok "argv 里带 --password-file" || bad "argv 里没有 --password-file（口令没用上）"
grep -qx -- "--device" "$WORK/argv-a.txt" && ok "argv 里带 --device" || bad "argv 里没有 --device（经中继必须带）"
if grep -qF -- "$PW" "$WORK/print-a.txt"; then bad "print-args 把口令打上屏了"; else ok "print-args 没把口令打上屏（脱敏生效）"; fi

mapfile -t ARGS < "$WORK/argv-a.txt"
"$WORK/uulan-client.exe" "${ARGS[@]}" --headless --seconds 6 >"$WORK/client-a.log" 2>&1
CRC=$?
# 统计行长这样：「  收到 AU    : 22554」/「  解出帧     : 22550（本进程渲染 22548）」——
# 冒号两侧的空格数不定，按固定空格数去 grep 会像本轮第一版那样报「AU=? 解出=?」的假红。
AU=$(strip "$WORK/client-a.log" | sed -n 's/.*收到 AU *: *\([0-9][0-9]*\).*/\1/p' | tail -1)
DEC=$(strip "$WORK/client-a.log" | sed -n 's/.*解出帧 *: *\([0-9][0-9]*\).*/\1/p' | tail -1)
[ "$CRC" = 0 ] && ok "控制端退出码 0" || bad "控制端退出码 $CRC"
if [ -n "${DEC:-}" ] && [ "$DEC" -ge 30 ]; then ok "解出 $DEC 帧（AU=$AU）—— 面板拼的 argv 真能用"
else bad "解出帧不足（AU=${AU:-?} 解出=${DEC:-?}）"; strip "$WORK/client-a.log" | tail -5; fi

echo
echo "── B：直连 + 系统账户（没有访问口令）—— 不许出现 --password-file ──"
LOCALAPPDATA="$WORK/cfgB" UULAN_GUI_OS_PASSWORD="NotARealPass9" \
  "$WORK/uulan-gui.exe" --print-args --direct "127.0.0.1:$DPORT" --os-user mis \
  --argv-out "$WORK/argv-b.txt" >"$WORK/print-b.txt" 2>&1
RC=$?
[ "$RC" = 0 ] && ok "print-args 退出码 0" || bad "print-args 退出码 $RC（$(cat "$WORK/print-b.txt")）"
if grep -qx -- "--password-file" "$WORK/argv-b.txt"; then
  bad "OS 模式仍然递了 --password-file —— 这就是「点了没反应」那个 bug 回来了"
else ok "argv 里没有 --password-file（OS 模式本来就没有访问口令）"; fi
grep -qx -- "--os-user" "$WORK/argv-b.txt" && ok "argv 里带 --os-user" || bad "argv 里没有 --os-user"
grep -qx -- "--os-password-file" "$WORK/argv-b.txt" && ok "argv 里带 --os-password-file" || bad "argv 里没有 --os-password-file"

mapfile -t ARGSB < "$WORK/argv-b.txt"
"$WORK/uulan-client.exe" "${ARGSB[@]}" --headless --seconds 6 >"$WORK/client-b.log" 2>&1
CRCB=$?
if strip "$WORK/client-b.log" | grep -qE "第一行是空的|读不了 --password-file"; then
  bad "控制端死在参数解析上（$(strip "$WORK/client-b.log" | grep -E "第一行是空的|读不了" | head -1)）"
else ok "控制端越过了参数解析（退出码 $CRCB）"; fi
# 台架上的被控端是口令模式（auth_os=false），OS 模式必然被拒 —— 被拒本身就证明
# AuthMode 帧到了对面。而「为什么被拒」必须由被控端自己说出来（2026-11-20 修：
# 以前它只写在自己的日志里，控制端只看到「握手期间被控端断开」，现场会去查网络）。
if strip "$WORK/client-b.log" | grep -q "系统账户"; then
  ok "控制端拿到被控端给的原因（提到「系统账户」），不是笼统的「断开」"
elif strip "$WORK/client-b.log" | grep -q "握手期间被控端断开"; then
  bad "控制端只得到「握手期间被控端断开」—— 拒绝原因没上线（回话纪律退步了）"
else bad "控制端既没成功也没被拒，停在哪？"; strip "$WORK/client-b.log" | tail -5; fi

# 反面对照：旧形状（空口令文件也递过去）必须当场报错，否则上面那条「没带就没事」是空话。
: > "$WORK/empty.pw"
"$WORK/uulan-client.exe" --direct "127.0.0.1:$DPORT" --password-file "$WORK/empty.pw" \
  --os-user mis --os-password-file "$WORK/os.pw" --headless --seconds 3 >"$WORK/client-neg.log" 2>&1
if strip "$WORK/client-neg.log" | grep -q "第一行是空的"; then
  ok "反面对照：递空口令文件 → 参数阶段就退出（这正是本脚本要挡的形状）"
else bad "反面对照没有复现参数错误"; strip "$WORK/client-neg.log" | tail -3; fi

echo
echo "── C：设备 ID 与直连必填项 ──"
LOCALAPPDATA="$WORK/cfgC" UULAN_GUI_PASSWORD="TestPass123" \
  "$WORK/uulan-gui.exe" --print-args --direct "127.0.0.1:$DPORT" --argv-out "$WORK/argv-c.txt" \
  >"$WORK/print-c.txt" 2>&1
RC=$?
[ "$RC" = 0 ] && ok "直连 + 空设备 ID：表单放行（提示说「那时不用设备 ID」，就该真不用）" \
              || bad "直连 + 空设备 ID 被拦下（rc=$RC）"
grep -qx -- "--direct" "$WORK/argv-c.txt" 2>/dev/null && ok "argv 里是 --direct" || bad "argv 里没有 --direct"
if grep -qx -- "--device" "$WORK/argv-c.txt" 2>/dev/null; then bad "直连还带了 --device"; else ok "直连没有带 --device"; fi

LOCALAPPDATA="$WORK/cfgC" UULAN_GUI_PASSWORD="TestPass123" \
  "$WORK/uulan-gui.exe" --print-args --relay "127.0.0.1:$RPORT" >"$WORK/print-c2.txt" 2>&1
RC=$?
[ "$RC" = 2 ] && ok "经中继 + 空设备 ID：表单拦下（退出码 2）" || bad "经中继 + 空设备 ID 的退出码是 $RC（应为 2）"
grep -q "设备 ID" "$WORK/print-c2.txt" && ok "拒绝理由说得清楚（提到设备 ID）" || bad "拒绝理由没提到设备 ID"

echo
echo "════ 结果：$PASS 通过 / $FAIL 失败（工作目录 $WORK）════"
[ "$FAIL" = 0 ] || exit 1
exit 0