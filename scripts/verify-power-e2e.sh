#!/usr/bin/env bash
# --power 真 exe 冒烟（本机验收台，不碰真电源）：
#   闸门在 spawn 之前 / 放行后真执行 / 退出码与 stderr 回到线上 /
#   宽限期外回「仍在执行」/ --json 单行 / 参数闸门报错。
# 假执行器 = target/release/examples/fake_power.exe（lock→0，shutdown→3，logout→睡15s）。
set -u
ROOT="D:/default-workspace/uulan"
S="D:/uulan-bin/power-smoke.$(date +%s)"
mkdir -p "$S/one" "$S/two"
for b in uulan-server uulan-host uulan-client; do
  cp -f "$ROOT/target/release/$b.exe" "$S/" || { echo "COPY_FAIL $b"; exit 9; }
done
cp -f "$ROOT/target/release/examples/fake_power.exe" "$S/" || { echo "COPY_FAIL fake_power（先 cargo build --release --examples -p uulan-host）"; exit 9; }

strip() { sed -E 's/\x1b\[[0-9;]*m//g'; }   # tracing 落盘仍上色，grep 前必去 ANSI（坑 5）

"$S/uulan-server.exe" --bind 127.0.0.1:21993 --state "$S/one/devices.json" >"$S/one/server.log" 2>&1 &
SRV=$!
for _ in $(seq 1 40); do grep -q "已就绪" "$S/one/server.log" 2>/dev/null && break; sleep 0.2; done

start_host() { # $1 = 目录名
  UULAN_CONFIG="$S/$1/host.json" "$S/uulan-host.exe" --server 127.0.0.1:21993 \
    --capture "lavfi:testsrc2=size=320x240:rate=5" --name 冒烟 >"$S/$1/host.log" 2>&1 &
  HP=$!
  for _ in $(seq 1 60); do grep -q "已上线" "$S/$1/host.log" 2>/dev/null && break; sleep 0.2; done
}

read_id_pw() { # $1 = 目录名
  DEV=$(python -c "import json;print(json.load(open(r'$S/$1/host.json'))['device_id'])")
  python -c "import json,sys;sys.stdout.buffer.write(json.load(open(r'$S/$1/host.json'))['password'].encode())" >"$S/$1/pw"
  chmod 600 "$S/$1/pw"
}

echo "== A) 默认配置（power_allow 不写 = 全拒）"
start_host one
read_id_pw one

"$S/uulan-client.exe" --server 127.0.0.1:21993 --device "$DEV" --password-file "$S/one/pw" \
  --power lock >"$S/one/a1.txt" 2>&1; RC1=$?
echo "  A1 RC=$RC1  首行: $(head -1 "$S/one/a1.txt")"

"$S/uulan-client.exe" --server 127.0.0.1:21993 --device "$DEV" --password-file "$S/one/pw" \
  --power lock --json >"$S/one/a2.txt" 2>&1; RC2=$?
echo "  A2 RC=$RC2  输出: $(cat "$S/one/a2.txt")"

"$S/uulan-client.exe" --server 127.0.0.1:21993 --device "$DEV" --password-file "$S/one/pw" \
  --power poweer >"$S/one/a3.txt" 2>&1; RC3=$?
echo "  A3 RC=$RC3  尾行: $(tail -1 "$S/one/a3.txt")"

"$S/uulan-client.exe" --server 127.0.0.1:21993 --device "$DEV" --password-file "$S/one/pw" \
  --power lock --quality fast >"$S/one/a4.txt" 2>&1; RC4=$?
echo "  A4 RC=$RC4  尾行: $(tail -1 "$S/one/a4.txt")"

kill "$HP" 2>/dev/null; wait "$HP" 2>/dev/null

echo "== B) 放行 lock/shutdown/logout + 假执行器"
# 预写配置：load_or_create 会补齐 device_id/口令（与生产同一条加载路径）。
printf '{"power_allow":["lock","shutdown","logout"],"power_test_program":"%s"}' "$S/fake_power.exe" >"$S/two/host.json"
start_host two
read_id_pw two

"$S/uulan-client.exe" --server 127.0.0.1:21993 --device "$DEV" --password-file "$S/two/pw" \
  --power lock >"$S/two/b1.txt" 2>&1; RC5=$?
echo "  B1(lock, 放行+退出0) RC=$RC5  $(head -1 "$S/two/b1.txt")"

"$S/uulan-client.exe" --server 127.0.0.1:21993 --device "$DEV" --password-file "$S/two/pw" \
  --power reboot >"$S/two/b2.txt" 2>&1; RC6=$?
echo "  B2(reboot, 名单外) RC=$RC6  $(head -1 "$S/two/b2.txt")"

