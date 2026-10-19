//! 会合协议的**客户端**实现（被控端和控制端共用）。
//!
//! 服务端的会合逻辑在 `uulan-server` 里；这里是它对面那一半，
//! 两端都要用同一份实现，免得各写一遍、各自踩一遍坑。
//!
//! 一个重要细节：`relay_open` 会**另开一条 TCP 连接**，而不是复用控制连接。
//! 三条通道各有独立连接，控制连接只负责注册与收「有人要连你」的通知 ——
//! 这样视频或文件的大流量不会把控制消息堵住。

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpStream;

use crate::frame::{read_frame, write_frame, FrameKind};
use crate::rv::{Channel, ErrCode, Role, ToEndpoint, ToServer};
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
    pub async fn host_register(
        &mut self,
        device_id: Option<String>,
        name: String,
    ) -> Result<(String, bool), RvError> {
        let asked = device_id.clone();
        self.send(&ToServer::HostRegister { device_id, name, proto: PROTO_VERSION }).await?;
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

    /// 一次拨齐三条通道，并等三条都配对完成。
    ///
    /// 两端都要调用它；它会阻塞到对端也拨号为止。
    pub async fn relay_open_all(
        &self,
        session: &str,
        role: Role,
    ) -> Result<RelaySet, RvError> {
        // 先把三条都拨出去，再一起等 ready。
        // 若改成「拨一条等一条」，对端在第一条上就会和我们互相等待（虽然也能成，
        // 但会把配对串行化，三条通道的建立时间叠加）。
        let mut control = self.relay_dial(session, Channel::Control, role).await?;
        let mut video = self.relay_dial(session, Channel::Video, role).await?;
        let mut bulk = self.relay_dial(session, Channel::Bulk, role).await?;
        await_relay_ready(&mut control).await?;
        await_relay_ready(&mut video).await?;
        await_relay_ready(&mut bulk).await?;
        Ok(RelaySet { control, video, bulk })
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

/// 一次会话的三条通道。
pub struct RelaySet {
    pub control: TcpStream,
    pub video: TcpStream,
    pub bulk: TcpStream,
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
