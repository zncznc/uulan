//! FFmpeg 命令行编排。
//!
//! 这里刻意**不链接 libavcodec**，只拼命令行、起子进程、读写管道。理由：
//! 麒麟的源里有现成的 ffmpeg 包，而被控端机器往往不方便装 Rust 的 FFI 构建依赖
//! （`ffmpeg-sys` 需要一套 pkg-config + 开发头文件）。走 CLI 的代价是管道拷贝，
//! 换来的是「零编译依赖、任何发行版都能跑」。实测若成为瓶颈，再换 FFI。
//!
//! 参数里的关键点（都是踩过坑才写上去的）：
//!
//! - **`-aud 1`**：每个访问单元写一个 AUD，于是读取端能精确切分 AU。
//!   不要用 `-flags +aud` —— ffmpeg 9 下该值解析失败（`Undefined constant ... in 'aud'`）。
//! - **`-tune zerolatency`**：关掉 B 帧与前瞻，否则画面要等好几帧才出。
//! - **GOP 不能太长**：全 I 帧在 Wi-Fi 上会打满带宽，但 GOP 太长时新接入的
//!   控制端要等下一个 I 帧才出图。折中是 fps 的一半（30fps → 15 帧 ≈ 0.5 秒）。
//! - **`-draw_mouse 0`**：不把光标画进视频流。光标由控制端本地绘制，这样
//!   移动是零延迟的，也不会出现双光标。
//! - **`-nostdin`**：否则 ffmpeg 会去抢 stdin，干扰我们用管道通信。
//!
//! ## 画质档位不在这一层
//!
//! 这一层只认两个数字：`cq`（恒定质量档）和 `max_bitrate_kbps`（码率上限，0 = 不限）。
//! 「自动 / 高清 / 超清 / 原画」那套档位表在 `uulan_protocol::msg::quality_preset`，
//! 因为它是**协议契约**的一部分（控制端发档位、被控端换算成编码参数）。
//! 早先这里自己定义过一个 `Quality` 枚举，结果和 protocol 里那个同名不同物，
//! 编译时在两个类型之间来回报错 —— 同一个概念只允许有一处定义。

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::{Child, Command};

/// 默认恒定质量档（x264 CRF）。桌面场景文字锐利度优先，23 是常用默认。
pub const DEFAULT_CQ: u8 = 23;

/// 硬件编码器选择。被控端按机器实际情况挑一个。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoder {
    /// 软件编码，哪台机器都能跑。默认值。
    X264,
    /// NVIDIA 显卡硬件编码。
    Nvenc,
    /// Intel / AMD 集显硬件编码（VAAPI）。
    Vaapi,
}

impl Encoder {
    pub fn label(self) -> &'static str {
        match self {
            Encoder::X264 => "libx264 (软件)",
            Encoder::Nvenc => "h264_nvenc (NVIDIA)",
            Encoder::Vaapi => "h264_vaapi (VAAPI)",
        }
    }

    pub fn codec_name(self) -> &'static str {
        match self {
            Encoder::X264 => "libx264",
            Encoder::Nvenc => "h264_nvenc",
            Encoder::Vaapi => "h264_vaapi",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "x264" | "cpu" | "libx264" | "auto" => Some(Encoder::X264),
            "nvenc" | "h264_nvenc" => Some(Encoder::Nvenc),
            "vaapi" | "h264_vaapi" => Some(Encoder::Vaapi),
            _ => None,
        }
    }
}

/// 被控端采集 + 编码的全部参数。
#[derive(Debug, Clone)]
pub struct CaptureOptions {
    /// X11 显示，如 `:0.0`。
    pub display: String,
    /// 采集区域尺寸。
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// 恒定质量档（x264 CRF）。`max_bitrate_kbps == 0` 时生效。
    pub cq: u8,
    /// 码率上限（kbps）。`0` 表示纯恒定质量，不设上限。
    pub max_bitrate_kbps: u32,
    /// 缩放系数，1.0 表示不缩放。只作用于编码，不改被控端显示器分辨率。
    pub scale: f32,
    pub encoder: Encoder,
    /// 是否把光标画进视频流（我们默认不画，由控制端本地绘制）。
    pub draw_mouse: bool,
    /// VAAPI 设备节点，仅 [`Encoder::Vaapi`] 使用。
    pub vaapi_device: String,
}

