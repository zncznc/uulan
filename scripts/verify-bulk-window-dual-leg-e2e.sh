#!/usr/bin/env bash
# 双端延迟台架（**本机全本地**：控制端 / 中继 / 被控端 / 两个延迟代理都在开发机上）
#
# 为什么要有这一条：`verify-bulk-window-wan-e2e.sh` 只把延迟加在「控制端 ↔ 中继」一腿，
# 报告里必须写「RTT 是下界、吞吐是上界」。真实部署是**两端都慢**：控制端 → 中继 → 被控端。
# 这里在同一条链路上插**两个**延迟代理（控制端腿 + 被控端腿），把「路程 RTT = 2 × (2d)」
# 的形态造出来，回答两个工程问题：
#
#   A. 窗口帧数协商出来的在途量（16 帧 = 1 MiB / 64 帧 = 4 MiB）在**双程都慢**的链路上
#      是否仍然按「窗口 / RTT」的比例给出吞吐（模型能不能外推到两跳）。
#   B. **文件传输（unwindowed fs）**在同样的双程延迟下会不会比端口映射慢一个数量级
#      —— 如果会，那才值得给 fs 也接窗口；不会就别为了「对称好看」去改它。
#
# 判据（机器可查）：
#   ① 每个 (延迟, 窗口) 组合下 1 MiB / 4 MiB 全双工 sha256 一致（信用不丢字节）
#   ② 两端各自算出的窗口帧数**对上**（控制端 `按流窗口 : Some(n)`；被控端日志
#      `window=Some(n) client_frames=n host_frames=n`）
#   ③ 双程 50 ms（路程 RTT ≈ 200 ms）下 64 帧明显快于 16 帧（窗口真的在起作用）
#   ④ 同一档位下 fs 4 MiB put/get sha256 一致（只报时间，不当硬判据 —— 见 B）
#
# ⚠ 口径：两个代理都在**同一台机器**上，所以这测的是「窗口/RTT 模型在两跳链路上成不成立」，
#   **不是**真实广域网（没有丢包、没有带宽上限、没有抖动）。报告里必须这么写。
#
# 用法：
#   bash scripts/verify-bulk-window-dual-leg-e2e.sh
# 可覆盖：UULAN_DL_DELAYS("0 25 50") / UULAN_DL_WINDOWS("16 64") / UULAN_DL_MIB(4)
# 退出码：0 = 全过；1 = 断言失败；2 = 环境问题（端口占用 / 进程没起来）。
set -u

PORT="${UULAN_DL_PORT:-21981}"          # 中继
TARGET_PORT="${UULAN_DL_TARGET:-21982}" # 被控端侧 echo 目标
LOCAL_PORT="${UULAN_DL_LOCAL:-21983}"   # 控制端本地监听
PXA="${UULAN_DL_PXA:-23361}"            # 控制端腿延迟代理
PXB="${UULAN_DL_PXB:-23362}"            # 被控端腿延迟代理
DELAYS="${UULAN_DL_DELAYS:-0 25 50}"
WINDOWS="${UULAN_DL_WINDOWS:-16 64}"
MIB="${UULAN_DL_MIB:-4}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${UULAN_STAGE:-D:/uulan-bin/dual-leg-$(date +%Y%m%d-%H%M%S)}"
NO_COLOR=1
export NO_COLOR

PASS=0; FAIL=0; SKIP=0
ok()   { echo "  [PASS] $*"; PASS=$((PASS+1)); }
bad()  { echo "  [FAIL] $*"; FAIL=$((FAIL+1)); }
skip() { echo "  [SKIP] $*"; SKIP=$((SKIP+1)); }
env_out() { echo "  [环境] $*"; echo "退出码 2：这台机器此刻给不出这条判据，别把它记成产品缺陷"; exit 2; }

mkdir -p "$WORK" || env_out "建不了 $WORK"
cp -f "$ROOT"/target/release/uulan-server.exe \
      "$ROOT"/target/release/uulan-host.exe \
      "$ROOT"/target/release/uulan-client.exe "$WORK"/ 2>/dev/null || env_out "拷二进制失败（上一轮还在跑？）"
