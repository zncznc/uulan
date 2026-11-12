#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""tools/win_window_capture.py — 毫秒级抓取控制端窗口的客户区像素（Windows 侧，判据工具）

用途：`verify-window-blackframe-e2e.sh` 用它把「窗口在换代那一刻有没有黑屏一拍」
从「没人眼看过就仍算未验」变成机器可查的证据。打印走 `PrintWindow`（PW_CLIENTONLY ——
**不要**加 PW_RENDERFULLCONTENT，实测每次 280~770 ms，会把毫秒级采样打成 20 Hz 以下）
→ 32bpp top-down DIB → 网格采样，每行一条 CSV。

每行：`epoch_ms,black_frac,mean,fingerprint`
 - black_frac：三通道都 ≤24 的采样点占比。0x101010 的黑边（blit 的底色）与真黑都算进来 ——
   所以「黑边占比」本身就是画面在动、抓取通路是活的证明（分辨率换代 → 画幅变化 → 黑边跳变）。
 - mean：采样点平均亮度（0..255）。用来区分「抓到了东西」和「整条抓取是黑的」。
 - fingerprint：量化后的内容指纹（crc32），用来证明前后内容真的变了。

退出码：0 = 正常采完；2 = 找不到窗口（环境原因，不代表功能回归）。
"""
import argparse
import ctypes
import sys
import time
import zlib
from ctypes import wintypes

BI_RGB = 0


class BITMAPINFOHEADER(ctypes.Structure):
    _fields_ = [
        ("biSize", wintypes.DWORD),
        ("biWidth", wintypes.LONG),
        ("biHeight", wintypes.LONG),
        ("biPlanes", wintypes.WORD),
        ("biBitCount", wintypes.WORD),
        ("biCompression", wintypes.DWORD),
        ("biSizeImage", wintypes.DWORD),
        ("biXPelsPerMeter", wintypes.LONG),
        ("biYPelsPerMeter", wintypes.LONG),
        ("biClrUsed", wintypes.DWORD),
        ("biClrImportant", wintypes.DWORD),
    ]


class BITMAPINFO(ctypes.Structure):
    _fields_ = [("bmiHeader", BITMAPINFOHEADER), ("bmiColors", wintypes.DWORD * 3)]


class RECT(ctypes.Structure):
    _fields_ = [("left", wintypes.LONG), ("top", wintypes.LONG),
                ("right", wintypes.LONG), ("bottom", wintypes.LONG)]


def find_windows(user32, prefix):
    """返回所有标题以 prefix 开头的可见顶层窗口（hwnd, title）。"""
    WNDENUMPROC = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    hits = []

    def cb(hwnd, _):
        if not user32.IsWindowVisible(hwnd):
            return True
        n = user32.GetWindowTextLengthW(hwnd)
        if n == 0 or n > 1024:
            return True
        buf = ctypes.create_unicode_buffer(n + 1)
        user32.GetWindowTextW(hwnd, buf, n + 1)
        if buf.value.lower().startswith(prefix.lower()):
            hits.append((hwnd, buf.value))
        return True

    user32.EnumWindows(WNDENUMPROC(cb), 0)
    return hits


def sample_grid(bits, w, h, grid):
    """网格采样：返回 (black_frac, mean, fingerprint)。bits 支持整数下标（BGRA 字节）。
    注意：**不要**先把整块 DIB bytes() 拷出来 —— 3.7 MB 的 ctypes 数组转 bytes 每帧要 ~200 ms，
    会把 20 ms 采样拖成 260 ms（2026-11-13 实测）。"""
    sx = max(1, w // grid)
    sy = max(1, h // grid)
    black = 0
    total = 0
    lsum = 0
    q = bytearray()
    y = sy // 2
    while y < h:
        row = y * w * 4
        x = sx // 2
        while x < w:
            o = row + x * 4
            b = bits[o]
            g = bits[o + 1]
            r = bits[o + 2]
            if b <= 24 and g <= 24 and r <= 24:
                black += 1
            lsum += (b + g + r) // 3
            total += 1
            q.append((r >> 4) & 0x0F)
            q.append((g >> 4) & 0x0F)
            q.append((b >> 4) & 0x0F)
            x += sx
        y += sy
    if total == 0:
        return 1.0, 0.0, 0
    fp = zlib.crc32(bytes(q)) & 0xFFFFFFFF
    return black / total, lsum / total, fp


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--match", default="uulan", help="窗口标题前缀")
    ap.add_argument("--out", required=True, help="CSV 输出路径（原生 Windows 路径）")
    ap.add_argument("--seconds", type=float, default=30.0)
    ap.add_argument("--interval-ms", type=int, default=20)
    ap.add_argument("--grid", type=int, default=64)
    ap.add_argument("--find-timeout", type=float, default=20.0)
    a = ap.parse_args()

    try:
        ctypes.windll.shcore.SetProcessDpiAwareness(2)
    except Exception:
        pass
    user32 = ctypes.windll.user32
    gdi32 = ctypes.windll.gdi32

    hwnd = None
    title = ""
    deadline = time.time() + a.find_timeout
    while time.time() < deadline:
        hits = find_windows(user32, a.match)
        if len(hits) == 1:
            hwnd, title = hits[0]
            break
        if len(hits) > 1:
            print(f"AMBIGUOUS {len(hits)} 个匹配窗口：{[t for _, t in hits]}")
            return 2
        time.sleep(0.25)
    if hwnd is None:
        print("NOWINDOW 找不到标题匹配的控制端窗口（环境原因，不代表回归）")
        return 2

    # 建 DC / DIB（尺寸随窗口变化重建）
    screen_dc = user32.GetDC(0)
    memdc = gdi32.CreateCompatibleDC(screen_dc)
    bmi = BITMAPINFO()
    bmi.bmiHeader.biSize = ctypes.sizeof(BITMAPINFOHEADER)
    dib = None
    bits_addr = ctypes.c_void_p()
    buf = None
    w_cur = h_cur = 0

    def make_dib(w, h):
        nonlocal dib, buf, w_cur, h_cur, bmi
        if dib:
            # 删除旧 DIB 前必须先把别的位图选出去，否则 DeleteObject 失败
            dummy = gdi32.CreateCompatibleBitmap(memdc, 1, 1)
            gdi32.SelectObject(memdc, dummy)
            gdi32.DeleteObject(dib)
            gdi32.DeleteObject(dummy)
        bmi.bmiHeader.biWidth = w
        bmi.bmiHeader.biHeight = -h  # top-down
        bmi.bmiHeader.biPlanes = 1
        bmi.bmiHeader.biBitCount = 32
        bmi.bmiHeader.biCompression = BI_RGB
        bpp = ctypes.c_void_p()
        dib = gdi32.CreateDIBSection(memdc, ctypes.byref(bmi), 0, ctypes.byref(bpp), None, 0)
        if not dib or not bpp.value:
            raise RuntimeError("CreateDIBSection 失败（抓不了位图，环境原因）")
        gdi32.SelectObject(memdc, dib)
        size = w * h * 4
        buf = (ctypes.c_ubyte * size).from_address(bpp.value)
        w_cur, h_cur = w, h

    rc = RECT()
    t_end = time.time() + a.seconds
    n = 0
    pw_fail = 0
    interval = a.interval_ms / 1000.0
    tick = time.time()
    with open(a.out, "w", encoding="utf-8") as f:
        f.write("# epoch_ms,black_frac,mean,fingerprint\n")
        while time.time() < t_end:
            if not user32.IsWindow(hwnd):
                print(f"WINDOW_GONE at n={n}")
                break
            user32.GetClientRect(hwnd, ctypes.byref(rc))
            w = rc.right - rc.left
            h = rc.bottom - rc.top
            if w <= 0 or h <= 0:
                tick += interval
                time.sleep(max(0, tick - time.time()))
                continue
            if dib is None or w != w_cur or h != h_cur:
                make_dib(w, h)
            # PW_CLIENTONLY(1)。实测：带 PW_RENDERFULLCONTENT(2) 会走 DWM 重定向拷贝，
            # 每帧 280~770 ms（2026-11-13 pwtest 对照），softbuffer 是 GDI 窗口，flag 1 就够，2 ms/帧。
            if not user32.PrintWindow(hwnd, memdc, 1):
                pw_fail += 1
            black_frac, mean, fp = sample_grid(buf, w, h, a.grid)
            f.write(f"{int(time.time() * 1000)},{black_frac:.4f},{mean:.2f},{fp}\n")
            n += 1
            tick += interval
            time.sleep(max(0, tick - time.time()))
    if dib:
        gdi32.DeleteObject(dib)
    gdi32.DeleteDC(memdc)
    user32.ReleaseDC(0, screen_dc)
    print(f"CAPTURED n={n} pw_fail={pw_fail} hwnd_title={title}")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as e:  # GDI/环境问题一律按「这台环境验不了」收（exit 2），不许报成功能回归
        print(f"CAPFAIL {e!r}")
        sys.exit(2)