impl CaptureOptions {
    pub fn new(display: impl Into<String>, width: u32, height: u32) -> Self {
        Self {
            display: display.into(),
            width,
            height,
            fps: 30,
            cq: DEFAULT_CQ,
            max_bitrate_kbps: 0,
            scale: 1.0,
            encoder: Encoder::X264,
            draw_mouse: false,
            vaapi_device: "/dev/dri/renderD128".into(),
        }
    }
}

/// GOP 长度：fps 的一半（约 0.5 秒），至少 5 帧。
pub fn gop_for(fps: u32) -> u32 {
    (fps / 2).max(5)
}

/// X11 抓屏的输入段参数（`-f x11grab ... -i :0.0+0,0`）。
///
/// 单独拆出来是为了让测试能用 `-f lavfi` 合成源把它换掉 —— 测试机上未必有 X11，
/// 但除输入源之外的参数必须逐字一致地验证。
pub fn x11grab_input(o: &CaptureOptions) -> Vec<String> {
    let mut a = vec![
        "-f".to_string(),
        "x11grab".to_string(),
        "-draw_mouse".to_string(),
        if o.draw_mouse { "1".to_string() } else { "0".to_string() },
        "-framerate".to_string(),
        o.fps.to_string(),
        "-video_size".to_string(),
        format!("{}x{}", o.width, o.height),
    ];
    if o.encoder == Encoder::Vaapi {
        a.push("-vaapi_device".to_string());
        a.push(o.vaapi_device.clone());
    }
    a.push("-i".to_string());
    // x11grab 的输入地址形如 :0.0+0,0
    a.push(format!("{}+0,0", o.display));
    a
}

/// 拼出被控端「采集 + 编码 → stdout 输出裸 H.264」的完整参数。
///
/// 抽成纯函数是为了能单测：这些参数是本项目最容易踩坑的地方，
/// 改错一个（比如把 `-aud` 写成 `-flags +aud`）就会让控制端黑屏。
pub fn encode_args(o: &CaptureOptions) -> Vec<String> {
    encode_args_with_input(o, &x11grab_input(o))
}

/// 同 [`encode_args`]，但输入段由调用方提供（测试用合成源、或回放文件）。
pub fn encode_args_with_input(o: &CaptureOptions, input: &[String]) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    let push = |a: &mut Vec<String>, s: &str| a.push(s.to_string());

    push(&mut a, "-hide_banner");
    push(&mut a, "-loglevel");
    push(&mut a, "error");
    // 不抢 stdin，避免和上层管道通信打架
    push(&mut a, "-nostdin");

    // ---- 输入段（默认是 X11 抓屏，测试里可换成 lavfi / 文件）----
    a.extend(input.iter().cloned());

    // ---- 滤镜：可选缩放 / VAAPI 上传 ----
    let scaled = (o.scale - 1.0).abs() > f32::EPSILON;
    match o.encoder {
        Encoder::Vaapi => {
            let mut vf = String::from("format=nv12,hwupload");
            if scaled {
                vf = format!("scale=iw*{s}:ih*{s},format=nv12,hwupload", s = o.scale);
            }
            push(&mut a, "-vf");
            a.push(vf);
        }
        _ => {
            if scaled {
                push(&mut a, "-vf");
                a.push(format!("scale=iw*{s}:ih*{s}", s = o.scale));
            }
        }
    }

    // ---- 编码器 ----
    push(&mut a, "-c:v");
    push(&mut a, o.encoder.codec_name());
    match o.encoder {
        Encoder::X264 => {
            push(&mut a, "-preset");
            push(&mut a, "ultrafast");
            push(&mut a, "-tune");
            push(&mut a, "zerolatency");
        }
        Encoder::Nvenc => {
            // p1 = 最快档；tune ll = 低延迟
            push(&mut a, "-preset");
            push(&mut a, "p1");
            push(&mut a, "-tune");
            push(&mut a, "ll");
        }
        Encoder::Vaapi => {}
    }

    // 关键：每个访问单元写 AUD，读取端据此精确切帧
    push(&mut a, "-aud");
    push(&mut a, "1");

    push(&mut a, "-g");
    a.push(gop_for(o.fps).to_string());
    push(&mut a, "-pix_fmt");
    push(&mut a, "yuv420p");

    // ---- 码率 ----
    if o.max_bitrate_kbps == 0 {
        // 恒定质量：让编码器按画面复杂度自己决定花多少码率。
        // 桌面静止时几乎不花码率，动起来才花 —— 比固定码率清楚得多。
        push(&mut a, "-crf");
        a.push(o.cq.to_string());
    } else {
        // 有上限：capped VBR。bufsize 取上限的一半，让码率能随内容起伏但不长时间越线。
        let k = o.max_bitrate_kbps;
        push(&mut a, "-b:v");
        a.push(format!("{k}k"));
        push(&mut a, "-maxrate");
        a.push(format!("{k}k"));
        push(&mut a, "-bufsize");
        a.push(format!("{}k", (k / 2).max(1)));
    }

    // ---- 输出到 stdout ----
    push(&mut a, "-f");
    push(&mut a, "h264");
    push(&mut a, "pipe:1");
    a
}