echo "工作目录：$WORK  中继 :$PORT  echo :$TARGET_PORT  本地监听 :$LOCAL_PORT  代理 :$PXA/$PXB"

cat >"$WORK/echo.py" <<'PY'
import socket, sys, threading
port = int(sys.argv[1])
srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", port)); srv.listen(32)
print("echo-ready", flush=True)

def serve(c):
    with c:
        while True:
            b = c.recv(65536)
            if not b: break
            c.sendall(b)

while True:
    c, _ = srv.accept()
    threading.Thread(target=serve, args=(c,), daemon=True).start()
PY

cat >"$WORK/probe.py" <<'PY'
import json, socket, sys, threading, time
port = int(sys.argv[1]); n = int(sys.argv[2]); seed = int(sys.argv[3])
data = bytes(((i * 37 + 11 + seed * 97) % 256) for i in range(n))
out = {"ok": False, "sent": 0, "got": 0, "err": "", "ms": 0}
t0 = time.time()
try:
    s = socket.create_connection(("127.0.0.1", port), timeout=180)
    got = bytearray(); err = []
    def reader():
        try:
            while len(got) < n:
                b = s.recv(65536)
                if not b: break
                got.extend(b)
        except Exception as e:
            err.append("read: %s" % e)
    t = threading.Thread(target=reader); t.start()
    for i in range(0, n, 65536):
        s.sendall(data[i:i + 65536])
    t.join(timeout=180)
    s.close()
    out.update(sent=n, got=len(got), err=";".join(err), ms=int((time.time() - t0) * 1000))
    out["ok"] = (bytes(got) == data and not err)
except Exception as e:
    out["err"] = str(e)
print(json.dumps(out))
sys.exit(0 if out["ok"] else 1)
PY

SPID=""; HPID=""; CPID=""; PA=""; PB=""; ECHO=""
cleanup() {
  for p in "$CPID" "$HPID" "$SPID" "$PA" "$PB" "$ECHO"; do
    [ -n "$p" ] && kill "$p" 2>/dev/null
  done
  for p in "$CPID" "$HPID" "$SPID" "$PA" "$PB" "$ECHO"; do
    [ -n "$p" ] && python scripts/kill-pid.py "$p" >/dev/null 2>&1
  done
  return 0
}
trap cleanup EXIT

strip() { sed -E 's/\x1b\[[0-9;]*m//g' "$1"; }

echo "== 0. 前置 =="
for p in "$PORT" "$TARGET_PORT" "$LOCAL_PORT" "$PXA" "$PXB"; do
  c=$(netstat -ano 2>/dev/null | grep -E "LISTENING" | grep -c ":$p " || true)
  [ "${c:-0}" = "0" ] || env_out "本机 $p 已被占用（上一轮台架没清干净？）"
done
python -c "print(1)" >/dev/null || env_out "没有 python"

echo "== 1. 起 echo 目标 / 中继（本机）=="
python "$WORK/echo.py" "$TARGET_PORT" >"$WORK/echo.log" 2>&1 &
ECHO=$!
for _ in $(seq 1 60); do grep -aq echo-ready "$WORK/echo.log" 2>/dev/null && break; sleep 0.1; done
grep -aq echo-ready "$WORK/echo.log" 2>/dev/null && ok "echo 目标在 127.0.0.1:$TARGET_PORT" || env_out "echo 没起来"

RUST_LOG=info "$WORK/uulan-server.exe" --bind "127.0.0.1:$PORT" --state "$WORK/devices.json" \
  >"$WORK/server.log" 2>&1 &
SPID=$!
for _ in $(seq 1 100); do
  python -c "import socket,sys; socket.create_connection(('127.0.0.1',$PORT),0.5)" >/dev/null 2>&1 && break
  sleep 0.1
done
grep -aq "" "$WORK/server.log" 2>/dev/null && ok "中继在 127.0.0.1:$PORT" || env_out "中继没起来"

