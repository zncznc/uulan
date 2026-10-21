//! M4 端口映射验收：**两端真的接上了**，而且是真的 TCP。
//!
//! 链路（除了白名单与画面源，全是生产代码）：
//!
//! ```text
//! 本机 TcpStream → 控制端 Forwarder（真监听、真 id 分配）
//!   → C2H::PortMap(Open) → 中继盲转发 → 被控端 control_loop → portmap_loop
//!   → 白名单判定 → TcpStream::connect(目标) → bulk 多路复用里的一条流
//!   → 字节 ↔ 字节 → 目标的 echo 服务
//! ```
//!
//! 断言的量都是机器可查的：回显字节一致、并发多条流互不串、白名单外被拒。
//! 被控端用 `lavfi` 合成画面（本机没有 X11），这一点和文件传输测试相同。

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

use uulan_client::connect;
use uulan_client::forward::{ForwardSpec, Forwarder};
use uulan_host::capture::CaptureInput;
use uulan_host::config::HostConfig;
use uulan_host::session::{self, HostState, SessionParams, VideoSettings};
use uulan_media::ffmpeg::{self, Encoder};
use uulan_protocol::frame::{read_frame, write_frame, FrameKind};
use uulan_protocol::msg::{C2H, Cap, H2C, PortMapOp};
use uulan_protocol::rv::{Role, ToEndpoint};
use uulan_protocol::rvclient::RvConn;

const PW: &str = "aB3dE5fG";

async fn spawn_server() -> String {
    let server = uulan_server::Server::new(None).expect("建服务器失败");
    let listener = server.bind("127.0.0.1:0").await.expect("绑定失败");
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = server.serve(listener).await;
    });
    addr
}

fn test_config(server: &str) -> HostConfig {
    let mut cfg = HostConfig::default();
    cfg.server = server.to_string();
    cfg.name = "端口映射测试被控端".into();
    cfg.password = PW.into();
    cfg.capture = CaptureInput::parse("lavfi:testsrc2=size=320x240:rate=5").unwrap();
    cfg.fps = 5;
    cfg.screen_width = 320;
    cfg.screen_height = 240;
    // 白名单留空 = 只允许回环（默认值）。这正是这条测试要用的路径。
    cfg
}

/// 起一个被控端，返回设备 ID。
async fn spawn_host(server: &str, cfg: HostConfig) -> String {
    let mut conn = RvConn::connect(server).await.expect("被控端连不上中继");
    let (device_id, _) = conn.host_register(None, cfg.name.clone()).await.expect("注册失败");

    let state = std::sync::Arc::new(HostState::new(cfg.clone(), None));
    let screen = session::resolve_screen(&cfg.capture, &cfg).await;
    let settings = VideoSettings::from_config(&cfg);
    let addr = conn.server_addr().to_string();
    let ffmpeg = ffmpeg::find_ffmpeg(None).unwrap_or_else(|| std::path::PathBuf::from("ffmpeg"));

    tokio::spawn(async move {
        loop {
            let msg = match conn.recv().await {
                Ok(m) => m,
                Err(_) => break,
            };
            if let ToEndpoint::Incoming { session: sid, client_addr, .. } = msg {
                let params = SessionParams {
                    state: state.clone(),
                    ffmpeg: ffmpeg.clone(),
                    capture: cfg.capture.clone(),
                    screen,
                    encoder: Encoder::X264,
                    settings,
                    clipboard: Some(Box::new(uulan_clip::NoClipboard::new("端口映射测试"))),
                    screen_probe: None,
                    fs_roots: None,
                };
                let a = addr.clone();
                tokio::spawn(async move {
                    if let Ok(c) = RvConn::connect(&a).await {
                        if let Ok(relays) = c.relay_open_all(&sid, Role::Host).await {
                            let _ = session::run(params, relays, client_addr).await;
                        }
                    }
                });
            }
        }
    });
    device_id
}

/// 一个把收到的字节原样送回的 TCP 服务（port 由系统分配）。
async fn echo_server() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.expect("echo 服务绑不上");
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            match l.accept().await {
                Ok((mut s, _)) => {
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 16 * 1024];
                        loop {
                            match s.read(&mut buf).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    if s.write_all(&buf[..n]).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                    });
                }
                Err(_) => break,
            }
        }
    });
    port
}

async fn session_to(server: &str, device: &str) -> uulan_client::Connected {
    timeout(
        Duration::from_secs(15),
        connect::connect(server, device, PW, "cli", vec![Cap::PortMap]),
    )
    .await
    .expect("连接超时")
    .expect("连接失败")
}

/// 起 server + host + echo 目标，返回一个已经监听好的转发器与它的本机端口。
async fn harness() -> (Forwarder, u16) {
    let srv = spawn_server().await;
    let device = spawn_host(&srv, test_config(&srv)).await;
    let echo = echo_server().await;
    let c = session_to(&srv, &device).await;
    let fwd = Forwarder::start(c.control, c.bulk);
    let spec = ForwardSpec {
        bind: "127.0.0.1:0".parse().unwrap(),
        target_host: "127.0.0.1".into(),
        target_port: echo,
    };
    let addr = fwd.listen(spec).await.expect("本机监听失败");
    (fwd, addr.port())
}

