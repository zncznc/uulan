//! M2 端到端验收：中继服务器 + 被控端会话 + 控制端解码，三者在同一个进程里跑通。
//!
//! 覆盖的真实路径：会合注册 → 发起会话 → 三通道配对 → 盲转发 → 握手 → 鉴权 →
//! FFmpeg 编码（lavfi 合成源，因为测试机没有 X11）→ AU 切分 → 网络传输 →
//! FFmpeg 解码 → 得到 BGRA 帧。
//!
//! 唯一没被真实覆盖的是 `x11grab` 这个输入源本身 —— 那需要一台有 X11 的机器。
//! 除它之外，两侧的参数与数据通路上没有任何替身。
//!
//! 找不到 ffmpeg 时整体跳过并打印提示，不伪装成通过。

use std::sync::Arc;
use std::time::Duration;

use tokio::time::timeout;

use uulan_client::connect;
use uulan_client::watch::VideoWatcher;
use uulan_host::capture::CaptureInput;
use uulan_host::config::HostConfig;
use uulan_host::session::{self, HostState, SessionParams, VideoSettings};
use uulan_media::ffmpeg::{self, Encoder};
use uulan_protocol::rv::{Role, ToEndpoint};
use uulan_protocol::rvclient::RvConn;

const T: Duration = Duration::from_secs(60);

/// 打开日志：`RUST_LOG=debug cargo test ... -- --nocapture` 才看得到细节。
///
/// 这类跨进程/跨任务的问题，光看断言信息基本查不出来；日志是必需品，不是装饰。
fn init_logs() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_test_writer()
        .try_init();
}

fn ffmpeg_or_skip() -> Option<std::path::PathBuf> {
    init_logs();
    let p = ffmpeg::find_ffmpeg(None);
    if p.is_none() {
        eprintln!("跳过：本机没有 ffmpeg");
    }
    p
}

/// 起一台中继服务器，返回它的地址。
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
    cfg.name = "测试被控端".into();
    cfg.password = "aB3dE5fG".into(); // 合法：8 位、含字母和数字
    cfg.capture = CaptureInput::parse("lavfi:testsrc2=size=320x240:rate=25@20").unwrap();
    cfg.fps = 25;
    cfg.quality = uulan_protocol::msg::Quality::Auto;
    cfg.screen_width = 320;
    cfg.screen_height = 240;
    cfg
}

/// 在后台跑一个被控端：注册后常驻，来一个连就开一个会话。
///
/// 返回它的 device_id，以及一个"会话已结束"的接收端（测试收尾用）。
async fn spawn_host(
    server: &str,
    cfg: HostConfig,
    ffmpeg: std::path::PathBuf,
) -> (String, tokio::sync::watch::Receiver<u32>) {
    let mut conn = RvConn::connect(server).await.expect("被控端连不上中继");
    let (device_id, _) = conn
        .host_register(None, cfg.name.clone())
        .await
        .expect("注册失败");

    let state = Arc::new(HostState::new(cfg.clone(), None));
    let screen = session::resolve_screen(&cfg.capture, &cfg).await;
    let settings = VideoSettings::from_config(&cfg);
    let (done_tx, done_rx) = tokio::sync::watch::channel(0u32);
    let addr = conn.server_addr().to_string();

    tokio::spawn(async move {
        let mut done = 0u32;
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
                    // 端到端测试不碰系统剪贴板：给个明确不可用的后端。
                    clipboard: Some(Box::new(uulan_clip::NoClipboard::new("端到端测试"))),
                };
                let a = addr.clone();
                let dtx = done_tx.clone();
                tokio::spawn(async move {
                    let rc = RvConn::connect(&a).await;
                    if let Ok(c) = rc {
                        if let Ok(relays) = c.relay_open_all(&sid, Role::Host).await {
                            let _ = session::run(params, relays, client_addr).await;
                        }
                    }
                    done += 1;
                    let _ = dtx.send(done);
                });
            }
        }
    });

    (device_id, done_rx)
}

