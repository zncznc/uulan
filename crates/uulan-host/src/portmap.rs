//! 端口映射（M4）：控制端在本机监听一个端口，字节经被控端转到**被控端所在网络**的目标上。
//!
//! 能省掉「被控端在内网、外面连不进去」这一整类麻烦：用户在自己机器上连
//! `127.0.0.1:2222`，实际连到的是被控端那台机器的 `127.0.0.1:22`。
//!
//! 数据全部走 bulk 连接（按 `id` 多路复用，见 `uulan_bulk`），控制通道只负责
//! 「建/拆」两条消息（`PortMapOp::Open/Close` → `PortMapOk/PortMapErr`）。
//!
//! ## 白名单是这套东西的安全边界
//!
//! 被控端**必须**自己决定能连哪儿，否则它就是一个内网跳板机：任何拿到密码的人都能
//! 用它访问被控端所在的整个内网。所以：
//!
//! - 默认只允许 **本机回环**（`127.0.0.0/8` / `::1`）。远控端口映射最常见的用途正是
//!   「连被控端本机的服务」，这个默认值既能用又不危险。
//! - 要放开到网段必须**显式写进配置** `portmap_allow`（`10.0.0.0/8`、`192.168.1.5`、
//!   `*` 表示不限制）。
//! - 域名解析出**多个地址时逐个检查**，有一个不在白名单里就整条拒绝 ——
//!   否则 `localhost` 这类名字可以被做成「一半合规一半不合规」绕过检查。
//! - 连接建立后再核对一次 `peer_addr()`（解析与实际连上的 IP 不一致时以实际为准）。

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use uulan_bulk::{Mux, Stream};
use uulan_protocol::bulk::BulkKind;

/// 单次转发读缓冲区。和 bulk 的 `CHUNK`（64 KiB）不必一致：这里只决定
/// 「一次 read 最多搬多少字节」，多出来的会被拆成多个 bulk 帧。
const BUF: usize = 32 * 1024;

/// 解析目标域名 / 建立 TCP 连接的超时。远控里用户点一下就要有反应，
/// 宁可报「连不上」也不要让界面上转圈几十秒。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// 一条白名单规则。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
    /// `*`：不限制（配置里写出来就意味着用户明确同意当跳板）。
    Any,
    Net(IpAddr, u8),
}

/// 端口映射目标白名单。
#[derive(Debug, Clone)]
pub struct PortPolicy {
    rules: Vec<Rule>,
}

impl Default for PortPolicy {
    fn default() -> Self {
        Self::loopback()
    }
}

impl PortPolicy {
    /// 默认策略：只允许本机回环。
    pub fn loopback() -> Self {
        Self {
            rules: vec![Rule::Net(IpAddr::V4(Ipv4Addr::LOCALHOST), 8), Rule::Net(IpAddr::V6(Ipv6Addr::LOCALHOST), 128)],
        }
    }

    /// 从配置里的字符串列表解析。空列表 = 默认（只允许回环）。
    ///
    /// 支持 `*`、`10.0.0.0/8`、`192.168.1.5`（等价 `/32`）、`::1`、`fe80::/10`。
    pub fn from_config(entries: &[String]) -> Result<Self, String> {
        if entries.is_empty() {
            return Ok(Self::loopback());
        }
        let mut rules = Vec::new();
        for raw in entries {
            let e = raw.trim();
            if e.is_empty() {
                continue;
            }
            if e == "*" {
                rules.push(Rule::Any);
                continue;
            }
            let (ip_part, prefix) = match e.split_once('/') {
                Some((ip, p)) => {
                    let prefix: u8 = p
                        .parse()
                        .map_err(|_| format!("portmap_allow 里的 {raw} 前缀长度不是数字"))?;
                    (ip, Some(prefix))
                }
                None => (e, None),
            };
            let ip: IpAddr = ip_part
                .parse()
                .map_err(|_| format!("portmap_allow 里的 {raw} 不是 IP 或 CIDR"))?;
            let max = if ip.is_ipv4() { 32 } else { 128 };
            let prefix = prefix.unwrap_or(max);
            if prefix > max {
                return Err(format!("portmap_allow 里的 {raw} 前缀长度超过 {max}"));
            }
            rules.push(Rule::Net(ip, prefix));
        }
        if rules.is_empty() {
            return Ok(Self::loopback());
        }
        Ok(Self { rules })
    }

