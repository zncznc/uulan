#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""局域网在线审计 —— 判「那台机器是断电了，还是被 DHCP 挪走了」。

为什么需要它
------------
被控端失联时，这台开发机上的 `connect` / `ping` **都不可信**：本机开着透明代理
（v2rayN + sing-box 的 TUN，strict_route）时，会对**整个内网段的 TCP 发假应答**——
连一个不存在的地址也能「连上、无 banner」。那种情况下整条扫描都是摆设，
在假象上重试或者据此改代码，能烧掉半天。所以本脚本第一件事不是扫，是**自证扫描可信**。

判定的三层真相（顺序不能换）
--------------------------
1. 控制探针：对一个**确定不存在**的地址做裸 TCP connect。连得上 = 代理在劫持
   → 本次端口扫描作废（脚本会直接说「扫描不可信」并停止下结论）。
2. 二层真相：`netsh -c i i show neighbors <ifIndex>`。带真实 MAC 的行 = 二层活人，
   不受代理影响。**ARP 里缺席只能排除「还在本子网」，不能排除「还在线路上」**——
   它要是被 DHCP 分到别的子网，就不会应答本段的广播。
3. 机型特征端口：135/445=Windows、631=CUPS/打印、3389=有 RDP 的办公机、
   22 上的 SSH banner 能进一步分开 Kylin / Ubuntu。

⚠ 刻意**不用 ICMP TTL 判系统**：实测这套办公网里有开着 3389 的 Windows 办公机
   回 TTL=64（Linux 的默认值）。TTL 在这里不是指纹。

剩下两条路（脚本给不出结论时）：路由器上的 DHCP 租约表（要管理页/口令），
或者人到机器跟前看电源灯与网口灯。

用法
----
    python scripts/lan-presence-audit.py                      # 默认 192.168.223.0/24，找 .15
    python scripts/lan-presence-audit.py --prefix 192.168.223 --target 192.168.223.15
    python scripts/lan-presence-audit.py --ports 22,21116 --no-arp
