#!/usr/bin/env python3
"""协议点名：把 C2H / H2C 每一条消息的**生产构造点**与**生产 match 臂**各数一遍。

为什么口径是「构造点存在不存在」而不是「match 在不在」：11-16 那次按 match 点名，
恰好漏掉 `MouseMoveRel` 那种「接收半边早就写好了、发送半边没人写」的消息 ——
match 表看起来是全的，链子却是断的。只有构造点能证明「真有人发」。

用法：python scripts/rollcall-protocol.py（在本仓库任意位置跑都行，只读源码）
退出码恒 0；看输出里带 `<==` 的行：**没有构造点 = 死消息**（要么接线要么裁决删除），
**没有 match 臂 = 发出去没人收**（对面会掉进兜底分支）。

⚠ 元组变体（`C2H::Fs(..)`、`H2C::HelloOk(..)` 这种）也是构造点。
  第一版正则只认 `Variant {`，把 7 条元组变体全报成「没人发」——差点又造出一批假死消息。
"""
import os
import re

ROOT = r"D:/default-workspace/uulan/crates"
proto = open(os.path.join(ROOT, "uulan-protocol/src/msg.rs"), encoding="utf-8").read()

def variants(name):
    m = re.search(r"pub enum %s \{(.*?)\n\}" % name, proto, re.S)
    body = m.group(1)
    out, seen = [], set()
    for line in body.splitlines():
        line = line.strip()
        if line.startswith("//") or line.startswith("#"):
            continue
        mm = re.match(r"([A-Z][A-Za-z0-9]*)\s*[\{(,]", line)
        if mm and mm.group(1) not in seen:
            seen.add(mm.group(1))
            out.append(mm.group(1))
    return out

files = []
for c in sorted(os.listdir(ROOT)):
    d = os.path.join(ROOT, c, "src")
    if not os.path.isdir(d):
        continue
    for dirpath, _, names in os.walk(d):
        for nm in names:
            if nm.endswith(".rs"):
                files.append(os.path.join(dirpath, nm))
blobs = [(p, open(p, encoding="utf-8", errors="replace").read()) for p in files]

def is_test(p):
    q = p.replace("\\", "/")
    return "/tests/" in q or "/examples/" in q or "uulan-protocol/" in q

for kind in ("C2H", "H2C"):
    print("\n==== %s ====" % kind)
    for v in variants(kind):
        ctor = re.compile(r"\b%s::%s\s*[\({]" % (kind, v))
        bare = re.compile(r"\b%s::%s\s*[,;)]" % (kind, v))
        arm = re.compile(r"%s::%s\b[^\n]*=>" % (kind, v))
        prod_c, prod_a, test_c = [], [], 0
        for p, txt in blobs:
            lines = txt.splitlines()
            for pat in (ctor, bare):
                for m in pat.finditer(txt):
                    ln = txt[:m.start()].count("\n")
                    seg = lines[ln]
                    if "=>" in seg:
                        continue
                    if is_test(p):
                        test_c += 1
                    else:
                        prod_c.append("%s:%d" % (os.path.relpath(p, ROOT).replace("\\", "/"), ln + 1))
            for m in arm.finditer(txt):
                ln = txt[:m.start()].count("\n")
                if not is_test(p):
                    prod_a.append("%s:%d" % (os.path.relpath(p, ROOT).replace("\\", "/"), ln + 1))
        flag = ""
        if not prod_c and not test_c:
            flag = "   <== 全仓库没有构造点（真·没人发）"
        elif not prod_c:
            flag = "   <== 只有测试里的构造点"
        elif not prod_a:
            flag = "   <== 没有生产 match 臂（没人收）"
        print("  %-14s 构造 %-3d %-46s match %-2d %s%s" % (
            v, len(set(prod_c)), ",".join(sorted(set(prod_c))[:2]) or "-",
            len(set(prod_a)), ",".join(sorted(set(prod_a))[:1]) or "-", flag))