start_proxies() {  # start_proxies <delay_ms>
  local d="$1"
  [ -n "$PA" ] && kill "$PA" 2>/dev/null; [ -n "$PB" ] && kill "$PB" 2>/dev/null
  PA=""; PB=""
  if [ "$d" = "0" ]; then
    HOSTLEG="127.0.0.1:$PORT"; CLIENTLEG="127.0.0.1:$PORT"; return 0
  fi
  python scripts/latency-proxy.py "$PXA" "127.0.0.1:$PORT" "$d" >"$WORK/proxyA-$d.out" 2>&1 &
  PA=$!
  python scripts/latency-proxy.py "$PXB" "127.0.0.1:$PORT" "$d" >"$WORK/proxyB-$d.out" 2>&1 &
  PB=$!
  for _ in $(seq 1 60); do
    grep -aq PROXY_READY "$WORK/proxyA-$d.out" 2>/dev/null && \
    grep -aq PROXY_READY "$WORK/proxyB-$d.out" 2>/dev/null && break
    sleep 0.2
  done
  grep -aq PROXY_READY "$WORK/proxyA-$d.out" 2>/dev/null || { bad "控制端腿延迟代理 $d ms 没起来"; return 1; }
  grep -aq PROXY_READY "$WORK/proxyB-$d.out" 2>/dev/null || { bad "被控端腿延迟代理 $d ms 没起来"; return 1; }
  HOSTLEG="127.0.0.1:$PXB"
  CLIENTLEG="127.0.0.1:$PXA"
  return 0
}

start_host() {  # start_host <win>
  local w="$1"
  [ -n "$HPID" ] && { kill "$HPID" 2>/dev/null; python scripts/kill-pid.py "$HPID" >/dev/null 2>&1; HPID=""; sleep 0.5; }
  RUST_LOG=info "$WORK/uulan-host.exe" --server "$HOSTLEG" --config "$WORK/host.json" \
    --capture "lavfi:testsrc2=size=640x360:rate=10" --name dual-leg --bulk-window "$w" \
    >"$WORK/host-w$w.log" 2>&1 &
  HPID=$!
  DEVICE=""
  for _ in $(seq 1 300); do
    DEVICE=$(grep -aoE "设备 ID : [0-9]{6,}" "$WORK/host-w$w.log" 2>/dev/null | tail -1 | sed 's/.*: *//' | tr -d '[:space:]')
    [ -n "$DEVICE" ] && break
    sleep 0.1
  done
  if [ -z "$DEVICE" ]; then
    grep -avE "访问密码" "$WORK/host-w$w.log" | sed -n 1,15p | sed 's/^/      /'
    bad "窗口 $w：被控端没注册上"
    return 1
  fi
  PW=$(sed -n 's/.*"password": *"\([^"]*\)".*/\1/p' "$WORK/host.json" | head -1)
  [ -n "$PW" ] || { bad "host.json 里没有 password"; return 1; }
  return 0
}

# 被控端算出来的窗口（协商结果）**要等有会话连上来才打印** —— 这个断言必须在
# 控制端跑完之后再查（第一版排在 start_host 里，永远查不到，自己给自己造了 6 个红）。
check_host_window() {  # check_host_window <win> <tag>
  local w="$1" tag="$2" hl
  for _ in $(seq 1 40); do
    grep -aq "window=Some" "$WORK/host-w$w.log" 2>/dev/null && break
    sleep 0.2
  done
  hl=$(grep -a "window=Some" "$WORK/host-w$w.log" 2>/dev/null | tail -1)
  case "$hl" in
    *"window=Some($w)"*"client_frames=$w"*"host_frames=$w"*) ok "$tag：被控端对上（${hl#*uulan_host::session: }）" ;;
    *) bad "$tag：被控端协商结果不符：${hl:-（没有 window=Some 行）}" ;;
  esac
}

