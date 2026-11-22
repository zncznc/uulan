"""判据器标定：延迟代理本身给 1 MiB 全双工往返加了多少时间？

不含 uulan 的任何东西：本机 echo 服务 ← 延迟代理 ← 探针。
如果这一步就和 uulan 那条链路同量级，那 wan 台架量到的数就是**代理的上限**，不是产品的上限。
"""
import json
import os
import socket
import subprocess
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = r"D:/default-workspace/uulan"


def echo_server(port):
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", port))
    s.listen(16)

    def h(c):
        try:
            while True:
                d = c.recv(65536)
                if not d:
                    break
                c.sendall(d)
        except Exception:
            pass
        finally:
            try:
                c.close()
            except Exception:
                pass

    def accept_loop():
        while True:
            c, _ = s.accept()
            threading.Thread(target=h, args=(c,), daemon=True).start()

    threading.Thread(target=accept_loop, daemon=True).start()
    return s


def probe(port, n, seed=1):
    data = bytes(((i * 37 + 11 + seed * 97) % 256) for i in range(n))
    t0 = time.time()
    s = socket.create_connection(("127.0.0.1", port), timeout=120)
    got = bytearray()

    def reader():
        while len(got) < n:
            b = s.recv(65536)
            if not b:
                break
            got.extend(b)

    t = threading.Thread(target=reader)
    t.start()
    for i in range(0, n, 65536):
        s.sendall(data[i:i + 65536])
    t.join(timeout=120)
    s.close()
    return {"ok": bytes(got) == data, "ms": int((time.time() - t0) * 1000)}


def main():
    base = 23500
    echo_server(base)
    out = {}
    for delay in (0, 25, 50, 100):
        port = base + 1 + delay // 25
        px = None
        if delay:
            px = subprocess.Popen(
                [sys.executable, os.path.join(ROOT, "scripts", "latency-proxy.py"),
                 str(port), "127.0.0.1:%d" % base, str(delay)],
                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
            line = px.stdout.readline()
            if "PROXY_READY" not in line:
                print("proxy failed:", line)
                return 1
            target = port
        else:
            target = base
        p1 = probe(target, 1048576, 1)
        p4 = probe(target, 4194304, 2)
        out[delay] = {"p1": p1, "p4": p4}
        print("delay=%3d ms  p1=%4d ms ok=%s   p4=%5d ms ok=%s" %
              (delay, p1["ms"], p1["ok"], p4["ms"], p4["ok"]))
        if px:
            px.terminate()
            try:
                px.wait(timeout=3)
            except Exception:
                px.kill()
    print(json.dumps(out))
    return 0


if __name__ == "__main__":
    sys.exit(main())
