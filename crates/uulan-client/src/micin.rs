//! 麦克风转接的**发送侧**（M7 的最后一格）：控制端自己的声音 → Opus/Ogg → audio 通道上行。
//!
//! ## 这条路是什么
//!
//! 「UU 远程」级远控的现场动作：控制端操作员对着自己机器的麦克风说话，声音从
//! **被控端**的音箱放出来（隔空对会议室说话 / 提醒对面座位上的人）。方向与
//! `audioin`（被控端 → 控制端）相反，复用同一条 `audio` 通道 —— 两个方向的
//! 密钥与 nonce 计数器本来就是分开派生的（见 `uulan-protocol::crypto`），
//! 一条连接、两个方向，互不干扰。
//!
//! ## 「停」没有消息
//!
//! 停止 = 采集任务结束 → 上行写半端被 drop → TCP 写半边关闭 → 被控端 `read_frame`
//! 拿到 EOF 自行收尾。**没有 `C2H::MicStop`**：它唯一的构造点会是「会话中途用户
//! 关麦克风」，这个 UI 还不存在 —— 协议里躺一条没人发的消息，正是历轮点名一直在清的东西。
//!
//! ## 采集源为什么默认各平台不同
//!
//! - Windows：DirectShow（`-f dshow`，这台麒麟之外的主控制端平台）。默认设备要枚举：
//!   `ffmpeg -list_devices true -f dshow -i dummy` 的 stderr 里取第一个音频设备名
//!   （解析在 [`parse_dshow_audio`]，纯函数，喂真机抓回来的样例文本测）。
//! - unix（麒麟控制端）：`pulse:default` —— PulseAudio 的**默认输入源**就是麦克风
//!   （与被控端下行采集抓 `.monitor` 是同一后端、不同源，不冲突）。
//! - `--mic-source lavfi:…`：合成源。验收台用它把「采集→编码→上线→对面播放」
//!   整条链钉成 440 Hz 正弦的声学判据（`-re` 的纪律同 [`uulan_media`] 那头：
//!   合成源不加 `-re` 会拼尽全力灌爆链路）。
//!
//! ## 编码参数钉死在共享层的常量上
//!
//! 上行与下行是**同一个解码器**（`uulan_media::playback`，被控端拿它播本端送的声音），
//! 它只认 48 kHz / 立体声 / s16le。所以本端编码器 `-ar/-ac` 必须钉成这两个数 ——
//! 对面送 mono 上来，aplay 按 48k/2ch 播 mono 字节流的表现为**变速+串声道**，
//! 与「缩放尺寸差 1 像素就永远花屏」同族。单测钉死（[`encode_args_pins_the_shared_format`]）。

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tracing::{debug, info, warn};

use uulan_media::playback::{Counters, CHANNELS, SAMPLE_RATE};
use uulan_protocol::crypto::SecWriteHalf;
use uulan_protocol::frame::{write_frame, FrameKind};

/// 与下行同一口径：16 KiB ≈ 85 ms 的 PCM（Opus 出流更慢，按编码后的字节数算更长）。
const CHUNK: usize = 16 * 1024;

/// 上行码率的默认与上下限 —— 与 `uulan-host` 的 `AudioParams::normalized` 同一组数，
/// 改一边必须改另一边（现场「两边码率口径不一样」没人能查）。
pub const DEFAULT_BITRATE_KBPS: u32 = 64;

/// 「0 = 默认」的归一化 + 夹住越界值。纯函数，单测钉上下限的每个边界。
pub fn normalize_bitrate(kbps: u32) -> u32 {
    if kbps == 0 {
        DEFAULT_BITRATE_KBPS
    } else {
        kbps.clamp(16, 256)
    }
}

/// 采集源。默认按平台挑（见模块头），`--mic-source` 可以显式钉死。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MicSource {
    /// `pulse:<设备名>`。`pulse:default` = 这台机器的默认输入源（麦克风）。
    Pulse { device: String },
    /// `dshow:<设备名>`（Windows DirectShow）。名字必须**原样**给（带括号空格都行，
    /// 整串当设备名）—— 枚举在 [`parse_dshow_audio`]。
    Dshow { name: String },
    /// `lavfi:<表达式>`，例如 `sine=frequency=440:sample_rate=48000`。验收台/演示用。
    Lavfi { spec: String },
}