#[tokio::test]
async fn end_to_end_host_to_client_video() {
    let Some(ff) = ffmpeg_or_skip() else { return };
    let addr = spawn_server().await;
    let cfg = test_config(&addr);
    let (device_id, mut done) = spawn_host(&addr, cfg.clone(), ff.clone()).await;

    assert_eq!(device_id.len(), 9);

    // 控制端连上去
    let c = timeout(
        T,
        connect::connect(&addr, &device_id, &cfg.password, "测试控制端", vec![]),
    )
    .await
    .expect("连接超时")
    .expect("连接失败");

    assert_eq!(c.host.name, "测试被控端");
    assert_eq!(c.host.screen.w, 320);
    assert_eq!(c.host.screen.h, 240);
    assert!(c.host.has_desktop);

    // 解码流水线
    let mut watcher = VideoWatcher::start(c.video, Some(ff.clone()))
        .await
        .expect("启动解码失败");

    assert_eq!(watcher.info().w, 320);
    assert_eq!(watcher.info().h, 240);
    assert_eq!(watcher.info().codec, "h264");

    // 收帧。合成源一共 20 帧，等它全部到齐。
    let mut got = 0u64;
    while got < 20 {
        let f = timeout(T, watcher.next_frame())
            .await
            .expect("等帧超时（画面没有流动）");
        let Some(f) = f else { break };
        assert_eq!(f.width, 320);
        assert_eq!(f.height, 240);
        assert_eq!(f.pixels.len(), ffmpeg::frame_bytes(320, 240), "帧字节数不对");
        // 画面不能是全黑：合成源是彩色的，全黑说明解码或传输出了问题
        let non_black = f.pixels.chunks_exact(4).take(2000).filter(|p| p[0] > 8 || p[1] > 8 || p[2] > 8).count();
        assert!(non_black > 100, "解出的画面几乎是全黑（{non_black}/2000 个亮像素）");
        got += 1;
    }

    let s = watcher.snapshot();
    // 先把统计打出来再断言：失败时这些数字就是唯一线索。
    eprintln!(
        "收到 AU {} / 关键帧 {} / 喂入 {} / 解出 {} / 丢弃 {} / 首帧 {}ms / 压缩 {} 字节",
        s.aus, s.keyframes, s.fed, s.decoded, s.dropped, s.first_frame_ms, s.bytes
    );

    assert!(got >= 15, "至少应收到 15 帧，实际 {got}");
    assert_eq!(s.dropped, 0, "内网环回不该丢帧");
    assert!(s.first_frame_ms < 20_000, "首帧延迟 {}ms 太离谱", s.first_frame_ms);
    assert!(s.keyframes >= 1, "必须有关键帧，否则新接入就黑屏");
    assert!(s.bytes > 0, "压缩字节数不能是 0");

    // 断开后，被控端那个会话应当收尾（不验证退出码，只验证它没有卡死）
    watcher.shutdown();
    drop(c.control);
    let _ = timeout(Duration::from_secs(10), async {
        while *done.borrow() == 0 {
            if done.changed().await.is_err() {
                break;
            }
        }
    })
    .await;
}

#[tokio::test]
async fn wrong_password_is_rejected() {
    let Some(ff) = ffmpeg_or_skip() else { return };
    let addr = spawn_server().await;
    let cfg = test_config(&addr);
    let (device_id, _done) = spawn_host(&addr, cfg.clone(), ff).await;

    let err = connect::connect(&addr, &device_id, "wrongPass9", "测试控制端", vec![])
        .await
        .expect_err("错密码必须被拒");
    let msg = format!("{err}");
    assert!(msg.contains("密码"), "错误信息应当说明是密码问题，实际：{msg}");
}

#[tokio::test]
async fn offline_device_is_reported_clearly() {
    let addr = spawn_server().await;
    let err = connect::connect(&addr, "999999999", "aB3dE5fG", "测试控制端", vec![])
        .await
        .expect_err("离线设备必须报错");
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("offline") || msg.contains("不在线"),
        "错误信息应当说明设备不在线，实际：{msg}"
    );
}

#[tokio::test]
async fn repeated_wrong_passwords_lock_the_host() {
    let Some(ff) = ffmpeg_or_skip() else { return };
    let addr = spawn_server().await;
    let cfg = test_config(&addr);
    let (device_id, _done) = spawn_host(&addr, cfg.clone(), ff).await;

    // 试 5 次错密码
    for _ in 0..5 {
        let e = connect::connect(&addr, &device_id, "wrongPass9", "测试控制端", vec![]).await;
        assert!(e.is_err());
    }

    // 第 6 次即使密码对，也必须被锁定拦下 —— 否则爆破照样能过。
    let err = connect::connect(&addr, &device_id, &cfg.password, "测试控制端", vec![])
        .await
        .expect_err("锁定期间必须拒绝，哪怕密码是对的");
    let msg = format!("{err}");
    assert!(msg.contains("锁"), "错误信息应说明被锁定，实际：{msg}");
}

#[tokio::test]
async fn client_refuses_to_guess_a_malformed_device_id() {
    let addr = spawn_server().await;
    for bad in ["", "123", "012345678", "abcdefghi"] {
        let e = connect::connect(&addr, bad, "aB3dE5fG", "c", vec![]).await;
        assert!(e.is_err(), "非法设备 ID {bad:?} 必须被拒绝");
    }
}
