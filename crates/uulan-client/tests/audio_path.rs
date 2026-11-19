//! M7 音频端到端验收：真中继 + 真被控端 + 真控制端，声音从「合成源」走到「WAV 文件」。
//!
//! 覆盖的真实路径：四条通道配对 → 握手/鉴权 → `C2H::AudioStart` → 被控端起 ffmpeg
//! （lavfi `sine` 合成 440 Hz）→ Opus/Ogg 编码 → audio 通道（加密）→ 控制端 ffmpeg 解码
//! → s16le PCM → WAV 落盘。
//!
//! 唯一没被覆盖的是 `-f pulse` 这个输入后端本身（测试机没有 PulseAudio）——
//! 那一段在真机上验，见 `docs/milestones/`。
//!
//! 断言的是**声音本身对不对**：不只是「收到字节了」，而是「解出来的波形里 440 Hz 占主导」。
//! 只数字节数的话，一段白噪声也能通过。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::timeout;

use uulan_client::audioin::{AudioPhase, AudioStats, AudioWatch, Sink};
use uulan_client::connect;
use uulan_client::session::{self, Plan};
use uulan_host::capture::CaptureInput;
use uulan_host::config::HostConfig;
use uulan_host::session::{self as host_session, HostState, SessionParams, VideoSettings};
use uulan_media::ffmpeg::{self, Encoder};
use uulan_protocol::msg::Cap;
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

fn ffmpeg_or_skip() -> Option<PathBuf> {
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

fn test_config(server: &str, audio_source: Option<&str>) -> HostConfig {
    let mut cfg = HostConfig::default();
    cfg.server = server.to_string();
    cfg.name = "测试被控端".into();
    cfg.password = "aB3dE5fG".into();
    cfg.capture = CaptureInput::parse("lavfi:testsrc2=size=320x240:rate=25@60").unwrap();
    cfg.fps = 25;
    cfg.quality = uulan_protocol::msg::Quality::Auto;
    cfg.screen_width = 320;
    cfg.screen_height = 240;
    cfg.audio_source = audio_source.map(|s| s.to_string());
    cfg
}

async fn spawn_host(
    server: &str,
    cfg: HostConfig,
    ff: PathBuf,
) -> (String, tokio::sync::watch::Receiver<u32>) {
    let mut conn = RvConn::connect(server).await.expect("被控端连不上中继");
    let (device_id, _) = conn
        .host_register(None, cfg.name.clone(), None)
        .await
        .expect("注册失败");

    let state = Arc::new(HostState::new(cfg.clone(), None));
    let screen = host_session::resolve_region(&cfg.capture, &cfg).await;
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
                    ffmpeg: ff.clone(),
                    capture: cfg.capture.clone(),
                    screen,
                    encoder: Encoder::X264,
                    settings,
                    clipboard: Some(Box::new(uulan_clip::NoClipboard::new("音频测试"))),
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
                            let _ = host_session::run(params, relays, client_addr).await;
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

/// 找一个真能写文件的目录。
///
/// **这台机器的坑**：可执行文件位于工作区里时，连写 `%TEMP%` 都是「拒绝访问」
/// （见 DESIGN 的构建陷阱 2，与代码无关）。所以这里**真写一个文件**再删掉做探测，
/// 探测不到就跳过 —— 硬断言会让环境一变就是一片假红。
fn writable_dir() -> Option<PathBuf> {
    let mut cands = vec![std::env::temp_dir()];
    // 工作区外的固定目录（仓库脚本也用这个）。有了它，这个测试在本机就能真跑。
    cands.push(PathBuf::from("D:/uulan-bin"));
    cands.push(PathBuf::from("C:/uulan-audio-test"));
    for d in cands {
        if std::fs::create_dir_all(&d).is_err() {
            continue;
        }
        let probe = d.join(format!("uulan-w-{}.tmp", std::process::id()));
        if std::fs::write(&probe, b"x").is_ok() {
            let _ = std::fs::remove_file(&probe);
            return Some(d);
        }
    }
    None
}

/// 解析 WAV（只认我们自己写的那种：44 字节头 + s16le 立体声）。
fn wav_samples(bytes: &[u8]) -> Vec<i16> {
    assert!(bytes.len() > 44, "WAV 太短（{} 字节）", bytes.len());
    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    let channels = u16::from_le_bytes([bytes[22], bytes[23]]);
    let rate = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]);
    assert_eq!(channels, 2, "声道数不对");
    assert_eq!(rate, 48_000, "采样率不对");
    let declared = u32::from_le_bytes([bytes[40], bytes[41], bytes[42], bytes[43]]) as usize;
    assert_eq!(
        declared,
        bytes.len() - 44,
        "WAV 头里的长度没回填（说明收尾没走 finish）"
    );
    bytes[44..]
        .chunks_exact(2)
        .map(|p| i16::from_le_bytes([p[0], p[1]]))
        .collect()
}