impl MicSource {
    /// 解析 `--mic-source` 的写法。不带前缀的报错（麦克风来源没法靠猜：
    /// Windows 与 PulseAudio 的设备名长得太像了，猜错的表现是「采集到了别的东西」）。
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.is_empty() {
            return Err("麦克风源不能为空".into());
        }
        if let Some(rest) = s.strip_prefix("lavfi:") {
            if rest.trim().is_empty() {
                return Err("lavfi 源后面要给出表达式，例如 lavfi:sine=frequency=440".into());
            }
            return Ok(MicSource::Lavfi { spec: rest.to_string() });
        }
        if let Some(rest) = s.strip_prefix("pulse:") {
            if rest.trim().is_empty() {
                return Err("pulse 源后面要给出设备名（default = 系统默认输入源）".into());
            }
            return Ok(MicSource::Pulse { device: rest.to_string() });
        }
        if let Some(rest) = s.strip_prefix("dshow:") {
            if rest.trim().is_empty() {
                return Err("dshow 源后面要给出设备名（先用 ffmpeg -list_devices true -f dshow -i dummy 看有什么）".into());
            }
            return Ok(MicSource::Dshow { name: rest.to_string() });
        }
        Err(format!(
            "不认识前缀的麦克风源 {s:?}（可用 pulse:<设备> / dshow:<设备> / lavfi:<表达式>）"
        ))
    }

    /// 给 ffmpeg 的输入段参数。
    pub fn ffmpeg_input(&self) -> Vec<String> {
        match self {
            MicSource::Pulse { device } => {
                vec!["-f".into(), "pulse".into(), "-i".into(), device.clone()]
            }
            MicSource::Dshow { name } => vec![
                "-f".into(),
                "dshow".into(),
                "-i".into(),
                format!("audio={name}"),
            ],
            MicSource::Lavfi { spec } => {
                // `-re`：合成源按真实时间出样本（不加的话 4 秒真时间能吐出十分钟
                // 音频，链路被灌满、CPU 白烧 —— 与被控端下行同一个坑，别再踩）。
                vec!["-re".into(), "-f".into(), "lavfi".into(), "-i".into(), spec.clone()]
            }
        }
    }

    /// 日志里能印的那一行（设备名不是秘密）。
    pub fn describe(&self) -> String {
        match self {
            MicSource::Pulse { device } => format!("pulse:{device}"),
            MicSource::Dshow { name } => format!("dshow:{name}"),
            MicSource::Lavfi { spec } => format!("lavfi:{spec}"),
        }
    }
}

