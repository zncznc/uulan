//! 音频采集 + 编码（M7）：PulseAudio 的 monitor 源 → Opus（Ogg 封装）→ audio 通道。
//!
//! ## 为什么是「抓 monitor」而不是「抓麦克风」
//!
//! 远控要听到的是**被控端正在放的声音**（视频里的对白、系统提示音、会议里的别人）。
//! 麦克风是另一件事（而且涉及隐私，得单独开关）。PulseAudio 给每个输出设备配一个
//! `.monitor` 源 —— 它就是「这块声卡正在播的东西」，采集它等于在本机内录。
//!
//! ## 为什么还是走 ffmpeg 子进程
//!
//! 与采集/编码同一条理由：内网离线打包，不许再引一堆 C 库（libopus 的绑定要
//! pkg-config + 头文件，麒麟源里装不上 dev 包这个坑已经在 X11 上踩过一次）。
//! ffmpeg 本来就在依赖里，`-f pulse -i … -c:a libopus -f ogg` 一行就够。
//!
//! ## 边界
//!
//! 一次会话一条音频流、单向（被控端 → 控制端）。控制端不发音频 —— 说话是另一项功能。

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, info, warn};

use uulan_protocol::crypto::SecWriteHalf;
use uulan_protocol::frame::{write_frame, FrameKind};
use uulan_protocol::msg::H2C;

/// 默认码率。64 kbps 的 Opus 对「人声 + 一般音乐」都够用，一条内网链路完全吃得下。
pub const DEFAULT_BITRATE_KBPS: u32 = 64;
pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;
pub const DEFAULT_CHANNELS: u16 = 2;

/// 一次读多少字节就发一帧。
///
/// Ogg 是字节流，帧边界只是传输粒度，客户端那边拼起来喂解码器即可。
/// 16 KiB 在 48 kHz 立体声 s16（192 kB/s）下约等于 85 ms ——
/// 取太小会让每帧的加密/帧头开销占比上升，取太大则首包延迟变长。
const CHUNK: usize = 16 * 1024;

/// 采集源。生产是 PulseAudio 的 monitor；测试/演示可以用 lavfi 合成音。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioSource {
    /// `pulse:<设备名>`，例如 `alsa_output.usb-….analog-stereo.monitor`。
    Pulse { device: String },
    /// `lavfi:<表达式>`，例如 `sine=frequency=440:sample_rate=48000`。
    /// 本机（Windows）没有 PulseAudio，靠它把整条链路测通。
    Lavfi { spec: String },
}

impl AudioSource {
    /// 解析命令行 / 配置文件里的写法。不带前缀的一律当 PulseAudio 设备名。
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.is_empty() {
            return Err("音频源不能为空".into());
        }
        if let Some(rest) = s.strip_prefix("lavfi:") {
            if rest.trim().is_empty() {
                return Err("lavfi 源后面要给出表达式，例如 lavfi:sine=frequency=440".into());
            }
            return Ok(AudioSource::Lavfi { spec: rest.to_string() });
        }
        if let Some(rest) = s.strip_prefix("pulse:") {
            if rest.trim().is_empty() {
                return Err("pulse 源后面要给出设备名".into());
            }
            return Ok(AudioSource::Pulse { device: rest.to_string() });
        }
        if s.starts_with("alsa:") || s.starts_with("default") || s.contains(':') && !s.contains('=') {
            // 别的 backend 第一版不支持 —— 与其猜，不如明确报错（同 portmap 的处理）。
            return Err(format!(
                "不支持的音频源 {s:?}（可用 pulse:<设备名> 或 lavfi:<表达式>）"
            ));
        }
        Ok(AudioSource::Pulse { device: s.to_string() })
    }

    /// 给 ffmpeg 的输入段参数。
    pub fn ffmpeg_input(&self) -> Vec<String> {
        match self {
            AudioSource::Pulse { device } => vec![
                "-f".into(),
                "pulse".into(),
                "-i".into(),
                device.clone(),
            ],
            AudioSource::Lavfi { spec } => {
                // `-re` 让合成源按真实时间出样本（默认它会**拼尽全力**地生成：
                // 4 秒真时间能吐出十分钟音频，链路被灌满、CPU 也白烧）。
                // 只有合成源需要它；`-f pulse` 本来就由设备按实时节奏喂。
                vec!["-re".into(), "-f".into(), "lavfi".into(), "-i".into(), spec.clone()]
            }
        }
    }

    /// 日志里能印的那一行（不含任何凭据 —— 音频源名字本来就不是秘密）。
    pub fn describe(&self) -> String {
        match self {
            AudioSource::Pulse { device } => format!("pulse:{device}"),
            AudioSource::Lavfi { spec } => format!("lavfi:{spec}"),
        }
    }
}

