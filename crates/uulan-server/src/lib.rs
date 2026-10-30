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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use uulan_protocol::frame::{read_frame, write_frame, Frame, FrameKind};
use uulan_protocol::redact;
use uulan_protocol::rv::{
    gen_device_id, gen_session, is_valid_device_id, Channel, DeviceEntry, ErrCode, Role, ToEndpoint,
    ToServer,
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

/// 一台被控端「多久没消息就不算在线」。
///
/// 为什么需要它：注册表原来只靠「推送通道关了没有」判在线。网络被切成半开
/// （拔网线、机器休眠、NAT 表项掉了）时那条 TCP 连接在服务端仍然是「开着」的，
/// 于是这台早就不在的机器会长时间显示在线 —— 控制端点上去要等到握手 15 秒超时
/// 才知道没人应答，而且它每次重连都会先撞一遍。
///
/// 45 秒 = 被控端心跳（10 秒一次）连丢四次的量级：偶尔丢一两拍不会误判，
/// 真的没消息了也能在一分钟内从列表里下去。
const DEVICE_STALE_TTL: Duration = Duration::from_secs(45);

/// 一台已上线的被控端。
struct Device {
    name: String,
    /// 向这台被控端的控制连接推送消息。被控端断开时 send 失败 → 视为离线。
    tx: mpsc::UnboundedSender<ToEndpoint>,
    last_seen: Instant,
    /// 注册令牌的哈希（`None` = 这台机器是旧版本，没给令牌）。
    ///
    /// 只在「这个 device_id 已经有人注册着」时用得上：出示的哈希与表里一致才认作
    /// 同一台机器重连。没有它，重连只能被当成「别人来占位」而换个新 ID ——
    /// 现场就是「中继重启一次，设备 ID 变了」。
    reg_token: Option<String>,
    /// 这条注册的流水号。
    ///
    /// 同一台设备重连时，旧连接的读循环迟早也会结束 —— 它结束时若按 device_id 盲删，
    /// 就会把**刚上线的新注册**一起删掉（现象：重连成功、随即显示离线）。
    /// 所以收尾要看「表里那条还是不是我这条」。
    conn: u64,
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
    /// 注册流水号的发号器（只为区分「同一 ID 的不同连接」，不是对外的东西）。
    next_conn: AtomicU64,
    sessions: Mutex<HashMap<String, Session>>,
    book: Mutex<DeviceBook>,
    state_path: Option<PathBuf>,
    /// 允不允许控制端拉设备目录（`--no-list` 关掉）。
    ///
    /// 目录只是「有哪些机器、叫什么名字、在不在线」，没有任何凭据；
    /// 但它确实把内网的机器清单一次交出去，所以给一个能关的开关。
    allow_list: bool,
}

/// 服务器句柄。`Clone` 之后共享同一份状态。
#[derive(Clone)]
pub struct Server {
    inner: Arc<Inner>,
}

impl Server {
    pub fn new(state_path: Option<PathBuf>) -> Result<Self> {
        Self::new_with_list(state_path, true)
    }

    /// 同 [`Server::new`]，但可以关掉设备目录（对应 `uulan-server --no-list`）。
    ///
    /// 为什么是构造参数而不是运行时开关：`allow_list` 在 `Arc<Inner>` 里，
    /// 共享出去之后就改不动（`Arc::get_mut` 拿不到）。而且「一台中继开不开目录」
    /// 本来就是部署决策，不是运行期状态。
    pub fn new_with_list(state_path: Option<PathBuf>, allow_list: bool) -> Result<Self> {
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
                next_conn: AtomicU64::new(1),
                sessions: Mutex::new(HashMap::new()),
                book: Mutex::new(book),
                state_path,
                allow_list,
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

    /// 这台设备是不是已经「沉默太久」了。
    ///
    /// 抽成纯函数（`now` 由调用方给）是为了能在单测里把时间往后推 —— 真去睡 45 秒
    /// 的测试没人会跑。
    fn device_is_stale(last_seen: Instant, now: Instant) -> bool {
        now.saturating_duration_since(last_seen) > DEVICE_STALE_TTL
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
                // 票据是 60 秒有效的 bearer token：日志里只能出现不可逆标签（`tXXXXXXXX`），
                // 与「发起会话」（下发那侧）和「有人要连进来」（被控端那侧）用的是同一套标签，
                // 排障时仍然能把三条日志对上。原值在这里泄露过一版，本轮的日志脱敏验收抓到了。
                debug!(
                    session = %redact::tag(sid),
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

        // 控制连接断了但没人来清理的情况：靠 mpsc 的 closed() 判断；半开连接靠 last_seen。
        let mut devices = self.inner.devices.lock().unwrap();
        devices.retain(|id, d| {
            let alive = !d.tx.is_closed();
            if !alive {
                info!(device_id = %id, "被控端离线");
                return false;
            }
            if Self::device_is_stale(d.last_seen, now) {
                // 只摘「在线状态」，**不动已有会话**：正在被控的那条 relay 通路不经过这张表，
                // 摘掉注册只是让**新的**连接看到「离线」，不会把在场的人踢下线。
                warn!(
                    device_id = %id,
                    silent_s = now.saturating_duration_since(d.last_seen).as_secs(),
                    ttl_s = DEVICE_STALE_TTL.as_secs(),
                    "被控端超过时限没有任何消息（半开连接？），按离线处理"
                );
                return false;
            }
            true
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
            ToServer::HostRegister { device_id, name, proto, reg_token } => {
                self.handle_host_register(stream, peer, device_id, name, proto, reg_token).await
            }
            ToServer::ClientConnect { device_id, client_name, proto } => {
                self.handle_client_connect(stream, peer, device_id, client_name, proto).await
            }
            ToServer::Query { device_id } => self.handle_query(stream, &device_id).await,
            ToServer::ListDevices => self.handle_list_devices(stream).await,
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
        reg_token: Option<String>,
    ) -> Result<()> {
        let presented = reg_token.as_deref().map(uulan_protocol::rv::reg_token_hash);
        if proto != PROTO_VERSION {
            send_error(&mut stream, ErrCode::ProtoMismatch, &format!("服务端协议 v{PROTO_VERSION}，被控端 v{proto}")).await;
            return Ok(());
        }

        // 决定这个被控端用哪个 device_id：给过来的合法且没被别人占用就用它，
        // 否则分配一个新的。这样被控端重装/丢配置后不会把 ID 用串。
        let id = {
            let devices = self.inner.devices.lock().unwrap();
            // 用 as_ref + cloned：Option::filter 会吃掉所有权，后面还要用 device_id 判非法值。
            //
            // 「这个 ID 能不能给我」有三种情形：
            //  · 空着 → 照收；
            //  · 已经有人注册，但**对方出示的注册令牌哈希与表里那条一致** → 同一台机器
            //    重连，照收并替换旧注册（旧连接的推送通道被丢弃 → 它读到 EOF 自行退出）；
            //  · 已经有人注册、令牌对不上或没给 → 不能顶掉别人，分配新 ID。
            //
            // 第三条是原有的安全边界（`rvclient.rs` 里那条「在线设备的 ID 不能被后来者
            // 顶掉」把它钉住了），第一条是重连能不能拿回自己身份的关键。以前只有第一、
            // 第三条，于是被控端每次重连都可能被换成一个新 ID 并写回 host.json ——
            // 现场手里那个设备 ID 当场失效。
            let mut refused_busy = None;
            let requested = device_id
                .as_ref()
                .filter(|d| is_valid_device_id(d))
                .filter(|d| match devices.get(d.as_str()) {
                    None => true,
                    Some(cur) => {
                        let same = match (cur.reg_token.as_deref(), presented.as_deref()) {
                            (Some(a), Some(b)) => a == b,
                            _ => false,
                        };
                        if !same {
                            refused_busy = Some((*d).clone());
                        }
                        same
                    }
                })
                .cloned();
            if let Some(busy) = &refused_busy {
                warn!(%peer, device_id = %busy, "这个设备 ID 已有人注册且注册令牌不符，改为分配新 ID");
            }
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
        let conn_no = self.inner.next_conn.fetch_add(1, Ordering::Relaxed);

        {
            let mut devices = self.inner.devices.lock().unwrap();
            if let Some(old) = devices.insert(
                id.clone(),
                Device {
                    name: name.clone(),
                    tx: tx.clone(),
                    last_seen: Instant::now(),
                    reg_token: presented.clone(),
                    conn: conn_no,
                },
            ) {
                // 同一台设备重新上线（重连 / 上次异常退出后残留）：踢掉旧的推送通道。
                // 旧连接的读循环随后读到 EOF 自行退出，表里只留新注册。
                drop(old);
                warn!(device_id = %id, "同一 device_id 重新注册（重连），旧注册已被替换");
            }
            self.inner.book.lock().unwrap().remember(&id, &name);
        }
        // 注册表里的 tx 是唯一剩下的发送端（下面 push 任务的 rx 才能收到「没有发送端了」
        // → 旧连接被替换时 push 退出、写半边关闭、对端读到 EOF）。留着这个局部 tx 的话，
        // 旧注册永远关不掉，表里就会挂着一个谁也连不到的死设备。
        drop(tx);
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
        {
            // **只摘自己这一条**：重连时旧连接的收尾不能把新注册踢掉。
            let mut devices = self.inner.devices.lock().unwrap();
            let mine = devices.get(&id).map(|d| d.conn == conn_no).unwrap_or(false);
            if mine {
                devices.remove(&id);
                info!(device_id = %id, "被控端控制连接断开");
            } else {
                info!(device_id = %id, "旧控制连接收尾（该 ID 已由新连接接管，不动注册表）");
            }
        }
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
        // 票据是 60 秒有效的 bearer token：日志里只留不可逆标签，两端的标签能对上。
        let session_tag = redact::tag(&session);
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

        info!(device_id = %device_id, %peer, client = %client_name, session = %session_tag, "发起会话");

        write_frame(
            &mut stream,
            FrameKind::Json,
            &json_bytes(&ToEndpoint::ClientReady { session, device_id, device_name }),
        )
        .await?;
        Ok(())
    }

    /// 设备目录：在线注册表 ∪ 台账，按 ID 升序。
    ///
    /// 抽成纯函数（`now` 由调用方给）是为了把「在线判定」与「台账补全」分开测 ——
    /// 真去等 45 秒 TTL 的测试没人会跑。
    fn inventory_at(
        devices: &HashMap<String, Device>,
        book: &DeviceBook,
        now: Instant,
    ) -> Vec<DeviceEntry> {
        let mut out: Vec<DeviceEntry> = Vec::with_capacity(devices.len() + book.devices.len());
        for (id, d) in devices {
            // 判据与 `sweep` 完全一致：推送通道关了（对端没了），或者沉默超过 TTL
            // （半开连接）。两处必须同一套，否则会出现「列表显示在线、连过去却说离线」。
            let online = !d.tx.is_closed() && !Self::device_is_stale(d.last_seen, now);
            out.push(DeviceEntry {
                device_id: id.clone(),
                name: d.name.clone(),
                online,
                idle_s: online.then(|| now.saturating_duration_since(d.last_seen).as_secs()),
            });
        }
        for (id, name) in book.list() {
            if !devices.contains_key(&id) {
                out.push(DeviceEntry { device_id: id, name, online: false, idle_s: None });
            }
        }
        out.sort_by(|a, b| a.device_id.cmp(&b.device_id));
        out
    }

    fn inventory(&self) -> Vec<DeviceEntry> {
        let now = Instant::now();
        let devices = self.inner.devices.lock().unwrap();
        let book = self.inner.book.lock().unwrap();
        Self::inventory_at(&devices, &book, now)
    }

    async fn handle_list_devices(&self, mut stream: TcpStream) -> Result<()> {
        if !self.inner.allow_list {
            send_error(
                &mut stream,
                ErrCode::Denied,
                "这台中继没有开放设备目录（服务端用 --no-list 关掉了）",
            )
            .await;
            return Ok(());
        }
        let devices = self.inventory();
        let online = devices.iter().filter(|d| d.online).count();
        info!(total = devices.len(), online, "设备目录查询");
        write_frame(
            &mut stream,
            FrameKind::Json,
            &json_bytes(&ToEndpoint::DeviceList { devices }),
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
        // 同上：票据原值一律不进日志，只留不可逆标签。
        let session_tag = redact::tag(session_id);
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
                debug!(session = %session_tag, ?channel, ?role, "等待对端拨号");
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
                info!(session = %session_tag, ?channel, "通道已配对，开始盲转发");
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一台「`age` 秒之前有过消息」的设备，并把接收端**留在调用方手里**：
    /// 留着它，推送通道就是「开着的」—— 这正是半开连接的样子（服务端看不出对端没了）。
    fn device_with_age(age: Duration) -> (Device, mpsc::UnboundedReceiver<ToEndpoint>) {
        let (tx, rx) = mpsc::unbounded_channel();
        // 用 `checked_sub` 而不是 `-`：`Instant` 的零点是系统定的（Windows 上是开机时刻），
        // 开机不足 `age` 时直接相减会 panic。
        let last_seen = Instant::now()
            .checked_sub(age)
            .expect("系统运行时间比这个测试的 age 还短（开机一分钟之后再跑）");
        (
            Device { name: "test".into(), tx, last_seen, reg_token: None, conn: 1 },
            rx,
        )
    }

    fn book_of(rows: &[(&str, &str)]) -> DeviceBook {
        let mut b = DeviceBook::default();
        for (id, name) in rows {
            b.remember(id, name);
        }
        b
    }

    #[test]
    fn inventory_merges_live_registrations_with_the_book() {
        let (live, _rx_live) = device_with_age(Duration::from_secs(2));
        let (silent, _rx_silent) = device_with_age(Duration::from_secs(120));
        let mut devices = HashMap::new();
        devices.insert("805747189".to_string(), live);
        devices.insert("805747111".to_string(), silent);
        let book = book_of(&[
            ("805747189", "麒麟-mis-pc"),
            // 台账里故意写一个与注册表不同的名字：注册表里的显示名优先。
            ("805747111", "台账里的旧名字"),
            ("805747000", "早就关掉的机器"),
        ]);

        let inv = Server::inventory_at(&devices, &book, Instant::now());
        // 三台：两台在注册表、一台只在台账里；按 ID 升序。
        assert_eq!(inv.len(), 3);
        // 000 只在台账里；111 在注册表但沉默 120 秒（> TTL）；189 在注册表且刚心跳过。
        assert_eq!(inv[0].device_id, "805747000");
        assert!(!inv[0].online, "只在台账里的设备必须报离线");
        assert_eq!(inv[0].idle_s, None, "离线条目不该编一个空闲秒数");

        assert_eq!(inv[1].device_id, "805747111");
        assert!(!inv[1].online, "沉默超过 TTL 的注册不算在线");
        // 名字以**注册表**里的为准（设备自己上报的），台账只在它没注册时兜底。
        assert_eq!(inv[1].name, "test", "注册表里的显示名必须优先于台账：{}", inv[1].name);
        assert_eq!(inv[1].idle_s, None);

        assert_eq!(inv[2].device_id, "805747189");
        assert!(inv[2].online);
        assert_eq!(inv[2].idle_s, Some(2));
    }

    #[test]
    fn inventory_is_empty_when_nothing_ever_registered() {
        let inv = Server::inventory_at(&HashMap::new(), &DeviceBook::default(), Instant::now());
        assert!(inv.is_empty());
    }

    #[test]
    fn a_closed_registration_shows_offline_but_stays_in_the_book() {
        let (tx, rx) = mpsc::unbounded_channel::<ToEndpoint>();
        drop(rx); // 推送通道关了 = 对端没了
        let mut devices = HashMap::new();
        devices.insert(
            "805747189".to_string(),
            Device { name: "刚断开".into(), tx, last_seen: Instant::now(), reg_token: None, conn: 1 },
        );
        let book = book_of(&[("805747189", "刚断开")]);
        let inv = Server::inventory_at(&devices, &book, Instant::now());
        assert_eq!(inv.len(), 1, "同一台机器不该因为既在注册表又在台账里出现两次");
        assert!(!inv[0].online);
        assert_eq!(inv[0].idle_s, None);
    }

    #[test]
    fn silence_past_the_ttl_counts_as_stale() {
        let now = Instant::now();
        assert!(!Server::device_is_stale(now, now), "刚说完话的不算");
        assert!(
            !Server::device_is_stale(now, now + DEVICE_STALE_TTL),
            "刚好等于 TTL 还不算（边界是「超过」）"
        );
        assert!(Server::device_is_stale(
            now,
            now + DEVICE_STALE_TTL + Duration::from_millis(1)
        ));
    }

    #[test]
    fn sweep_drops_a_half_open_registration_and_keeps_the_live_one() {
        let s = Server::new(None).expect("建服务器句柄失败");
        let (silent, _silent_rx) = device_with_age(DEVICE_STALE_TTL + Duration::from_secs(5));
        let (fresh, _fresh_rx) = device_with_age(Duration::from_secs(1));
        {
            let mut d = s.inner.devices.lock().unwrap();
            d.insert("111111111".into(), silent);
            d.insert("222222222".into(), fresh);
        }
        assert_eq!(s.online_count(), 2);

        s.sweep();

        let d = s.inner.devices.lock().unwrap();
        assert!(
            !d.contains_key("111111111"),
            "半开连接（推送通道还开着、但一直没消息）必须按离线摘掉"
        );
        assert!(
            d.contains_key("222222222"),
            "还在心跳的设备绝不能被误判 —— 那会把在线用户踢出列表"
        );
    }

    #[test]
    fn a_registration_with_a_closed_channel_is_offline_regardless_of_age() {
        // 老路径不能因为新规则而退化：通道关了就是离线，不用等 TTL
        let s = Server::new(None).expect("建服务器句柄失败");
        let (dev, rx) = device_with_age(Duration::from_secs(1));
        drop(rx);
        s.inner.devices.lock().unwrap().insert("333333333".into(), dev);
        s.sweep();
        assert_eq!(s.online_count(), 0);
    }
}
