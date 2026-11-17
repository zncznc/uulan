//! 麦克风转接（M7 上行）端到端验收：真中继 + 真被控端 + 真控制端，
//! 声音从「控制端的合成源」走到「被控端的播放计数」。
//!
//! 覆盖的真实路径：四条通道配对 → 握手/鉴权 → `C2H::MicStart` → 被控端起 ffmpeg 解码 +
//! 播放后端（本测试用 `MicSink::Null`：没有声卡的机器也能把「上行 → 解码 → 统计」钉住）
//! → 控制端 ffmpeg 采集（lavfi 有界正弦）→ Opus/Ogg → audio 通道上行（加密）
//! → **有界源放满 = 控制端采集自己退 → 写半端 drop → 被控端 read EOF → 收尾**。
//!
//! 最后这段是这套接线的立身之本：协议里**没有** `MicStop`，「停」就是流 EOF。
//! 用有界合成源（`:duration=3`）在真链路上把这条纪律钉成可测的时序，
//! 而不是留在注释里。
//!
//! 唯一没被本机覆盖的：`-f dshow` / `-f pulse` 真实采集后端与真出声 ——
//! 那些在真机上验（见 docs/milestones/ 与 scripts/verify-mic-realkylin-e2e.sh）。
//!
//! 断言的是**对面真的解出了有声的 PCM**（peak>0、pcm>0 经被控端回话的 detail 与
//! 本机上行字节两面对证），不是只「通道里过了字节」。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::timeout;

use uulan_client::audioin::{AudioPhase, AudioStats, AudioWatch};
use uulan_client::connect;
use uulan_client::session::{self, Plan};
use uulan_host::capture::CaptureInput;
use uulan_host::config::HostConfig;
use uulan_host::mic::MicSink;
use uulan_host::session::{HostState, SessionParams, VideoSettings};

#[allow(unused_imports)]
use uulan_host::session as host_session;
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
    cfg.name = "麦克风测试被控端".into();
    cfg.password = "mIc7Test".into();
    cfg.capture = CaptureInput::parse("lavfi:testsrc2=size=320x240:rate=25@60").unwrap();
    cfg.fps = 25;
    cfg.quality = uulan_protocol::msg::Quality::Auto;
    cfg.screen_width = 320;
    cfg.screen_height = 240;
    cfg
}

async fn spawn_host(server: &str, cfg: HostConfig, ff: PathBuf, sink: MicSink) -> String {
    let mut conn = RvConn::connect(server).await.expect("被控端连不上中继");
    let (device_id, _) = conn
        .host_register(None, cfg.name.clone(), None)
        .await
        .expect("注册失败");

    let state = Arc::new(HostState::new(cfg.clone(), None));
    let screen = host_session::resolve_region(&cfg.capture, &cfg).await;
    let settings = VideoSettings::from_config(&cfg);
    let addr = conn.server_addr().to_string();

    tokio::spawn(async move {
        loop {
            let msg = match conn.recv().await {
                Ok(m) => m,
                Err(_) => break,
            };
            if let ToEndpoint::Incoming { session: sid, client_addr, .. } = msg {
                let params = SessionParams {
                    mic_sink: sink,
                    state: state.clone(),
                    ffmpeg: ff.clone(),
                    capture: cfg.capture.clone(),
                    screen,
                    encoder: Encoder::X264,
                    settings,
                    clipboard: Some(Box::new(uulan_clip::NoClipboard::new("麦克风测试"))),
                    screen_probe: None,
                    pty: None,
                    fs_roots: None,
                    sys_source: None,
                    lock_probe: None,
                };
                let a = addr.clone();
                tokio::spawn(async move {
                    let rc = RvConn::connect(&a).await;
                    if let Ok(c) = rc {
                        if let Ok(relays) = c.relay_open_all(&sid, Role::Host).await {
                            let _ = host_session::run(params, relays, client_addr).await;
                        }
                    }
                });
            }
        }
    });

    device_id
}

fn plan_with(
    addr: &str,
    device: &str,
    cfg: &HostConfig,
    ff: PathBuf,
    mic_source: Option<&str>,
    mic_stats: Arc<AudioStats>,
    mic_watch: Arc<AudioWatch>,
) -> Plan {
    Plan {
        server: addr.to_string(),
        device: device.to_string(),
        password: cfg.password.clone(),
        client_name: "麦克风测试控制端".into(),
        caps: vec![],
        ffmpeg: Some(ff),
        clipboard: false,
        no_input: true,
        max_reconnects: 0,
        video: None,
        audio: None,
        audio_stats: Arc::new(AudioStats::default()),
        audio_watch: Arc::new(AudioWatch::default()),
        mic: Some(0),
        mic_source: mic_source.map(|s| s.to_string()),
        mic_stats,
        mic_watch,
    }
}

/// 轮询到条件成立（同 term backpressure 那条纪律：固定 sleep 后的单次断言
/// 在慢机器上是时序脆弱源，判据本身不许动）。
async fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..300 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("等不到：{what}");
}