/// Goertzel：单个频点的功率。比引一个 FFT 库省事，而这里只需要「440 比 1000 强多少」。
fn goertzel(samples: &[i16], freq: f64, rate: f64) -> f64 {
    let n = samples.len();
    if n == 0 {
        return 0.0;
    }
    let k = (freq / rate) * n as f64;
    let w = 2.0 * std::f64::consts::PI * k / n as f64;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for s in samples {
        let s0 = *s as f64 + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    s1 * s1 + s2 * s2 - coeff * s1 * s2
}

#[tokio::test]
async fn audio_flows_from_host_to_client_wav() {
    let Some(ff) = ffmpeg_or_skip() else { return };
    let addr = spawn_server().await;
    let cfg = test_config(&addr, Some("lavfi:sine=frequency=440:sample_rate=48000"));
    let (device_id, _done) = spawn_host(&addr, cfg.clone(), ff.clone()).await;

    let c = timeout(
        T,
        connect::connect(&addr, &device_id, &cfg.password, "音频测试控制端", vec![],
            None,  // 集成测试都走中继
),
    )
    .await
    .expect("连接超时")
    .expect("连接失败");

    assert!(
        c.host.caps.contains(&Cap::Audio),
        "被控端配了音频源就该声明 Cap::Audio，实际能力表 {:?}",
        c.host.caps
    );

    let Some(dir) = writable_dir() else {
        eprintln!("跳过：这台机器上从工作区里跑出来的 exe 写不了盘（见 DESIGN 的构建陷阱 2）");
        return;
    };
    let out = dir.join(format!("uulan-audio-{}.wav", std::process::id()));
    let stats = Arc::new(AudioStats::default());
    let watch = Arc::new(AudioWatch::default());
    let plan = Plan {
            direct: None,  // 测试都走中继
        server: addr.clone(),
        device: device_id.clone(),
        password: cfg.password.clone(),
        client_name: "音频测试控制端".into(),
        caps: vec![Cap::Audio],
        ffmpeg: Some(ff.clone()),
        clipboard: false,
        no_input: true,
        max_reconnects: 0,
        video: None,
        audio: Some(Sink::Wav(out.clone())),
        audio_stats: stats.clone(),
        audio_watch: watch.clone(),
        mic: None,
        mic_source: None,
        mic_stats: Arc::new(uulan_client::audioin::AudioStats::default()),
        mic_watch: Arc::new(uulan_client::audioin::AudioWatch::default()),
    };

    let open = timeout(T, session::open(None, &plan, c))
        .await
        .expect("开会话超时")
        .expect("开会话失败");
    assert!(open.audio.is_some(), "开了 --audio 就该有接收任务");

    // 让它流 4 秒：合成源是无限正弦，够算出稳定的频谱。
    tokio::time::sleep(Duration::from_secs(4)).await;
    let live = stats.summary();
    eprintln!("中途统计：{live}");
    // 被控端那句 `AudioState{running:true}` 必须真的落到状态机里：
    // 这条是「两端接上了」的判据，光有 WAV 只能证明音频通道通了。
    assert_eq!(
        watch.phase(),
        AudioPhase::Running,
        "音频状态没被控制端接住：{}",
        watch.summary(stats.got_bytes())
    );
    assert!(watch.states() >= 1, "一条 AudioState 都没收到");
    assert_eq!(watch.failures(), 0, "{}", watch.summary(true));

    // 丢掉会话 → 音频任务收尾（读端先断，解码器把剩下的吐完，WAV 头回填）。
    drop(open);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let ok = std::fs::metadata(&out).map(|m| m.len() > 48 * 1024).unwrap_or(false);
        if ok || tokio::time::Instant::now() > deadline {
            break;
        }
    }

    let bytes = std::fs::read(&out).expect("没写出 WAV");
    let samples = wav_samples(&bytes);
    let seconds = samples.len() as f64 / 48_000.0 / 2.0;
    let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
    // 只看后 2 秒：开头那段可能还在等第一条 Ogg 页，能量不稳。
    let tail: Vec<i16> = samples[samples.len().saturating_sub(48_000 * 2 * 2)..].to_vec();
    let p440 = goertzel(&tail, 440.0, 48_000.0);
    let p1000 = goertzel(&tail, 1000.0, 48_000.0);
    eprintln!(
        "WAV {} 字节 / {:.2} 秒 / 峰值 {} / 440Hz 功率 {:.3e} vs 1000Hz {:.3e}（{:.1} 倍）",
        bytes.len(),
        seconds,
        peak,
        p440,
        p1000,
        if p1000 > 0.0 { p440 / p1000 } else { f64::INFINITY }
    );

    assert!(seconds > 2.5, "只有 {seconds:.2} 秒音频，太短（编码/传输没跟上）");
    assert!(peak > 2000, "峰值只有 {peak}，几乎是静音");
    assert!(
        p440 > p1000 * 20.0,
        "440 Hz 没有占主导（{p440:.3e} vs {p1000:.3e}）—— 解出来的不是那个正弦波"
    );
    assert_eq!(stats.errors.load(std::sync::atomic::Ordering::Relaxed), 0, "{}", stats.summary());

    let _ = std::fs::remove_file(&out);
}