#[tokio::test]
async fn forwards_bytes_to_a_loopback_target() {
    let (fwd, local) = harness().await;
    let mut sock = timeout(Duration::from_secs(10), TcpStream::connect(("127.0.0.1", local)))
        .await
        .expect("连本机监听超时")
        .expect("连本机监听失败");

    sock.write_all(b"hello-uulan-portmap").await.unwrap();
    let mut buf = vec![0u8; 19];
    timeout(Duration::from_secs(10), sock.read_exact(&mut buf))
        .await
        .expect("等回显超时")
        .expect("读回显失败");
    assert_eq!(&buf, b"hello-uulan-portmap", "回显内容必须逐字节一致");

    let (conns, up, down) = fwd.stats();
    assert_eq!(conns, 1, "应当恰好报告一条转发连接");
    assert_eq!(up, 19, "上行（本机→目标）字节数");
    assert_eq!(down, 19, "下行（目标→本机）字节数");
    fwd.shutdown();
}

/// 多条连接同时跑：证明 bulk 上是**按 id 分派**的，不是「谁先来谁霸占」。
/// 每条连接发不同长度、不同内容的数据，回显必须各自对称 —— 串了立刻能看出来。
#[tokio::test]
async fn concurrent_streams_do_not_mix() {
    let (fwd, local) = harness().await;

    let mut tasks = Vec::new();
    for k in 0..3usize {
        let n = 4_000 + k * 7_000; // 4 KB / 11 KB / 18 KB，跨越多个 bulk 帧
        tasks.push(tokio::spawn(async move {
            let mut sock = TcpStream::connect(("127.0.0.1", local)).await.expect("连不上");
            let payload: Vec<u8> = (0..n).map(|i| ((i * 31 + k * 7) % 251) as u8).collect();
            let mut got = vec![0u8; n];
            let (mut r, mut w) = sock.split();
            let send = async {
                w.write_all(&payload).await.expect("写失败");
                w.shutdown().await.expect("半关闭失败");
            };
            let recv = async {
                r.read_exact(&mut got).await.expect("读失败");
            };
            tokio::join!(send, recv);
            assert_eq!(got, payload, "第 {k} 条流的回显与发送不一致（数据串流了）");
        }));
    }
    for t in tasks {
        timeout(Duration::from_secs(20), t).await.expect("转发超时").expect("转发任务失败");
    }

    let (conns, up, down) = fwd.stats();
    assert_eq!(conns, 3);
    assert_eq!(up, 4_000 + 11_000 + 18_000);
    assert_eq!(down, up, "回显方向应当等量");
    fwd.shutdown();
}

/// 目标没人听：必须**很快**把本机连接关掉，而不是让它挂到超时。
/// 这正是「被控端先连目标、再回 Ok」的用意 —— 连不上就回 Err，不注册流。
#[tokio::test]
async fn dead_target_fails_the_local_connection_quickly() {
    let srv = spawn_server().await;
    let device = spawn_host(&srv, test_config(&srv)).await;

    // 拿一个刚释放的端口当「没人听」的目标（绑定后立刻释放）。
    let dead_port = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    };

    let c = session_to(&srv, &device).await;
    let fwd = Forwarder::start(c.control, c.bulk);
    let spec = ForwardSpec {
        bind: "127.0.0.1:0".parse().unwrap(),
        target_host: "127.0.0.1".into(),
        target_port: dead_port,
    };
    let local = fwd.listen(spec).await.unwrap().port();

    let mut sock = TcpStream::connect(("127.0.0.1", local)).await.expect("连本机监听失败");
    let mut buf = [0u8; 16];
    let r = timeout(Duration::from_secs(12), sock.read(&mut buf)).await;
    match r {
        Ok(Ok(0)) => {} // 对端直接关掉 = 正是我们要的
        Ok(Ok(n)) => panic!("目标不存在却读到了 {n} 字节"),
        Ok(Err(_)) => {} // 连接被重置也算「立刻失败」
        Err(_) => panic!("目标连不上时本机连接应当很快被关掉，而不是一直挂着"),
    }
    assert_eq!(fwd.stats().0, 0, "建立失败的连接不该计入「已转发」");
    fwd.shutdown();
}

/// 白名单是**被控端**说了算：直接拿控制通道发一条 Open，看它回什么。
/// 用 192.0.2.1（TEST-NET-1，永远不可路由）—— 但它必须是因为「白名单」被拒，
/// 而不是因为「连不上」：这说明检查发生在连接之前。
#[tokio::test]
async fn target_outside_whitelist_is_rejected_by_the_host() {
    let srv = spawn_server().await;
    let device = spawn_host(&srv, test_config(&srv)).await;
    let c = session_to(&srv, &device).await;
    let (mut rd, mut wr) = c.control.into_split();
    // 视频/数据通道留着别动：这条测试验的是「白名单拒绝」，不是「断线时报什么」。
    let _keep = (c.video, c.bulk);

    let m = C2H::PortMap(PortMapOp::Open {
        id: 7,
        target_host: "192.0.2.1".into(),
        target_port: 80,
    });
    write_frame(&mut wr, FrameKind::Json, &serde_json::to_vec(&m).unwrap())
        .await
        .expect("写控制帧失败");

    let mut msg = None;
    for _ in 0..50 {
        let f = timeout(Duration::from_secs(10), read_frame(&mut rd))
            .await
            .expect("等被控端回应超时")
            .expect("读控制帧失败");
        if f.kind != FrameKind::Json {
            continue;
        }
        match f.decode_json::<H2C>() {
            Ok(H2C::PortMapErr { id, error }) if id == 7 => {
                msg = Some(error);
                break;
            }
            Ok(H2C::PortMapOk { id }) if id == 7 => panic!("白名单外的目标不该被放行"),
            _ => continue,
        }
    }
    let e = msg.expect("被控端没有回 PortMapErr");
    assert!(e.contains("白名单"), "错误信息应当指明是白名单拒绝，实际：{e}");
}