/// 探测「这台机器上该抓哪个源」。
///
/// 只在没显式配置时调用。失败要给出**人能照着修**的原因：现场最常见的是
/// 「这台机器根本没有 PulseAudio」（headless 服务器、纯 ALSA 的最小安装），
/// 或者是声卡只有输出、没有 util（`pactl` 不在 PATH）。
pub fn autodetect() -> Result<AudioSource, String> {
    if let Some(s) = std::env::var_os("UULAN_AUDIO_SOURCE") {
        let s = s.to_string_lossy().to_string();
        if !s.trim().is_empty() {
            return AudioSource::parse(&s);
        }
    }
    // `pactl get-default-sink` 直接给出默认输出设备名，比解析 `list short sinks` 稳。
    let default_sink = run_quiet("pactl", &["get-default-sink"]).ok();
    if let Some(sink) = default_sink {
        let sink = sink.trim().to_string();
        if !sink.is_empty() {
            let monitor = format!("{sink}.monitor");
            if source_exists(&monitor) {
                return Ok(AudioSource::Pulse { device: monitor });
            }
        }
    }
    // 有些环境默认 sink 拿不到（或 monitor 名字不带后缀），退回「第一个 .monitor 源」。
    if let Ok(list) = run_quiet("pactl", &["list", "short", "sources"]) {
        for line in list.lines() {
            let mut it = line.split_whitespace();
            let _idx = it.next();
            if let Some(name) = it.next() {
                if name.ends_with(".monitor") {
                    return Ok(AudioSource::Pulse { device: name.to_string() });
                }
            }
        }
    }
    Err("这台机器上没有可用的 PulseAudio monitor 源（pactl 不可用或没有输出设备）".into())
}

/// 这台机器能不能送音频（用于 `Cap::Audio` 的声明）。
///
/// 与 [`autodetect`] 的区别：这里只回答「有没有这条路」，**不偷偷拉起编码器**，
/// 也不碰声卡。显式配了源就只解析配置（写错了就当不可用 —— 会话里会带原因说明）。
pub fn available(source: Option<&str>) -> bool {
    match source {
        Some(s) => AudioSource::parse(s).is_ok(),
        None => run_quiet("pactl", &["info"]).is_ok(),
    }
}

fn source_exists(name: &str) -> bool {
    run_quiet("pactl", &["list", "short", "sources"])
        .map(|list| list.lines().any(|l| l.split_whitespace().nth(1) == Some(name)))
        .unwrap_or(false)
}