/// 拼出控制端「stdin 收裸 H.264 → 解码为 BGRA 原始帧到 stdout」的参数。
///
/// **这里每个延时相关参数都是实测出来的，别随手删。**
///
/// ffmpeg 从管道读裸流时默认会先做输入分析（`probesize` 默认 5 MB、
/// `analyzeduration` 默认 5 秒）：它会一直攒数据直到「有把握」才吐出第一帧。
/// 在直播流上，这个「攒」直接表现为**首帧要等一两秒**。实测麒麟真机到 Windows
/// 控制端，加 `probesize`/`analyzeduration` 之前首帧 1769 ms。
///
/// - `-probesize 32` / `-analyzeduration 0`：别分析，SPS/PPS 就在流里，
///   第一个 AU 就够解码器开工。**实测零代价**（10 帧素材进出都是 10 帧）。
/// - `-flags low_delay`：让解码器走低延迟路径。**实测零代价**。
/// - **不要加 `nobuffer`**：实测它会丢帧 —— 从文件读时甚至输出 0 帧，
///   配合 `probesize` 时末尾稳定少一帧（10 帧变 9 帧）。它是拿正确性换延迟，
///   而延迟的收益本来就来自上面两项。这个参数看起来很像该加，但代价是实打实的。
pub fn decode_args(width: u32, height: u32) -> Vec<String> {
    vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
        "-probesize".into(),
        "32".into(),
        "-analyzeduration".into(),
        "0".into(),
        "-flags".into(),
        "low_delay".into(),
        // 容忍丢包/丢帧后继续解，不要直接退出。
        // 内网基本不丢包，但用户切网线/换 Wi-Fi 时能保住连接。
        "-fflags".into(),
        "+genpts+discardcorrupt".into(),
        "-flags2".into(),
        "showall".into(),
        "-f".into(),
        "h264".into(),
        "-i".into(),
        "pipe:0".into(),
        "-f".into(),
        "rawvideo".into(),
        "-pix_fmt".into(),
        "bgra".into(),
        "-s".into(),
        format!("{width}x{height}"),
        "pipe:1".into(),
    ]
}

/// 每帧原始像素的字节数（BGRA）。
pub fn frame_bytes(width: u32, height: u32) -> usize {
    (width as usize) * (height as usize) * 4
}