/// 被控端「说在推、其实一字节没到」：控制端必须把这句回音记下来并说清是哪一种。
///
/// 这里配的是**能解析、但这台机器上根本起不来**的采集源（`-f pulse` 在开发机上不存在）。
/// 被控端因此会先回 `running:true`（ffmpeg 进程确实拉起来了），紧接着因为读不到输出而回
/// `running:false` + 「音频流结束（0 秒，0 字节）」。控制端把它归成 **Ended**，并且因为
/// 本机一个字节都没收到，统计行上要自己补出那句人话 —— 这一条就是「协议先行、两端未接」
/// 被补上之后应该有的样子。
#[tokio::test]
async fn client_surfaces_a_source_that_never_produced_bytes() {
    let Some(ff) = ffmpeg_or_skip() else { return };
    let addr = spawn_server().await;
    let cfg = test_config(&addr, Some("pulse:alsa_input.uulan-does-not-exist"));
    let (device_id, _done) = spawn_host(&addr, cfg.clone(), ff.clone()).await;

    let c = timeout(
        T,
        connect::connect(&addr, &device_id, &cfg.password, "状态测试控制端", vec![],
            None,  // 集成测试都走中继
),
    )
    .await
    .expect("连接超时")
    .expect("连接失败");
    assert!(
        c.host.caps.contains(&Cap::Audio),
        "配了能解析的音频源就该声明 Cap::Audio，实际能力表 {:?}",
        c.host.caps
    );

    let Some(dir) = writable_dir() else {
        eprintln!("跳过：这台机器上从工作区里跑出来的 exe 写不了盘（见 DESIGN 的构建陷阱 2）");
        return;
    };
    let out = dir.join(format!("uulan-audio-bad-{}.wav", std::process::id()));
    let stats = Arc::new(AudioStats::default());
    let watch = Arc::new(AudioWatch::default());
    let plan = Plan {
            direct: None,  // 测试都走中继
        server: addr.clone(),
        device: device_id.clone(),
        password: cfg.password.clone(),
        client_name: "状态测试控制端".into(),
        caps: vec![Cap::Audio],
        ffmpeg: Some(ff.clone()),
        clipboard: false,
        no_input: true,
        max_reconnects: 0,
        video: None,
        audio: Some(Sink::Wav(out.clone())),
        audio_stats: stats.clone(),
        audio_watch: watch.clone(),
        mic: None,
        mic_source: None,
        mic_stats: Arc::new(uulan_client::audioin::AudioStats::default()),
        mic_watch: Arc::new(uulan_client::audioin::AudioWatch::default()),
    };

    let open = timeout(T, session::open(None, &plan, c))
        .await
        .expect("开会话超时")
        .expect("开会话失败");

    // 等到被控端至少回过一句话（它两条都会来，等 10 秒足够）。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if watch.states() >= 2 || tokio::time::Instant::now() > deadline {
            break;
        }
    }
    let s = watch.summary(stats.got_bytes());
    eprintln!("坏源状态行：{s}");
    assert!(watch.states() >= 1, "被控端一句 AudioState 都没回（控制端没接？）：{s}");
    assert!(
        !matches!(watch.phase(), AudioPhase::Idle | AudioPhase::Requested),
        "相位还停在「没请求 / 等回话」，说明状态没被接住：{s}"
    );
    if watch.phase() == AudioPhase::Ended {
        // 一字节都没到 —— 这句必须自己说出来，不能让人以为「播完了」。
        assert!(s.contains("一个字节都没收到"), "{s}");
        assert_eq!(stats.ogg_bytes.load(std::sync::atomic::Ordering::Relaxed), 0);
    }
    drop(open);
    let _ = std::fs::remove_file(&out);
}

