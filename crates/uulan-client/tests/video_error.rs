//! 被控端的编码源收尾时，必须把「画面为什么停了」说出来（`H2C::VideoError`）。
//!
//! 这条测试钉的是**真实链路上的那句话**：真中继 + 真被控端 + 真 ffmpeg + 真加密视频通道。
//! 改动以前被控端在 EOF / 读出错两条路径上都是直接 `return`，控制端只能看见「帧不来了」，
//! 于是 15 秒后被静默看门狗判成「连接半开？」—— 结论是猜的，而且猜错的方向最害人
//! （现场照着这句话去查网络，而真正死的是编码器）。
//!
//! 找不着 ffmpeg 就整体跳过并说明，不伪装成通过。

use std::sync::Arc;
use std::time::{Duration, Instant};

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

async fn spawn_server() -> String {
    let server = uulan_server::Server::new(None).expect("建服务器失败");
    let listener = server.bind("127.0.0.1:0").await.expect("绑定失败");
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = server.serve(listener).await;
    });
    addr
}

/// 有界合成源（`@frames`）：ffmpeg 放完这么多帧就正常退出 —— 正好是被控端
/// 「视频源结束」那条以前什么都不说的路径。测试机没有 X11，这是唯一能稳定
/// 复现「会话中途画面停住」的办法。
fn test_config(server: &str, frames: u32) -> HostConfig {
    let mut cfg = HostConfig::default();
    cfg.server = server.to_string();
    cfg.name = "测试被控端".into();
    cfg.password = "aB3dE5fG".into();
    cfg.capture =
        CaptureInput::parse(&format!("lavfi:testsrc2=size=320x240:rate=25@{frames}")).unwrap();
    cfg.fps = 25;
    cfg.quality = uulan_protocol::msg::Quality::Auto;
    cfg.screen_width = 320;
    cfg.screen_height = 240;
    cfg
}

async fn spawn_host(
    server: &str,
    cfg: HostConfig,
    ffmpeg: std::path::PathBuf,
) -> (String, tokio::sync::watch::Receiver<u32>) {
    let mut conn = RvConn::connect(server).await.expect("被控端连不上中继");
    let (device_id, _) = conn
        .host_register(None, cfg.name.clone(), None)
        .await
        .expect("注册失败");

    let state = Arc::new(HostState::new(cfg.clone(), None));
    let screen = session::resolve_region(&cfg.capture, &cfg).await;
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
                    mic_sink: uulan_host::mic::MicSink::Speaker,
                    state: state.clone(),
                    ffmpeg: ffmpeg.clone(),
                    capture: cfg.capture.clone(),
                    screen,
                    encoder: Encoder::X264,
                    settings,
                    clipboard: Some(Box::new(uulan_clip::NoClipboard::new("端到端测试"))),
                    screen_probe: None,
                    pty: None,
                    fs_roots: None,
                    sys_source: None,
                    lock_probe: None,
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

/// 核心断言：画面停住之后，控制端**当场**就知道为什么，而不是干等 15 秒去猜。
#[tokio::test]
async fn the_client_learns_why_the_picture_stopped_without_waiting_for_the_watchdog() {
    let Some(ff) = ffmpeg_or_skip() else { return };
    let addr = spawn_server().await;
    let cfg = test_config(&addr, 12);
    let (device_id, _done) = spawn_host(&addr, cfg.clone(), ff.clone()).await;

    let c = timeout(
        T,
        connect::connect(&addr, &device_id, &cfg.password, "测试控制端", vec![]),
    )
    .await
    .expect("连接超时")
    .expect("连接失败");

    let mut watcher = VideoWatcher::start(c.video, Some(ff))
        .await
        .expect("启动解码失败");

    // 先确认真的出过画面：没有这一段，「结束了」可能只是「根本没连上」。
    let first = timeout(T, watcher.next_frame()).await.expect("帧没来（画面没流动）");
    assert!(first.is_some(), "一帧都没解出来，这条测试就没在验「中途停住」");

    // 关键：从「开始等结束」到「真的结束」应当是毫秒级，而不是静默看门狗的 15 秒。
    let t0 = Instant::now();
    timeout(Duration::from_secs(10), watcher.wait_end())
        .await
        .expect("10 秒内没结束：被控端还是什么都不说，只能等静默看门狗（15 秒）判死");
    let took = t0.elapsed();

    let reason = watcher.video_end();
    let s = watcher.snapshot();
    eprintln!("结束原因：{reason:?}（等了 {took:?}，收到 AU {}）", s.aus);

    assert!(!reason.is_empty(), "被控端什么都没说 —— 控制端只能靠静默看门狗猜原因");
    assert!(s.video_said, "统计里必须留下「对面说过话」的痕迹");
    assert!(
        reason.contains("已出帧"),
        "原因里要带得出画面的量，否则分不清「没连上」与「连上后断了」：{reason}"
    );
    // 有界合成源放完是**正常结束**。把它记成故障，会让每一轮无人值守验收凭空变红
    // （同音频状态里「Ended 不算 Failed」那条口径）。
    assert!(!s.video_failed, "合成源按计划放完（12/12 帧）不该算编码器故障：{reason}");
    assert_eq!(s.silent_s, 0, "既然被控端当场就说了原因，静默看门狗就不该介入");
}

/// 老被控端（从来不发这句话）的行为不能变：仍然由静默看门狗收尾，
/// 而且原因栏是「空的」——上层据此退回原来的推断措辞。
#[tokio::test]
async fn silence_from_an_old_host_still_leaves_the_reason_empty() {
    // 这条不用真链路：只钉「空」这个状态与「说了但不是故障」分得开。
    // 真链路那两条测试覆盖的是「说了」的那一半。
    init_logs();
    let st = uulan_client::watch::WatchStats::default();
    assert!(!st.snapshot().video_said);
    assert!(st.video_end_text().is_empty());
    st.note_video_end("被控端编码源出错：ffmpeg 没了", true);
    let s = st.snapshot();
    assert!(s.video_said && s.video_failed);
}
