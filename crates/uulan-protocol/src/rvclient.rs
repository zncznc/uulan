//! 会合协议的**客户端**实现（被控端和控制端共用）。
//!
//! 服务端的会合逻辑在 `uulan-server` 里；这里是它对面那一半，
//! 两端都要用同一份实现，免得各写一遍、各自踩一遍坑。
//!
//! 一个重要细节：`relay_open` 会**另开一条 TCP 连接**，而不是复用控制连接。
//! 三条通道各有独立连接，控制连接只负责注册与收「有人要连你」的通知 ——
//! 这样视频或文件的大流量不会把控制消息堵住。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};

use crate::frame::{read_frame, write_frame, FrameKind};
use crate::rv::{Channel, DeviceEntry, ErrCode, Role, ToEndpoint, ToServer};
use crate::PROTO_VERSION;

#[derive(Debug, thiserror::Error)]
pub enum RvError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("协议错误: {0}")]
    Proto(#[from] crate::frame::FrameError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("服务端拒绝: {code:?} {message}")]
    Server { code: ErrCode, message: String },
    #[error("连接超时（{0:?}）")]
    Timeout(Duration),
    #[error("期望 {expected}，收到 {got}")]
    Unexpected { expected: &'static str, got: String },
}

/// 一条到中继服务器的连接。
pub struct RvConn {
    stream: TcpStream,
    addr: String,
    peer: Option<SocketAddr>,
    timeout: Duration,
}

impl RvConn {
    /// 连上中继服务器。
    pub async fn connect(addr: &str) -> Result<Self, RvError> {
        Self::connect_with_timeout(addr, Duration::from_secs(10)).await
    }

    pub async fn connect_with_timeout(addr: &str, timeout: Duration) -> Result<Self, RvError> {
        let stream = match tokio::time::timeout(timeout, TcpStream::connect(addr)).await {
            Err(_) => return Err(RvError::Timeout(timeout)),
            Ok(r) => r?,
        };
        // 控制消息都是小包且延迟敏感，必须关掉 Nagle。
        stream.set_nodelay(true).ok();
        let peer = stream.peer_addr().ok();
        Ok(Self { stream, addr: addr.to_string(), peer, timeout })
    }

    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.peer
    }

    pub fn server_addr(&self) -> &str {
        &self.addr
    }

    pub async fn send(&mut self, m: &ToServer) -> Result<(), RvError> {
        write_frame(&mut self.stream, FrameKind::Json, &serde_json::to_vec(m)?).await?;
        Ok(())
    }

    /// 收一条服务端推送。把 `Error` 也当成一条消息返回（由调用方决定怎么处理）。
    pub async fn recv(&mut self) -> Result<ToEndpoint, RvError> {
        let f = read_frame(&mut self.stream).await?;
        if f.kind != FrameKind::Json {
            return Err(RvError::Unexpected {
                expected: "json 帧",
                got: format!("{:?}", f.kind),
            });
        }
        Ok(serde_json::from_slice(&f.payload)?)
    }

    /// 收一条推送，但把 `Error` 转成 `Err`。
    pub async fn recv_ok(&mut self) -> Result<ToEndpoint, RvError> {
        match self.recv().await? {
            ToEndpoint::Error { code, message } => Err(RvError::Server { code, message }),
            ok => Ok(ok),
        }
    }

    /// 发心跳并等回应。返回服务端时间（毫秒）。
    pub async fn ping(&mut self) -> Result<u64, RvError> {
        self.send(&ToServer::Ping).await?;
        match self.recv_ok().await? {
            ToEndpoint::Pong { server_time_ms } => Ok(server_time_ms),
            other => Err(RvError::Unexpected { expected: "Pong", got: format!("{other:?}") }),
        }
    }

    /// 被控端注册。返回 `(device_id, 是否由服务端新分配)`。
    /// 注册进中继。`reg_token` 是这台机器的注册令牌（见 [`ToServer::HostRegister`]）：
    /// 带着它，重连时服务端才会把这个 device_id 还给**同一台机器**。
    pub async fn host_register(
        &mut self,
        device_id: Option<String>,
        name: String,
        reg_token: Option<String>,
    ) -> Result<(String, bool), RvError> {
        let asked = device_id.clone();
        self.send(&ToServer::HostRegister { device_id, name, proto: PROTO_VERSION, reg_token })
            .await?;
        match self.recv_ok().await? {
            ToEndpoint::HostWelcome { device_id, .. } => {
                // 服务端在「给的 ID 不合法或已被占用」时会换一个新的。
                let fresh = asked.as_deref() != Some(device_id.as_str());
                Ok((device_id, fresh))
            }
            other => Err(RvError::Unexpected { expected: "HostWelcome", got: format!("{other:?}") }),
        }
    }

    /// 控制端请求连到某台设备。返回 `(session, device_id, device_name)`。
    pub async fn client_connect(
        &mut self,
        device_id: &str,
        client_name: &str,
    ) -> Result<(String, String, String), RvError> {
        self.send(&ToServer::ClientConnect {
            device_id: device_id.to_string(),
            client_name: client_name.to_string(),
            proto: PROTO_VERSION,
        })
        .await?;
        match self.recv_ok().await? {
            ToEndpoint::ClientReady { session, device_id, device_name } => {
                Ok((session, device_id, device_name))
            }
            other => Err(RvError::Unexpected { expected: "ClientReady", got: format!("{other:?}") }),
        }
    }

    /// 查询设备是否在线。
    pub async fn query(&mut self, device_id: &str) -> Result<(bool, Option<String>), RvError> {
        self.send(&ToServer::Query { device_id: device_id.to_string() }).await?;
        match self.recv_ok().await? {
            ToEndpoint::QueryResult { online, name, .. } => Ok((online, name)),
            other => Err(RvError::Unexpected { expected: "QueryResult", got: format!("{other:?}") }),
        }
    }

    /// 拉一份设备目录（在线 + 台账里登记过的），按 `device_id` 升序。
    ///
    /// 不发起会话、也不需要设备口令：这是「我该连哪台」的查询。
    /// 服务端用 `--no-list` 关掉时返回 [`RvError::Server`]（`ErrCode::Denied`），
    /// 调用方应当把它当成「这台中继没开目录」而不是错误日志里的 bug。
    pub async fn list_devices(&mut self) -> Result<Vec<DeviceEntry>, RvError> {
        self.send(&ToServer::ListDevices).await?;
        match self.recv_ok().await? {
            ToEndpoint::DeviceList { devices } => Ok(devices),
            other => Err(RvError::Unexpected { expected: "DeviceList", got: format!("{other:?}") }),
        }
    }

    /// 把已注册的会合连接拆成**读写两半**。
    ///
    /// 为什么必须拆：被控端上线后，这条连接上是双向同时跑消息的 ——
    /// 服务端会主动推 `Incoming`（有人要连你），被控端会周期性发 `Ping`。
    /// 如果用一个 `select!` 同时等「读一条推送」和「心跳定时器」，有两个坑：
    ///
    /// 1. **`select!` 会丢弃没被选中的那个 future**。定时器一响，正在读到一半的
    ///    `recv()` 被直接扔掉，已经吃进缓冲区的字节就永远丢了，后续解析全部错位。
    ///    `read_exact` 不是取消安全的，这一点没法靠加容错绕过。
    /// 2. **`Ping` 若要等 `Pong`，就必须独占读取权**，那么它必然会撞上插进来的
    ///    `Incoming`，并把后者当成协议违规 —— 表现为「控制端一发起连接，被控端
    ///    自己判自己违规然后退出」。
    ///
    /// 拆开之后就再也没有这两个问题：读端只有一个循环、永不取消；写端独立，
    /// 只写不读；`Pong` 到了由读端记录，没有人再死等它。
    pub fn into_split(self) -> (RvReader, RvWriter) {
        let (rd, wr) = self.stream.into_split();
        (RvReader { stream: rd }, RvWriter { stream: wr })
    }

    /// 拨出一条 relay 通道但**不等对端**。
    ///
    /// 「先拨号、之后再让对端拨」是合法且常见的时序（被控端收到 Incoming 就立刻拨，
    /// 控制端可能还在路上）。用 [`RvConn::relay_open`] 做这件事会自我死锁：
    /// 它要等对端到齐才返回，而调用方正等着它返回才去连对端。
    pub async fn relay_dial(
        &self,
        session: &str,
        channel: Channel,
        role: Role,
    ) -> Result<TcpStream, RvError> {
        let mut s = match tokio::time::timeout(self.timeout, TcpStream::connect(&self.addr)).await {
            Err(_) => return Err(RvError::Timeout(self.timeout)),
            Ok(r) => r?,
        };
        s.set_nodelay(true).ok();
        write_frame(
            &mut s,
            FrameKind::Json,
            &serde_json::to_vec(&ToServer::RelayOpen {
                session: session.to_string(),
                channel,
                role,
            })?,
        )
        .await?;
        Ok(s)
    }

    /// 拨出一条 relay 通道并等 `RelayReady`（即等对端也到齐）。
    ///
    /// 返回的是**裸 TCP 流**：从这一刻起这条连接上跑的就是 uulan 帧了，
    /// 服务端在中间只做字节转发。
    pub async fn relay_open(
        &self,
        session: &str,
        channel: Channel,
        role: Role,
    ) -> Result<TcpStream, RvError> {
        let mut s = self.relay_dial(session, channel, role).await?;
        await_relay_ready(&mut s).await?;
        Ok(s)
    }

    /// 一次拨齐四条通道，并等四条都配对完成。
    ///
    /// 两端都要调用它；它会阻塞到对端也拨号为止。
    pub async fn relay_open_all(
        &self,
        session: &str,
        role: Role,
    ) -> Result<RelaySet, RvError> {
        // 并发拨出四条通道、再并发等 ready：
        // 过去四条串行 `relay_dial().await?` 会把 4 次 TCP 三次握手串起来（高 RTT 下多烧 3 个单腿 RTT），
        // `try_join!` 让四条 SYN 与 RelayOpen 同时在途，零协议改动、100% 兼容老端与老中继。
        //
        // **音频通道也在这里一次拨齐**（M7）。做成「要用了再拨」的话，两端都得
        // 在会话中途协商谁先拨、谁等谁，多出一整条状态机；而多一条空闲 TCP
        // 连接的代价是可以忽略的（内网 RTT < 1ms，不传就一个字节都没有）。
        // 用不用它由控制端的 `C2H::AudioStart` 决定 —— 通道开着 ≠ 有声音在传。
        let (mut control, mut video, mut bulk, mut audio) = tokio::try_join!(
            self.relay_dial(session, Channel::Control, role),
            self.relay_dial(session, Channel::Video, role),
            self.relay_dial(session, Channel::Bulk, role),
            self.relay_dial(session, Channel::Audio, role),
        )?;
        tokio::try_join!(
            await_relay_ready(&mut control),
            await_relay_ready(&mut video),
            await_relay_ready(&mut bulk),
            await_relay_ready(&mut audio),
        )?;
        Ok(RelaySet { control, video, bulk, audio })
    }

    /// 拆出底层流（需要自己读循环时用）。
    pub fn into_stream(self) -> TcpStream {
        self.stream
    }

    pub fn stream_mut(&mut self) -> &mut TcpStream {
        &mut self.stream
    }
}

