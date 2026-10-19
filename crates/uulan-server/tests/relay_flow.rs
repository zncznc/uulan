//! M1 验收：一个假被控端 + 一个假控制端，跑通「注册 → 发起会话 → 三条通道配对 → 盲转发」全流程。
//!
//! 这些测试不碰 Linux、不碰 X11、不碰 FFmpeg —— 只验证会合协议和中继本身。
//! 它们跑在所有平台上，是后续改动的地基。

use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::net::TcpStream;
use tokio::time::timeout;
use uulan_protocol::frame::{read_frame, write_frame, Frame, FrameKind};
use uulan_protocol::rv::{Channel, ErrCode, Role, ToEndpoint, ToServer};
use uulan_protocol::{PROTO_VERSION};

const T: Duration = Duration::from_secs(5);

async fn send(stream: &mut TcpStream, msg: &ToServer) -> Result<()> {
    write_frame(stream, FrameKind::Json, &serde_json::to_vec(msg)?).await?;
    Ok(())
}

async fn recv(stream: &mut TcpStream) -> Result<ToEndpoint> {
    let f = timeout(T, read_frame(stream)).await.context("读帧超时")??;
    if f.kind != FrameKind::Json {
        bail!("期望 json 帧，收到 {:?}", f.kind);
    }
    Ok(serde_json::from_slice(&f.payload)?)
}

/// 起一台服务器，返回它监听的地址。
async fn spawn_server() -> Result<String> {
    let server = uulan_server::Server::new(None)?;
    let listener = server.bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?.to_string();
    tokio::spawn(async move { server.serve(listener).await });
    Ok(addr)
}

/// 一台假被控端：注册并保持控制连接。
struct FakeHost {
    ctl: TcpStream,
    device_id: String,
    addr: String,
}

impl FakeHost {
    async fn connect(addr: &str, device_id: Option<String>) -> Result<Self> {
        let mut ctl = TcpStream::connect(addr).await?;
        send(&mut ctl, &ToServer::HostRegister { device_id: device_id.clone(), name: "假麒麟".into(), proto: PROTO_VERSION }).await?;
        match recv(&mut ctl).await? {
            ToEndpoint::HostWelcome { device_id, .. } => Ok(Self { ctl, device_id, addr: addr.to_string() }),
            other => bail!("期望 host_welcome，收到 {other:?}"),
        }
    }

    /// 等一个 Incoming，返回 session。
    async fn expect_incoming(&mut self) -> Result<String> {
        match self.recv().await? {
            ToEndpoint::Incoming { session, .. } => Ok(session),
            other => bail!("期望 incoming，收到 {other:?}"),
        }
    }

    async fn recv(&mut self) -> Result<ToEndpoint> {
        recv(&mut self.ctl).await
    }

    /// 只拨号，不等 RelayReady —— 对端到齐后服务器才会下发 RelayReady。
    ///
    /// 「谁先拨号」是合法的：服务器必须把先到的一方挂起等待，不能断开、也不能提前回 ready。
    async fn dial_relay(&self, session: &str, channel: Channel) -> Result<TcpStream> {
        let mut s = TcpStream::connect(&self.addr).await?;
        send(&mut s, &ToServer::RelayOpen { session: session.into(), channel, role: Role::Host }).await?;
        Ok(s)
    }

    /// 拨出并等 RelayReady（两端都已到齐时用）。
    ///
    /// 注意别用「先拨号、再让对端拨」的时序去调它：它会等对端，而对端要等你
    /// 返回之后才拨 —— 两边互等。那种场景用 [`FakeHost::dial_relay`] 先拨、
    /// 之后自己 `expect_relay_ready`。
    #[allow(dead_code)]
    async fn open_relay(&self, session: &str, channel: Channel) -> Result<TcpStream> {
        let mut s = self.dial_relay(session, channel).await?;
        expect_relay_ready(&mut s).await?;
        Ok(s)
    }
}

async fn expect_relay_ready(s: &mut TcpStream) -> Result<()> {
    let f = timeout(T, read_frame(s)).await.context("等 relay_ready 超时")??;
    match serde_json::from_slice::<ToEndpoint>(&f.payload)? {
        ToEndpoint::RelayReady { .. } => Ok(()),
        other => bail!("期望 relay_ready，收到 {other:?}"),
    }
}

