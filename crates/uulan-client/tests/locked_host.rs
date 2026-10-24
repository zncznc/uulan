//! M6 收尾验收：`H2C::SystemState` / `H2C::Metrics` 两端真的接上了。
//!
//! 链路（除「系统信息源」与「注入后端」两个替身外全是生产代码）：
//!
//! ```text
//! 被控端 /proc（这里换成假来源）→ MetricsSampler → H2C::Metrics ┐
//! 被控端 X11 焦点名（换成假探针）→ SystemState ────────────────┼→ control_loop
//!                                                              └→ 中继 → 控制端 ControlLink
//!                                                                       → HostStatus（锁屏门禁 / 标题）
//! ```
//!
//! 为什么这两条必须能在本机验：以前它们是「协议里有、两端都没接」的典型 ——
//! 控制端既看不到对面负载，也不知道对面锁屏，还会把键鼠继续往锁屏里送。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::timeout;

use uulan_client::connect;
use uulan_client::control::ControlLink;
use uulan_host::capture::CaptureInput;
use uulan_host::config::HostConfig;
use uulan_host::input::{Inject, InputSink};
use uulan_host::probe::{CpuTimes, DesktopState, SysSource};
use uulan_host::session::{self, HostState, SessionParams, VideoSettings};
use uulan_media::ffmpeg::{self, Encoder};
use uulan_protocol::msg::C2H;
use uulan_protocol::rv::{Role, ToEndpoint};
use uulan_protocol::rvclient::RvConn;

/// 记录型注入后端：锁屏门禁要是失效了，这里会多出东西。
struct RecordingSink(Arc<Mutex<Vec<Inject>>>);

impl InputSink for RecordingSink {
    fn name(&self) -> &'static str {
        "recording"
    }
    fn apply(&mut self, batch: &[Inject]) -> Result<(), String> {
        self.0.lock().unwrap().extend_from_slice(batch);
        Ok(())
    }
}

/// 假的 `/proc`：每次采样 CPU 总时间 +100、空闲 +40（= 60% 忙），内存固定 3/8 GiB。
struct FakeProc {
    calls: AtomicUsize,
}

impl SysSource for FakeProc {
    fn cpu_times(&self) -> Option<CpuTimes> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) as u64;
        Some(CpuTimes { total: 1_000 + n * 100, idle: 400 + n * 40 })
    }
    fn mem(&self) -> Option<(u64, u64)> {
        Some((3 * 1024 * 1024 * 1024, 8 * 1024 * 1024 * 1024))
    }
    fn net(&self) -> Option<(u64, u64)> {
        Some((0, 0))
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
    cfg.name = "锁屏测试被控端".into();
    cfg.password = "aB3dE5fG".into();
    cfg.capture = CaptureInput::parse("lavfi:testsrc2=size=320x240:rate=10").unwrap();
    cfg.fps = 10;
    cfg.screen_width = 320;
    cfg.screen_height = 240;
    cfg
}

async fn spawn_locked_host(server: &str, cfg: HostConfig, out: Arc<Mutex<Vec<Inject>>>) -> String {
    let mut conn = RvConn::connect(server).await.expect("被控端连不上中继");
    let (device_id, _) = conn
        .host_register(None, cfg.name.clone())
        .await
        .expect("注册失败");
    let state = Arc::new(HostState::new(cfg.clone(), None));
    let screen = session::resolve_screen(&cfg.capture, &cfg).await;
    let settings = VideoSettings::from_config(&cfg);
    let addr = conn.server_addr().to_string();
    let ffmpeg = ffmpeg::find_ffmpeg(None).unwrap_or_else(|| std::path::PathBuf::from("ffmpeg"));

    tokio::spawn(async move {
        loop {
            let Ok(msg) = conn.recv().await else { break };
            if let ToEndpoint::Incoming { session: sid, client_addr, .. } = msg {
                let params = SessionParams {
                    state: state.clone(),
                    ffmpeg: ffmpeg.clone(),
                    capture: cfg.capture.clone(),
                    screen,
                    encoder: Encoder::X264,
                    settings,
                    clipboard: Some(Box::new(uulan_clip::NoClipboard::new("锁屏链路测试"))),
                    screen_probe: None,
                    pty: None,
                    fs_roots: None,
                    sys_source: Some(Arc::new(FakeProc { calls: AtomicUsize::new(0) })),
                    // 这台「被控端」一直是锁屏状态（真机上是 UKUI 的
                    // `ukui-screensaver-dialog` 抢走了输入焦点）。
                    lock_probe: Some(Arc::new(|| {
                        Some(DesktopState { locked: true, has_desktop: true })
                    })),
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
                                Box::new(RecordingSink(rec)),
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
async fn locked_host_reports_state_and_load_and_the_client_stops_injecting() {
    let addr = spawn_server().await;
    let cfg = test_config(&addr);
    let out = Arc::new(Mutex::new(Vec::<Inject>::new()));
    let device_id = spawn_locked_host(&addr, cfg.clone(), out.clone()).await;

    let c = timeout(
        Duration::from_secs(20),
        connect::connect(&addr, &device_id, &cfg.password, "锁屏测试控制端", vec![]),
    )
    .await
    .expect("连接超时")
    .expect("连接失败");
    drop(c.video);
    drop(c.bulk);

    let link = ControlLink::start(c.control);
    let st = link.status();

    // ① 锁屏状态必须到达控制端（被控端每秒探一次，第一拍就是立即的）
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !(st.snapshot().known && st.snapshot().locked) {
        assert!(std::time::Instant::now() < deadline, "10 秒内没收到 SystemState{{locked:true}}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // ② 两拍指标（第一拍没有基线 ⇒ cpu=0；第二拍才有真数）
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while st.snapshot().metrics_recv < 2 {
        assert!(std::time::Instant::now() < deadline, "10 秒内没收到两条 Metrics");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let s = st.snapshot();
    assert_eq!((s.mem_used_mb, s.mem_total_mb), (3072, 8192), "内存要按 MB 报：{s:?}");
    assert!(
        (s.cpu_pct_x100 as f64 - 6000.0).abs() < 200.0,
        "CPU 应当约 60%（假来源给的差分），实际 {}",
        s.cpu_pct_x100 as f64 / 100.0
    );

    // ③ 锁屏期间控制端根本不该把键鼠发出去
    let input = link.input();
    assert!(input.send(C2H::MouseMoveAbs { x: 0.5, y: 0.5 }), "通道没坏，返回 true");
    assert!(input.send(C2H::Key { vk: 0x41, down: true, repeat: false }));
    // 非输入消息照走（锁屏时还要能传文件、发剪贴板）—— 用 Ping 当哨兵
    assert!(input.send(C2H::Ping { seq: 7, sent_ms: 0 }));

    tokio::time::sleep(Duration::from_millis(300)).await;
    let dropped = st.snapshot().dropped_locked;
    assert_eq!(dropped, 2, "两条输入应当被控制端挡下，实际挡了 {dropped}");
    assert!(
        out.lock().unwrap().is_empty(),
        "锁屏时被控端一条注入指令都不该收到：{:?}",
        out.lock().unwrap()
    );
    assert_eq!(link.stats().snapshot().total(), 0, "没发出去就不该计入上报数");

    link.close("锁屏链路测试结束").await;
}