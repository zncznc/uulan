//! 会合客户端（`uulan_protocol::rvclient`）对着真服务器的验收测试。
//!
//! 被控端和控制端都只用这一份客户端实现，所以它必须先被证明是对的 ——
//! 否则两端会各自在同一个地方出错，还互相以为是对方的问题。

use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::time::timeout;
use uulan_protocol::frame::{read_frame, write_frame, FrameKind};
use uulan_protocol::msg::{C2H, ClientHello, HostHelloOk, Screen};
use uulan_protocol::rv::{Channel, ErrCode, Role, ToEndpoint, ToServer};
use uulan_protocol::rvclient::{await_relay_ready, RvConn, RvError};

const T: Duration = Duration::from_secs(5);

/// 打开日志：`RUST_LOG=debug cargo test ... -- --nocapture` 才看得到细节。
fn init_logs() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug")),
        )
        .with_test_writer()
        .try_init();
}

async fn spawn_server() -> Result<String> {
    init_logs();
    let server = uulan_server::Server::new(None)?;
    let listener = server.bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?.to_string();
    tokio::spawn(async move { server.serve(listener).await });
    Ok(addr)
}

#[tokio::test]
async fn host_keeps_its_device_id_across_restarts() -> Result<()> {
    let addr = spawn_server().await?;
    let mut c = RvConn::connect(&addr).await?;
    let (id, fresh) = c.host_register(None, "麒麟-01".into()).await?;
    assert!(fresh, "首次注册必须标记为「服务端新分配」");
    assert_eq!(id.len(), 9);

    // 模拟被控端重启：旧连接断开，再拿同一个 ID 上线。
    drop(c);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut c2 = RvConn::connect(&addr).await?;
    let (id2, fresh2) = c2.host_register(Some(id.clone()), "麒麟-01".into()).await?;
    assert_eq!(id2, id, "重启用自己的 ID 必须拿回同一个，否则用户手上的 ID 天天变");
    assert!(!fresh2, "复用已有 ID 时不应标记为新分配");
    Ok(())
}

/// 一个 ID 已被**在线**设备占用时，服务端不让后来者顶掉它。
///
/// 这是刻意选的行为：允许顶替的话，局域网里任何人只要重复注册某个 ID
/// 就能把真实设备踢下线（拒绝服务）。宁可让重复注册者拿一个新 ID ——
/// 用户顶多看到列表里多出一台设备，而不是原有设备莫名其妙连不上。
#[tokio::test]
async fn live_duplicate_registration_does_not_steal_the_id() -> Result<()> {
    let addr = spawn_server().await?;
    let mut c = RvConn::connect(&addr).await?;
    let (id, _) = c.host_register(None, "麒麟-01".into()).await?;

    // c 仍然在线：第二个连接拿同一个 ID 来注册
    let mut c2 = RvConn::connect(&addr).await?;
    let (id2, fresh2) = c2.host_register(Some(id.clone()), "冒名者".into()).await?;
    assert_ne!(id2, id, "在线设备的 ID 不能被后来者顶掉");
    assert!(fresh2, "被分配了新 ID，应当标记为新分配");

    // 原设备仍然在线，且名字没被改掉
    let mut q = RvConn::connect(&addr).await?;
    let (online, name) = q.query(&id).await?;
    assert!(online, "原设备应仍然在线");
    assert_eq!(name.as_deref(), Some("麒麟-01"), "名字不该被冒名者改掉");
    Ok(())
}

#[tokio::test]
async fn invalid_device_id_comes_back_fresh() -> Result<()> {
    let addr = spawn_server().await?;
    let mut c = RvConn::connect(&addr).await?;
    let (id, fresh) = c.host_register(Some("012345678".into()), "x".into()).await?;
    assert!(fresh, "非法 ID 必须被换掉");
    assert!(!id.starts_with('0'));
    Ok(())
}

