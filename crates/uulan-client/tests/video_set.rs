//! 画面参数热更新（`C2H::VideoSet`）的端到端验收：**缩放编码 + 改帧率**，整条真链路。
//!
//! 全是生产代码：真 server、真被控端会话、真 lavfi 采集、真 x264 编码、真协议、真 ffmpeg 解码。
//! 唯一替身是注入 sink（记录型，用来断言「坐标仍按**屏幕**尺寸换算」而不真的点桌面）。
//!
//! 这条测试要钉住的是两个**很容易被写成同一个量**的事实：
//!
//! * **屏幕尺寸**（`VideoCfg.w/h`）：控制端拿它把归一化坐标乘回像素；
//! * **编码画幅**（`VideoCfg.enc_w/enc_h`）：解码器按它启动，rawvideo 每帧字节数在那一刻定死。
//!
//! 缩放编码时两者不相等。早期实现只报一个尺寸 —— 于是 `--scale` 一开，控制端会按
//! 全尺寸去读 1/4 画幅的字节流（斜条纹），或者把点击落点乘错（点哪儿都偏）。
//!
//! 断言四件事：
//!   · 下发 `scale=0.25` 之后解出 80x60 的帧（解码器真的按编码画幅重建了）
//!   · `VideoCfg` 里的屏幕尺寸仍是 320x240（不能被缩放改掉）
//!   · 被控端接受了 `fps=6`（`VideoCfg.fps` 回报 6，不是配置里的 10）
//!   · 归一化坐标仍按屏幕尺寸换算：0.5,0.5 → 160,120（不是 40,30）

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::timeout;

use uulan_client::connect;
use uulan_client::control::ControlLink;
use uulan_client::watch::VideoWatcher;
use uulan_host::capture::CaptureInput;
use uulan_host::config::HostConfig;
use uulan_host::input::{Inject, InputSink};
use uulan_host::session::{self, HostState, SessionParams, VideoSettings};
use uulan_media::ffmpeg::{self, Encoder};
use uulan_protocol::msg::{C2H, Quality, VideoSet};
use uulan_protocol::rv::{Role, ToEndpoint};
use uulan_protocol::rvclient::RvConn;

struct RecordingSink {
    out: Arc<Mutex<Vec<Inject>>>,
}

impl InputSink for RecordingSink {
    fn name(&self) -> &'static str {
        "recording"
    }

    fn apply(&mut self, batch: &[Inject]) -> Result<(), String> {
        self.out.lock().unwrap().extend_from_slice(batch);
        Ok(())
    }
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

fn test_config(server: &str) -> HostConfig {
    let mut cfg = HostConfig::default();
    cfg.server = server.to_string();
    cfg.name = "画面参数测试被控端".into();
    cfg.password = "aB3dE5fG".into();
    cfg.capture = CaptureInput::parse("lavfi:testsrc2=size=320x240:rate=10").unwrap();
    cfg.fps = 10;
    cfg.screen_width = 0;
    cfg.screen_height = 0;
    cfg
}

/// 起一个被控端：采集源是 320x240 的合成画面，屏幕上的一切都由 lavfi 规格决定。
async fn spawn_host(server: &str, cfg: HostConfig, out: Arc<Mutex<Vec<Inject>>>) -> String {
    let mut conn = RvConn::connect(server).await.expect("被控端连不上中继");
    let (device_id, _) = conn
        .host_register(None, cfg.name.clone(), None)
        .await
        .expect("注册失败");

    let state = Arc::new(HostState::new(cfg.clone(), None));
    let initial = session::resolve_screen(&cfg.capture, &cfg).await;
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
                    screen: initial,
                    encoder: Encoder::X264,
                    settings,
                    clipboard: Some(Box::new(uulan_clip::NoClipboard::new("画面参数测试"))),
                    screen_probe: None,
                    pty: None,
                    fs_roots: None,
                    sys_source: None,
                    lock_probe: None,
                };
                let a = addr.clone();
                let rec = out.clone();
                tokio::spawn(async move {
                    if let Ok(c) = RvConn::connect(&a).await {
                        if let Ok(relays) = c.relay_open_all(&sid, Role::Host).await {
                            let _ = session::run_with_sink(
                                params,
                                relays,
                                client_addr,
                                Box::new(RecordingSink { out: rec }),
                            )
                            .await;
                        }
                    }
                });
            }
        }
    });

    device_id
}

