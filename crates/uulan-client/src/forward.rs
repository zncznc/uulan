//! 端口映射（M4）的控制端半边：在本机监听一个端口，字节经被控端转到目标上。
//!
//! 用法（`uulan-client`）：
//!
//! ```text
//! --forward 2222:127.0.0.1:22              本机 2222 → 被控端本机 22
//! --forward 0.0.0.0:8080:10.1.2.3:80       指定监听地址
//! ```
//!
//! 语义等价于 `ssh -L`：**目标地址是站在被控端的立场解析的**，
//! 白名单也在被控端判定（见 `uulan_host::portmap`）。
//!
//! ## 为什么是一个独立会话
//!
//! 这一版把转发做成「连上、监听、退出」的一条独立会话（和 `--ls/--put` 一样），
//! 不开窗口、不收画面。原因是控制通道的读半边在窗口模式下归 `ControlLink` 管，
//! 而 `PortMapOk/Err` 需要按 id 派发给各自的连接 —— 硬塞进同一个读循环会让
//! 窗口模式多一个「谁的响应」的分派表。代价是：**转发和开窗要跑两个客户端进程**
//! （被控端允许并存，不校验「第二个人」，所以两个进程能同时连上）。
//!
//! ## 流与 id
//!
//! 每次 accept 分配一个 id，先在控制通道上 `Open`，等到 `PortMapOk` 才在 bulk 上
//! 注册这条流。这一步的先后顺序很重要：**被控端是先连上目标、再回 Ok**
//! （失败会回 `PortMapErr`），所以我们注册流之后收到的一定是真实业务字节，
//! 不会出现「本地应用已经发了一堆、对端还不知道要往哪儿送」。

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use uulan_bulk::{Mux, Stream};
use uulan_protocol::bulk::BulkKind;
use uulan_protocol::frame::{read_frame, write_frame, FrameKind};
use uulan_protocol::msg::{C2H, PortMapOp, H2C};

/// 一次转发请求的读缓冲区。32 KiB 是「系统调用次数」与「单帧延迟」的折中。
const BUF: usize = 32 * 1024;

/// 等被控端回 `PortMapOk/Err` 的超时。被控端自己连目标最多等 5 秒，
/// 这里留一倍余量：真的连不上时用户要尽快看到失败，而不是界面上一直转。
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);

/// 一条转发规则（命令行解析出来的）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardSpec {
    /// 本机监听地址。
    pub bind: SocketAddr,
    /// 目标（站在被控端立场解析）。
    pub target_host: String,
    pub target_port: u16,
}

impl ForwardSpec {
    /// 解析 `[绑定地址:]本地端口:目标主机:目标端口`。
    ///
    /// 只接受 IPv4 绑定地址与主机名/IPv4 目标。IPv6 字面量要写成 `[::1]` 形式，
    /// 第一版不支持（远控的典型用法都是 `127.0.0.1` 或内网 IP）；
    /// 与其猜错分隔符，不如明确报错。
    pub fn parse(s: &str) -> Result<Self> {
        let t = s.trim();
        if t.contains('[') {
            bail!("--forward 第一版不支持 IPv6 字面量：{s}（请用 IPv4 或主机名）");
        }
        let parts: Vec<&str> = t.split(':').collect();
        let (bind_ip, local_port, host, port) = match parts.len() {
            3 => (IpAddr::V4(Ipv4Addr::LOCALHOST), parts[0], parts[1], parts[2]),
            4 => (
                parts[0]
                    .parse::<IpAddr>()
                    .with_context(|| format!("--forward 的绑定地址看不懂：{}", parts[0]))?,
                parts[1],
                parts[2],
                parts[3],
            ),
            _ => bail!(
                "--forward 的格式是 [绑定地址:]本地端口:目标主机:目标端口（例如 2222:127.0.0.1:22），收到：{s}"
            ),
        };
        let local_port: u16 = local_port
            .parse()
            .with_context(|| format!("--forward 的本地端口不是数字：{local_port}"))?;
        let target_port: u16 = port
            .parse()
            .with_context(|| format!("--forward 的目标端口不是数字：{port}"))?;
        if host.trim().is_empty() {
            bail!("--forward 的目标主机为空：{s}");
        }
        if target_port == 0 {
            bail!("--forward 的目标端口不能是 0");
        }
        // 本地端口允许 0：那是「让系统随便挑一个」，脚本化验证要用它。
        Ok(Self {
            bind: SocketAddr::new(bind_ip, local_port),
            target_host: host.to_string(),
            target_port,
        })
    }
}

/// 控制通道的出站消息（Pong 要复用同一个写任务）。
enum Out {
    Json(C2H),
    Pong(bytes::Bytes),
}

#[derive(Default)]
struct Counters {
    conns: AtomicU64,
    up: AtomicU64,
    down: AtomicU64,
}

/// 端口映射的转发器。克隆廉价（内部 `Arc`）。
#[derive(Clone)]
pub struct Forwarder {
    inner: Arc<Inner>,
}