/// 本机解码器起不来 → 控制端发 `C2H::AudioStop`，被控端停编码器并回一句确认。
///
/// 这条同时钉住三件事：
/// 1. `AudioStop` 不再是一条「协议里有、没人发」的死消息；
/// 2. 被控端那句收尾的 `running:false` 落在 **Stopped**，不计入失败（是我们自己叫的停）；
/// 3. 被控端确实停了 —— 那句确认只有 `AudioCtl::stop()` 会发，收到它就等于对方停了。
#[tokio::test]
async fn client_stops_the_host_when_it_cannot_decode() {
    let Some(ff) = ffmpeg_or_skip() else { return };
    let addr = spawn_server().await;
    // 被控端这一侧用能跑的合成源：要的就是「对面起来了、本机接不住」这个组合。
    let cfg = test_config(&addr, Some("lavfi:sine=frequency=440:sample_rate=48000"));
    let (device_id, _done) = spawn_host(&addr, cfg.clone(), ff.clone()).await;

    let c = timeout(
        T,
        connect::connect(&addr, &device_id, &cfg.password, "叫停测试控制端", vec![],
            None,  // 集成测试都走中继
),
    )
    .await
    .expect("连接超时")
    .expect("连接失败");

    let Some(dir) = writable_dir() else {
        eprintln!("跳过：这台机器上从工作区里跑出来的 exe 写不了盘（见 DESIGN 的构建陷阱 2）");
        return;
    };
    let out = dir.join(format!("uulan-audio-stop-{}.wav", std::process::id()));
    let stats = Arc::new(AudioStats::default());
    let watch = Arc::new(AudioWatch::default());
    let plan = Plan {
            direct: None,  // 测试都走中继
        server: addr.clone(),
        device: device_id.clone(),
        password: cfg.password.clone(),
        client_name: "叫停测试控制端".into(),
        caps: vec![Cap::Audio],
        // 一个必然起不来的「ffmpeg」：play_loop 里 std::process::Command 直接报 NotFound。
        ffmpeg: Some(PathBuf::from("C:/uulan-no-such-dir/ffmpeg.exe")),
        clipboard: false,
        no_input: true,
        max_reconnects: 0,
        video: None,
        audio: Some(Sink::Wav(out.clone())),
        audio_stats: stats.clone(),
        audio_watch: watch.clone(),
        mic: None,
        mic_source: None,
        mic_stats: Arc::new(uulan_client::audioin::AudioStats::default()),
        mic_watch: Arc::new(uulan_client::audioin::AudioWatch::default()),
    };

    let open = timeout(T, session::open(None, &plan, c))
        .await
        .expect("开会话超时")
        .expect("开会话失败");

    // 看门狗 500ms 一拍：等它发 AudioStop、等被控端回确认。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if watch.stop_acks() >= 1 {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            break;
        }
    }
    let s = watch.summary(stats.got_bytes());
    eprintln!("叫停状态行：{s}");
    assert_eq!(watch.phase(), AudioPhase::Stopped, "没走到「本机叫停」：{s}");
    // `stop_acks` 才是「被控端确实处理了这条 AudioStop」的证据：`phase==Stopped`
    // 是本机发之前就自己记下的意图，分不清「对面停了」与「对面没理我们」。
    // 本轮实测：本机的失败在 AudioStart 之后 2ms 就判出来，被控端因此连 ffmpeg
    // 都没来得及起（起完立刻被 abort），只回 `AudioCtl::stop()` 那一句确认。
    assert!(watch.stop_acks() >= 1, "被控端没理这条 AudioStop：{s}");
    assert!(s.contains("被控端已确认停止"), "{s}");
    assert_eq!(watch.failures(), 0, "自己叫的停不能记成被控端故障：{s}");
    assert!(s.contains("本机已请求停止"), "{s}");
    // 真链路上的顺序也要钉住：本机那句原因必须在被控端的确认之后**还在**
    // （单测里同一条规则被自己的断言抓出来过一次）。
    assert!(s.contains("本机音频解码器起不来"), "本机原因被对面那句确认盖掉了：{s}");
    drop(open);
    let _ = std::fs::remove_file(&out);
}