    /// 这个地址是否允许被连。**注意这是「允许列表」，不是「拒绝列表」。**
    pub fn allows(&self, ip: IpAddr) -> bool {
        self.rules.iter().any(|r| match r {
            Rule::Any => true,
            Rule::Net(base, prefix) => same_net(*base, ip, *prefix),
        })
    }

    /// 给日志用的一行摘要（不含敏感信息，就是规则本身）。
    pub fn describe(&self) -> String {
        if self.rules.iter().any(|r| matches!(r, Rule::Any)) {
            return "不限制（portmap_allow=*）".to_string();
        }
        self.rules
            .iter()
            .map(|r| match r {
                Rule::Any => "*".to_string(),
                Rule::Net(ip, p) => format!("{ip}/{p}"),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn same_net(base: IpAddr, other: IpAddr, prefix: u8) -> bool {
    match (base, other) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            let p = prefix.min(32) as u32;
            let mask = if p == 0 { 0 } else { u32::MAX << (32 - p) };
            (u32::from(a) & mask) == (u32::from(b) & mask)
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            let p = prefix.min(128) as u32;
            let mask = if p == 0 { 0 } else { u128::MAX << (128 - p) };
            (u128::from(a) & mask) == (u128::from(b) & mask)
        }
        _ => false,
    }
}

/// 一次会话的端口映射状态：白名单 + 每条流的桥任务。
pub struct PortMap {
    mux: Mux,
    policy: PortPolicy,
    /// 正在跑的桥任务，按流 id 索引（`Close` 要能立刻拆掉）。
    /// `Arc` 是为了让桥任务自己跑完时能把表项删掉（否则长会话里会攒一堆已完成的句柄）。
    tasks: Arc<Mutex<HashMap<u32, JoinHandle<()>>>>,
}

impl PortMap {
    pub fn new(mux: Mux, policy: PortPolicy) -> Self {
        info!(allow = %policy.describe(), "端口映射白名单");
        Self { mux, policy, tasks: Arc::new(Mutex::new(HashMap::new())) }
    }

    pub fn policy(&self) -> &PortPolicy {
        &self.policy
    }

    /// 建一条映射：解析目标 → 白名单检查 → 连上去 → 注册 bulk 流 → 起桥任务。
    ///
    /// 返回 `Err` 时**没有**任何东西留在表里（连上的 socket 也一起丢掉），
    /// 对应的 `PortMapErr` 会由调用方回给控制端。
    pub async fn open(&self, id: u32, host: &str, port: u16) -> Result<(), String> {
        if port == 0 {
            return Err("目标端口不能是 0".into());
        }
        if host.trim().is_empty() {
            return Err("目标地址为空".into());
        }
        // 注意顺序：白名单判定在「通道还活着吗」之前 —— 拒绝的理由必须是策略，
        // 而不是「bulk 断了」，否则日志里会看不出到底是哪种拒绝。
        let addrs = self.resolve(host, port).await?;
        // 先把所有解析结果过一遍白名单：不许「一个合规一个不合规」蒙混过去。
        for a in &addrs {
            if !self.policy.allows(a.ip()) {
                warn!(id, target = %a, allow = %self.policy.describe(), "端口映射目标不在白名单，拒绝");
                return Err(format!("目标 {} 不在端口映射白名单里", a.ip()));
            }
        }

        let tcp = self.connect(&addrs, id).await?;
        // 连上之后再核对一次实际对端（防止解析结果与实际连接不一致）。
        if let Ok(peer) = tcp.peer_addr() {
            if !self.policy.allows(peer.ip()) {
                return Err(format!("实际连到的 {} 不在白名单里", peer.ip()));
            }
        }

        // 连接建好才注册 id：控制端要等到 `PortMapOk` 之后才发数据，
        // 所以这中间不会丢帧；反过来先注册、连接失败还要记得摘掉，容易漏。
        let Some(stream) = self.mux.register(id) else {
            return Err("bulk 通道不可用，或该 id 已被占用".into());
        };
        let target = tcp.peer_addr().map(|a| a.to_string()).unwrap_or_else(|_| host.to_string());
        let tasks = self.tasks.clone();
        let shown = target.clone();
        let h = tokio::spawn(async move {
            bridge(id, stream, tcp, shown).await;
            // 跑完自己摘掉表项：长会话里开开关关几百次也不该把表撑大。
            tasks.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
        });
        self.tasks.lock().unwrap_or_else(|e| e.into_inner()).insert(id, h);
        info!(id, target = %target, "端口映射已建立");
        Ok(())
    }

    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
        // 已经是 IP 就不查 DNS（内网里很多机器没有可用 DNS，能省一次超时）。
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(vec![SocketAddr::new(ip, port)]);
        }
        let lookup = tokio::net::lookup_host((host, port));
        let addrs = match tokio::time::timeout(CONNECT_TIMEOUT, lookup).await {
            Ok(Ok(it)) => it.collect::<Vec<_>>(),
            Ok(Err(e)) => return Err(format!("目标 {host} 解析失败：{e}")),
            Err(_) => return Err(format!("目标 {host} 解析超时")),
        };
        if addrs.is_empty() {
            return Err(format!("目标 {host} 解析不到地址"));
        }
        Ok(addrs)
    }