/// 在一条已拨出的 relay 流上等服务器的 `RelayReady`。
///
/// 服务器只在**两条**（host + client）都到齐后才下发它，所以这一步等价于
/// 「确认对端也已经拨号」。
pub async fn await_relay_ready(stream: &mut TcpStream) -> Result<Channel, RvError> {
    let f = read_frame(stream).await?;
    match serde_json::from_slice::<ToEndpoint>(&f.payload)? {
        ToEndpoint::RelayReady { channel } => Ok(channel),
        ToEndpoint::Error { code, message } => Err(RvError::Server { code, message }),
        other => Err(RvError::Unexpected { expected: "RelayReady", got: format!("{other:?}") }),
    }
}

/// 一次会话的四条通道。
pub struct RelaySet {
    pub control: TcpStream,
    pub video: TcpStream,
    pub bulk: TcpStream,
    /// 音频（M7）。控制端不开 `--audio` 时它只是一条空闲连接。
    pub audio: TcpStream,
}

// ───────────────────────── 会合连接的读写两半 ─────────────────────────
//
// 见 [`RvConn::into_split`] 的说明：上线之后这条连接是双向同时跑消息的，
// 必须由**两个互不干扰的实体**分别负责读和写。

/// 会合连接的**只读半边**。
///
/// 一个连接只能有一个 `RvReader`，而且必须由**一个循环独占**它 ——
/// 任何 `select!` 或超时包装都会在取消时丢弃读到一半的字节。
pub struct RvReader {
    stream: tokio::net::tcp::OwnedReadHalf,
}

