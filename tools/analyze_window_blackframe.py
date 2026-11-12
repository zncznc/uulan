#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""tools/analyze_window_blackframe.py — 判读 win_window_capture.py 的 CSV

回答的问题（M2.5 遗留的最后一格）：**窗口在换代那一刻有没有黑屏一拍？**

方法不是「看一眼」，是两条独立的几何/时间判据：
 1. **活性**：`--fb` 缩到比窗口小的画幅后，客户区黑边占比必须**跳变**（fit_rect 只取整数倍、
    从不缩小 → 800x600 画面放进 ≥1280x720 窗口至少 48% 黑边）。黑边跟着变 = 抓到的确实是
    真窗口内容，抓取通路本身是活的。恢复原分辨率后黑边占比必须落回基线。
 2. **黑闪**：任何**严格位于文件内部**的「连续 ≥1 个采样点 black_frac ≥ max(0.90, 基线+0.25)」
    的区段（run），且前后邻点都比它低 0.25 以上 → 计为一次「瞬时全黑」。文件头部的
    run 不算 —— 那是「还没收到首帧」的深灰占位，不是换代黑屏。
采样间隔 20 ms：一拍全黑（≈33 ms，到下一帧才有新内容）必然落进至少一个采样点。

输出若干行 `RESULT <名>=<PASS|FAIL|ENV> <数据>`；退出码 0 全过 / 1 有 FAIL / 2 环境不成立
（抓不到内容时**必须**走 2，不许把环境问题报成功能回归）。
"""
import argparse
import json
import statistics
import sys


def load_csv(path):
    rows = []
    with open(path, encoding="utf-8") as f:
        for ln in f:
            ln = ln.strip()
            if not ln or ln.startswith("#"):
                continue
            t, b, m, fp = ln.split(",")
            rows.append((int(t), float(b), float(m), fp))
    return rows


def load_events(path):
    ev = {}
    with open(path, encoding="utf-8") as f:
        for ln in f:
            ln = ln.strip()
            if not ln:
                continue
            name, t = ln.split()
            ev[name] = int(t)
    return ev


def med(rows, i):
    return statistics.median([r[i] for r in rows]) if rows else None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--csv", required=True)
    ap.add_argument("--events", required=True)
    ap.add_argument("--settle", type=int, default=3000, help="换代后跳过多少毫秒再统计（探针 1s + 编码器重启）")
    a = ap.parse_args()

    rows = load_csv(a.csv)
    ev = load_events(a.events)
    if len(rows) < 60:
        print(f"RESULT samples=ENV 只有 {len(rows)} 个采样点，判不了")
        return 2
    if "shrink" not in ev or "restore" not in ev:
        print("RESULT events=ENV 缺 shrink/restore 事件时刻")
        return 2

    t0, t1 = rows[0][0], rows[-1][0]
    sh, rs = ev["shrink"], ev["restore"]
    A = [r for r in rows if t0 + 2000 <= r[0] <= sh - 2000]
    B = [r for r in rows if sh + a.settle <= r[0] <= rs - 2000]
    C = [r for r in rows if rs + a.settle <= r[0] <= t1 - 1000]
    medA, medB, medC = med(A, 1), med(B, 1), med(C, 1)
    print(f"PHASES nA={len(A)} nB={len(B)} nC={len(C)} "
          f"medA={medA} medB={medB} medC={medC} "
          f"span_s={(t1 - t0) / 1000:.1f} interval_ms="
          f"{(rows[1][0] - rows[0][0]) if len(rows) > 1 else 0}")

    rc = 0
    # —— 基线有效性：至少 30% 的 A 段样本平均亮度 ≥10，否则整条抓取可能全是黑的 ——
    if len(A) < 15 or medA is None:
        print("RESULT baseline=ENV A 段样本不足")
        return 2
    lit = sum(1 for r in A if r[2] >= 10) / len(A)
    if lit < 0.30:
        print(f"RESULT baseline=ENV A 段只有 {lit:.0%} 的样本亮度≥10（抓取通路大概率被挡：锁屏/无桌面）——"
              f"按环境不成立处理，不得据此判定回归")
        return 2
    print(f"RESULT baseline=PASS 亮度样本占比 {lit:.0%}")

    # —— 活性：B 段黑边占比必须显著高于 A 段 ——
    if medB is None or len(B) < 10:
        print("RESULT liveness=ENV B 段样本不足")
        rc = 2
    elif medB >= medA + 0.20:
        print(f"RESULT liveness=PASS 黑边 {medA:.3f} -> {medB:.3f}（跳变 ≥0.20，抓取与窗口都是活的）")
    else:
        print(f"RESULT liveness=FAIL 黑边 {medA:.3f} -> {medB:.3f} 没跳变（换代没反映到窗口？或窗口比 800x600 还小）")
        rc = 1

    # —— 恢复 ——
    if medC is None or len(C) < 10:
        print("RESULT restore=ENV C 段样本不足")
        rc = max(rc, 2)
    elif abs(medC - medA) <= 0.15:
        print(f"RESULT restore=PASS 黑边回到基线（{medC:.3f} vs {medA:.3f}）")
    else:
        print(f"RESULT restore=FAIL 恢复后黑边 {medC:.3f} 与基线 {medA:.3f} 差 >0.15")
        rc = 1

    # —— 黑闪 run 检测 ——
    # 两类「整窗近黑」必须分开：
    #  * 控制端空白（FAIL，本次要判的就是它）：统一 0x101010 铺满 —— 首帧前占位、
    #    或 blit 清屏后帧被跳过提前 return。签名：mean ≈ 16（≥13）。
    #  * 对面真黑（INFO）：真机桌面在 xrandr 换尺寸的瞬间自己黑了一帧，被如实直播过来。
    #    签名：黑边(16)+黑视频(0) 混合，mean 明显低于 13（本轮实测 7.54 = 0.4716*16，
    #    恰好是「黑边占比 × 16」——数学上只可能是「视频区全黑 + 黑边」，不可能是统一占位）。
    thr = max(0.90, medA + 0.25)
    runs = []
    i = 0
    n = len(rows)
    while i < n:
        if rows[i][1] >= thr:
            j = i
            while j + 1 < n and rows[j + 1][1] >= thr:
                j += 1
            interior = i > 0 and j < n - 1
            if interior:
                lo = min(r[1] for r in rows[i:j + 1])
                prev_ok = rows[i - 1][1] < lo - 0.25
                next_ok = rows[j + 1][1] < lo - 0.25
                if prev_ok and next_ok:
                    rmean = sum(r[2] for r in rows[i:j + 1]) / (j - i + 1)
                    runs.append((rows[i][0], rows[j][0], j - i + 1, round(rmean, 2)))
            i = j + 1
        else:
            i += 1
    client_blank = [r for r in runs if r[3] >= 13]
    host_black = [r for r in runs if r[3] < 13]
    if client_blank:
        rc = 1
    print(f"RESULT flash={'PASS' if not client_blank else 'FAIL'} 阈值 black≥{thr:.3f} "
          f"控制端空白 run={len(client_blank)} 明细(起,止,采样数,mean)={client_blank[:6]}")
    print(f"INFO host_black runs={len(host_black)} 明细(相对 shrink/restore 由外层换算)={host_black[:6]}")

    # —— 内容指纹：各阶段不同指纹数（只作参考数据打印）——
    fpA = len(set(r[3] for r in A))
    fpB = len(set(r[3] for r in B)) if B else 0
    print(f"INFO fingerprints nA={fpA} nB={fpB}")
    return rc


if __name__ == "__main__":
    sys.exit(main())