"$S/uulan-client.exe" --server 127.0.0.1:21993 --device "$DEV" --password-file "$S/two/pw" \
  --power shutdown >"$S/two/b3.txt" 2>&1; RC7=$?
echo "  B3(shutdown, 假程序退3) RC=$RC7  $(sed -n 2p "$S/two/b3.txt")"

T0=$(date +%s)
"$S/uulan-client.exe" --server 127.0.0.1:21993 --device "$DEV" --password-file "$S/two/pw" \
  --power logout >"$S/two/b4.txt" 2>&1; RC8=$?
T1=$(date +%s)
echo "  B4(logout, 假程序睡15s) RC=$RC8 用时 $((T1-T0))s  $(sed -n 2p "$S/two/b4.txt")"

"$S/uulan-client.exe" --server 127.0.0.1:21993 --device "$DEV" --password-file "$S/two/pw" \
  --power lock --json >"$S/two/b5.txt" 2>&1; RC9=$?
echo "  B5(--json) RC=$RC9  $(cat "$S/two/b5.txt")"

kill "$HP" 2>/dev/null
kill "$SRV" 2>/dev/null; wait 2>/dev/null

echo "== 断言"
OK=1
[ "$RC1" != 0 ] || { echo "[FAIL] A1 未拒绝"; OK=0; }
grep -q "未执行" "$S/one/a1.txt" || { echo "[FAIL] A1 没说「未执行」"; OK=0; }
grep -q "power_allow" "$S/one/a1.txt" || { echo "[FAIL] A1 拒绝理由没指向配置"; OK=0; }
[ "$RC2" != 0 ] || { echo "[FAIL] A2 未拒绝"; OK=0; }
grep -q '"ok":false' "$S/one/a2.txt" || { echo "[FAIL] A2 JSON 没出 ok:false"; OK=0; }
[ "$RC3" != 0 ] || { echo "[FAIL] A3 错词没报错"; OK=0; }
grep -q "lock | logout | reboot | shutdown" "$S/one/a3.txt" || { echo "[FAIL] A3 没列有效值"; OK=0; }
[ "$RC4" != 0 ] || { echo "[FAIL] A4 --power+--quality 没报错"; OK=0; }
grep -q -- "--power" "$S/one/a4.txt" || { echo "[FAIL] A4 报错文案没提 --power"; OK=0; }
# 默认配置那一台：host.log 里绝不能出现「执行电源动作」（闸门在 spawn 之前）。
if strip <"$S/one/host.log" | grep -q "执行电源动作"; then echo "[FAIL] A 台竟然执行了"; OK=0; fi
[ "$RC5" = 0 ] || { echo "[FAIL] B1 未采纳"; OK=0; }
grep -q "已执行" "$S/two/b1.txt" || { echo "[FAIL] B1 没报「已执行」"; OK=0; }
[ "$RC6" != 0 ] || { echo "[FAIL] B2 名单外竟然采纳"; OK=0; }
grep -q "不在放行名单" "$S/two/b2.txt" || { echo "[FAIL] B2 拒绝理由不对"; OK=0; }
grep -q "lock" "$S/two/b2.txt" || { echo "[FAIL] B2 没列出当前名单"; OK=0; }
[ "$RC7" != 0 ] || { echo "[FAIL] B3 非零退出竟然采纳"; OK=0; }
grep -q "退出码 3" "$S/two/b3.txt" || { echo "[FAIL] B3 退出码没回到线上"; OK=0; }
grep -q "模拟拒绝切断电源" "$S/two/b3.txt" || { echo "[FAIL] B3 stderr 没带回来"; OK=0; }
[ "$RC8" = 0 ] || { echo "[FAIL] B4 超宽限期应算采纳"; OK=0; }
grep -q "仍在执行" "$S/two/b4.txt" || { echo "[FAIL] B4 没说「仍在执行」"; OK=0; }
[ $((T1-T0)) -le 9 ] || { echo "[FAIL] B4 没在宽限期口径内回话（$((T1-T0))s）"; OK=0; }
[ "$RC9" = 0 ] || { echo "[FAIL] B5 未采纳"; OK=0; }
grep -q '"ok":true' "$S/two/b5.txt" || { echo "[FAIL] B5 JSON 没出 ok:true"; OK=0; }
# 执行日志计数：恰好 4 次真执行（lock、shutdown、logout、b5 的 lock）；B2 被拒没执行。
N=$(strip <"$S/two/host.log" | grep -c "执行电源动作")
[ "$N" = 4 ] || { echo "[FAIL] 执行次数=$N（应为 4：lock/shutdown/logout/lock，B2 不该执行）"; OK=0; }
[ "$OK" = 1 ] && echo "POWER_SMOKE_OK 全部断言通过" || echo "POWER_SMOKE_BAD 有失败"
[ "$OK" = 1 ]