run_level() {  # run_level <delay_ms> <win>
  local d="$1" w="$2" tag="d${1}-w${2}"
  "$WORK/uulan-client.exe" --server "$CLIENTLEG" --device "$DEVICE" --password "$PW" \
    --name dual-leg-client --bulk-window "$w" \
    --forward "127.0.0.1:$LOCAL_PORT:127.0.0.1:$TARGET_PORT" --seconds 18 \
    >"$WORK/fwd-$tag.out" 2>"$WORK/fwd-$tag.err" &
  CPID=$!
  local ready=NO
  for _ in $(seq 1 120); do
    grep -aq "转发已就绪" "$WORK/fwd-$tag.out" 2>/dev/null && { ready=YES; break; }
    sleep 0.25
  done
  if [ "$ready" != "YES" ]; then
    bad "$tag：控制端没就绪（$(head -c 200 "$WORK/fwd-$tag.err" 2>/dev/null)）"
    kill "$CPID" 2>/dev/null; CPID=""; return 1
  fi
  local wl
  wl=$(strip "$WORK/fwd-$tag.out" | grep -a "按流窗口" | tail -1)
  case "$wl" in
    *"Some($w)"*) ok "$tag：控制端窗口 Some($w)" ;;
    *) bad "$tag：控制端窗口不符：${wl:-（没打印）}" ;;
  esac
  python "$WORK/probe.py" "$LOCAL_PORT" 1048576 1 >"$WORK/p1-$tag.json" 2>&1
  python "$WORK/probe.py" "$LOCAL_PORT" 4194304 2 >"$WORK/p4-$tag.json" 2>&1
  wait "$CPID" 2>/dev/null; local rc=$?
  CPID=""
  [ "$rc" = "0" ] || bad "$tag：控制端收工 rc=$rc"
  echo "      端口映射 1 MiB：$(cat "$WORK/p1-$tag.json")"
  echo "      端口映射 4 MiB：$(cat "$WORK/p4-$tag.json")"
  echo "      统计：$(strip "$WORK/fwd-$tag.out" | grep -a '端口映射结束' | tail -1)"
  return 0
}

measure_setup_cost() {  # measure_setup_cost <delay_ms> <win> —— 一次「只用控制通道」的操作的总耗时
  local d="$1" w="$2" tag="d${1}-w${2}"
  local t0 t1 ms
  t0=$(date +%s%N)
  "$WORK/uulan-client.exe" --server "$CLIENTLEG" --device "$DEVICE" --password "$PW" \
    --name dual-leg-ls --bulk-window "$w" --ls ":/" >"$WORK/ls-$tag.log" 2>&1
  local rc=$?
  t1=$(date +%s%N)
  ms=$(( (t1 - t0) / 1000000 ))
  echo "      固定开销（--ls :/ 一次）：${ms} ms（rc=$rc）"
  printf '{"d":%s,"w":%s,"ls_ms":%s}\n' "$d" "$w" "$ms" >> "$WORK/ls.jsonl"
  [ "$rc" = "0" ] || bad "$tag：--ls 失败 rc=$rc"
  return 0
}

run_fs() {  # run_fs <delay_ms> <win>
  local d="$1" w="$2" tag="d${1}-w${2}"
  local sub="$WORK/fsdata-$tag"
  mkdir -p "$sub"
  local src="$WORK/src-$tag.bin"
  python -c "import sys;open(sys.argv[1],'wb').write(bytes((i*37+11)%256 for i in range($MIB*1024*1024)))" "$src" 2>/dev/null
  if [ ! -s "$src" ]; then bad "$tag：造测试文件失败"; return 1; fi
  local remote="$sub/up.bin" back="$WORK/back-$tag.bin"
  local t0 t1 t2
  t0=$(date +%s%N)
  "$WORK/uulan-client.exe" --server "$CLIENTLEG" --device "$DEVICE" --password "$PW" \
    --name dual-leg-fs --bulk-window "$w" --put "$src" "$remote" >"$WORK/put-$tag.log" 2>&1
  local rc=$?
  t1=$(date +%s%N)
  "$WORK/uulan-client.exe" --server "$CLIENTLEG" --device "$DEVICE" --password "$PW" \
    --name dual-leg-fs --bulk-window "$w" --get "$remote" "$back" >"$WORK/get-$tag.log" 2>&1
  local rc2=$?
  t2=$(date +%s%N)
  local a b c
  a=$(sha256sum "$src" | cut -d' ' -f1)
  b=$( [ -f "$remote" ] && sha256sum "$remote" | cut -d' ' -f1 || echo missing)
  c=$( [ -f "$back" ] && sha256sum "$back" | cut -d' ' -f1 || echo missing)
  local putms=$(( (t1 - t0) / 1000000 )) getms=$(( (t2 - t1) / 1000000 ))
  echo "      fs ${MIB} MiB：put ${putms} ms（rc=$rc）/ get ${getms} ms（rc=$rc2）"
  if [ "$rc" = "0" ] && [ "$rc2" = "0" ] && [ "$a" = "$b" ] && [ "$a" = "$c" ]; then
    ok "$tag：fs ${MIB} MiB put/get sha256 三处一致（put ${putms} ms / get ${getms} ms）"
  else
    bad "$tag：fs 不一致（rc=$rc/$rc2，sha ${a:0:12}/${b:0:12}/${c:0:12}）"
  fi
  printf '{"d":%s,"w":%s,"put_ms":%s,"get_ms":%s,"mib":%s}\n' "$d" "$w" "$putms" "$getms" "$MIB" >> "$WORK/fs.jsonl"
  return 0
}