/// 会合连接的**只写半边**。
///
/// 只写不读，因此可以安全地放在心跳任务里周期性地调用，不会和读端抢数据。
pub struct RvWriter {
    stream: tokio::net::tcp::OwnedWriteHalf,
}

impl RvReader {
    /// 收一条服务端推送。把 `Error` 也当成一条消息返回（由调用方决定怎么处理）。
    pub async fn recv(&mut self) -> Result<ToEndpoint, RvError> {
        let f = read_frame(&mut self.stream).await?;
        if f.kind != FrameKind::Json {
            return Err(RvError::Unexpected {
                expected: "json 帧",
                got: format!("{:?}", f.kind),
            });
        }
        Ok(serde_json::from_slice(&f.payload)?)
    }

    /// 收一条推送，但把 `Error` 转成 `Err`。
    pub async fn recv_ok(&mut self) -> Result<ToEndpoint, RvError> {
        match self.recv().await? {
            ToEndpoint::Error { code, message } => Err(RvError::Server { code, message }),
            ok => Ok(ok),
        }
    }
}

impl RvWriter {
    pub async fn send(&mut self, m: &ToServer) -> Result<(), RvError> {
        write_frame(&mut self.stream, FrameKind::Json, &serde_json::to_vec(m)?).await?;
        Ok(())
    }

