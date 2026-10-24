#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""验「控制端本机终端尺寸变了，远端 PTY 跟着变」（Linux 侧，用真 pty 起真客户端）。

用法（在能跑客户端的机器上，客户端与 GUI 无关，麒麟/任意 Linux 都行）：

    python3 scripts/verify-term-size-follow.py \
        --bin ~/.local/uulan/bin/uulan-client \
        --server 192.168.223.15:21116 --device 805747189 \
        --password-file ~/.uulan-pw

它做两遍，缺一不可：

1. **跟随**：本地 pty 设 100x30 → 远端 `stty size` 应为 `30 100`；把本地 pty 改成 132x43，
   等 1.2 秒（客户端轮询间隔 300 ms）→ 远端应为 `43 132`。
2. **对照**（`--term-size 80x24` 钉死）：本地改成 132x43 后远端必须还是 `24 80`。

第 2 遍是第 1 遍的判据：没有它，「尺寸变了」也可能只是远端每次都报本机自己的尺寸。
退出码 0 = 两遍都按预期；非 0 = 有一遍不对（打印实际收到的行）。
"""
import argparse
import fcntl
import os
import pty
import select
import struct
import subprocess
import sys
import termios
import time


class Session:
    def __init__(self, args, extra, cols, rows, logpath):
        self.logpath = logpath
        self.log = open(logpath, "wb")
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        env = dict(os.environ, RUST_LOG="info", NO_COLOR="1")
        cmd = [args.bin, "--server", args.server, "--device", args.device,
               "--password-file", args.password_file, "--term"] + extra
        self.p = subprocess.Popen(cmd, stdin=slave, stdout=slave, stderr=self.log,
                                  env=env, close_fds=True)
        os.close(slave)
        self.buf = b""

    def read_some(self, timeout=1.0):
        r, _, _ = select.select([self.master], [], [], timeout)
        if not r:
            return b""
        try:
            chunk = os.read(self.master, 65536)
        except OSError:
            return b""
        self.buf += chunk
        return chunk

    def wait_for(self, needle, timeout=25.0):
        deadline = time.time() + timeout
        while time.time() < deadline:
            if needle.encode() in self.buf:
                return True
            self.read_some(0.3)
        return False

    def ask_size(self, want, timeout=25.0):
        """发一条 `stty size`，等远端回出期望值（先清空缓冲，别读到上一次的回显）。"""
        self.buf = b""
        os.write(self.master, b"stty size\n")
        return self.wait_for(want, timeout)

    def resize(self, cols, rows):
        fcntl.ioctl(self.master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

    def finish(self, timeout=15):
        os.write(self.master, b"exit 0\n")
        deadline = time.time() + timeout
        while time.time() < deadline:
            self.read_some(0.3)
            if self.p.poll() is not None:
                break
        if self.p.poll() is None:
            self.p.kill()
        self.log.close()
        log = open(self.logpath, "rb").read().decode("utf-8", "replace")
        return self.p.poll(), log.count("本机终端尺寸变了")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=os.path.expanduser("~/.local/uulan/bin/uulan-client"))
    ap.add_argument("--server", required=True)
    ap.add_argument("--device", required=True)
    ap.add_argument("--password-file", required=True)
    args = ap.parse_args()

    ok = True

    s = Session(args, [], 100, 30, "/tmp/uulan-size-follow.log")
    first = s.ask_size("30 100")
    s.resize(132, 43)
    time.sleep(1.2)
    second = s.ask_size("43 132")
    code, notices = s.finish()
    print("跟随 : 初始 30x100=%s  改 132x43 后=%s  日志通知=%d  退出码=%s"
          % (first, second, notices, code))
    ok = ok and first and second and notices >= 1 and code == 0

    s = Session(args, ["--term-size", "80x24"], 100, 30, "/tmp/uulan-size-fixed.log")
    first = s.ask_size("24 80")
    s.resize(132, 43)
    time.sleep(1.2)
    still = s.ask_size("24 80")
    code, notices = s.finish()
    print("钉死 : 初始 24x80=%s  本地改 132x43 后仍是 24x80=%s  日志通知=%d  退出码=%s"
          % (first, still, notices, code))
    ok = ok and first and still and notices == 0 and code == 0

    print("结果 :", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())