#[tokio::test]
async fn host_registers_and_gets_a_well_formed_device_id() {
    let addr = spawn_server().await.unwrap();
    let h = FakeHost::connect(&addr, None).await.unwrap();
    assert_eq!(h.device_id.len(), 9, "device_id={}", h.device_id);
    assert!(h.device_id.bytes().all(|b| b.is_ascii_digit()));
    assert!(!h.device_id.starts_with('0'));
}

#[tokio::test]
async fn host_can_keep_its_own_device_id() {
    let addr = spawn_server().await.unwrap();
    let h = FakeHost::connect(&addr, Some("100000001".into())).await.unwrap();
    assert_eq!(h.device_id, "100000001");
}

#[tokio::test]
async fn illegal_device_id_is_replaced_not_echoed() {
    let addr = spawn_server().await.unwrap();
    // 首位 0，不合法 —— 服务端必须分配新的，而不是照抄。
    let h = FakeHost::connect(&addr, Some("012345678".into())).await.unwrap();
    assert_ne!(h.device_id, "012345678");
    assert!(!h.device_id.starts_with('0'));
}

#[tokio::test]
async fn connecting_to_an_offline_device_reports_offline() {
    let addr = spawn_server().await.unwrap();
    let mut c = TcpStream::connect(&addr).await.unwrap();
    send(&mut c, &ToServer::ClientConnect { device_id: "999999999".into(), client_name: "c".into(), proto: PROTO_VERSION })
        .await
        .unwrap();
    match recv(&mut c).await.unwrap() {
        ToEndpoint::Error { code, .. } => assert_eq!(code, ErrCode::Offline),
        other => panic!("期望 error/offline，收到 {other:?}"),
    }
}

#[tokio::test]
async fn malformed_device_id_is_rejected_before_lookup() {
    let addr = spawn_server().await.unwrap();
    let mut c = TcpStream::connect(&addr).await.unwrap();
    // 位数不对：必须立刻报 bad_device_id，而不是当成「不在线」。
    send(&mut c, &ToServer::ClientConnect { device_id: "123".into(), client_name: "c".into(), proto: PROTO_VERSION })
        .await
        .unwrap();
    match recv(&mut c).await.unwrap() {
        ToEndpoint::Error { code, .. } => assert_eq!(code, ErrCode::BadDeviceId),
        other => panic!("期望 error/bad_device_id，收到 {other:?}"),
    }
}

#[tokio::test]
async fn proto_mismatch_is_refused() {
    let addr = spawn_server().await.unwrap();
    let mut h = TcpStream::connect(&addr).await.unwrap();
    send(&mut h, &ToServer::HostRegister { device_id: None, name: "x".into(), proto: 999 }).await.unwrap();
    match recv(&mut h).await.unwrap() {
        ToEndpoint::Error { code, .. } => assert_eq!(code, ErrCode::ProtoMismatch),
        other => panic!("期望 proto_mismatch，收到 {other:?}"),
    }
}

#[tokio::test]
async fn query_reports_online_then_unknown() {
    let addr = spawn_server().await.unwrap();
    let h = FakeHost::connect(&addr, None).await.unwrap();

    let mut c = TcpStream::connect(&addr).await.unwrap();
    send(&mut c, &ToServer::Query { device_id: h.device_id.clone() }).await.unwrap();
    match recv(&mut c).await.unwrap() {
        ToEndpoint::QueryResult { online, name, .. } => {
            assert!(online);
            assert_eq!(name.as_deref(), Some("假麒麟"));
        }
        other => panic!("期望 query_result，收到 {other:?}"),
    }

    let mut c = TcpStream::connect(&addr).await.unwrap();
    send(&mut c, &ToServer::Query { device_id: "999999998".into() }).await.unwrap();
    match recv(&mut c).await.unwrap() {
        ToEndpoint::QueryResult { online, .. } => assert!(!online),
        other => panic!("期望 query_result，收到 {other:?}"),
    }
}