struct Inner {
    ctl: mpsc::Sender<Out>,
    /// 等 `PortMapOk/Err` 的 id → 等待者。读任务按 id 派发时要用同一份。
    pending: Arc<Mutex<HashMap<u32, oneshot::Sender<Result<(), String>>>>>,
    mux: Mux,
    next_id: AtomicU64,
    stats: Arc<Counters>,
    /// 监听任务，`shutdown()` 时全部 abort。
    listeners: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Forwarder {
    /// 接管已鉴权的控制连接与 bulk 连接。
    pub fn start(control: TcpStream, bulk: TcpStream) -> Self {
        let (cr, mut cw) = control.into_split();
        let (br, bw) = bulk.into_split();
        let (tx, mut rx) = mpsc::channel::<Out>(64);
        let pending: Arc<Mutex<HashMap<u32, oneshot::Sender<Result<(), String>>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // 写任务：控制通道上唯一的写者。
        tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                let ok = match m {
                    Out::Json(m) => match serde_json::to_vec(&m) {
                        Ok(b) => write_frame(&mut cw, FrameKind::Json, &b).await.is_ok(),
                        Err(e) => {
                            debug!(error = %e, "控制消息序列化失败");
                            true
                        }
                    },
                    Out::Pong(p) => write_frame(&mut cw, FrameKind::Pong, &p).await.is_ok(),
                };
                if !ok {
                    debug!("控制通道写入失败，转发收尾");
                    break;
                }
            }
        });

        // 读任务：只关心 PortMapOk/Err（按 id 派发），别的都当噪声。
        // 心跳要回，否则对端的心跳探测会一直失败（这条连接上没人替我们做这件事）。
        let p = pending.clone();
        let ping_tx = tx.clone();
        tokio::spawn(async move {
            let mut rd = cr;
            loop {
                let f = match read_frame(&mut rd).await {
                    Ok(f) => f,
                    Err(e) => {
                        debug!(error = %e, "控制通道读取结束");
                        break;
                    }
                };
                match f.kind {
                    FrameKind::Ping => {
                        let _ = ping_tx.send(Out::Pong(f.payload)).await;
                    }
                    FrameKind::Json => {
                        let Ok(m) = f.decode_json::<H2C>() else { continue };
                        match m {
                            H2C::PortMapOk { id } => resolve(&p, id, Ok(())),
                            H2C::PortMapErr { id, error } => resolve(&p, id, Err(error)),
                            H2C::Bye { reason } => {
                                info!(reason = %reason, "被控端道别，转发结束");
                                break;
                            }
                            _ => {}
                        }
                    }
                    FrameKind::Bye => break,
                    _ => {}
                }
            }
            // 读任务结束 = 会话没了。把还在等 Ok 的连接全部判失败，
            // 而不是让它们挂到超时。
            let mut g = p.lock().unwrap_or_else(|e| e.into_inner());
            for (_, tx) in g.drain() {
                let _ = tx.send(Err("控制通道已断开".into()));
            }
        });

        Self {
            inner: Arc::new(Inner {
                ctl: tx,
                pending,
                mux: uulan_bulk::spawn(br, bw),
                next_id: AtomicU64::new(1),
                stats: Arc::new(Counters::default()),
                listeners: Mutex::new(Vec::new()),
            }),
        }
    }

    /// 在本机起监听。返回**实际**绑定的地址（本地端口写 0 时由系统挑）。
    pub async fn listen(&self, spec: ForwardSpec) -> Result<SocketAddr> {
        let listener = TcpListener::bind(spec.bind)
            .await
            .with_context(|| format!("本机监听 {} 失败", spec.bind))?;
        let addr = listener.local_addr()?;
        let me = self.clone();
        let h = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((sock, peer)) => {
                        debug!(%peer, "本机有新连接，开始转发");
                        let me = me.clone();
                        let spec = spec.clone();
                        tokio::spawn(async move { me.serve(sock, spec).await });
                    }
                    Err(e) => {
                        // accept 出错通常意味着监听套接字没了（被关掉），不是单次失败。
                        debug!(error = %e, "accept 失败，监听结束");
                        break;
                    }
                }
            }
        });
        self.inner.listeners.lock().unwrap_or_else(|e| e.into_inner()).push(h);
        Ok(addr)
    }

    /// 处理一条本机连接。
    async fn serve(&self, tcp: TcpStream, spec: ForwardSpec) {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed) as u32;
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id, tx);

        let open = C2H::PortMap(PortMapOp::Open {
            id,
            target_host: spec.target_host.clone(),
            target_port: spec.target_port,
        });
        if self.inner.ctl.send(Out::Json(open)).await.is_err() {
            self.forget(id);
            return;
        }
        let ok = match tokio::time::timeout(OPEN_TIMEOUT, rx).await {
            Ok(Ok(Ok(()))) => true,
            Ok(Ok(Err(e))) => {
                warn!(id, target = %format!("{}:{}", spec.target_host, spec.target_port), error = %e, "被控端拒绝建立映射");
                false
            }
            Ok(Err(_)) => {
                warn!(id, "被控端在建立映射期间断线");
                false
            }
            Err(_) => {
                warn!(id, "等被控端建立映射超时");
                false
            }
        };
        if !ok {
            // 直接把本机连接关掉：让发起连接的程序立刻看到「连不上」，
            // 而不是让它对着一个永远没数据的连接等到自己的超时。
            drop(tcp);
            return;
        }

        let Some(stream) = self.inner.mux.register(id) else {
            warn!(id, "bulk 通道不可用，转发无法开始");
            return;
        };
        self.inner.stats.conns.fetch_add(1, Ordering::Relaxed);
        bridge(id, stream, tcp, self.inner.stats.clone()).await;

        // 会话里主动拆掉，别让被控端那条目标的 socket 一直挂着等 TCP 超时。
        let _ = self.inner.ctl.send(Out::Json(C2H::PortMap(PortMapOp::Close { id }))).await;
    }

    fn forget(&self, id: u32) {
        self.inner.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
    }

    /// `(连接数, 上行字节, 下行字节)`。
    pub fn stats(&self) -> (u64, u64, u64) {
        (
            self.inner.stats.conns.load(Ordering::Relaxed),
            self.inner.stats.up.load(Ordering::Relaxed),
            self.inner.stats.down.load(Ordering::Relaxed),
        )
    }

    pub fn shutdown(&self) {
        for h in self.inner.listeners.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
            h.abort();
        }
        self.inner.mux.close();
    }
}