    async fn connect(&self, addrs: &[SocketAddr], id: u32) -> Result<TcpStream, String> {
        let mut last = String::from("没有可连的地址");
        for a in addrs {
            match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(a)).await {
                Ok(Ok(s)) => {
                    let _ = s.set_nodelay(true);
                    return Ok(s);
                }
                Ok(Err(e)) => last = format!("{a}: {e}"),
                Err(_) => last = format!("{a}: 连接超时"),
            }
            debug!(id, target = %a, error = %last, "这个地址连不上，试下一个");
        }
        Err(format!("连不上目标：{last}"))
    }

    /// 拆掉一条映射（控制端主动关，或者会话收尾）。
    pub fn close(&self, id: u32) {
        if let Some(h) = self.tasks.lock().unwrap_or_else(|e| e.into_inner()).remove(&id) {
            h.abort();
            debug!(id, "端口映射已拆除");
        }
    }

    /// 会话结束：把所有桥任务停掉，别留下「会话没了、内网连接还开着」。
    pub async fn shutdown(&self) {
        let handles: Vec<JoinHandle<()>> =
            self.tasks.lock().unwrap_or_else(|e| e.into_inner()).drain().map(|(_, h)| h).collect();
        for h in handles {
            h.abort();
        }
    }
}

/// 把一条 bulk 流和一条 TCP 连接对拷。
///
/// 收尾规则（两端对称）：本地方向读到 EOF 就发 `END`；收到对端 `END` 就 `shutdown()`
/// 本地的写半边。两边的 END 都到了才退出 —— 只关一半的话，
/// 「对面还在发数据、我已经不听了」会把数据丢掉（SSH 里的交互式会话尤其明显）。
async fn bridge(id: u32, mut s: Stream, mut tcp: TcpStream, target: String) {
    let mut buf = vec![0u8; BUF];
    let mut to_target_sent_end = false; // 目标方向的 END 已经发过
    let mut from_target_closed = false; // 收到了控制端的 END
    let (mut up, mut down) = (0u64, 0u64);

    loop {
        tokio::select! {
            // 控制端 → 目标
            f = s.recv(), if !from_target_closed => match f {
                Some(f) => match f.kind {
                    BulkKind::Data => {
                        if tcp.write_all(&f.data).await.is_err() {
                            debug!(id, target = %target, "写目标失败，映射收尾");
                            break;
                        }
                        down += f.data.len() as u64;
                    }
                    BulkKind::End => {
                        from_target_closed = true;
                        // 半关闭：让目标看到 EOF，但它还能继续往回发。
                        let _ = tcp.shutdown().await;
                    }
                },
                None => break, // bulk 流没了（连接断了 / 会话收尾）
            },
            // 目标 → 控制端
            n = tcp.read(&mut buf), if !to_target_sent_end => match n {
                Ok(0) => {
                    to_target_sent_end = true;
                    if s.send_end().await.is_err() { break }
                }
                Ok(k) => {
                    up += k as u64;
                    if s.send_data(&buf[..k]).await.is_err() { break }
                }
                Err(e) => {
                    debug!(id, target = %target, error = %e, "读目标失败，映射收尾");
                    let _ = s.send_end().await;
                    break;
                }
            },
            else => break,
        }
        if to_target_sent_end && from_target_closed {
            break;
        }
    }
    info!(id, target = %target, up, down, "端口映射结束");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(entries: &[&str]) -> PortPolicy {
        PortPolicy::from_config(&entries.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            .expect("规则应当能解析")
    }

    #[test]
    fn default_is_loopback_only() {
        let pol = PortPolicy::from_config(&[]).unwrap();
        assert!(pol.allows("127.0.0.1".parse().unwrap()));
        assert!(pol.allows("127.9.9.9".parse().unwrap()), "回环是整段 127/8");
        assert!(pol.allows("::1".parse().unwrap()));
        assert!(!pol.allows("192.168.1.10".parse().unwrap()));
        assert!(!pol.allows("8.8.8.8".parse().unwrap()));
        assert!(!pol.allows("::2".parse().unwrap()));
    }

    #[test]
    fn cidr_rules_match_and_do_not_leak_neighbours() {
        let pol = p(&["10.0.0.0/8", "192.168.5.7"]);
        assert!(pol.allows("10.1.2.3".parse().unwrap()));
        assert!(pol.allows("192.168.5.7".parse().unwrap()));
        assert!(!pol.allows("192.168.5.8".parse().unwrap()), "单个 IP 不能变成整段");
        assert!(!pol.allows("11.0.0.1".parse().unwrap()));

        let pol = p(&["172.16.16.0/24"]);
        assert!(pol.allows("172.16.16.3".parse().unwrap()));
        assert!(!pol.allows("172.16.17.3".parse().unwrap()));
    }

    #[test]
    fn ipv6_prefixes_and_family_are_handled() {
        let pol = p(&["fe80::/10"]);
        assert!(pol.allows("fe80::1".parse().unwrap()));
        assert!(!pol.allows("fd00::1".parse().unwrap()));
        // 混族比较必须为 false，而不是 panic 或者「按位与出奇迹」。
        assert!(!pol.allows("127.0.0.1".parse().unwrap()));
    }

    #[test]
    fn star_means_everything() {
        let pol = p(&["*"]);
        assert!(pol.allows("8.8.8.8".parse().unwrap()));
        assert!(pol.allows("fd00::1".parse().unwrap()));
        assert_eq!(pol.describe(), "不限制（portmap_allow=*）");
    }

    #[test]
    fn bad_rules_are_errors_not_silent_defaults() {
        // 写错一条规则就当「只允许回环」会让人以为已经放开了 —— 必须报错。
        assert!(PortPolicy::from_config(&["10.0.0.0/33".to_string()]).is_err());
        assert!(PortPolicy::from_config(&["not-an-ip".to_string()]).is_err());
        assert!(PortPolicy::from_config(&["10.0.0.0/x".to_string()]).is_err());
    }

    #[test]
    fn empty_and_blank_entries_fall_back_to_loopback() {
        let pol = PortPolicy::from_config(&["  ".to_string(), "".to_string()]).unwrap();
        assert!(pol.allows("127.0.0.1".parse().unwrap()));
        assert!(!pol.allows("10.0.0.1".parse().unwrap()));
    }
}