/// 被控端没声明 `Cap::Audio` 时，原因要落到状态里（收工统计那一行才说得清为什么没声音）。
#[tokio::test]
async fn missing_audio_capability_is_recorded_not_only_logged() {
    let Some(ff) = ffmpeg_or_skip() else { return };
    let addr = spawn_server().await;
    // 不配音频源：这台开发机没有 PulseAudio，`available(None)` 为假 → 不声明 Cap::Audio。
    let cfg = test_config(&addr, None);
    let (device_id, _done) = spawn_host(&addr, cfg.clone(), ff.clone()).await;

    let c = timeout(
        T,
        connect::connect(&addr, &device_id, &cfg.password, "无能力测试端", vec![],
            None,  // 集成测试都走中继
),
    )
    .await
    .expect("连接超时")
    .expect("连接失败");
    // 这里要验的是「没声明能力时原因要落到状态里」，不是「这台机器一定没声卡」。
    // 换台有声卡的机器跑这套测试（比如真麒麟），「无能力」这条前提就不成立 ——
    // 必须**先跳过再断言**：2026-11-16 真机首跑就是把硬断言排在跳过分支前面，
    // 在有 PulseAudio 的麒麟上必红（SKIP 写在断言之后等于没写）。
    if c.host.caps.contains(&Cap::Audio) {
        eprintln!("跳过：这台机器的被控端声明了 Cap::Audio，测不到「无能力」这条分支");
        return;
    }
    assert!(
        !c.host.caps.contains(&Cap::Audio),
        "走到这里 caps 不该含 Cap::Audio，实际 {:?}",
        c.host.caps
    );

    let Some(dir) = writable_dir() else {
        eprintln!("跳过：这台机器上从工作区里跑出来的 exe 写不了盘（见 DESIGN 的构建陷阱 2）");
        return;
    };
    let out = dir.join(format!("uulan-audio-nocap-{}.wav", std::process::id()));
    let stats = Arc::new(AudioStats::default());
    let watch = Arc::new(AudioWatch::default());
    let plan = Plan {
            direct: None,  // 测试都走中继
        server: addr.clone(),
        device: device_id.clone(),
        password: cfg.password.clone(),
        client_name: "无能力测试端".into(),
        caps: vec![Cap::Audio],
        ffmpeg: Some(ff),
        clipboard: false,
        no_input: true,
        max_reconnects: 0,
        video: None,
        audio: Some(Sink::Wav(out.clone())),
        audio_stats: stats.clone(),
        audio_watch: watch.clone(),
        mic: None,
        mic_source: None,
        mic_stats: Arc::new(uulan_client::audioin::AudioStats::default()),
        mic_watch: Arc::new(uulan_client::audioin::AudioWatch::default()),
    };

    let open = timeout(T, session::open(None, &plan, c))
        .await
        .expect("开会话超时")
        .expect("开会话失败");
    let s = watch.summary(stats.got_bytes());
    eprintln!("无能力状态行：{s}");
    assert_eq!(watch.phase(), AudioPhase::Unsupported, "{s}");
    assert_eq!(watch.states(), 0, "没请求过就不该收到任何状态");
    assert_eq!(watch.failures(), 0, "对面没能力不是故障：{s}");
    assert!(s.contains("没有音频能力"), "{s}");
    drop(open);
    let _ = std::fs::remove_file(&out);
}