退出码：0 = 目标在线；1 = 目标不在本子网（二层无它）；2 = 扫描不可信（代理劫持）。
"""

from __future__ import annotations

import argparse
import ipaddress
import re
import socket
import subprocess
import sys
import threading
import queue

# 特征端口 → 判读。宁可少说，不要瞎猜：只列这套网里实测有意义的。
FEATURE_PORTS = {
    22: "SSH",
    135: "Windows RPC",
    445: "SMB（Windows 办公机）",
    631: "CUPS（打印/类 Unix）",
    3389: "RDP（办公机）",
    21116: "uulan 中继",
}


def run(cmd: str, timeout: int = 25) -> str:
    """跑一条 shell 并拿 stdout。Windows 上是原生命令，别喂 MSYS 路径。"""
    try:
        p = subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=timeout,
                           encoding="utf-8", errors="replace")
        return (p.stdout or "") + (p.stderr or "")
    except Exception as e:  # 环境再怪也不该让审计本身崩掉
        return f"__ERR__ {e}"


def control_probe(probe_ip: str, port: int = 22, timeout: float = 2.5) -> bool:
    """对一个确定不存在的地址 connect。返回 True = 连上了 = 有人在发假应答。"""
    s = socket.socket()
    s.settimeout(timeout)
    try:
        s.connect((probe_ip, port))
        return True
    except Exception:
        return False
    finally:
        try:
            s.close()
        except Exception:
            pass


def physical_ifindex() -> list[tuple[int, str]]:
    """物理网卡 ifIndex。用 `Get-NetAdapter -Physical` 而不是按别名猜：
    VMware / WSL / 回环那些虚拟卡也有邻居表，读它们等于读到自己。"""
    script = (
        "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; "
        "Get-NetAdapter -Physical | ForEach-Object { "
        "$i = (Get-NetIPInterface -InterfaceIndex $_.ifIndex -AddressFamily IPv4 -ErrorAction SilentlyContinue); "
        "if ($i) { '{0}|{1}' -f $_.ifIndex, $_.Name } }"
    )
    out = run(f'powershell -NoProfile -Command "{script}"')
    res: list[tuple[int, str]] = []
    for line in out.splitlines():
        m = re.match(r"\s*(\d+)\|(.+?)\s*$", line)
        if m:
            res.append((int(m.group(1)), m.group(2).strip()))
    return res


def arp_table(ifindex: int) -> dict[str, str]:
    """netsh 邻居表：IP → MAC（静态/动态都算二层活人）。"""
    out = run(f'netsh -c i i show neighbors {ifindex}')
    table: dict[str, str] = {}
    for line in out.splitlines():
        m = re.match(r"\s*(\d+\.\d+\.\d+\.\d+)\s+([0-9a-fA-F-]{12,17})", line)
        if m:
            mac = m.group(2).replace("-", ":").lower()
            ip = m.group(1)
            # 「无法访问 / unreachable」的行不会带 MAC，正则已经把它挡在外面。
            # 组播与广播项（224./239./255.255.255.255、末位 .255）不是「人」，
            # 算进邻居数会把「二层活人」虚报一台 —— 这个数就是用来数活人的。
            if mac in ("", "00:00:00:00:00:00"):
                continue
            if ip.startswith(("224.", "239.", "255.")) or ip.endswith(".255"):
                continue
            table[ip] = mac
    return table


def wake_broadcast(prefix: str) -> None:
    """来一发广播 + 定向组播，把睡下去的机器叫醒（ARP 缺席有一部分只是没人理我们）。"""
    out = run(f'ping -n 1 -w 300 {prefix}.255')
    if "无法" in out or "could not find" in out.lower():
        pass  # 有些栈不允许广播 ping，不是错误：后面还有直连扫描


def scan_one(ip: str, port: int, timeout: float) -> tuple | None:
    """一次 connect。开了就顺手抓 banner（SSH/中继的版本串比 TTL 靠谱得多）。"""
    s = socket.socket()
    s.settimeout(timeout)
    try:
        s.connect((ip, port))
        banner = ""
        if port in (22, 21116):
            s.settimeout(1.5)
            try:
                banner = s.recv(80).decode("utf-8", "replace").strip()
            except Exception:
                banner = ""
        return (ip, port, banner)
    except Exception:
        return None
    finally:
        try:
            s.close()
        except Exception:
            pass


def scan_subnet(prefix: str, ports: list[int], timeout: float, workers: int) -> list[tuple]:
    """全段慢扫。线程池：串行 254×N 次 connect 在这里要跑几十分钟，没人等得起。"""
    jobs: "queue.Queue" = queue.Queue()
    for last in range(1, 255):
        for p in ports:
            jobs.put((f"{prefix}.{last}", p))
    found: list[tuple] = []
    lock = threading.Lock()

    def worker():
        while True:
            try:
                ip, port = jobs.get_nowait()
            except queue.Empty:
                return
            hit = scan_one(ip, port, timeout)
            if hit:
                with lock:
                    found.append(hit)

    ts = [threading.Thread(target=worker, daemon=True) for _ in range(workers)]
    for t in ts:
        t.start()
    for t in ts:
        t.join()
    return sorted(found, key=lambda x: (ipaddress.IPv4Address(x[0]), x[1]))


def main() -> int:
    ap = argparse.ArgumentParser(description="局域网在线审计（断电 vs DHCP 漂移）")
    ap.add_argument("--prefix", default="192.168.223", help="网段前三段，默认 192.168.223")
    ap.add_argument("--target", default="192.168.223.15", help="要找的那台机器（被控端）")
    ap.add_argument("--probe", default="", help="控制探针用的「确定不存在」地址，默认取 .99")
    ap.add_argument("--ports", default=",".join(str(p) for p in sorted(FEATURE_PORTS)),
                    help="扫描端口表，逗号分隔")
    ap.add_argument("--timeout", type=float, default=2.5, help="connect 超时（秒），别低于 2.5")
    ap.add_argument("--workers", type=int, default=96)
    ap.add_argument("--no-arp", action="store_true", help="跳过 netsh（非 Windows / 无权限时）")
    a = ap.parse_args()

    ports = [int(x) for x in a.ports.split(",") if x.strip()]
    probe_ip = a.probe or f"{a.prefix}.99"
    print("=" * 72)
    print(f"局域网在线审计：{a.prefix}.0/24  目标 {a.target}  端口 {ports}")
    print("=" * 72)

    # ── 第 1 步：先自证扫描可信，否则后面全是噪声 ──
    print(f"\n[1/4] 控制探针 {probe_ip}:22（这台地址上应当什么都没有）…")
    if control_probe(probe_ip, 22):
        print("    ✗ 竟然「连上了」→ 本机透明代理正在对整个内网段发**假应答**。")
        print("      本次端口扫描作废：任何「在线/离线」结论都可能是编出来的。")
        print("      解法：把托盘里 v2rayN 的 TUN 模式关掉（或把内网段写进")
        print("      guiNConfig.json → TunModeItem.RouteExcludeAddress 后重启 v2rayN），")
        print("      再跑一遍这个脚本。")
        return 2
    print("    ✓ 连不上 → 没有代理劫持，本次扫描可信。")

    # ── 第 2 步：二层真相 ──
    arp: dict[str, str] = {}
    if not a.no_arp:
        print("\n[2/4] 读 netsh 邻居表（带真实 MAC 的才是二层活人）…")
        wake_broadcast(a.prefix)
        ifs = physical_ifindex()
        if not ifs:
            print("    ！没认出物理网卡，跳过（用 --no-arp 显式跳过这一步）")
        for idx, alias in ifs:
            t = arp_table(idx)
            mine = {ip: mac for ip, mac in t.items() if ip.startswith(a.prefix + ".")}
            print(f"    网卡 {idx} {alias}: 邻居 {len(mine)} 台")
            arp.update(mine)
    else:
        print("\n[2/4] 跳过邻居表（--no-arp）")

    # ── 第 3 步：全段慢扫 ──
    print(f"\n[3/4] 全段慢扫 {len(ports)} 个特征端口（超时 {a.timeout}s，{a.workers} 线程）…")
    found = scan_subnet(a.prefix, ports, a.timeout, a.workers)
    hosts: dict[str, list[tuple[int, str]]] = {}
    for ip, port, banner in found:
        hosts.setdefault(ip, []).append((port, banner))
    print(f"    有响应的地址：{len(hosts)} 个")

    # ── 第 4 步：逐台判读 + 目标结论 ──
    print("\n[4/4] 判读：")
    for ip, svc in sorted(hosts.items(), key=lambda x: ipaddress.IPv4Address(x[0])):
        in_arp = ip in arp
        mac = arp.get(ip, "—")
        tags = ", ".join(f"{p}/{FEATURE_PORTS.get(p, '?')}" + (f"（{b[:34]}）" if b else "")
                         for p, b in svc)
        kind = guess_kind(svc)
        print(f"    {ip:<15} ARP={'有' if in_arp else '无'} MAC={mac:<17} {kind}  [{tags}]")

    if not arp:
        print("    （邻居表为空 —— 只用端口扫描的结果，别据此说「这台上没东西」）")

    target_in_arp = a.target in arp
    target_open = a.target in hosts
    print("\n" + "=" * 72)
    print(f"结论 —— {a.target}")
    if target_open or target_in_arp:
        print("    在线。要唤醒/上线排查请直接在它上面跑 uulan-host --diagnose。")
        return 0
    print("    · 二层（ARP）：没有它 —— 只能排除「还在本子网」，**不能**排除「还在线路上」。")
    print("      它要是被 DHCP 分到别的子网，就不会应答本段广播。")
    print("    · 特征端口：全段无响应。")
    print("    剩下两条裁决路径（脚本给不出）：① 路由器上的 DHCP 租约表（要管理页/口令）；")
    print("    ② 人到机器跟前看电源灯与网口灯。")
    print("    提醒：WoL 需要 MAC，而这套环境里哪都没存过 —— 连上真机的任何时候")
    print("    顺手 `ip link` 采一次存进技能文档，那是失联时唯一能远程唤醒的凭据。")
    return 1


def guess_kind(svc: list[tuple[int, str]]) -> str:
    """按特征端口组合猜机型。**只在有把握时说**，否则说「未定」——
    错误结论比没有结论更贵（这条纪律来自一次把 Windows 判成麒麟的事故）。"""
    got = {p for p, _ in svc}
    banner = " ".join(b for _, b in svc).lower()
    if 21116 in got:
        return "跑着 uulan 中继"
    if 135 in got or 445 in got:
        return "Windows 办公机"
    if 3389 in got and 22 not in got:
        return "开 RDP 的办公机（多半 Windows）"
    if 22 in got:
        if "kylin" in banner or "nkylin" in banner:
            return "麒麟（SSH banner 带 kylin）"
        if "ubuntu" in banner or "debian" in banner:
            return "Ubuntu/Debian 系（SSH banner）"
        return "有 SSH 的机器（机型未定）"
    if 631 in got:
        return "打印机 / CUPS"
    return "机型未定"


if __name__ == "__main__":
    sys.exit(main())