    /// **只发** `Ping`，不等 `Pong`。
    ///
    /// 早先这里叫 `ping()` 并且会死等 `Pong`，于是任何插进来的服务端推送都会
    /// 被误判成协议违规。回包由读端处理，发的一方不参与。
    pub async fn ping(&mut self) -> Result<(), RvError> {
        self.send(&ToServer::Ping).await
    }
}

// ───────────────────────── 直连模式（不经中继）─────────────────────────
//
// 为什么要有这一条路：物理隔离内网里「IP + 口令」是最顺手的用法（VNC/RDP 都这样），
// 而中继的价值在于跨网段/NAT/不被路由直达的机器。两者不该互相替代，所以这里
// **复用同一套通道与同一套会话代码**，只把「谁在另一端」从中继换成控制端本身：
// 认证（PAKE + 口令/临时码/每用户独立口令）、加密、四通道复用、锁屏判定全都照旧。
//
// 与中继的差别只有两点：
// 1. 没有 `HostRegister`，也就没有设备 ID 路由 —— IP 就是地址；
// 2. 每条通道开头由控制端自己声明是哪条通道（`ToServer::DirectOpen`），
//    被控端回执仍用现成的 `ToEndpoint::RelayReady`，于是 await 逻辑与失败判定
//    都不用重写：指错端口/连到别的HTTP服务，第一帧就报错，不会静默卡住。

/// 拨一条直连通道并等被控端回执。
async fn direct_dial_one(addr: &str, channel: Channel, role: Role) -> Result<TcpStream, RvError> {
    let mut s = TcpStream::connect(addr).await?;
    s.set_nodelay(true).ok();
    write_frame(
        &mut s,
        FrameKind::Json,
        &serde_json::to_vec(&ToServer::DirectOpen { channel, role })?,
    )
    .await?;
    // 复用中继那套：被控端在每条通道上都回一个 RelayReady{channel}，
    // 于是「对端是不是 uulan」这件事在拨号阶段就能确定，而不是等画面黑屏才发现。
    await_relay_ready(&mut s).await?;
    Ok(s)
}

