//! uulan 中继 / 会合服务器。
//!
//! 它是**盲转发**的：不认识 CONTROL 消息、不看视频帧、不碰文件内容，
//! 只在两条 TCP 连接之间 `copy_bidirectional`。设备密码由被控端自己校验，
//! 所以这台服务器被拿下也拿不到任何屏幕内容或凭据（二期叠加端到端加密后，
//! 连密文都解不开）。
//!
//! 内网里随便找一台常开的机器跑它即可 —— 麒麟服务器、Windows 盒子、树莓派都行。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use uulan_protocol::frame::{read_frame, write_frame, Frame, FrameKind};
use uulan_protocol::rv::{
    gen_device_id, gen_session, is_valid_device_id, Channel, ErrCode, Role, ToEndpoint, ToServer,
};
use uulan_protocol::PROTO_VERSION;

pub mod registry;
pub use registry::DeviceBook;

/// 会话票据有效期。控制端拿到票据后要立刻拨三条 relay 连接，
/// 60 秒足够，也不至于让僵尸票据长期占内存。
const SESSION_TTL: Duration = Duration::from_secs(60);

/// 单条 relay 连接等待对端的最长时间。超时就关掉，避免半条连接长期挂着。
const RELAY_WAIT_TTL: Duration = Duration::from_secs(30);

/// 清理任务的节拍。
const SWEEP_INTERVAL: Duration = Duration::from_secs(10);

/// 一台已上线的被控端。
struct Device {
    name: String,
    /// 向这台被控端的控制连接推送消息。被控端断开时 send 失败 → 视为离线。
    tx: mpsc::UnboundedSender<ToEndpoint>,
    last_seen: Instant,
}

/// 一条已配对或等待配对的 relay 通道。
#[derive(Default)]
struct PendingChannel {
    host: Option<TcpStream>,
    client: Option<TcpStream>,
    /// 一侧已经到位的时间，用于 RELAY_WAIT_TTL 回收。
    since: Option<Instant>,
}

struct Session {
    device_id: String,
    client_name: String,
    created: Instant,
    channels: HashMap<Channel, PendingChannel>,
}

struct Inner {
    devices: Mutex<HashMap<String, Device>>,
    sessions: Mutex<HashMap<String, Session>>,
    book: Mutex<DeviceBook>,
    state_path: Option<PathBuf>,
}

/// 服务器句柄。`Clone` 之后共享同一份状态。
#[derive(Clone)]
pub struct Server {
    inner: Arc<Inner>,
}

impl Server {
    pub fn new(state_path: Option<PathBuf>) -> Result<Self> {
        let mut book = DeviceBook::default();
        if let Some(p) = &state_path {
            match DeviceBook::load(p) {
                Ok(b) => {
                    info!(devices = b.devices.len(), path = %p.display(), "已载入设备台账");
                    book = b;
                }
                Err(e) => {
                    // 台账读不了不是致命错误：设备下次上线会重新注册。
                    warn!(path = %p.display(), error = %e, "设备台账读取失败，从空台账开始");
                }
            }
        }
        Ok(Self {
            inner: Arc::new(Inner {
                devices: Mutex::new(HashMap::new()),
                sessions: Mutex::new(HashMap::new()),
                book: Mutex::new(book),
                state_path,
            }),
        })
    }

    /// 绑定端口并开始服务。返回实际绑定的地址（便于测试里用 0 端口）。
    pub async fn bind(&self, addr: &str) -> Result<TcpListener> {
        let l = TcpListener::bind(addr).await.with_context(|| format!("绑定 {addr} 失败"))?;
        Ok(l)
    }

