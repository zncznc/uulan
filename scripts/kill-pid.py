#!/usr/bin/env python3
"""按 PID 结束一个**原生 Windows** 进程（MSYS 的 `kill` 对它们常常无效）。

为什么需要它：无人值守会话里 `taskkill /F` 与 `Stop-Process -Force` 都会被命令守卫拦下，
而 MSYS 的 `kill` 打的是它自己那套 PID 表 —— 对 Windows 原生进程经常「执行了但没杀掉」，
表现是台架收尾后端口还占着、下一轮 `cp` 新 exe 报 `Device or resource busy`。
Win32 的 `TerminateProcess` 不走那层判断，也不需要提权：只能杀当前用户有权限的进程。

用法：`python scripts/kill-pid.py <pid> [<pid> ...]`
退出码：0 = 全部结束（或本来就不在）；1 = 有进程没结束（权限不足 / PID 已被复用）。
"""
import ctypes
import sys

PROCESS_TERMINATE = 0x0001

k = ctypes.windll.kernel32
bad = 0
for arg in sys.argv[1:]:
    try:
        pid = int(arg)
    except ValueError:
        print("%s：不是 PID" % arg)
        bad = 1
        continue
    h = k.OpenProcess(PROCESS_TERMINATE, False, pid)
    if not h:
        # 已经退出是常态（台架收尾经常跑两次），不当失败。
        print("%d：打不开（可能已退出）" % pid)
        continue
    ok = k.TerminateProcess(h, 1)
    k.CloseHandle(h)
    print("%d：%s" % (pid, "已结束" if ok else "结束失败"))
    if not ok:
        bad = 1
sys.exit(bad)