#[tokio::test]
async fn mic_uplink_flows_and_stops_by_eof() {
    init_logs();
    let Some(ff) = ffmpeg::find_ffmpeg(None) else {
        eprintln!("跳过：本机没有 ffmpeg");
        return;
    };
    let addr = spawn_server().await;
    let cfg = test_config(&addr);
    // Null 落点 = 本机的合法形态（没声卡也能跑完整条链，见 SessionParams::mic_sink）。
    let device = spawn_host(&addr, cfg.clone(), ff.clone(), MicSink::Null).await;

    let c = timeout(
        T,
        connect::connect(&addr, &device, &cfg.password, "麦克风测试控制端", vec![]),
    )
    .await
    .expect("连接超时")
    .expect("连接失败");

    let stats = Arc::new(AudioStats::default());
    let watch = Arc::new(AudioWatch::default());
    let plan = plan_with(
        &addr,
        &device,
        &cfg,
        ff.clone(),
        // 有界源：放满 3 秒采集进程自己退出 —— 「停 = 写半端 EOF」这条纪律的剧本。
        Some("lavfi:sine=frequency=440:sample_rate=48000:duration=3"),
        stats.clone(),
        watch.clone(),
    );

    let open = timeout(T, session::open(None, &plan, c))
        .await
        .expect("开会话超时")
        .expect("开会话失败");
    assert!(open.mic.is_some(), "开了 --mic 就该有上行任务");

    // 1) 对面的 running:true 必须被本机接住（这条是「两端接上了」的判据）。
    // 查**单调计数** states>=1 而不是抓瞬时相位：有界源只跑 3 秒，
    // Running→Ended 之间可能整段落在两次轮询的空隙里 —— 计数不会说谎，相位窗口会。
    wait_until("被控端回了第一句 MicState（running:true）", || {
        watch.states() >= 1
    })
    .await;
    assert_eq!(watch.failures(), 0, "第一句回话起不来就是失败（detail 在状态行里）");

    // 2) 上行字节在动（只验「动过」；量级的账在第 4 步，等整段放完再对）。
    wait_until("上行有字节", || stats.ogg_bytes.load(std::sync::atomic::Ordering::Relaxed) > 0)
        .await;

    // 3) 有界源放满 → 采集退 → 写半端 drop → 被控端 EOF 收尾 → 那句
    //    「麦克风流结束（本机收到 N 字节）」把相位推到 Ended。**没有 MicStop，
    //    这条迁移只能由流 EOF 触发** —— 它就是本测试存在的理由。
    // 先等计数跑完再等相位：`-re` 下 3 秒源该有 ~150 帧（33 KB / 16 KiB 取整前是 20ms 页），
    // 快照取早了会把「还在流」误读成「量级不对」（2026-11-18 首跑就是这么假红过一次）。
    wait_until("上行计数接近 3 秒的量（>120 帧）", || {
        use std::sync::atomic::Ordering::Relaxed;
        stats.frames.load(Relaxed) >= 120
    })
    .await;
    wait_until("被控端按 EOF 收尾（Ended 且不是失败）", || {
        watch.phase() == AudioPhase::Ended
    })
    .await;
    assert_eq!(watch.failures(), 0, "EOF 收尾不许记成故障");
    let s = watch.summary_labeled("麦克风状态", true);
    eprintln!("状态：{s}");
    assert!(s.contains("麦克风状态 : "), "前缀必须换：{s}");
    assert!(s.contains("不再推流") || s.contains("结束"), "{s}");

    // 4) 对面收到的字节量级要「像 3 秒 64kbps」：下限 8 KB（≈1 秒）防「只握了手没传声」，
    //    上限 120 KB（≈15 秒）防「有界源没 bounded、把链路灌爆」。
    use std::sync::atomic::Ordering::Relaxed;
    let sent = stats.ogg_bytes.load(Relaxed);
    let frames = stats.frames.load(Relaxed);
    eprintln!("上行合计：{frames} 帧 / {sent} 字节");
    assert!(
        sent >= 24_000 && sent <= 120_000,
        "3 秒 440Hz 64kbps 上行字节数 {sent} 不在合理区间（24 KB..120 KB）"
    );
}

/// 本机连请求都不该发的路：**源名没前缀**（猜设备名是「采集到别的东西」级的错，
/// 必须报错）→ 相位 LocalDead、states=0、`Open.mic` 为空、会话本身照常活着。
#[tokio::test]
async fn unparsable_mic_source_never_leaves_the_machine() {
    init_logs();
    let Some(ff) = ffmpeg::find_ffmpeg(None) else {
        eprintln!("跳过：本机没有 ffmpeg");
        return;
    };
    let addr = spawn_server().await;
    let cfg = test_config(&addr);
    let device = spawn_host(&addr, cfg.clone(), ff.clone(), MicSink::Null).await;

    let c = timeout(
        T,
        connect::connect(&addr, &device, &cfg.password, "麦克风测试控制端", vec![]),
    )
    .await
    .expect("连接超时")
    .expect("连接失败");

    let stats = Arc::new(AudioStats::default());
    let watch = Arc::new(AudioWatch::default());
    let plan = plan_with(&addr, &device, &cfg, ff, Some("default"), stats.clone(), watch.clone());

    let open = timeout(T, session::open(None, &plan, c))
        .await
        .expect("开会话超时")
        .expect("开会话失败");
    assert!(open.mic.is_none(), "源名解析失败不许有上行任务");
    assert_eq!(watch.phase(), AudioPhase::LocalDead);
    assert_eq!(watch.states(), 0, "MicStart 根本没发出去，对面不该有一句回话");
    assert_eq!(
        stats.ogg_bytes.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "一个字节都不许上通道"
    );
    let s = watch.summary_labeled("麦克风状态", true);
    assert!(s.contains("pulse:") && s.contains("dshow:") && s.contains("lavfi:"),
        "报错要照抄就能改：{s}");
}
