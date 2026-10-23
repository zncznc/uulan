//! M2.5 验收：**被控端屏幕分辨率变了 → 编码源重开 → 控制端重建解码器**，整条真链路。
//!
//! 全是生产代码：真 server、真被控端会话、真 lavfi 采集、真 x264 编码、真协议、真 ffmpeg 解码。
//! 唯一替身是两处：
//!   1. 屏幕尺寸探针（本机没有 X11，塞一个按剧本返回尺寸的假探针）
//!   2. 注入 sink（记录型，断言「坐标按新尺寸换算」而不真的点桌面）
//!
//! 断言三件事：
//!   · 换之前解出 320x240 的帧
//!   · 换之后解出 480x360 的帧（控制端真的重建了解码器，而不是继续按旧尺寸读）
//!   · 换之后控制端发的归一化坐标被按新尺寸换算（0.5,0.5 → 240,180）

use std::sync::atomic::{AtomicU32, Ordering};
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
use uulan_protocol::msg::{C2H, Screen};
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
    cfg.name = "分辨率测试被控端".into();
    cfg.password = "aB3dE5fG".into();
    cfg.capture = CaptureInput::parse("lavfi:testsrc2=size=320x240:rate=10").unwrap();
    cfg.fps = 10;
    // 不钉死尺寸：让探针说了算（钉死时生产探针是关掉的）
    cfg.screen_width = 0;
    cfg.screen_height = 0;
    cfg
}

/// 「每有人连进来就造一个新探针」的工厂。
///
/// 用工厂而不是直接传探针，是因为被控端每接一个会话都要一个**新**探针闭包
/// （生产上每条会话各开一个 `xdpyinfo` 探针，互不干扰）。
type ProbeFactory = Arc<dyn Fn() -> session::ScreenProbe + Send + Sync>;

/// 起一个被控端；屏幕尺寸由 `screen_w/screen_h` 两个原子量驱动。
async fn spawn_host(
    server: &str,
    cfg: HostConfig,
    out: Arc<Mutex<Vec<Inject>>>,
    screen: (Arc<AtomicU32>, Arc<AtomicU32>),
) -> String {
    let (sw, sh) = screen;
    let probes: ProbeFactory = Arc::new(move || {
        let (pw, ph) = (sw.clone(), sh.clone());
        Arc::new(move || {
            Some(Screen {
                w: pw.load(Ordering::Relaxed),
                h: ph.load(Ordering::Relaxed),
            })
        }) as session::ScreenProbe
    });
    spawn_host_with(server, cfg, out, probes).await
}