/// 主流程：注册 → 发起会话 → 三条通道配对 → 字节原样透传。
#[tokio::test]
async fn full_session_pairs_three_channels_and_relays_bytes() {
    let addr = spawn_server().await.unwrap();
    let mut host = FakeHost::connect(&addr, None).await.unwrap();

    // 控制端发起连接
    let mut cc = TcpStream::connect(&addr).await.unwrap();
    send(&mut cc, &ToServer::ClientConnect { device_id: host.device_id.clone(), client_name: "控制端".into(), proto: PROTO_VERSION })
        .await
        .unwrap();
    let session = match recv(&mut cc).await.unwrap() {
        ToEndpoint::ClientReady { session, device_id, device_name } => {
            assert_eq!(device_id, host.device_id);
            assert_eq!(device_name, "假麒麟");
            session
        }
        other => panic!("期望 client_ready，收到 {other:?}"),
    };

    // 被控端必须同时收到 incoming
    let host_session = host.expect_incoming().await.unwrap();
    assert_eq!(host_session, session, "两端的 session 必须是同一个");

    // 三条通道，故意让被控端先拨号（控制端后拨），验证服务器会等对端。
    for channel in [Channel::Control, Channel::Video, Channel::Bulk] {
        // 被控端先拨号：此刻对端还没来，服务器必须挂起等待（既不断开、也不提前回 ready）。
        let mut hs = host.dial_relay(&session, channel).await.unwrap();

        let mut cs = TcpStream::connect(&addr).await.unwrap();
        send(&mut cs, &ToServer::RelayOpen { session: session.clone(), channel, role: Role::Client }).await.unwrap();

        // 两端到齐后才各自收到 RelayReady。必须先收干净这两帧再收发业务帧，
        // 否则下面第一个 read_frame 会把 ready 当业务帧读走。
        expect_relay_ready(&mut cs).await.unwrap();
        expect_relay_ready(&mut hs).await.unwrap();

        // 控制端 → 被控端
        let payload: Vec<u8> = (0..=255u8).cycle().take(70_000).collect();
        write_frame(&mut cs, FrameKind::Blob, &payload).await.unwrap();
        let got = timeout(T, read_frame(&mut hs)).await.unwrap().unwrap();
        assert_eq!(got.kind, FrameKind::Blob);
        assert_eq!(&got.payload[..], &payload[..], "大 payload 必须原样透传");

        // 被控端 → 控制端
        let reply: Frame = Frame::json(FrameKind::Json, &serde_json::json!({"channel": format!("{channel:?}")})).unwrap();
        write_frame(&mut hs, reply.kind, &reply.payload).await.unwrap();
        let got = timeout(T, read_frame(&mut cs)).await.unwrap().unwrap();
        assert_eq!(&got.payload[..], &reply.payload[..]);
    }
}

/// 同一通道重复拨号必须被拒绝，否则会把先到的那条连接挤掉。
#[tokio::test]
async fn duplicate_channel_is_rejected() {
    let addr = spawn_server().await.unwrap();
    let mut host = FakeHost::connect(&addr, None).await.unwrap();

    let mut cc = TcpStream::connect(&addr).await.unwrap();
    send(&mut cc, &ToServer::ClientConnect { device_id: host.device_id.clone(), client_name: "c".into(), proto: PROTO_VERSION })
        .await
        .unwrap();
    let session = match recv(&mut cc).await.unwrap() {
        ToEndpoint::ClientReady { session, .. } => session,
        other => panic!("{other:?}"),
    };
    let _ = host.expect_incoming().await.unwrap();

    // 必须是 dial 而不是 open：这条通道没有控制端来配对，open 会一直等 ready。
    let _first = host.dial_relay(&session, Channel::Control).await.unwrap();

    let mut dup = TcpStream::connect(&addr).await.unwrap();
    send(&mut dup, &ToServer::RelayOpen { session: session.clone(), channel: Channel::Control, role: Role::Host }).await.unwrap();
    match recv(&mut dup).await.unwrap() {
        ToEndpoint::Error { code, .. } => assert_eq!(code, ErrCode::DuplicateChannel),
        other => panic!("期望 duplicate_channel，收到 {other:?}"),
    }
}