/// 从 `ffmpeg -list_devices true -f dshow -i dummy` 的 **stderr** 里取第一个音频设备名。
///
/// 纯函数：输出文本是 2026-11-18 从本机 ffmpeg 9.0 抓的真实格式（见单测样例），
/// 解析规则只认「DirectShow audio devices」小节里、`Alternative name` 之外的第一条
/// 双引号名。取不到返回 `None`（这台机器没有输入设备，或格式变了 ——  callers
/// 要把这两种如实分开，不能都报「找不到麦克风」）。
pub fn parse_dshow_audio(stderr: &str) -> Option<String> {
    let mut in_audio = false;
    for line in stderr.lines() {
        if line.contains("DirectShow audio devices") {
            in_audio = true;
            continue;
        }
        if line.contains("DirectShow video devices") {
            // 视频小节在音频小节**之后**出现时也到此为止（ffmpeg 两种顺序都见过）。
            in_audio = false;
            continue;
        }
        if !in_audio {
            continue;
        }
        if line.contains("Alternative name") {
            continue;
        }
        // 设备行的形状：`[dshow @ 0xADDR]  "Microphone (Realtek(R) Audio)"`
        if let Some(rest) = line.split('"').nth(1) {
            let name = rest.trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// 本平台的默认采集源（不碰进程的部分）。
///
/// unix 是 `pulse:default` 一句话；Windows 必须枚举 —— 那要起 ffmpeg 子进程，
/// 所以它是 [`detect_windows_default`]，这里只如实报告「Windows 没有免枚举的默认值」。
pub fn platform_default() -> Option<MicSource> {
    #[cfg(windows)]
    {
        None
    }
    #[cfg(not(windows))]
    {
        Some(MicSource::Pulse { device: "default".into() })
    }
}

/// Windows：问 ffmpeg「这台机器第一个录音设备叫什么」。
/// 失败原因要分两种（ffmpeg 起不来 / 没有设备），现场排查方向完全不同。
pub async fn detect_windows_default(ffmpeg: &PathBuf) -> Result<MicSource, String> {
    let out = tokio::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-list_devices", "true", "-f", "dshow", "-i", "dummy"])
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("ffmpeg 设备枚举起不来：{e}"))?;
    // 这条命令**必定**以非零退出（dummy 输入本来就打不开）—— 判据只有 stderr 的设备表，
    // 拿退出码判成败会把所有枚举都判成失败。
    let text = String::from_utf8_lossy(&out.stderr).to_string();
    parse_dshow_audio(&text)
        .map(|name| MicSource::Dshow { name })
        .ok_or_else(|| {
            let listed = text.contains("DirectShow audio devices");
            if listed {
                "DirectShow 列出了音频小节但一个设备名都没有（这台机器没有可用的录音输入）".into()
            } else {
                "ffmpeg 没列出 DirectShow 音频设备（没有输入设备 / dshow 后端不可用）".into()
            }
        })
}

/// ffmpeg 的完整上行参数：采集源 → Opus/Ogg（48k 立体声钉死，见模块头）。
pub fn encode_args(src: &MicSource, bitrate_kbps: u32) -> Vec<String> {
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
        format!("{}k", normalize_bitrate(bitrate_kbps)),
        "-ar".into(),
        SAMPLE_RATE.to_string(),
        "-ac".into(),
        CHANNELS.to_string(),
        "-application".into(),
        "lowdelay".into(),
        "-frame_duration".into(),
        "20".into(),
        "-f".into(),
        "ogg".into(),
        // Ogg 默认一页 1 秒 —— 声音会晚一秒才到对面（下行同款实测教训）。
        "-page_duration".into(),
        "20000".into(),
        "-flush_packets".into(),
        "1".into(),
        "pipe:1".into(),
    ]);
    a
}

/// 定下这条会话用哪个采集源：显式 `--mic-source` 优先；否则按平台。
///
/// Windows 的「平台默认」需要起一次 ffmpeg 枚举（DirectShow 没有免进程的默认设备
/// 概念），所以整个函数是 async 的；unix 分支不碰进程，立刻返回。
/// 失败原因分「解析错 / 枚举起不来 / 这台机器根本没有输入设备」三类如实带回 ——
/// 现场照着那句话修，不用翻代码。
pub async fn resolve_source(explicit: Option<&str>, ffmpeg: &PathBuf) -> Result<MicSource, String> {
    if let Some(s) = explicit {
        return MicSource::parse(s);
    }
    if let Some(d) = platform_default() {
        return Ok(d);
    }
    detect_windows_default(ffmpeg).await
}

/// 一条上行麦克风任务。`Drop` = abort 采集 + 关写半端（= 对被控端的「停」）。
pub struct MicOut {
    stats: Arc<Counters>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl MicOut {
    pub fn stats(&self) -> Arc<Counters> {
        self.stats.clone()
    }
}

impl Drop for MicOut {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}

/// 起采集并把 Ogg 字节写进 audio 通道的**写半端**。
///
/// 返回 `Err` = 本机采集根本没起来（ffmpeg 不在 / 设备被占 / 源名不存在）——
/// 调用方据此**不要**把 `MicStart` 发给被控端（别让人家白起播放管道），
/// 并把原因记进状态机（`AudioPhase::LocalDead`）。
pub async fn start(
    ffmpeg: Option<PathBuf>,
    src: MicSource,
    bitrate_kbps: u32,
    mut wr: SecWriteHalf,
    stats: Arc<Counters>,
) -> Result<MicOut, String> {
    let ffmpeg = ffmpeg.unwrap_or_else(|| PathBuf::from("ffmpeg"));
    let args = encode_args(&src, bitrate_kbps);
    debug!(?args, "启动麦克风采集");
    let mut child = match tokio::process::Command::new(&ffmpeg)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // 任务被 abort（会话结束 / 控制端退出）时把 ffmpeg 一起收掉，
        // 别留下一个还占着麦克风的进程 —— 对面的录音指示灯会一直亮着。
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Err(format!("ffmpeg 启动失败：{e}")),
    };
    if let Some(err) = child.stderr.take() {
        uulan_media::ffmpeg::drain_stderr(err, "mic");
    }
    let Some(mut out) = child.stdout.take() else {
        return Err("拿不到 ffmpeg 的 stdout".into());
    };

