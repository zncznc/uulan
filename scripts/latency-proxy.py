#!/usr/bin/env python3
"""TCP 延迟代理：给「控制端 ↔ 中继」这一腿加固定单向延迟（模拟广域网）。

为什么不用 tc netem：真机 mis 没有免密 sudo，改不了 qdisc；而且这里要的是
「同一台机器上可重复的 RTT 档位」，用户态代理就够了 —— 协议字节盲转发，
加密在两端做，代理看不见内容也改不了语义。

用法：python latency-proxy.py <listen_port> <target_host:port> <delay_ms> [--chunk=16384]

实现要点：两个方向各自「读 → 入队（带到期时间）→ 定时写出」，
**不是**读完 sleep 完再读 —— 那样等于按 16 KiB/延迟 限速，量出来的不是 RTT 而是带宽。
"""
import asyncio
import sys
import time


async def pipe(reader: asyncio.StreamReader, writer: asyncio.StreamWriter, delay_s: float):
    q = asyncio.Queue()

    async def pump():
        while True:
            try:
                data = await reader.read(65536)
            except Exception:
                data = b""
            await q.put((time.monotonic() + delay_s, data if data else None))
            if not data:
                return

    async def send():
        while True:
            due, data = await q.get()
            now = time.monotonic()
            if due > now:
                await asyncio.sleep(due - now)
            if data is None:
                try:
                    writer.write_eof()
                except Exception:
                    pass
                return
            try:
                writer.write(data)
                await writer.drain()
            except Exception:
                return

    await asyncio.gather(pump(), send())


async def handle(cr: asyncio.StreamReader, cw: asyncio.StreamWriter, host: str, port: int, delay_s: float):
    try:
        sr, sw = await asyncio.open_connection(host, port)
    except Exception:
        cw.close()
        return
    await asyncio.gather(
        pipe(cr, sw, delay_s),
        pipe(sr, cw, delay_s),
        return_exceptions=True,
    )
    for w in (cw, sw):
        try:
            w.close()
        except Exception:
            pass


async def main():
    lp = int(sys.argv[1])
    host, _, port = sys.argv[2].rpartition(":")
    delay_s = float(sys.argv[3]) / 1000.0
    srv = await asyncio.start_server(
        lambda r, w: handle(r, w, host, int(port), delay_s), "127.0.0.1", lp
    )
    print("PROXY_READY %d -> %s:%s delay=%.0fms" % (lp, host, port, delay_s * 1000), flush=True)
    async with srv:
        await srv.serve_forever()


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