#[tokio::test]
async fn unknown_session_ticket_is_rejected() {
    let addr = spawn_server().await.unwrap();
    let mut s = TcpStream::connect(&addr).await.unwrap();
    send(&mut s, &ToServer::RelayOpen { session: "deadbeef".into(), channel: Channel::Control, role: Role::Host }).await.unwrap();
    match recv(&mut s).await.unwrap() {
        ToEndpoint::Error { code, .. } => assert_eq!(code, ErrCode::BadSession),
        other => panic!("期望 bad_session，收到 {other:?}"),
    }
}

/// 回归：先拨号的一方必须被**挂起等待**，不能被服务器提前断开。
///
/// 曾经的写法是无条件 `(slot.host.take(), slot.client.take())`：先到的那条流
/// 在未配对的 match 分支里被当成临时值丢掉，对端立刻读到 EOF。
/// 「被控端先拨号、控制端还在路上」是完全正常的时序，所以这个 bug 在生产里等于连不上。
#[tokio::test]
async fn first_dialer_is_held_open_not_dropped() {
    let addr = spawn_server().await.unwrap();
    let mut host = FakeHost::connect(&addr, None).await.unwrap();

    let mut cc = TcpStream::connect(&addr).await.unwrap();
    send(&mut cc, &ToServer::ClientConnect { device_id: host.device_id.clone(), client_name: "c".into(), proto: PROTO_VERSION })
        .await
        .unwrap();
    let session = match recv(&mut cc).await.unwrap() {
        ToEndpoint::ClientReady { session, .. } => session,
        other => panic!("{other:?}"),
    };
    let _ = host.expect_incoming().await.unwrap();

    let mut hs = host.dial_relay(&session, Channel::Control).await.unwrap();

    // 对端还没拨号：这段时间里既不该收到 ready，也不该被断开。
    match timeout(Duration::from_millis(500), read_frame(&mut hs)).await {
        Err(_elapsed) => {}                                    // 超时 = 一直挂着，正是期望
        Ok(Ok(f)) => panic!("对端未到齐却收到了帧 kind={:?}", f.kind),
        Ok(Err(e)) => panic!("先拨号的一方被提前断开了：{e}"),  // 就是曾经那个 bug
    }

    // 对端到齐后必须能正常配对并透传。
    let mut cs = TcpStream::connect(&addr).await.unwrap();
    send(&mut cs, &ToServer::RelayOpen { session, channel: Channel::Control, role: Role::Client }).await.unwrap();
    expect_relay_ready(&mut cs).await.unwrap();
    expect_relay_ready(&mut hs).await.unwrap();

    write_frame(&mut cs, FrameKind::Blob, b"still alive").await.unwrap();
    let got = timeout(T, read_frame(&mut hs)).await.unwrap().unwrap();
    assert_eq!(&got.payload[..], b"still alive");
}

/// 被控端控制连接断开后，控制端必须立刻拿到 offline，而不是一直转圈。
#[tokio::test]
async fn host_going_offline_frees_the_device_id() {
    let addr = spawn_server().await.unwrap();
    let host = FakeHost::connect(&addr, Some("100000002".into())).await.unwrap();
    let id = host.device_id.clone();
    drop(host);

    // 服务器的设备表在下次 sweep 时才清理，但 mpsc 的 closed 标志是立刻生效的。
    // 这里给一点时间让断开被处理。
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut c = TcpStream::connect(&addr).await.unwrap();
    send(&mut c, &ToServer::ClientConnect { device_id: id, client_name: "c".into(), proto: PROTO_VERSION }).await.unwrap();
    match recv(&mut c).await.unwrap() {
        ToEndpoint::Error { code, .. } => assert_eq!(code, ErrCode::Offline),
        other => panic!("期望 offline，收到 {other:?}"),
    }
}

/// 心跳。
#[tokio::test]
async fn ping_gets_a_pong_with_server_time() {
    let addr = spawn_server().await.unwrap();
    let mut s = TcpStream::connect(&addr).await.unwrap();
    send(&mut s, &ToServer::Ping).await.unwrap();
    match recv(&mut s).await.unwrap() {
        ToEndpoint::Pong { server_time_ms } => assert!(server_time_ms > 1_700_000_000_000),
        other => panic!("期望 pong，收到 {other:?}"),
    }
}