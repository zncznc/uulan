#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""两机对表：persistent-ssh 往返采样，报「远端钟 − 本地钟」的偏移与不确定度。

原理：在一条常驻 `ssh host "while read; do <ts-cmd>; done"` 会话里逐轮喂一行、
收一个远端时间戳。单轮往返 rtt 只有毫秒级（不重握手），取 rtt 最小的那几轮的
中位数当偏移，rtt/2 当不确定度 —— 握手 300ms 的老办法（每次新 ssh）在这个
秒级判据面前不够用，这里把 ssh 建连的固定开销一次性摊掉。

用法：python clock-offset.py <ssh目标> [轮数=25]
输出：一行 JSON {offset_s, unc_s, samples, best_rtt_s, span_s}
退出码：0=拿到可用估计；2=环境给不出时间基（不确定度 > 0.15 s 或样本不足）。
"""
import json
import subprocess
import sys
import time

TARGET = sys.argv[1] if len(sys.argv) > 1 else "kylin"
ROUNDS = int(sys.argv[2]) if len(sys.argv) > 2 else 25
# 远端要一条「读一行、打一个纳秒级 UTC epoch」的极简循环；不经过 shell 变量展开。
REMOTE = "while read _; do date -u +%s.%N; done"


def main() -> int:
    try:
        p = subprocess.Popen(
            ["ssh", "-T", TARGET, REMOTE],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, bufsize=0,
        )
    except FileNotFoundError:
        print(json.dumps({"error": "ssh 不可用"}))
        return 2
    ests = []  # (rtt, offset)
    t_start = time.time()
    assert p.stdin is not None and p.stdout is not None  # Pyright: Popen(bufsize=0) 必给管道
    try:
        # 预热两轮：第一轮含远端 shell 起动态，rtt 虚高。
        for i in range(-2, ROUNDS):
            t0 = time.time()
            p.stdin.write(b"x\n")
            p.stdin.flush()
            line = p.stdout.readline().decode().strip()
            t1 = time.time()
            if i < 0 or not line:
                continue
            try:
                remote = float(line)
            except ValueError:
                continue
            rtt = t1 - t0
            ests.append((rtt, remote + rtt / 2 - (t0 + t1) / 2))
    finally:
        try:
            p.stdin.close()
            p.kill()
        except OSError:
            pass
    if len(ests) < 5:
        print(json.dumps({"error": "样本不足", "samples": len(ests)}))
        return 2
    ests.sort(key=lambda x: x[0])
    best = ests[: max(3, len(ests) // 4)]  # rtt 最小的 1/4（至少 3 个）
    offs = sorted(o for _, o in best)
    offset = offs[len(offs) // 2]
    # 不确定度：rtt/2 的离散 与 最好样本集内部的偏移极差 取大者。
    unc = max(best[0][0] / 2, (offs[-1] - offs[0]) / 2 + 0.002)
    out = {
        "offset_s": round(offset, 4),
        "unc_s": round(unc, 4),
        "samples": len(ests),
        "best_rtt_s": round(best[0][0], 4),
        "span_s": round(time.time() - t_start, 1),
    }
    print(json.dumps(out))
    return 0 if unc <= 0.15 else 2


if __name__ == "__main__":
    sys.exit(main())