/// 把子进程 stderr 拉干净的后台任务。
///
/// **必须拉**：ffmpeg 的 stderr 管道写满后它会阻塞，把整条视频链路卡死。
/// 这类故障的表现是「画面看着看着就不动了」，而且进程还活着，极难排查。
pub fn drain_stderr(
    mut err: tokio::process::ChildStderr,
    label: &'static str,
) -> tokio::task::JoinHandle<()> {
    use tokio::io::AsyncReadExt;
    tokio::spawn(async move {
        let mut buf = vec![0u8; 4096];
        let mut tail: Vec<u8> = Vec::new();
        loop {
            match err.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    tail.extend_from_slice(&buf[..n]);
                    if tail.len() > 8192 {
                        let cut = tail.len() - 8192;
                        tail.drain(..cut);
                    }
                    for line in String::from_utf8_lossy(&buf[..n]).lines() {
                        if !line.trim().is_empty() {
                            tracing::debug!(target: "uulan::ffmpeg", "{label}: {line}");
                        }
                    }
                }
            }
        }
        if !tail.is_empty() {
            tracing::debug!(
                target: "uulan::ffmpeg",
                "{label} 退出，stderr 尾部: {}",
                String::from_utf8_lossy(&tail).lines().last().unwrap_or("")
            );
        }
    })
}

/// 找一个可用的 ffmpeg。
///
/// 优先级：显式传入 → 环境变量 `UULAN_FFMPEG` → `PATH` 上的 `ffmpeg`。
pub fn find_ffmpeg(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        if p.exists() {
            return Some(p.to_path_buf());
        }
    }
    if let Some(v) = std::env::var_os("UULAN_FFMPEG") {
        let p = PathBuf::from(v);
        if p.exists() {
            return Some(p);
        }
    }
    // PATH 上找：不真的去 spawn（省一次进程），只判断文件存在
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            for name in ["ffmpeg.exe", "ffmpeg"] {
                let cand = dir.join(name);
                if cand.exists() {
                    return Some(cand);
                }
            }
        }
    }
    None
}

/// 起一个被控端编码进程：抓屏 → H.264 → stdout。
pub fn spawn_encoder(ffmpeg: &Path, o: &CaptureOptions) -> std::io::Result<Child> {
    spawn_encoder_with_input(ffmpeg, o, &x11grab_input(o))
}

/// 同 [`spawn_encoder`]，但输入段由调用方提供。
pub fn spawn_encoder_with_input(
    ffmpeg: &Path,
    o: &CaptureOptions,
    input: &[String],
) -> std::io::Result<Child> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(encode_args_with_input(o, input))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd.spawn()
}