#[tokio::test]
async fn connect_to_offline_device_surfaces_the_server_error() -> Result<()> {
    let addr = spawn_server().await?;
    let mut c = RvConn::connect(&addr).await?;
    match c.client_connect("999999999", "控制端").await {
        Err(RvError::Server { code, .. }) => assert_eq!(code, ErrCode::Offline),
        other => bail!("期望 Offline 错误，得到 {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn malformed_device_id_is_reported_as_bad_device_id() -> Result<()> {
    let addr = spawn_server().await?;
    let mut c = RvConn::connect(&addr).await?;
    match c.client_connect("abc", "控制端").await {
        Err(RvError::Server { code, .. }) => assert_eq!(code, ErrCode::BadDeviceId),
        other => bail!("期望 BadDeviceId，得到 {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn ping_round_trips() -> Result<()> {
    let addr = spawn_server().await?;
    let mut c = RvConn::connect(&addr).await?;
    assert!(c.ping().await? > 1_700_000_000_000);
    Ok(())
}

#[tokio::test]
async fn query_reports_online_and_offline() -> Result<()> {
    let addr = spawn_server().await?;
    let mut host = RvConn::connect(&addr).await.context("被控端连接失败")?;
    let (id, _) = host.host_register(None, "麒麟-01".into()).await.context("注册失败")?;

    let mut c = RvConn::connect(&addr).await.context("查询连接建立失败")?;
    c.send(&ToServer::Query { device_id: id.clone() })
        .await
        .context("发送 Query 失败")?;
    let r = c.recv_ok().await.context("接收 QueryResult 失败")?;
    match r {
        ToEndpoint::QueryResult { online, name, .. } => {
            assert!(online);
            assert_eq!(name.as_deref(), Some("麒麟-01"));
        }
        other => bail!("期望 QueryResult，得到 {other:?}"),
    }

    let mut c2 = RvConn::connect(&addr).await.context("第二个查询连接建立失败")?;
    c2.send(&ToServer::Query { device_id: "999999998".into() })
        .await
        .context("发送第二个 Query 失败")?;
    match c2.recv_ok().await.context("接收第二个 QueryResult 失败")? {
        ToEndpoint::QueryResult { online, .. } => assert!(!online),
        other => bail!("期望 QueryResult，得到 {other:?}"),
    }
    Ok(())
}

/// 主流程：注册 → 发起会话 → 两端各拨三条通道 → 通道上互通字节（含业务帧）。
#[tokio::test]
async fn two_endpoints_pair_three_channels_and_exchange_application_frames() -> Result<()> {
    let addr = spawn_server().await?;

    // 被控端上线
    let mut host = RvConn::connect(&addr).await?;
    let (device_id, _) = host.host_register(None, "麒麟-01".into()).await?;

    // 控制端请求连接
    let mut client = RvConn::connect(&addr).await?;
    let (session, back_id, device_name) = client.client_connect(&device_id, "我的电脑").await?;
    assert_eq!(back_id, device_id);
    assert_eq!(device_name, "麒麟-01");

    // 被控端必须收到 Incoming，session 一致
    match timeout(T, host.recv_ok()).await?? {
        ToEndpoint::Incoming { session: s, client_name, .. } => {
            assert_eq!(s, session);
            assert_eq!(client_name, "我的电脑");
        }
        other => bail!("期望 Incoming，得到 {other:?}"),
    }

    // 故意让被控端先把三条都拨出去，控制端再拨 —— 服务端必须挂住先到的一方。
    //
    // 必须用 `relay_dial`（只拨不等）：`relay_open_all` 要等对端到齐才返回，
    // 而控制端要等我们返回之后才会去拨 ⇒ 两边互等，死锁到服务端 30 秒后回收会话。
    let mut hc = host.relay_dial(&session, Channel::Control, Role::Host).await?;
    let mut hv = host.relay_dial(&session, Channel::Video, Role::Host).await?;
    let mut hb = host.relay_dial(&session, Channel::Bulk, Role::Host).await?;

    let client_relays = client.relay_open_all(&session, Role::Client).await?;

    // 对端到齐后，被控端这边才收得到 RelayReady。
    await_relay_ready(&mut hc).await?;
    await_relay_ready(&mut hv).await?;
    await_relay_ready(&mut hb).await?;

    // 两条控制通道上各跑一遍真实的业务握手
    let mut cc = client_relays.control;
    let mut cv = client_relays.video;

    write_frame(
        &mut cc,
        FrameKind::Json,
        &serde_json::to_vec(&C2H::Hello(ClientHello {
            os: "windows".into(),
            version: "0.1.0".into(),
            name: "我的电脑".into(),
            caps: vec![],
        }))?,
    )
    .await?;

    let f = timeout(T, read_frame(&mut hc)).await??;
    match C2H::from_frame(&f)? {
        C2H::Hello(h) => assert_eq!(h.os, "windows"),
        other => bail!("期望 Hello，得到 {other:?}"),
    }

    // 被控端回 HelloOk
    let hello = uulan_protocol::msg::H2C::HelloOk(HostHelloOk {
        device_id: device_id.clone(),
        os: "kylin".into(),
        version: "0.1.0".into(),
        name: "麒麟-01".into(),
        screen: Screen { w: 1920, h: 1080 },
        caps: vec![],
        has_desktop: true,
    });
    write_frame(&mut hc, FrameKind::Json, &serde_json::to_vec(&hello)?).await?;

    let f = timeout(T, read_frame(&mut cc)).await??;
    match uulan_protocol::msg::H2C::from_frame(&f)? {
        uulan_protocol::msg::H2C::HelloOk(h) => {
            assert_eq!(h.screen.w, 1920);
            assert!(h.has_desktop);
        }
        other => bail!("期望 HelloOk，得到 {other:?}"),
    }

    // 视频通道：一整帧 AU（带时间戳前缀）原样透传
    let au = vec![0u8; 40_000];
    let packed = uulan_protocol::frame::video::pack_au(123_456, &au);
    write_frame(&mut hv, FrameKind::VideoAu, &packed).await?;

    let f = timeout(T, read_frame(&mut cv)).await??;
    assert_eq!(f.kind, FrameKind::VideoAu);
    let (ts, body) = uulan_protocol::frame::video::unpack_au(&f.payload).unwrap();
    assert_eq!(ts, 123_456, "时间戳必须原样到达");
    assert_eq!(body.len(), au.len(), "4 万字节的 AU 必须完整透传");

    Ok(())
}

/// 先拨号的一方被挂住时，不应被判成超时/断开。
#[tokio::test]
async fn first_dialer_survives_a_delay() -> Result<()> {
    let addr = spawn_server().await?;
    let mut host = RvConn::connect(&addr).await?;
    let (device_id, _) = host.host_register(None, "麒麟-01".into()).await?;

    let mut client = RvConn::connect(&addr).await?;
    let (session, _, _) = client.client_connect(&device_id, "c").await?;
    let _ = timeout(T, host.recv_ok()).await??;

    // 被控端先拨控制通道（此时对端还没来）。
    //
    // 必须用 `relay_dial`：`relay_open` 会等对端到齐，而控制端要等它返回之后才拨 ——
    // 两边互等，死锁到服务端把这条挂着的连接按超时回收（表现为 early eof）。
    let mut hc = host.relay_dial(&session, Channel::Control, Role::Host).await?;
    // 挂 800ms 再让控制端拨
    tokio::time::sleep(Duration::from_millis(800)).await;
    let mut cc = client.relay_open(&session, Channel::Control, Role::Client).await?;
    // 控制端拨上来之后，被控端这边才收到 RelayReady。
    await_relay_ready(&mut hc).await?;

    // 此时两条都已就绪，应能立刻传字节
    write_frame(&mut cc, FrameKind::Blob, b"ping").await?;
    let f = timeout(T, read_frame(&mut hc)).await??;
    assert_eq!(&f.payload[..], b"ping");
    Ok(())
}
