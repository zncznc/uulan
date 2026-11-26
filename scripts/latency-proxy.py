#!/usr/bin/env python3
"""TCP 广域网损伤代理：给「控制端 ↔ 中继」或「被控端 ↔ 中继」加单向延迟、抖动、限速与重传停顿。

为什么不用 tc netem：真机 mis 没有免密 sudo，改不了 qdisc；而且这里要的是
「同一台机器上可重复的广域网档位」，用户态代理就够了 —— 协议字节盲转发，
加密在两端做，代理看不见内容也改不了语义。

用法（完全向后兼容旧位置参数）：
  python latency-proxy.py <listen_port> <target_host:port> <delay_ms> \
      [--jitter-ms 0] [--rate-kbps 0] [--stall-every-s 0] [--stall-ms 0] [--seed 42]

实现要点：
- 两个方向各自「读 → 入队（带到期时间）→ 定时写出」，不是读完 sleep 完再读。
- 抖动保序：due = max(last_due, now + delay + uniform(0, jitter))，符合 TCP 字节流保序。
- 限速：单向按 rate_kbps 节拍流出（rate_kbps=0 不限速）。
- 停顿：每 stall_every_s 秒模拟一次持续 stall_ms 的 TCP 丢包重传队头阻塞。
"""
import argparse
import asyncio
import random
import time


async def pipe(
    reader: asyncio.StreamReader,
    writer: asyncio.StreamWriter,
    delay_s: float,
    jitter_s: float,
    rate_bps: float,
    stall_every_s: float,
    stall_ms: float,
    rng: random.Random,
):
    q: asyncio.Queue = asyncio.Queue()

    async def pump():
        last_due = 0.0
        while True:
            try:
                data = await reader.read(16384 if rate_bps > 0 else 65536)
            except Exception:
                data = b""
            now = time.monotonic()
            extra = rng.uniform(0.0, jitter_s) if jitter_s > 0 else 0.0
            due = now + delay_s + extra
            if due < last_due:
                due = last_due
            last_due = due
            await q.put((due, data if data else None))
            if not data:
                return

    async def send():
        next_rate_t = 0.0
        next_stall_t = (time.monotonic() + stall_every_s) if (stall_every_s > 0 and stall_ms > 0) else 0.0
        while True:
            due, data = await q.get()
            now = time.monotonic()
            wait_until = due
            if rate_bps > 0 and next_rate_t > wait_until:
                wait_until = next_rate_t
            if next_stall_t > 0 and wait_until >= next_stall_t:
                stall_end = next_stall_t + stall_ms / 1000.0
                if stall_end > wait_until:
                    wait_until = stall_end
                next_stall_t = stall_end + stall_every_s
            if wait_until > now:
                await asyncio.sleep(wait_until - now)
            if data is None:
                try:
                    writer.write_eof()
                except Exception:
                    pass
                return
            if rate_bps > 0:
                tx_s = (len(data) * 8.0) / rate_bps
                next_rate_t = max(time.monotonic(), wait_until) + tx_s
            try:
                writer.write(data)
                await writer.drain()
            except Exception:
                return

    await asyncio.gather(pump(), send())


async def handle(
    cr: asyncio.StreamReader,
    cw: asyncio.StreamWriter,
    host: str,
    port: int,
    args,
):
    try:
        sr, sw = await asyncio.open_connection(host, port)
    except Exception:
        cw.close()
        return
    rate_bps = args.rate_kbps * 1000.0 if args.rate_kbps > 0 else 0.0
    rng_up = random.Random(args.seed)
    rng_dn = random.Random(args.seed + 1)
    await asyncio.gather(
        pipe(cr, sw, args.delay_ms / 1000.0, args.jitter_ms / 1000.0, rate_bps, args.stall_every_s, args.stall_ms, rng_up),
        pipe(sr, cw, args.delay_ms / 1000.0, args.jitter_ms / 1000.0, rate_bps, args.stall_every_s, args.stall_ms, rng_dn),
        return_exceptions=True,
    )
    for w in (cw, sw):
        try:
            w.close()
        except Exception:
            pass


async def main():
    p = argparse.ArgumentParser()
    p.add_argument("listen_port", type=int)
    p.add_argument("target")
    p.add_argument("delay_ms", type=float)
    p.add_argument("--jitter-ms", type=float, default=0.0)
    p.add_argument("--rate-kbps", type=float, default=0.0)
    p.add_argument("--stall-every-s", type=float, default=0.0)
    p.add_argument("--stall-ms", type=float, default=0.0)
    p.add_argument("--seed", type=int, default=42)
    args = p.parse_args()
    host, _, port = args.target.rpartition(":")
    srv = await asyncio.start_server(
        lambda r, w: handle(r, w, host, int(port), args), "127.0.0.1", args.listen_port
    )
    print(
        "PROXY_READY %d -> %s:%s delay=%.0fms jitter=%.0fms rate=%.0fkbps stall=%.0fms/%.1fs"
        % (args.listen_port, host, port, args.delay_ms, args.jitter_ms, args.rate_kbps, args.stall_ms, args.stall_every_s),
        flush=True,
    )
    async with srv:
        await srv.serve_forever()


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