    /// 跑清理任务 + accept 循环。一直不返回。
    pub async fn serve(self, listener: TcpListener) -> Result<()> {
        let addr = listener.local_addr()?;
        info!(%addr, proto = PROTO_VERSION, "uulan-server 已就绪");

        // 后台清理：过期票据、超时等待的 relay 连接、掉线的设备。
        {
            let me = self.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(SWEEP_INTERVAL);
                loop {
                    tick.tick().await;
                    me.sweep();
                }
            });
        }

        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    // accept 出错（比如 fd 用尽）不应该拖垮整个服务。
                    warn!(error = %e, "accept 失败");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }
            };
            let me = self.clone();
            tokio::spawn(async move {
                if let Err(e) = me.handle_conn(stream, peer).await {
                    debug!(%peer, error = %e, "连接结束");
                }
            });
        }
    }

    fn sweep(&self) {
        let now = Instant::now();

        let mut sessions = self.inner.sessions.lock().unwrap();
        sessions.retain(|sid, s| {
            let expired = now.duration_since(s.created) > SESSION_TTL;
            let stalled = s
                .channels
                .values()
                .any(|c| c.since.map_or(false, |t| now.duration_since(t) > RELAY_WAIT_TTL));
            if expired || stalled {
                debug!(
                    session = %sid,
                    device_id = %s.device_id,
                    client = %s.client_name,
                    expired,
                    stalled,
                    "回收会话"
                );
                false
            } else {
                true
            }
        });
        drop(sessions);

        // 控制连接断了但没人来清理的情况：靠 mpsc 的 closed() 判断。
        let mut devices = self.inner.devices.lock().unwrap();
        devices.retain(|id, d| {
            let alive = !d.tx.is_closed();
            if !alive {
                info!(device_id = %id, "被控端离线");
            }
            alive
        });
    }

    async fn handle_conn(&self, mut stream: TcpStream, peer: SocketAddr) -> Result<()> {
        stream.set_nodelay(true).ok();

        let first = read_frame(&mut stream).await.context("读取首帧失败")?;
        if first.kind != FrameKind::Json {
            send_error(&mut stream, ErrCode::Internal, "首帧必须是 json").await;
            return Ok(());
        }
        let msg: ToServer = first.decode_json().context("首帧不是合法的会合消息")?;

        match msg {
            ToServer::HostRegister { device_id, name, proto } => {
                self.handle_host_register(stream, peer, device_id, name, proto).await
            }
            ToServer::ClientConnect { device_id, client_name, proto } => {
                self.handle_client_connect(stream, peer, device_id, client_name, proto).await
            }
            ToServer::Query { device_id } => self.handle_query(stream, &device_id).await,
            ToServer::Ping => {
                write_frame(&mut stream, FrameKind::Json, &json_bytes(&ToEndpoint::Pong { server_time_ms: now_ms() })).await?;
                Ok(())
            }
            ToServer::RelayOpen { session, channel, role } => {
                self.handle_relay_open(stream, &session, channel, role).await
            }
            ToServer::HostUpdate { .. } => {
                // 更新必须发生在自己那条控制连接上，不能另开连接来改别人的名字。
                send_error(&mut stream, ErrCode::Internal, "host_update 必须先 host_register").await;
                Ok(())
            }
        }
    }

    async fn handle_host_register(
        &self,
        mut stream: TcpStream,
        peer: SocketAddr,
        device_id: Option<String>,
        name: String,
        proto: u32,
    ) -> Result<()> {
        if proto != PROTO_VERSION {
            send_error(&mut stream, ErrCode::ProtoMismatch, &format!("服务端协议 v{PROTO_VERSION}，被控端 v{proto}")).await;
            return Ok(());
        }

        // 决定这个被控端用哪个 device_id：给过来的合法且没被别人占用就用它，
        // 否则分配一个新的。这样被控端重装/丢配置后不会把 ID 用串。
        let id = {
            let devices = self.inner.devices.lock().unwrap();
            // 用 as_ref + cloned：Option::filter 会吃掉所有权，后面还要用 device_id 判非法值。
            let requested = device_id
                .as_ref()
                .filter(|d| is_valid_device_id(d) && !devices.contains_key(d.as_str()))
                .cloned();
            let stale = device_id.as_deref().filter(|d| !is_valid_device_id(d));
            if let Some(bad) = stale {
                warn!(%peer, bad_id = %bad, "被控端给的 device_id 不合法，改为分配新 ID");
            }
            match requested {
                Some(d) => d,
                None => {
                    let mut d = gen_device_id();
                    while devices.contains_key(&d) {
                        d = gen_device_id();
                    }
                    d
                }
            }
        };

        let (tx, mut rx) = mpsc::unbounded_channel::<ToEndpoint>();

        {
            let mut devices = self.inner.devices.lock().unwrap();
            if let Some(old) = devices.insert(
                id.clone(),
                Device { name: name.clone(), tx: tx.clone(), last_seen: Instant::now() },
            ) {
                // 同一台设备重复上线（比如上次异常退出后残留）：踢掉旧的推送通道。
                drop(old);
                warn!(device_id = %id, "同一 device_id 重复注册，旧连接已被替换");
            }
            self.inner.book.lock().unwrap().remember(&id, &name);
        }
        self.persist_book();
        info!(device_id = %id, %peer, name = %name, "被控端上线");

        write_frame(
            &mut stream,
            FrameKind::Json,
            &json_bytes(&ToEndpoint::HostWelcome { device_id: id.clone(), name: name.clone(), server_proto: PROTO_VERSION }),
        )
        .await?;

        // 推送任务：把 Incoming 等消息写给这台被控端。
        let (rd, mut wr) = stream.into_split();
        let push = tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                if write_frame(&mut wr, FrameKind::Json, &json_bytes(&m)).await.is_err() {
                    break;
                }
            }
        });

        // 读循环：被控端只发 Ping / HostUpdate。
        let mut rd = rd;
        loop {
            match read_frame(&mut rd).await {
                Ok(f) if f.kind == FrameKind::Json => match f.decode_json::<ToServer>() {
                    Ok(ToServer::Ping) => {
                        let mut devices = self.inner.devices.lock().unwrap();
                        if let Some(d) = devices.get_mut(&id) {
                            d.last_seen = Instant::now();
                        }
                    }
                    Ok(ToServer::HostUpdate { name: Some(n) }) => {
                        if let Some(d) = self.inner.devices.lock().unwrap().get_mut(&id) {
                            d.name = n.clone();
                        }
                        self.inner.book.lock().unwrap().remember(&id, &n);
                        self.persist_book();
                    }
                    Ok(_) => {}
                    Err(e) => debug!(device_id = %id, error = %e, "被控端控制帧解析失败"),
                },
                Ok(_) => {}
                Err(_) => break,
            }
        }

        push.abort();
        self.inner.devices.lock().unwrap().remove(&id);
        info!(device_id = %id, "被控端控制连接断开");
        Ok(())
    }

    async fn handle_client_connect(
        &self,
        mut stream: TcpStream,
        peer: SocketAddr,
        device_id: String,
        client_name: String,
        proto: u32,
    ) -> Result<()> {
        if proto != PROTO_VERSION {
            send_error(&mut stream, ErrCode::ProtoMismatch, &format!("服务端协议 v{PROTO_VERSION}，控制端 v{proto}")).await;
            return Ok(());
        }
        if !is_valid_device_id(&device_id) {
            // 严格校验：位数不对要立刻报错，不能当成「查不到的设备」再去查一遍。
            send_error(&mut stream, ErrCode::BadDeviceId, "设备 ID 必须是 9 位数字且首位非 0").await;
            return Ok(());
        }

        // 锁不能跨 await：先在锁里取出需要的东西，再在锁外做 IO。
        // （std::sync::MutexGuard 不是 Send，持有它跨 await 会让连接任务无法 spawn。）
        let found = {
            let devices = self.inner.devices.lock().unwrap();
            devices.get(&device_id).map(|d| (d.name.clone(), d.tx.clone()))
        };
        let Some((device_name, host_tx)) = found else {
            send_error(&mut stream, ErrCode::Offline, "被控端不在线").await;
            return Ok(());
        };

        let session = gen_session();
        {
            let mut sessions = self.inner.sessions.lock().unwrap();
            sessions.insert(
                session.clone(),
                Session {
                    device_id: device_id.clone(),
                    client_name: client_name.clone(),
                    created: Instant::now(),
                    channels: HashMap::new(),
                },
            );
        }

        // 通知被控端拨号。发不出去说明它刚好掉了。
        let incoming = ToEndpoint::Incoming {
            session: session.clone(),
            client_name: client_name.clone(),
            client_addr: peer.to_string(),
        };
        if host_tx.send(incoming).is_err() {
            self.inner.sessions.lock().unwrap().remove(&session);
            send_error(&mut stream, ErrCode::Offline, "被控端不在线").await;
            return Ok(());
        }

        info!(device_id = %device_id, %peer, client = %client_name, session = %session, "发起会话");

        write_frame(
            &mut stream,
            FrameKind::Json,
            &json_bytes(&ToEndpoint::ClientReady { session, device_id, device_name }),
        )
        .await?;
        Ok(())
    }

    async fn handle_query(&self, mut stream: TcpStream, device_id: &str) -> Result<()> {
        if !is_valid_device_id(device_id) {
            send_error(&mut stream, ErrCode::BadDeviceId, "设备 ID 必须是 9 位数字且首位非 0").await;
            return Ok(());
        }
        let (online, name) = {
            let devices = self.inner.devices.lock().unwrap();
            match devices.get(device_id) {
                Some(d) => (true, Some(d.name.clone())),
                None => (
                    false,
                    self.inner.book.lock().unwrap().get(device_id).map(|s| s.to_string()),
                ),
            }
        };
        write_frame(
            &mut stream,
            FrameKind::Json,
            &json_bytes(&ToEndpoint::QueryResult { device_id: device_id.to_string(), online, name }),
        )
        .await?;
        Ok(())
    }

    async fn handle_relay_open(
        &self,
        stream: TcpStream,
        session_id: &str,
        channel: Channel,
        role: Role,
    ) -> Result<()> {
        stream.set_nodelay(true).ok();
        /// 锁内只做簿记，锁外才做 IO。
        /// `std::sync::MutexGuard` 不是 `Send`，持有它跨 await 会让整个连接任务无法 `tokio::spawn`。
        enum Outcome {
            Missing,
            Duplicate,
            Waiting,
            Paired(TcpStream, TcpStream),
        }

        // 用 Option 持有这条流：簿记时可能把它存进 map（或被配对取走），
        // 出错分支才需要把它拿回来写错误帧。
        let mut held = Some(stream);
        let outcome = {
            let mut sessions = self.inner.sessions.lock().unwrap();
            match sessions.get_mut(session_id) {
                None => Outcome::Missing,
                Some(s) => {
                    let slot = s.channels.entry(channel).or_default();
                    let mine = match role {
                        Role::Host => &mut slot.host,
                        Role::Client => &mut slot.client,
                    };
                    if mine.is_some() {
                        Outcome::Duplicate
                    } else {
                        *mine = held.take();
                        slot.since.get_or_insert_with(Instant::now);
                        // 只有两边都到齐才能 take()。先到的一方必须留在槽里继续持有这条连接：
                        // 早先写成无条件 `(slot.host.take(), slot.client.take())`，
                        // 先到的那条流会在未配对的 match 分支里被当成临时值丢掉，
                        // 对端立刻收到 EOF —— 表现为「被控端先拨号就必然失败」。
                        if slot.host.is_some() && slot.client.is_some() {
                            let h = slot.host.take().expect("host 槽刚判过非空");
                            let c = slot.client.take().expect("client 槽刚判过非空");
                            s.channels.remove(&channel);
                            Outcome::Paired(h, c)
                        } else {
                            Outcome::Waiting
                        }
                    }
                }
            }
        };

        match outcome {
            Outcome::Missing => {
                if let Some(mut s) = held {
                    send_error(&mut s, ErrCode::BadSession, "会话票据不存在或已过期").await;
                }
                Ok(())
            }
            Outcome::Duplicate => {
                if let Some(mut s) = held {
                    send_error(&mut s, ErrCode::DuplicateChannel, "同一通道重复拨号").await;
                }
                Ok(())
            }
            Outcome::Waiting => {
                debug!(session = %session_id, ?channel, ?role, "等待对端拨号");
                Ok(())
            }
            Outcome::Paired(mut h, mut c) => {
                // 先各写一个 RelayReady，之后这条连接就是纯字节流。
                let ready = json_bytes(&ToEndpoint::RelayReady { channel });
                if write_frame(&mut h, FrameKind::Json, &ready).await.is_err() {
                    return Ok(());
                }
                if write_frame(&mut c, FrameKind::Json, &ready).await.is_err() {
                    return Ok(());
                }
                info!(session = %session_id, ?channel, "通道已配对，开始盲转发");
                tokio::spawn(async move {
                    match copy_bidirectional(&mut h, &mut c).await {
                        Ok((a, b)) => debug!(?channel, up = a, down = b, "通道关闭"),
                        Err(e) => debug!(?channel, error = %e, "通道异常结束"),
                    }
                });
                Ok(())
            }
        }
    }

    fn persist_book(&self) {
        let Some(p) = &self.inner.state_path else { return };
        let book = self.inner.book.lock().unwrap().clone();
        if let Err(e) = book.save(p) {
            warn!(path = %p.display(), error = %e, "设备台账写入失败");
        }
    }

    /// 测试/运维用：当前在线设备数。
    pub fn online_count(&self) -> usize {
        self.inner.devices.lock().unwrap().len()
    }
}

async fn send_error(stream: &mut TcpStream, code: ErrCode, message: &str) {
    let msg = ToEndpoint::Error { code, message: message.to_string() };
    let _ = write_frame(stream, FrameKind::Json, &json_bytes(&msg)).await;
}

fn json_bytes(v: &ToEndpoint) -> Vec<u8> {
    serde_json::to_vec(v).unwrap_or_else(|_| b"{}".to_vec())
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 便捷函数：读到 RelayReady 为止（两端建立 relay 连接后调用）。
pub async fn read_relay_ready(stream: &mut TcpStream) -> Result<Channel> {
    let f = read_frame(stream).await?;
    let m: ToEndpoint = f.decode_json()?;
    match m {
        ToEndpoint::RelayReady { channel } => Ok(channel),
        other => anyhow::bail!("期望 relay_ready，收到 {other:?}"),
    }
}

/// 把任意 [`Frame`] 写进流里（两端工具函数共用）。
pub async fn send_frame(stream: &mut TcpStream, frame: &Frame) -> Result<()> {
    write_frame(stream, frame.kind, &frame.payload).await?;
    Ok(())
}