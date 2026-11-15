#!/usr/bin/env bash
# --terms 真 exe 冒烟（一次性，不常驻）：
#   1) 全新控制连接 --terms → 必须明说「没有终端会话」、退出码 0
#   2) --terms --json → [] 且退出码 0
#   3) --terms 与 --quality 同时给 → 必须当场报错（清单不开视频通道）
set -u
ROOT="D:/default-workspace/uulan"
S="D:/uulan-bin/terms-smoke.$(date +%s)"
mkdir -p "$S/one"
for b in uulan-server uulan-host uulan-client; do
  cp -f "$ROOT/target/release/$b.exe" "$S/" || { echo "COPY_FAIL $b"; exit 9; }
done

"$S/uulan-server.exe" --bind 127.0.0.1:21991 --state "$S/one/devices.json" >"$S/one/server.log" 2>&1 &
SRV=$!
for _ in $(seq 1 40); do grep -q "已就绪" "$S/one/server.log" 2>/dev/null && break; sleep 0.2; done

UULAN_CONFIG="$S/one/host.json" "$S/uulan-host.exe" --server 127.0.0.1:21991 \
  --capture "lavfi:testsrc2=size=320x240:rate=5" --name 冒烟 >"$S/one/host.log" 2>&1 &
HOST=$!
for _ in $(seq 1 60); do grep -q "已上线" "$S/one/host.log" 2>/dev/null && break; sleep 0.2; done

DEV=$(python -c "import json;print(json.load(open(r'$S/one/host.json'))['device_id'])")
python -c "import json,sys;sys.stdout.buffer.write(json.load(open(r'$S/one/host.json'))['password'].encode())" >"$S/one/pw"
echo "  设备 ID=$DEV（口令进 0600 文件，不打印）"
chmod 600 "$S/one/pw"

echo "== 1) 独立连接的 --terms（应当：明说没有 + RC=0）"
"$S/uulan-client.exe" --server 127.0.0.1:21991 --device "$DEV" --password-file "$S/one/pw" \
  --terms >"$S/one/t1.txt" 2>"$S/one/t1.err"; RC1=$?
echo "  RC=$RC1  输出: $(head -1 "$S/one/t1.txt")"

echo "== 2) --terms --json（应当 [] 且 RC=0）"
"$S/uulan-client.exe" --server 127.0.0.1:21991 --device "$DEV" --password-file "$S/one/pw" \
  --terms --json >"$S/one/t2.txt" 2>/dev/null; RC2=$?
echo "  RC=$RC2  输出: $(cat "$S/one/t2.txt")"

echo "== 3) --terms 与 --quality 同给（应当当场报错，不开视频通道）"
"$S/uulan-client.exe" --server 127.0.0.1:21991 --device "$DEV" --password-file "$S/one/pw" \
  --terms --quality fast >"$S/one/t3.txt" 2>&1; RC3=$?
echo "  RC=$RC3  尾行: $(tail -1 "$S/one/t3.txt")"

kill "$HOST" 2>/dev/null
kill "$SRV" 2>/dev/null
wait 2>/dev/null

OK=1
grep -q "远端当前没有终端会话" "$S/one/t1.txt" || { echo "[FAIL] 1 没明说「没有」"; OK=0; }
[ "$RC1" = 0 ] || { echo "[FAIL] 1 RC=$RC1"; OK=0; }
grep -q '^\[\]$' "$S/one/t2.txt" || { echo "[FAIL] 2 JSON 不是 []"; OK=0; }
[ "$RC2" = 0 ] || { echo "[FAIL] 2 RC=$RC2"; OK=0; }
[ "$RC3" != 0 ] || { echo "[FAIL] 3 竟然没报错"; OK=0; }
grep -q -- "--terms" "$S/one/t3.txt" || { echo "[FAIL] 3 报错文案没提 --terms"; OK=0; }
[ "$OK" = 1 ] && echo "SMOKE_OK 全部断言通过" || echo "SMOKE_BAD 有失败"
[ "$OK" = 1 ]