/// 控制端：一次并发拨齐四条直连通道（被控端 `direct_accept_all` 任意顺序归位，语义与 relay_open_all 对齐）。
///
/// 并发拨号可省掉 3 个串行 TCP+回执 RTT，零协议改动、完全兼容老被控端。
pub async fn direct_dial_all(addr: &str) -> Result<RelaySet, RvError> {
    let (control, video, bulk, audio) = tokio::try_join!(
        direct_dial_one(addr, Channel::Control, Role::Client),
        direct_dial_one(addr, Channel::Video, Role::Client),
        direct_dial_one(addr, Channel::Bulk, Role::Client),
        direct_dial_one(addr, Channel::Audio, Role::Client),
    )?;
    Ok(RelaySet { control, video, bulk, audio })
}

/// 被控端：收满四条通道（任何顺序都行，靠各自的声明归位）。
///
/// 为什么必须有期限：直连监听对**任何能到达这个端口的机器**开放，而四条通道是
/// 分批到达的。半套就晾着的对端不会被"读一次"发现 —— intro 已经读完、回执已发，
/// 那条流我们后面再没碰过，所以 EOF 不会自己找上门（这是我一开始写注释时想当然的地方，
/// 被自己的测试当场打回）。因此这里的真实防线就是这一条超时。
///
/// **期限从「第一条通道到达」开始算，而不是从「回到 accept()」开始算。** 这个区别是
/// 三条硬理由，不是口味：
///   · 没人来连是常态（机器闲着、或已经有一条会话在跑）。期限从 accept 起算的话，
///     每 5 秒就报一轮超时 —— 现场日志把"没人来"写成"来了又走"，真事件反而被噪声埋掉；
///   · 恰好在窗口末尾连进来的控制端只剩零点几秒去拨齐另外三条，那是**真丢连接**
///     （真机日志里就是这么刷的：一次会话进行中，每 5 秒一条「直连半途而废」）；
///   · 空闲端口不该每 5 秒被自己唤醒一次。
/// 于是第一条通道无限等，收齐四条才回去接下一轮；期限只用来管「来了一条、另三条迟迟不到」。
///
/// 为什么是 5 秒：真客户端在毫秒级拨齐四条，5 秒已经很宽；而对想占端口的人来说，
/// 他必须每 5 秒重投一次才能持续占位 —— 代价从"永久"降到"每轮 5 秒"。
/// 要更严可以再缩，但再小就要开始怀疑慢速内网了。
pub async fn direct_accept_all(listener: &TcpListener) -> Result<(RelaySet, SocketAddr), RvError> {
    const GATHER_TIMEOUT: Duration = Duration::from_secs(5);
    let (first, addr) = listener.accept().await?;
    tokio::time::timeout(GATHER_TIMEOUT, gather_channels(listener, first, addr))
        .await
        .map_err(|_| RvError::Timeout(GATHER_TIMEOUT))?
}