    info!(source = %src.describe(), bitrate = normalize_bitrate(bitrate_kbps),
          "麦克风转接已开始（Opus/Ogg 上行）");

    let st = stats.clone();
    // **child 必须跟着任务活**：`start()` 一返回，留在本作用域的 Child 就被 drop，
    // 而 `kill_on_drop(true)` 当场杀掉 ffmpeg —— 现象是「上行 2 毫秒 EOF、0 字节」。
    // 2026-11-18 首跑 mic_path 就是这样红的（教训同「帧通道必须由调用方持有」一族）。
    let task = tokio::spawn(async move {
        let mut child = child;
        let started = std::time::Instant::now();
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = match out.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    warn!(error = %e, "读麦克风编码器输出失败");
                    break;
                }
            };
            st.add_frame(n);
            if write_frame(&mut wr, FrameKind::Blob, &buf[..n]).await.is_err() {
                debug!("音频通道上行写入失败，采集收尾（对面可能已经走了）");
                break;
            }
            if wr.flush().await.is_err() {
                break;
            }
        }
        // 采集进程到点自己退（有界源）或被我们 abort。给它 2 秒收尸，超时才强杀 ——
        // 退出码进日志：现场「上行断在半句」时，这里能分清是采集崩了还是通道先退。
        match tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await {
            Ok(Ok(status)) => debug!(code = ?status.code(), "麦克风采集进程已退出"),
            Ok(Err(e)) => warn!(error = %e, "等麦克风采集进程退出时出错"),
            Err(_) => {
                let _ = child.kill().await;
                warn!("麦克风采集进程 2 秒没退，已强杀");
            }
        }
        // wr 随这里返回被 drop：TCP 写半边关闭，被控端拿到 EOF 自行收尾 —— 这就是「停」。
        info!(
            bytes = st.ogg_bytes.load(std::sync::atomic::Ordering::Relaxed),
            frames = st.frames.load(std::sync::atomic::Ordering::Relaxed),
            seconds = started.elapsed().as_secs_f64().round() as u64,
            "麦克风转接上行结束（写半端已关，被控端会跟着收尾）"
        );
    });

    Ok(MicOut { stats, task: Some(task) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_matrix() {
        // 三种写法各归各家
        assert_eq!(
            MicSource::parse("pulse:default").unwrap(),
            MicSource::Pulse { device: "default".into() }
        );
        assert_eq!(
            MicSource::parse("dshow:Microphone (Realtek(R) Audio)").unwrap(),
            MicSource::Dshow { name: "Microphone (Realtek(R) Audio)".into() }
        );
        assert_eq!(
            MicSource::parse("lavfi:sine=frequency=440").unwrap(),
            MicSource::Lavfi { spec: "sine=frequency=440".into() }
        );
        // 裸名字不放行：猜错设备名的表现是「采集到了别的东西」，比报错难查十倍。
        assert!(MicSource::parse("default").is_err());
        assert!(MicSource::parse("alsa:plughw:0,0").is_err());
        assert!(MicSource::parse("").is_err());
        assert!(MicSource::parse("pulse:").is_err());
        assert!(MicSource::parse("lavfi:").is_err());
        assert!(MicSource::parse("dshow:").is_err());
    }

    #[test]
    fn lavfi_gets_re_and_others_do_not() {
        // `-re` 只属于合成源：设备输入本来就由硬件按实时节奏喂，加了反而多一层节流。
        let l = MicSource::Lavfi { spec: "sine".into() }.ffmpeg_input();
        assert!(l.contains(&"-re".to_string()));
        let p = MicSource::Pulse { device: "default".into() }.ffmpeg_input();
        assert!(!p.contains(&"-re".to_string()));
        assert_eq!(p, vec!["-f", "pulse", "-i", "default"]);
        let d = MicSource::Dshow { name: "Mic".into() }.ffmpeg_input();
        assert!(d.contains(&"audio=Mic".to_string()), "dshow 的输入必须带 audio= 前缀：{d:?}");
    }

    /// 上行编码钉死的格式必须与被控端解码/播放要的格式一字不差（见模块头的解释）。
    #[test]
    fn encode_args_pins_the_shared_format() {
        let a = encode_args(&MicSource::Pulse { device: "default".into() }, 0);
        let val = |flag: &str| a.iter().position(|s| s == flag).map(|i| a[i + 1].clone());
        assert_eq!(val("-ar").as_deref(), Some(SAMPLE_RATE.to_string()).as_deref());
        assert_eq!(val("-ac").as_deref(), Some(CHANNELS.to_string()).as_deref());
        assert_eq!(val("-c:a").as_deref(), Some("libopus"));
        assert_eq!(val("-b:a").as_deref(), Some("64k"), "0 = 默认码率");
        assert_eq!(val("-page_duration").as_deref(), Some("20000"), "一页 20ms，否则声音晚一秒");
        assert_eq!(val("-frame_duration").as_deref(), Some("20"));
        assert_eq!(a.last().map(|s| s.as_str()), Some("pipe:1"));
        // pulse 输入段里不能混进 -re（只有 lavfi 需要）：
        assert!(!a.contains(&"-re".to_string()));
    }

    #[test]
    fn bitrate_is_clamped_like_the_downlink() {
        assert_eq!(normalize_bitrate(0), 64);
        assert_eq!(normalize_bitrate(8), 16, "低于 16 会被 Opus 拒收，夹住");
        assert_eq!(normalize_bitrate(320), 256);
        assert_eq!(normalize_bitrate(96), 96);
    }

    /// 样例是 2026-11-18 从本机 ffmpeg 9.0 抓的真实 stderr 形状
    /// （这台开发机没有录音设备，所以另附「只有小节头没有设备名」的反例）。
    #[test]
    fn parse_dshow_audio_takes_the_first_device_of_the_audio_section() {
        let sample = "\
[in#0 @ 0000020f] Auto-selected for input #0 (dshow).
[dshow @ 00000210] DirectShow video devices (some may be both video and audio devices)
[dshow @ 00000210] \"Integrated Camera\"
[dshow @ 00000210]     Alternative name \"@device_pvideo_...\"
[dshow @ 00000210] DirectShow audio devices
[dshow @ 00000210] \"麦克风 (Realtek(R) Audio)\"
[dshow @ 00000210]     Alternative name \"@device_sw:{...}\"
[dshow @ 00000210] \"立体声混音 (Realtek(R) Audio)\"
[in#0 @ 0000020f] Error opening input file dummy.
";
        assert_eq!(
            parse_dshow_audio(sample).as_deref(),
            Some("麦克风 (Realtek(R) Audio)"),
            "视频小节里的名字不算，只取音频小节第一个"
        );

        let no_audio = "\
[dshow @ 1] DirectShow video devices (some may be both video and audio devices)
[dshow @ 1] \"Camera\"
[dshow @ 1] DirectShow audio devices
[in#0] Could not enumerate audio only devices (or none found).
";
        // 「有小节头但没名字」是这台开发机的真实形态：必须老实返回 None，
        // 而不是把下一个小节的标题行或错误行当设备名。
        assert_eq!(parse_dshow_audio(no_audio), None);
        assert_eq!(parse_dshow_audio(""), None);
    }

    #[test]
    fn platform_default_is_honest() {
        // 麒麟（cfg 非 windows）是 pulse:default；Windows 的默认值要靠枚举，
        // 这里只声明「没有免进程的默认」——两种平台各钉各的，别互相蹭绿。
        let d = platform_default();
        if cfg!(windows) {
            assert!(d.is_none(), "Windows 不许凭空捏一个默认设备名");
        } else {
            assert_eq!(d, Some(MicSource::Pulse { device: "default".into() }));
        }
    }
}
