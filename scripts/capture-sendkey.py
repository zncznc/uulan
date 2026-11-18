#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""SendInput 键盘注入器 + 窗口置前 —— 仅验收台使用（Windows）。

为什么需要它：捕获模式的 F8 是**本地开关**，夜里没有手去按。SendInput 注入的按键
与硬件同源进消息队列（winit 收 KeyEvent），所以能验「真键盘路径」；「物理键 vs 注入键」
的差异不在本判据范围，如实写进归档（与 capture-sendinput.py 同一条口径）。

用法:
  python capture-sendkey.py --focus <窗口标题子串>        # 只把窗口置前
  python capture-sendkey.py <VK十进制或0x十六进制> [--focus <子串>]
退出码: 0 = 注入成功；1 = SendInput/置前失败。
"""
import ctypes
import ctypes.wintypes as wintypes
import sys

ULONG_PTR = ctypes.c_ulonglong if ctypes.sizeof(ctypes.c_void_p) == 8 else ctypes.c_ulong
INPUT_KEYBOARD = 1
KEYEVENTF_KEYUP = 0x0002


class KEYBDINPUT(ctypes.Structure):
    _fields_ = [
        ("wVk", ctypes.c_ushort),
        ("wScan", ctypes.c_ushort),
        ("dwFlags", ctypes.c_ulong),
        ("time", ctypes.c_ulong),
        ("dwExtraInfo", ULONG_PTR),
    ]


class _INPUTunion(ctypes.Union):
    _fields_ = [("ki", KEYBDINPUT), ("padding", ctypes.c_byte * 32)]


class INPUT(ctypes.Structure):
    _fields_ = [("type", ctypes.c_ulong), ("u", _INPUTunion)]


def _send_vk(vk: int, down: bool) -> int:
    """单边按键注入（ALT 技巧和正文都用它）。"""
    user32 = ctypes.windll.user32
    arr = (INPUT * 1)()
    arr[0].type = INPUT_KEYBOARD
    arr[0].u.ki = KEYBDINPUT(wVk=vk, wScan=0, dwFlags=0 if down else KEYEVENTF_KEYUP,
                             time=0, dwExtraInfo=0)
    return user32.SendInput(1, ctypes.byref(arr), ctypes.sizeof(INPUT))


def focus(title_sub: str) -> int:
    """把标题包含 title_sub 的第一个可见顶层窗口置前。"""
    user32 = ctypes.windll.user32
    kernel32 = ctypes.windll.kernel32
    found = []

    @ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    def cb(hwnd, _):
        if not user32.IsWindowVisible(hwnd):
            return True
        n = user32.GetWindowTextLengthW(hwnd)
        if n <= 0:
            return True
        buf = ctypes.create_unicode_buffer(n + 1)
        user32.GetWindowTextW(hwnd, buf, n + 1)
        if title_sub.lower() in buf.value.lower():
            found.append((hwnd, buf.value))
            return False
        return True

    user32.EnumWindows(cb, 0)
    if not found:
        print(f"FOCUS-ERR 找不到标题含 {title_sub!r} 的窗口")
        return 1
    hwnd, title = found[0]
    user32.ShowWindow(hwnd, 9)          # SW_RESTORE
    ok = bool(user32.SetForegroundWindow(hwnd))
    # 后台进程默认抢不到前台（Windows 的前台锁）。三条兜底，按可靠度排：
    #  ① ALT 技巧：注入一次 ALT 按下/抬起，系统会把前台锁松一档 —— 这是最有效的一条，
    #     实测比 AttachThreadInput 硬；
    #  ② AttachThreadInput：把本线程挂到当前前台窗口的输入队列上，借它的资格再设一次；
    #  ③ SwitchToThisWindow：老 API，最后手段。
    if not ok:
        _send_vk(0x12, down=True)        # VK_MENU
        ok = bool(user32.SetForegroundWindow(hwnd))
        _send_vk(0x12, down=False)
    if not ok:
        fg = user32.GetForegroundWindow()
        tid_fg = user32.GetWindowThreadProcessId(fg, None)
        tid_me = kernel32.GetCurrentThreadId()
        if tid_fg and tid_fg != tid_me:
            if user32.AttachThreadInput(tid_me, tid_fg, True):
                user32.SetForegroundWindow(hwnd)
                user32.AttachThreadInput(tid_me, tid_fg, False)
    if not ok:
        user32.SwitchToThisWindow(hwnd, True)
    ok = user32.GetForegroundWindow() == hwnd
    print(f"FOCUS {'OK' if ok else 'ERR'} hwnd={hwnd} title={title!r}")
    return 0 if ok else 1


def send_vk(vk: int) -> int:
    got = _send_vk(vk, down=True)
    got += _send_vk(vk, down=False)
    print(f"KEY VK=0x{vk:02X} sent={got}/2")
    return 0 if got == 2 else 1


def main() -> int:
    args = sys.argv[1:]
    title = None
    if "--focus" in args:
        i = args.index("--focus")
        title = args[i + 1]
        del args[i:i + 2]
    rc = 0
    if title is not None:
        rc |= focus(title)
    if args:
        vk = int(args[0], 0)
        rc |= send_vk(vk)
    if not args and title is None:
        print("用法: capture-sendkey.py [VK] [--focus <标题子串>]")
        return 1
    return rc


if __name__ == "__main__":
    sys.exit(main())