async fn gather_channels(
    listener: &TcpListener,
    first: TcpStream,
    first_addr: SocketAddr,
) -> Result<(RelaySet, SocketAddr), RvError> {
    let mut got: HashMap<Channel, TcpStream> = HashMap::new();
    let mut peer: Option<SocketAddr> = Some(first_addr);
    // 第一条已经在 accept() 那边收下了，这里先把它当"待处理"用掉，避免多一条分支。
    let mut pending: Option<(TcpStream, SocketAddr)> = Some((first, first_addr));
    while got.len() < 4 {
        let (mut s, addr) = match pending.take() {
            Some(x) => x,
            None => listener.accept().await?,
        };
        s.set_nodelay(true).ok();
        // 单条连接最多给 2 秒发 DirectOpen，防止只连不发的探针吃光整轮 5 秒超时。
        let f = match tokio::time::timeout(Duration::from_secs(2), read_frame(&mut s)).await {
            Ok(Ok(f)) => f,
            Ok(Err(e)) => {
                // 若已有合法通道在等齐（!got.is_empty()），绝不能因为中途插进来的
                // 0 字节探活连接（例如被控端 uulan-gui 的 probe_direct_port）或扫描器
                // 把整轮合法连接连坐 drop 掉（否则控制端直接收到 io: early eof）。
                if !got.is_empty() {
                    continue;
                }
                return Err(e.into());
            }
            Err(_) => {
                if !got.is_empty() {
                    continue;
                }
                return Err(RvError::Timeout(Duration::from_secs(2)));
            }
        };
        let intro: ToServer = match serde_json::from_slice(&f.payload) {
            Ok(v) => v,
            Err(e) => {
                if !got.is_empty() {
                    continue;
                }
                return Err(e.into());
            }
        };
        let channel = match intro {
            ToServer::DirectOpen { channel, role } => {
                // 角色在这里只用于日志与"谁在监听"的直觉检查：直连时被控端是 Host 侧，
                // 对端理应是 Client。真正的凭据判定在会话里的 PAKE，不在这里 ——
                // 所以这条不合规**不能**当致命错误（免得以后改成谁都能自称 Host）。
                if !matches!(role, Role::Client) {
                    if !got.is_empty() {
                        continue;
                    }
                    return Err(RvError::Unexpected {
                        expected: "直连对端角色为 client",
                        got: format!("{role:?}"),
                    });
                }
                channel
            }
            other => {
                if !got.is_empty() {
                    continue;
                }
                return Err(RvError::Unexpected { expected: "direct_open", got: format!("{other:?}") });
            }
        };
        if got.insert(channel, s).is_some() {
            // 同一条通道拨两次：多半是对端重连而旧连接还没散。当场拒绝这一轮，
            // 而不是留下两条流让会话自己碰上"谁在写这条通道"。
            return Err(RvError::Unexpected {
                expected: "每条通道只出现一次",
                got: format!("{channel:?} 重复"),
            });
        }
        if peer.is_none() {
            peer = Some(addr);
        }
        // 回执：告诉控制端这条通道认下了（与中继的 RelayReady 同构）。
        let s = got.get_mut(&channel).unwrap();
        write_frame(
            s,
            FrameKind::Json,
            &serde_json::to_vec(&ToEndpoint::RelayReady { channel })?,
        )
        .await?;
    }
    let mut take = |c: Channel| -> Result<TcpStream, RvError> {
        got.remove(&c).ok_or(RvError::Unexpected {
            expected: "四条通道齐了",
            got: format!("{c:?} 缺"),
        })
    };
    Ok((
        RelaySet {
            control: take(Channel::Control)?,
            video: take(Channel::Video)?,
            bulk: take(Channel::Bulk)?,
            audio: take(Channel::Audio)?,
        },
        peer.expect("有连接就有地址"),
    ))
}

#[cfg(test)]
mod direct_tests {
    use super::*;

    #[tokio::test]
    async fn direct_accept_ignores_interleaved_empty_probes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let accept_task = tokio::spawn(async move { direct_accept_all(&listener).await });

        // 先拨 control，再故意插两条 0 字节探活连接（模拟 uulan-gui 的 probe_direct_port），
        // 再拨齐其余三条通道 —— 旧实现会被探活的 early eof 连坐把整轮合法连接 drop 掉。
        let c1 = direct_dial_one(&addr, Channel::Control, Role::Client).await.unwrap();
        let _probe1 = TcpStream::connect(&addr).await.unwrap();
        drop(_probe1);
        let _probe2 = TcpStream::connect(&addr).await.unwrap();
        drop(_probe2);
        let c2 = direct_dial_one(&addr, Channel::Video, Role::Client).await.unwrap();
        let c3 = direct_dial_one(&addr, Channel::Bulk, Role::Client).await.unwrap();
        let c4 = direct_dial_one(&addr, Channel::Audio, Role::Client).await.unwrap();
        let _ = (c1, c2, c3, c4);

        let accepted = accept_task.await.unwrap();
        assert!(accepted.is_ok(), "探活连接不该打断合法直连：{accepted:?}");
    }
}