echo "== 2. 逐档跑（每腿单向延迟：$DELAYS ms ⇒ 路程 RTT = 4d）=="
for d in $DELAYS; do
  echo "──────── 延迟档 $d ms（路程 RTT ≈ $((d * 4)) ms）────────"
  start_proxies "$d" || continue
  for w in $WINDOWS; do
    echo "   窗口 $w 帧"
    start_host "$w" || continue
    run_level "$d" "$w"
    check_host_window "$w" "d$d-w$w"
    measure_setup_cost "$d" "$w"
    run_fs "$d" "$w"
  done
done
[ -n "$HPID" ] && { kill "$HPID" 2>/dev/null; python scripts/kill-pid.py "$HPID" >/dev/null 2>&1; HPID=""; }

echo "== 3. 判据 =="
# ① 每条流 sha256 一致（probe.py 内部就在比，这里把结果汇总成一条条 ok/bad）
for d in $DELAYS; do
  for w in $WINDOWS; do
    tag="d${d}-w${w}"
    p1=$(python -c "import json,sys;d=json.load(open(sys.argv[1]));print(1 if d.get('ok') else 0)" "$WORK/p1-$tag.json" 2>/dev/null || echo 0)
    p4=$(python -c "import json,sys;d=json.load(open(sys.argv[1]));print(1 if d.get('ok') else 0)" "$WORK/p4-$tag.json" 2>/dev/null || echo 0)
    if [ "$p1" = "1" ] && [ "$p4" = "1" ]; then
      ok "$tag：1 MiB + 4 MiB 全双工 sha256 一致"
    else
      bad "$tag：有流不通过（$(cat "$WORK/p1-$tag.json" 2>/dev/null) / $(cat "$WORK/p4-$tag.json" 2>/dev/null)）"
    fi
  done
done

# ② 吞吐（按单方向字节 / 时间算）与模型
python - "$WORK" "$WINDOWS" "$DELAYS" <<'PY' > "$WORK/verdict.txt" 2>&1
import json, os, sys
w, wins, delays = sys.argv[1], sys.argv[2].split(), sys.argv[3].split()
print("档位           1 MiB(ms)  4 MiB(ms)  1MiB MB/s   4MiB MB/s   模型(窗口/RTT) 实测/模型")
rows = {}
for d in delays:
    for win in wins:
        tag = "d%s-w%s" % (d, win)
        try:
            p1 = json.load(open(os.path.join(w, "p1-%s.json" % tag)))
            p4 = json.load(open(os.path.join(w, "p4-%s.json" % tag)))
        except Exception:
            print("%-12s  （缺数据）" % tag); continue
        rtt = 4 * int(d) / 1000.0
        winb = int(win) * 65536
        cap = (winb / rtt / 1e6) if rtt else 0.0
        m1 = 1048576 / (p1["ms"] / 1000.0) / 1e6 if p1.get("ms") else 0.0
        m4 = 4194304 / (p4["ms"] / 1000.0) / 1e6 if p4.get("ms") else 0.0
        rows[(d, win)] = (p1.get("ms"), p4.get("ms"), m1, m4, cap)
        print("%-12s %8s  %8s  %9.2f  %9.2f  %12.2f  %8s"
              % (tag, p1.get("ms"), p4.get("ms"), m1, m4, cap,
                 ("%.2f" % (m4 / cap)) if cap else "-"))