/// 同上，但探针由调用方决定（用来复现「源 EOF 之后才发现分辨率变了」这条路径）。
async fn spawn_host_with(
    server: &str,
    cfg: HostConfig,
    out: Arc<Mutex<Vec<Inject>>>,
    probes: ProbeFactory,
) -> String {
    let mut conn = RvConn::connect(server).await.expect("被控端连不上中继");
    let (device_id, _) = conn
        .host_register(None, cfg.name.clone())
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
                let probe = probes();
                let params = SessionParams {
                    state: state.clone(),
                    ffmpeg: ffmpeg.clone(),
                    capture: cfg.capture.clone(),
                    screen: initial,
                    encoder: Encoder::X264,
                    settings,
                    clipboard: Some(Box::new(uulan_clip::NoClipboard::new("分辨率测试"))),
                    screen_probe: Some(probe),
                    pty: None,   // 终端后端：None = 生产用的 portable-pty
                    fs_roots: None,
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
async fn host_resolution_change_rebuilds_client_decoder() {
    let Some(ff) = ffmpeg::find_ffmpeg(None) else {
        eprintln!("跳过：本机没有 ffmpeg");
        return;
    };
    let addr = spawn_server().await;
    let cfg = test_config(&addr);
    let out = Arc::new(Mutex::new(Vec::<Inject>::new()));
    let sw = Arc::new(AtomicU32::new(320));
    let sh = Arc::new(AtomicU32::new(240));
    let device_id = spawn_host(&addr, cfg.clone(), out.clone(), (sw.clone(), sh.clone())).await;

    let c = timeout(
        Duration::from_secs(30),
        connect::connect(&addr, &device_id, &cfg.password, "分辨率测试控制端", vec![]),
    )
    .await
    .expect("连接超时")
    .expect("连接失败");
    assert_eq!((c.host.screen.w, c.host.screen.h), (320, 240));

    drop(c.bulk);
    let link = ControlLink::start(c.control);
    let input = link.input();

    let mut watcher = VideoWatcher::start(c.video, Some(ff))
        .await
        .expect("启动解码失败");
    assert_eq!((watcher.info().w, watcher.info().h), (320, 240));

    // ① 换之前：至少要有一帧 320x240
    let mut small = 0u32;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while small < 3 && tokio::time::Instant::now() < deadline {
        match timeout(Duration::from_secs(5), watcher.next_frame()).await {
            Ok(Some(f)) => {
                assert_eq!((f.width, f.height), (320, 240), "换分辨率之前不该出现别的画幅");
                small += 1;
            }
            _ => break,
        }
    }
    assert!(small >= 3, "换分辨率之前只解出 {small} 帧");

    // ② 让被控端「换屏幕」：探针下一拍就会返回新尺寸
    sw.store(480, Ordering::Relaxed);
    sh.store(360, Ordering::Relaxed);

    // ③ 换之后：必须解出 480x360 的帧
    let mut big = 0u32;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
    while big < 3 && tokio::time::Instant::now() < deadline {
        match timeout(Duration::from_secs(10), watcher.next_frame()).await {
            Ok(Some(f)) => {
                if (f.width, f.height) == (480, 360) {
                    assert_eq!(
                        f.pixels.len(),
                        ffmpeg::frame_bytes(480, 360),
                        "480x360 的帧字节数不对"
                    );
                    big += 1;
                } else {
                    // 换代边界上可能有旧尺寸的帧还在路上，允许，但要有上限
                    assert_eq!((f.width, f.height), (320, 240), "出现了第三种画幅");
                }
            }
            _ => break,
        }
    }

    let s = watcher.snapshot();
    eprintln!(
        "帧：320x240 {small} 帧 / 480x360 {big} 帧；重建 {} 次；解出 {} / 喂入 {} AU / 丢帧 {}",
        s.resizes, s.decoded, s.fed, s.dropped
    );
    assert!(big >= 3, "换分辨率之后没解出足够的新尺寸帧（只 {big} 帧）");
    assert!(s.resizes >= 1, "控制端没有重建解码器");
    assert_eq!((s.cfg_w, s.cfg_h), (480, 360), "统计里的当前画幅没跟上");

    // ④ 坐标换算必须跟着换：正中 (0.5,0.5) 在 480x360 上应是 (240,180)
    assert!(input.send(C2H::MouseMoveAbs { x: 0.5, y: 0.5 }));
    let want = Inject::MoveAbs { x: 240, y: 180 };
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(hit) = out.lock().unwrap().iter().rev().find(|i| matches!(i, Inject::MoveAbs { .. })).cloned() {
            assert_eq!(hit, want, "点击落点没按新尺寸换算（输入映射器没同步分辨率）");
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("没等到注入指令");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    watcher.shutdown();
}

/// 抓屏源在「抓取区域比屏幕大」时是**直接退出**（ffmpeg stdout EOF），不是报错。
///
/// 2026-10-23 真麒麟实测：把 :0 从 1920x1080 改成 1600x900，会话当场断，
/// journal 里只有一句「视频源结束」—— 因为 `Ok(None)` 这条分支被当成
/// 「合成源放完了」直接 return，视频通道跟着关掉，控制端看到 EOF 就退出。
///
/// 复现方式：用**有限长度**的 lavfi 源（`duration=1`），它不限速（没加 `-re`），
/// 所以 1 秒的素材在这个进程里 0.1 秒就跑完 → EOF 在第 1 秒的常规探测**之前**到达。
/// 于是「发现尺寸变了」只可能发生在 EOF 的那次补探上：要是有人把 `Ok(None)`
/// 分支改回直接 return，视频通道会在 0.1 秒后就关掉，这里一帧新尺寸都拿不到。
#[tokio::test]
async fn source_eof_on_screen_shrink_restarts_encoder() {
    let Some(ff) = ffmpeg::find_ffmpeg(None) else {
        eprintln!("跳过：本机没有 ffmpeg");
        return;
    };
    let addr = spawn_server().await;
    let mut cfg = test_config(&addr);
    // 有限长度：素材放完 ffmpeg 自己退出 → 被控端读到 Ok(None)
    cfg.capture = CaptureInput::parse("lavfi:testsrc2=size=320x240:rate=10:duration=1").unwrap();
    let out = Arc::new(Mutex::new(Vec::<Inject>::new()));

    // 探针一律报「新尺寸」。第一代源 0.1 秒就放完了，1 秒的常规探测根本来不及跑，
    // 所以这次被问到的一定是 EOF 后的补探（`asked` 用来在失败时把这件事说清楚）。
    let asked = Arc::new(AtomicU32::new(0));
    let probes: ProbeFactory = {
        let asked = asked.clone();
        Arc::new(move || {
            let asked = asked.clone();
            Arc::new(move || {
                asked.fetch_add(1, Ordering::Relaxed);
                Some(Screen { w: 480, h: 360 })
            }) as session::ScreenProbe
        })
    };
    let device_id = spawn_host_with(&addr, cfg.clone(), out.clone(), probes).await;

    let c = timeout(
        Duration::from_secs(30),
        connect::connect(&addr, &device_id, &cfg.password, "EOF 测试控制端", vec![]),
    )
    .await
    .expect("连接超时")
    .expect("连接失败");
    drop(c.bulk);

    let mut watcher = VideoWatcher::start(c.video, Some(ff))
        .await
        .expect("启动解码失败");
    assert_eq!((watcher.info().w, watcher.info().h), (320, 240));

    // ① 起点：握手时被控端报的画幅是 320x240（第一代的 VideoCfg）
    assert_eq!((watcher.info().w, watcher.info().h), (320, 240));

    // ② EOF 之后：编码源必须按新尺寸重开，控制端跟着重建解码器。
    //
    // 第一代只有 10 帧、而且 lavfi 不限速（0.1 秒就放完），所以「换代前的帧」
    // 可能在测试开始读之前就被新尺寸盖掉了（watch 通道只留最新一帧）。
    // 因此这里不断言「先看到 320x240」，只数：真正有判别力的是**新尺寸的帧有没有**。
    let mut small = 0u32;
    let mut big = 0u32;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while big < 3 && tokio::time::Instant::now() < deadline {
        match timeout(Duration::from_secs(10), watcher.next_frame()).await {
            Ok(Some(f)) => match (f.width, f.height) {
                (320, 240) => small += 1,
                (480, 360) => big += 1,
                other => panic!("出现了第三种画幅 {other:?}"),
            },
            _ => break,
        }
    }

    let s = watcher.snapshot();
    eprintln!(
        "EOF 场景：320x240 {small} 帧 / 480x360 {big} 帧；重建 {} 次；解出 {} / 喂入 {} AU",
        s.resizes, s.decoded, s.fed
    );
    assert!(
        big >= 3,
        "源 EOF 之后没有按新尺寸重开（只拿到 {big} 帧 480x360；探针被问了 {} 次）\
         —— 会话被 EOF 断掉了",
        asked.load(Ordering::Relaxed)
    );
    assert!(s.resizes >= 1, "控制端没有重建解码器");
    assert_eq!((s.cfg_w, s.cfg_h), (480, 360), "统计里的当前画幅没跟上");

    watcher.shutdown();
}
