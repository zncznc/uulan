//! M4 终端验收（`--terms`）：两端真的接上了 —— 真 TCP + 真协议 + 被控端真注册表。
//!
//! 为什么这条能在本机验：`TermOp::List` → `H2C::TermSessions` 全程走控制通道的 JSON 消息，
//! 与 PTY 本身无关 —— 被控端用假 shell 也**真开**了会话（term::run 的注册表是真的，
//! 换掉的只是「谁来产生字节」）。链路骨架与 `term_path.rs` 同一套。
//!
//! 断言钉的是「清单回得来、条数对得上、id 对得上」；标题的具体值随假 shell 的实现走，
//! 不在这里钉（render 的反例已有纯函数单测钉住「没有名字」那条）。

use std::sync::Arc;
use std::time::Duration;

use tokio::time::timeout;

use uulan_client::connect;
use uulan_client::termx::{Options, TermLink};
use uulan_client::terms;
use uulan_host::capture::CaptureInput;
use uulan_host::config::HostConfig;
use uulan_host::session::{self, HostState, SessionParams, VideoSettings};
use uulan_host::term::testing::FakeShellPty;
use uulan_media::ffmpeg::{self, Encoder};
use uulan_protocol::msg::Cap;
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
    cfg.name = "清单测试被控端".into();
    cfg.password = PW.into();
    cfg.capture = CaptureInput::parse("lavfi:testsrc2=size=320x240:rate=5").unwrap();
    cfg.fps = 5;
    cfg.screen_width = 320;
    cfg.screen_height = 240;
    cfg
}

/// 起一个被控端，终端后端换成假 shell。返回设备 ID。
async fn spawn_host(server: &str, cfg: HostConfig) -> String {
    let mut conn = RvConn::connect(server).await.expect("被控端连不上中继");
    let (device_id, _) = conn
        .host_register(None, cfg.name.clone(), None)
        .await
        .expect("注册失败");

    let state = Arc::new(HostState::new(cfg.clone(), None));
    let screen = session::resolve_region(&cfg.capture, &cfg).await;
    let settings = VideoSettings::from_config(&cfg);
    let addr = conn.server_addr().to_string();
    let ffmpeg = ffmpeg::find_ffmpeg(None).unwrap_or_else(|| std::path::PathBuf::from("ffmpeg"));

    tokio::spawn(async move {
        loop {
            let msg = match conn.recv().await {
                Ok(m) => m,
                Err(_) => break,
            };
            if let ToEndpoint::Incoming {
                session: sid,
                client_addr,
                ..
            } = msg
            {
                let params = SessionParams {
                    state: state.clone(),
                    ffmpeg: ffmpeg.clone(),
                    capture: cfg.capture.clone(),
                    screen,
                    encoder: Encoder::X264,
                    settings,
                    clipboard: Some(Box::new(uulan_clip::NoClipboard::new("清单测试"))),
                    screen_probe: None,
                    fs_roots: None,
                    pty: Some(Box::new(FakeShellPty::new())),
                    sys_source: None,
                    lock_probe: None,
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

/// `--terms` 三步曲：空清单 → 真开一条会话 → 再列一次。
#[tokio::test]
async fn the_list_grows_when_a_session_opens_and_is_honest_when_empty() {
    let srv = spawn_server().await;
    let device = spawn_host(&srv, test_config(&srv)).await;

    // 第一步：还没人开过终端 —— `--terms` 必须明说「没有」，而不是空转超时。
    let c0 = timeout(
        Duration::from_secs(15),
        connect::connect(&srv, &device, PW, "terms-cli", vec![Cap::Terminal]),
    )
    .await
    .expect("连接超时")
    .expect("连接失败");
    assert!(
        c0.host.caps.contains(&Cap::Terminal),
        "被控端必须声明终端能力"
    );
    drop(c0.video);
    let empty = timeout(Duration::from_secs(12), terms::ask(c0.control))
        .await
        .expect("等 TermSessions 超时（List 没接上？）")
        .expect("ask List 失败");
    assert!(
        empty.is_empty(),
        "没开会话时清单必须是空: {empty:?}"
    );

    // 第二步：真开一条终端会话（假 shell、真注册表）。
    let c1 = timeout(
        Duration::from_secs(15),
        connect::connect(&srv, &device, PW, "terms-cli2", vec![Cap::Terminal]),
    )
    .await
    .expect("二次连接超时")
    .expect("二次连接失败");
    drop(c1.video);
    let mut link = timeout(
        Duration::from_secs(10),
        TermLink::open(
            c1.control,
            Options {
                shell: None,
                cols: 80,
                rows: 24,
            },
        ),
    )
    .await
    .expect("开终端超时")
    .expect("开终端失败");

    // 同一条控制连接上真问「这条连接开过哪些会话」—— 注册表作用域由被控端结构决定。
    let mine = timeout(Duration::from_secs(12), link.list())
        .await
        .expect("等清单回音超时")
        .expect("清单收发失败");
    assert_eq!(mine.len(), 1, "这条连接开了一条终端，清单必须看得见它: {mine:?}");
    assert_eq!(
        mine[0].id, 1,
        "`--term` 是独立会话：一条连接上只有一个终端，id 固定为 1"
    );

    // 第三条**独立**控制连接的清单必须还是空 —— 作用域按控制连接算，不是按整机。
    let c2 = timeout(
        Duration::from_secs(15),
        connect::connect(&srv, &device, PW, "terms-cli3", vec![Cap::Terminal]),
    )
    .await
    .expect("三次连接超时")
    .expect("三次连接失败");
    drop(c2.video);
    let got = timeout(Duration::from_secs(12), terms::ask(c2.control))
        .await
        .expect("等 TermSessions 超时")
        .expect("ask List 失败");
    assert!(
        got.is_empty(),
        "另一条控制连接不该看见这条会话，必须为空: {got:?}"
    );
}