/// 起一个控制端解码进程：stdin 收 H.264 → BGRA → stdout。
pub fn spawn_decoder(ffmpeg: &Path, width: u32, height: u32) -> std::io::Result<Child> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(decode_args(width, height))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd.spawn()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> CaptureOptions {
        CaptureOptions::new(":0.0", 1280, 720)
    }

    /// 把参数拼成便于断言的字符串（值之间用空格）。
    fn joined(o: &CaptureOptions) -> String {
        encode_args(o).join(" ")
    }

    #[test]
    fn encode_has_aud_before_output() {
        let s = joined(&opts());
        // AUD 开关必须存在 —— 少了它控制端只能靠启发式切帧
        assert!(s.contains("-aud 1"), "缺少 -aud 1: {s}");
        // 不能用 -flags +aud（ffmpeg 9 下会失败）
        assert!(!s.contains("+aud"), "出现了无效的 -flags +aud: {s}");
    }

    #[test]
    fn encode_is_low_latency() {
        let s = joined(&opts());
        assert!(s.contains("-tune zerolatency"), "{s}");
        assert!(s.contains("-preset ultrafast"), "{s}");
        assert!(s.contains("-pix_fmt yuv420p"), "{s}");
        assert!(s.contains("-nostdin"), "{s}");
    }

    #[test]
    fn encode_does_not_draw_mouse_by_default() {
        let s = joined(&opts());
        assert!(s.contains("-draw_mouse 0"), "默认不该把光标画进流里: {s}");

        let mut o = opts();
        o.draw_mouse = true;
        assert!(joined(&o).contains("-draw_mouse 1"));
    }

    #[test]
    fn encode_input_spec_is_well_formed() {
        let s = joined(&opts());
        assert!(s.contains("-f x11grab"), "{s}");
        assert!(s.contains("-video_size 1280x720"), "{s}");
        assert!(s.contains("-framerate 30"), "{s}");
        // 输入位置带偏移，且必须紧跟 -i
        assert!(s.contains("-i :0.0+0,0"), "{s}");
        // 输出必须是 stdout 上的裸 h264
        assert!(s.trim_end().ends_with("-f h264 pipe:1"), "{s}");
    }

    #[test]
    fn gop_is_about_half_a_second() {
        assert_eq!(gop_for(30), 15);
        assert_eq!(gop_for(60), 30);
        assert_eq!(gop_for(4), 5, "fps 很低时也要有下限，不能是 0");
    }

    #[test]
    fn no_bitrate_cap_means_crf() {
        let s = joined(&opts());
        assert!(s.contains("-crf 23"), "{s}");
        assert!(!s.contains("-b:v"), "不限码率时不该出现 -b:v: {s}");

        let mut o = opts();
        o.cq = 18;
        assert!(joined(&o).contains("-crf 18"), "cq 必须真的传下去");
    }

    #[test]
    fn bitrate_cap_switches_to_capped_vbr_without_crf() {
        let mut o = opts();
        o.max_bitrate_kbps = 8000;
        let s = joined(&o);
        assert!(s.contains("-b:v 8000k"), "{s}");
        assert!(s.contains("-maxrate 8000k"), "{s}");
        assert!(s.contains("-bufsize 4000k"), "bufsize 应取上限的一半: {s}");
        assert!(!s.contains("-crf"), "给了固定码率就不该同时给 crf（x264 两种模式互斥）: {s}");
    }

    #[test]
    fn tiny_bitrate_does_not_produce_bufsize_zero() {
        let mut o = opts();
        o.max_bitrate_kbps = 1;
        assert!(joined(&o).contains("-bufsize 1k"), "bufsize 不能是 0");
    }

    #[test]
    fn scale_is_only_added_when_needed() {
        assert!(!joined(&opts()).contains("-vf"), "scale=1 不该产生 -vf");

        let mut o = opts();
        o.scale = 0.5;
        assert!(joined(&o).contains("-vf scale=iw*0.5:ih*0.5"));
    }

    #[test]
    fn nvenc_args_are_valid() {
        let mut o = opts();
        o.encoder = Encoder::Nvenc;
        let s = joined(&o);
        assert!(s.contains("-c:v h264_nvenc"), "{s}");
        assert!(s.contains("-tune ll"), "{s}");
        assert!(s.contains("-aud 1"), "{s}");
        assert!(!s.contains("-preset ultrafast"), "nvenc 不该用 x264 的 preset: {s}");
    }

    #[test]
    fn vaapi_args_include_device_and_hwupload() {
        let mut o = opts();
        o.encoder = Encoder::Vaapi;
        let s = joined(&o);
        assert!(s.contains("-vaapi_device /dev/dri/renderD128"), "{s}");
        assert!(s.contains("format=nv12,hwupload"), "{s}");
        assert!(s.contains("-aud 1"), "{s}");
    }

    #[test]
    fn vaapi_scaled_still_uploads() {
        let mut o = opts();
        o.encoder = Encoder::Vaapi;
        o.scale = 0.5;
        let s = joined(&o);
        // 顺序必须是 scale → nv12 → hwupload，否则多一次自动转换就白费了
        assert!(s.contains("-vf scale=iw*0.5:ih*0.5,format=nv12,hwupload"), "{s}");
    }

    #[test]
    fn encoder_parse_accepts_expected_spellings() {
        assert_eq!(Encoder::parse("x264"), Some(Encoder::X264));
        assert_eq!(Encoder::parse("cpu"), Some(Encoder::X264));
        assert_eq!(Encoder::parse("nvenc"), Some(Encoder::Nvenc));
        assert_eq!(Encoder::parse("vaapi"), Some(Encoder::Vaapi));
        assert_eq!(Encoder::parse("qsv"), None);
    }

    #[test]
    fn decode_args_read_stdin_write_bgra() {
        let s = decode_args(1920, 1080).join(" ");
        assert!(s.contains("-f h264 -i pipe:0"), "{s}");
        assert!(s.contains("-pix_fmt bgra"), "{s}");
        assert!(s.contains("-s 1920x1080"), "{s}");
        assert!(s.trim_end().ends_with("pipe:1"), "{s}");
    }

    #[test]
    fn frame_bytes_matches_bgra() {
        assert_eq!(frame_bytes(1280, 720), 1280 * 720 * 4);
    }
}
