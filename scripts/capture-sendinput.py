#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""SendInput 相对鼠标移动注入器 —— 仅验收台使用（Windows）。

夜批没有手在动鼠标，而捕获模式的输入源头**就是**操作系统的原始输入流。
SendInput(MOUSEEVENTF_MOVE) 注入的相对移动与硬件同源进 raw input，winit 的
DeviceEvent::MouseMotion 会收到它（winit 不按 LLMHF_INJECTED 过滤）。
所以这条链验的是「raw input → DeviceEvent → MouseMoveRel → 被控端 XTest」
整条路；「硬件 vs 注入」的差异不在本判据范围，如实写进归档。

用法: python capture-sendinput.py DX DY [TIMES] [SLEEP_S]
退出码: 0 = 全部注入成功；1 = SendInput 有返回值不对。
"""
import ctypes
import sys
import time

ULONG_PTR = ctypes.c_ulonglong if ctypes.sizeof(ctypes.c_void_p) == 8 else ctypes.c_ulong
MOUSEEVENTF_MOVE = 0x0001
INPUT_MOUSE = 0


class MOUSEINPUT(ctypes.Structure):
    _fields_ = [
        ("dx", ctypes.c_long),
        ("dy", ctypes.c_long),
        ("mouseData", ctypes.c_ulong),
        ("dwFlags", ctypes.c_ulong),
        ("time", ctypes.c_ulong),
        ("dwExtraInfo", ULONG_PTR),
    ]


class INPUT(ctypes.Structure):
    class _U(ctypes.Union):
        # pad 必须与 MOUSEINPUT 同宽（32）：撑大了 cbSize 就是 ERROR_INVALID_SIZE(87)，
        # SendInput 直接返回 0。
        _fields_ = [("mi", MOUSEINPUT), ("pad", ctypes.c_byte * 32)]

    _anonymous_ = ("u",)
    _fields_ = [("type", ctypes.c_ulong), ("u", _U)]


def main() -> int:
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    dx, dy = int(sys.argv[1]), int(sys.argv[2])
    times = int(sys.argv[3]) if len(sys.argv) > 3 else 1
    sleep_s = float(sys.argv[4]) if len(sys.argv) > 4 else 0.25
    inp = INPUT(type=INPUT_MOUSE)
    inp.mi = MOUSEINPUT(
        dx=dx, dy=dy, mouseData=0, dwFlags=MOUSEEVENTF_MOVE, time=0, dwExtraInfo=0
    )
    sent = 0
    for _ in range(times):
        r = ctypes.windll.user32.SendInput(1, ctypes.byref(inp), ctypes.sizeof(INPUT))
        if r != 1:
            print(f"SendInput 返回 {r}（期望 1），已发出 {sent}/{times}")
            return 1
        sent += 1
        time.sleep(sleep_s)
    print(f"injected {sent} x ({dx},{dy})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