#[tokio::test]
async fn video_set_scales_the_encoder_without_touching_screen_size() {
    let Some(ff) = ffmpeg::find_ffmpeg(None) else {
        eprintln!("跳过：本机没有 ffmpeg");
        return;
    };
    let addr = spawn_server().await;
    let cfg = test_config(&addr);
    let out = Arc::new(Mutex::new(Vec::<Inject>::new()));
    let device_id = spawn_host(&addr, cfg.clone(), out.clone()).await;

    let c = timeout(
        Duration::from_secs(30),
        connect::connect(&addr, &device_id, &cfg.password, "画面参数测试控制端", vec![]),
    )
    .await
    .expect("连接超时")
    .expect("连接失败");
    assert_eq!((c.host.screen.w, c.host.screen.h), (320, 240));

    drop(c.bulk);
    let link = ControlLink::start(c.control);
    let input = link.input();
    let mut watcher = VideoWatcher::start(c.video, Some(ff)).await.expect("启动解码失败");
    assert_eq!(watcher.info().encoded_size(), (320, 240), "还没缩放时编码画幅 = 屏幕");

    // ① 先确认基线：解出几帧 320x240
    let mut before = 0u32;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while before < 3 && tokio::time::Instant::now() < deadline {
        match timeout(Duration::from_secs(5), watcher.next_frame()).await {
            Ok(Some(f)) => {
                assert_eq!((f.width, f.height), (320, 240));
                before += 1;
            }
            _ => break,
        }
    }
    assert!(before >= 3, "缩放之前只解出 {before} 帧");

    // ② 下发画面参数：缩到 1/4 画幅、帧率 6、画质不表态（Keep）
    let req = VideoSet {
        fps: 6,
        max_bitrate_kbps: 0,
        quality: Quality::Keep,
        scale: 0.25,
        chroma: "420".into(),
        encoder: "auto".into(),
    };
    assert!(input.send(C2H::VideoSet(req)), "控制通道没能下发画面参数");

    // ③ 之后必须解出 80x60（320x240 * 0.25）
    let mut after = 0u32;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
    while after < 3 && tokio::time::Instant::now() < deadline {
        match timeout(Duration::from_secs(10), watcher.next_frame()).await {
            Ok(Some(f)) => {
                if (f.width, f.height) == (80, 60) {
                    assert_eq!(
                        f.pixels.len(),
                        ffmpeg::frame_bytes(80, 60),
                        "80x60 的帧字节数不对"
                    );
                    after += 1;
                } else {
                    // 换代边界上可能有旧尺寸的帧还在路上，允许，但只能是一种
                    assert_eq!((f.width, f.height), (320, 240), "出现了第三种画幅");
                }
            }
            _ => break,
        }
    }

    let s = watcher.snapshot();
    eprintln!(
        "帧：320x240 {before} 帧 / 80x60 {after} 帧；重建 {} 次；解出 {} / 喂入 {} AU / 丢帧 {}；\
         请求帧率 {} / 实测 {:.1}",
        s.resizes,
        s.decoded,
        s.fed,
        s.dropped,
        s.cfg_fps,
        watcher.stats().host_fps()
    );
    assert!(after >= 3, "缩放之后没解出足够的小画幅帧（只 {after} 帧）");
    assert!(s.resizes >= 1, "控制端没有按编码画幅重建解码器");
    assert_eq!((s.enc_w, s.enc_h), (80, 60), "统计里的编码画幅没跟上");
    assert_eq!((s.cfg_w, s.cfg_h), (320, 240), "屏幕尺寸被缩放改掉了（输入换算会全偏）");
    assert_eq!(s.cfg_fps, 6, "被控端没接受请求的帧率（VideoCfg.fps 仍是 {})", s.cfg_fps);

    // ④ 归一化坐标必须仍按**屏幕**尺寸换算：正中 (0.5,0.5) → 160,120（不是 40,30）
    assert!(input.send(C2H::MouseMoveAbs { x: 0.5, y: 0.5 }));
    let want = Inject::MoveAbs { x: 160, y: 120 };
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(hit) = out
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|i| matches!(i, Inject::MoveAbs { .. }))
            .cloned()
        {
            assert_eq!(hit, want, "缩放编码把点击落点也缩了（输入映射器错用了编码画幅）");
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("没等到注入指令");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    watcher.shutdown();
}