fn resolve(
    p: &Arc<Mutex<HashMap<u32, oneshot::Sender<Result<(), String>>>>>,
    id: u32,
    r: Result<(), String>,
) {
    let tx = p.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
    match tx {
        Some(tx) => {
            let _ = tx.send(r);
        }
        // 没人等这个 id：多半是超时之后才到的 Err，记一笔就够了。
        None => debug!(id, "收到没有等待者的 PortMap 响应"),
    }
}

/// 本机 socket ↔ bulk 流 的对拷（与控制端相反的视角，规则一致）：
/// 本地读到 EOF 就发 `END`；收到 `END` 就 `shutdown()` 本地写半边；两边都结束才退出。
async fn bridge(id: u32, mut s: Stream, mut tcp: TcpStream, st: Arc<Counters>) {
    let mut buf = vec![0u8; BUF];
    let mut local_done = false;
    let mut remote_done = false;
    loop {
        tokio::select! {
            n = tcp.read(&mut buf), if !local_done => match n {
                Ok(0) => {
                    local_done = true;
                    if s.send_end().await.is_err() { break }
                }
                Ok(k) => {
                    st.up.fetch_add(k as u64, Ordering::Relaxed);
                    if s.send_data(&buf[..k]).await.is_err() { break }
                }
                Err(e) => {
                    debug!(id, error = %e, "本机连接读失败，转发收尾");
                    let _ = s.send_end().await;
                    break;
                }
            },
            f = s.recv(), if !remote_done => match f {
                Some(f) => match f.kind {
                    BulkKind::Data => {
                        st.down.fetch_add(f.data.len() as u64, Ordering::Relaxed);
                        if tcp.write_all(&f.data).await.is_err() {
                            debug!(id, "写本机连接失败，转发收尾");
                            break;
                        }
                    }
                    BulkKind::End => {
                        remote_done = true;
                        let _ = tcp.shutdown().await;
                    }
                },
                None => break,
            },
            else => break,
        }
        if local_done && remote_done {
            break;
        }
    }
    debug!(id, "一条转发结束");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(s: &str) -> ForwardSpec {
        ForwardSpec::parse(s).expect("应当能解析")
    }

    #[test]
    fn parses_local_port_form() {
        let s = spec("2222:127.0.0.1:22");
        assert_eq!(s.bind, SocketAddr::from(([127, 0, 0, 1], 2222)));
        assert_eq!(s.target_host, "127.0.0.1");
        assert_eq!(s.target_port, 22);
    }

    #[test]
    fn parses_explicit_bind_address() {
        let s = spec("0.0.0.0:8080:10.1.2.3:80");
        assert_eq!(s.bind, SocketAddr::from(([0, 0, 0, 0], 8080)));
        assert_eq!(s.target_host, "10.1.2.3");
        assert_eq!(s.target_port, 80);
    }

    #[test]
    fn accepts_hostname_targets_and_port_zero_bind() {
        let s = spec("0:db.internal:5432");
        assert_eq!(s.bind.port(), 0, "本地端口 0 = 让系统挑（脚本化验证要用）");
        assert_eq!(s.target_host, "db.internal");
        assert_eq!(s.target_port, 5432);
    }

    #[test]
    fn bad_specs_are_errors_not_defaults() {
        assert!(ForwardSpec::parse("2222:127.0.0.1").is_err(), "少一段必须报错");
        assert!(ForwardSpec::parse("x:127.0.0.1:22").is_err(), "本地端口不是数字");
        assert!(ForwardSpec::parse("2222:127.0.0.1:y").is_err(), "目标端口不是数字");
        assert!(ForwardSpec::parse("2222::22").is_err(), "目标主机不能为空");
        assert!(ForwardSpec::parse("2222:127.0.0.1:0").is_err(), "目标端口不能是 0");
        assert!(ForwardSpec::parse("[::1]:2222:[::1]:22").is_err(), "IPv6 字面量第一版不支持");
    }
}