# 16 vs 64 在同一档位下的对照
for d in delays:
    if d == "0":
        continue
    a = rows.get((d, wins[0])); b = rows.get((d, wins[-1]))
    if a and b and a[0] and b[0]:
        print("\n延迟 %s ms：1 MiB 往返 %s 帧 %s ms → %s 帧 %s ms（提速 %.0f%%）"
              % (d, wins[0], a[0], wins[-1], b[0], (a[0] / b[0] - 1) * 100))
PY
sed 's/^/      /' "$WORK/verdict.txt"

# ③ 双程延迟下 64 帧必须明显快于 16 帧（只有一档就跳过这条）
FIRST_W=${WINDOWS%% *}; LAST_W=${WINDOWS##* }
if [ "$FIRST_W" != "$LAST_W" ]; then
  for d in $DELAYS; do
    [ "$d" = "0" ] && continue
    T16=$(python -c "import json,sys;print(json.load(open(sys.argv[1]))['ms'])" "$WORK/p4-d$d-w$FIRST_W.json" 2>/dev/null || echo 0)
    T64=$(python -c "import json,sys;print(json.load(open(sys.argv[1]))['ms'])" "$WORK/p4-d$d-w$LAST_W.json" 2>/dev/null || echo 0)
    if [ "${T16:-0}" -gt 0 ] && [ "${T64:-0}" -gt 0 ]; then
      awk -v a="$T16" -v b="$T64" 'BEGIN{exit !(b < a/1.4)}' \
        && ok "延迟 $d ms：4 MiB 上 ${LAST_W} 帧比 ${FIRST_W} 帧快（$T64 ms vs $T16 ms，窗口在起作用）" \
        || bad "延迟 $d ms：放大窗口没有带来提速（4 MiB $T64 ms vs $T16 ms）"
    else
      skip "延迟 $d ms：缺少窗口对照数据"
    fi
  done
fi

echo
echo "==== 结果：$PASS 通过 / $FAIL 失败 / $SKIP 跳过 ===="
echo "证据目录：$WORK"
[ -f "$WORK/fs.jsonl" ] && { echo "fs 时间线（观察项，不是硬判据）："; sed 's/^/      /' "$WORK/fs.jsonl"; }
[ -f "$WORK/ls.jsonl" ] && { echo "固定开销（--ls 一次 = 建会话 + 控制通道往返，不含 bulk 数据）："; sed 's/^/      /' "$WORK/ls.jsonl"; }
if [ -f "$WORK/fs.jsonl" ] && [ -f "$WORK/ls.jsonl" ]; then
  python - "$WORK" <<'PY' | sed 's/^/      /'
import json, os, sys
w = sys.argv[1]
ls = {}
for line in open(os.path.join(w, "ls.jsonl")):
    j = json.loads(line); ls[(j["d"], j["w"])] = j["ls_ms"]
print("把固定开销扣掉之后的 fs 数据段：")
for line in open(os.path.join(w, "fs.jsonl")):
    j = json.loads(line)
    key = (j["d"], j["w"])
    base = ls.get(key)
    mib = j["mib"]
    for k in ("put_ms", "get_ms"):
        t = j[k]
        if base and t > base:
            mb = mib / ((t - base) / 1000.0)
            print("  d=%s w=%s %s：总 %d ms − 固定 %d ms = 数据 %d ms ⇒ %.1f MiB/s"
                  % (j["d"], j["w"], k.split("_")[0], t, base, t - base, mb))
        else:
            print("  d=%s w=%s %s：总 %d ms（固定 %s ms，扣不出正数：数据段可能比建会话还短）"
                  % (j["d"], j["w"], k.split("_")[0], t, base))
PY
fi
echo "口径提醒：两个延迟代理都在本机，测的是「窗口/RTT 模型在两跳链路上成不成立」；"
echo "          真实广域网的丢包 / 带宽上限 / 抖动不在这条台架的覆盖范围。"
[ "$FAIL" = "0" ] && exit 0 || exit 1