/// 跑一条命令并取 stdout。失败（找不到命令 / 非 0 退出）返回 Err。
///
/// 用 `std::process::Command` 而不是 tokio：调用点是 `spawn_blocking`（见 [`run`]），
/// 而 `pactl` 是毫秒级的本地 IPC，没必要为它把 tokio 的进程 API 拉进来。
fn run_quiet(cmd: &str, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{cmd} 启动失败：{e}"))?;
    if !out.status.success() {
        return Err(format!("{cmd} 退出码 {:?}", out.status.code()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// 控制端要的参数。0 表示「用默认」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioParams {
    pub bitrate_kbps: u32,
    pub sample_rate: u32,
    pub channels: u16,
}

impl Default for AudioParams {
    fn default() -> Self {
        AudioParams {
            bitrate_kbps: DEFAULT_BITRATE_KBPS,
            sample_rate: DEFAULT_SAMPLE_RATE,
            channels: DEFAULT_CHANNELS,
        }
    }
}

impl AudioParams {
    /// 把协议里那组「0 = 默认」的字段归一化。
    ///
    /// 上限也在这里夹住：控制端要是发个 `channels=32`，ffmpeg 会当场退出，
    /// 而现象只是「没声音」—— 不如我们自己收敛到一个它一定认的范围。
    pub fn normalized(bitrate_kbps: u32, sample_rate: u32, channels: u16) -> Self {
        AudioParams {
            bitrate_kbps: if bitrate_kbps == 0 { DEFAULT_BITRATE_KBPS } else { bitrate_kbps.clamp(16, 256) },
            sample_rate: if sample_rate == 0 { DEFAULT_SAMPLE_RATE } else { sample_rate.clamp(8_000, 48_000) },
            channels: if channels == 0 { DEFAULT_CHANNELS } else { channels.clamp(1, 2) },
        }
    }
}

/// ffmpeg 的完整参数。
///
/// `-page_duration 20000` 是关键：Ogg 默认一页 1 秒，那意味着「声音晚一秒才出来」。
/// 20 ms 一页 + `-flush_packets 1` 才能让它边编边吐，而不是攒够一页再发。
pub fn encode_args(src: &AudioSource, p: &AudioParams) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
    ];
    a.extend(src.ffmpeg_input());
    a.extend([
        "-vn".into(),
        "-c:a".into(),
        "libopus".into(),
        "-b:a".into(),
        format!("{}k", p.bitrate_kbps),
        "-ar".into(),
        p.sample_rate.to_string(),
        "-ac".into(),
        p.channels.to_string(),
        // 语音/音乐混着来，lowdelay 让编码器别攒帧。
        "-application".into(),
        "lowdelay".into(),
        "-frame_duration".into(),
        "20".into(),
        "-f".into(),
        "ogg".into(),
        "-page_duration".into(),
        "20000".into(),
        "-flush_packets".into(),
        "1".into(),
        "pipe:1".into(),
    ]);
    a
}

/// 一次音频传输的收尾统计。
#[derive(Debug, Default, Clone, Copy)]
pub struct AudioStats {
    /// 从 ffmpeg 读出的字节数（Ogg 流，含封装开销）。
    pub bytes: u64,
    /// 写出去的帧数。
    pub frames: u64,
}

/// 跑一条音频流，直到对端断开 / 任务被 abort。
///
/// `notify` 用于把 `H2C::AudioState` 交给会话的控制通道 —— 音频起不来时必须
/// **说清原因**，否则控制端那边只有「没声音」这一条信息。
pub async fn run(
    ffmpeg: PathBuf,
    source: Option<String>,
    params: AudioParams,
    audio: SecWriteHalf,
    notify: Box<dyn Fn(H2C) + Send + Sync>,
) {
    // 探测要跑子进程（pactl），别在异步 worker 上阻塞。
    let src = match source {
        Some(s) => match AudioSource::parse(&s) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, source = %s, "配置里的音频源不可用");
                notify(H2C::AudioState { running: false, codec: String::new(), detail: e });
                return;
            }
        },
        None => match tokio::task::spawn_blocking(autodetect).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                warn!(error = %e, "音频源探测失败");
                notify(H2C::AudioState { running: false, codec: String::new(), detail: e });
                return;
            }
            Err(e) => {
                warn!(error = %e, "音频源探测任务崩溃");
                notify(H2C::AudioState {
                    running: false,
                    codec: String::new(),
                    detail: format!("音频源探测失败：{e}"),
                });
                return;
            }
        },
    };

    let args = encode_args(&src, &params);
    debug!(?args, "启动音频编码器");
    let mut child = match tokio::process::Command::new(&ffmpeg)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // 任务被 abort 时把 ffmpeg 一起收掉，别留下一个还在抓声卡的进程。
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "音频编码器启动失败");
            notify(H2C::AudioState {
                running: false,
                codec: String::new(),
                detail: format!("ffmpeg 启动失败：{e}"),
            });
            return;
        }
    };
    let err = child.stderr.take();
    if let Some(err) = err {
        uulan_media::ffmpeg::drain_stderr(err, "audio");
    }
    let Some(mut out) = child.stdout.take() else {
        notify(H2C::AudioState {
            running: false,
            codec: String::new(),
            detail: "拿不到 ffmpeg 的 stdout".into(),
        });
        return;
    };

    info!(source = %src.describe(), bitrate = params.bitrate_kbps, rate = params.sample_rate,
          channels = params.channels, "音频已开始（Opus/Ogg）");
    notify(H2C::AudioState {
        running: true,
        codec: format!("opus/ogg {}k {}Hz {}ch", params.bitrate_kbps, params.sample_rate, params.channels),
        detail: src.describe(),
    });

    let started = Instant::now();
    let mut stats = AudioStats::default();
    let mut buf = vec![0u8; CHUNK];
    let mut audio = audio;
    loop {
        let n = match out.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                warn!(error = %e, "读音频编码器输出失败");
                break;
            }
        };
        if write_frame(&mut audio, FrameKind::Blob, &buf[..n]).await.is_err() {
            debug!("音频通道写入失败，退出（对端可能已经走了）");
            break;
        }
        stats.bytes += n as u64;
        stats.frames += 1;
        if audio.flush().await.is_err() {
            break;
        }
    }

    // 提前退出（编码器崩了 / 采集源没了）也要告诉控制端，别让它以为还在收。
    notify(H2C::AudioState {
        running: false,
        codec: String::new(),
        detail: format!(
            "音频流结束（{} 秒，{} 字节）",
            started.elapsed().as_secs_f32().round() as u64,
            stats.bytes
        ),
    });
    info!(
        bytes = stats.bytes,
        frames = stats.frames,
        seconds = started.elapsed().as_secs_f64().round() as u64,
        "音频收尾"
    );
}

/// 给自检用（`--diagnose`）：只回答「能不能抓」，不拉进程、不碰声卡。
pub fn probe_source(source: Option<&str>) -> Result<String, String> {
    match source {
        Some(s) => AudioSource::parse(s).map(|v| v.describe()),
        None => autodetect().map(|v| v.describe()),
    }
}

/// 给测试与自检用：本机能不能起音频（源能解析出来 + ffmpeg 在）。
pub fn probe(ffmpeg: &std::path::Path, source: Option<&str>) -> Result<String, String> {
    let src = match source {
        Some(s) => AudioSource::parse(s)?,
        None => autodetect()?,
    };
    if !ffmpeg.exists() {
        return Err(format!("找不到 ffmpeg：{}", ffmpeg.display()));
    }
    Ok(src.describe())
}

// 注意：这里**不需要**静默看门狗。Ogg 在完全静音时也会每 20 ms 出一页
// （Opus 的 comfort noise），所以「读不到字节」本身就等价于编码器没了。
// 视频那边要看门狗是因为 h264 在画面不变时可以长时间不出 AU —— 两者不一样。
