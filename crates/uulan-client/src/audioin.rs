//! 音频接收与播放（M7）：audio 通道 → Opus/Ogg 解码（ffmpeg）→ 声卡 / WAV 文件。
//!
//! ## 分工
//!
//! - 异步侧：从 `SecReadHalf` 读 `Blob` 帧，塞进一条 std 通道（有界，反压到 socket）；
//! - 阻塞侧（`spawn_blocking`）：喂解码器、取 PCM、写播放后端。
//!
//! 为什么解码放在阻塞线程里：播放本身就是阻塞的（`waveOutWrite` 要等缓冲放完），
//! 把它塞进 tokio worker 会占住调度器；而 tokio 的进程 API 与 std 的混用又会多出
//! 一整层「谁读 stdout」的协调。一个线程里 std::process + 阻塞写，最简单也最不容易错。
//!
//! ## 平台
//!
//! - Windows：`waveOut`（winmm，手写 FFI —— 与 `termx::wincon` 同样的理由，不引 windows-sys 特性）。
//! - 其它平台：把裸 PCM 原样灌进一个播放命令的 stdin（候选见 [`player_candidates`]）。
//!   2026-11-17 在真麒麟上验通：控制端播出来的声音被 `parecord` 从信宿的 monitor 录回来，
//!   440 Hz 仍然是主角。
//! - **挑的是 `aplay` 而不是 `paplay`，这是实测出来的**：这台麒麟的 paplay（PulseAudio 13.99）
//!   只吃 libsndfile 认得的文件，喂 raw 一律「打开声音文件失败」（退出码 1）；而 ALSA 的 default
//!   本来就经 pulse 插件落到同一个信宿 —— 录回来的波形证明了这条路由，不是想当然。

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

use tracing::{debug, info, warn};

use uulan_protocol::crypto::SecReadHalf;
use uulan_protocol::frame::{read_frame, FrameKind};

/// 采样率/声道：解码器与播放器都按这套参数，改一处就要两处一起改。
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;

/// 收尾时给播放命令的宽限：先把缓冲里的尾巴放完，超时才 kill。
#[cfg(not(windows))]
const PIPE_FINISH_MS: u64 = 1500;

/// 音频统计。窗口模式与 headless 都在收工时打这一行 —— 「有没有声音」唯一可 grep 的证据。
#[derive(Debug, Default)]
pub struct AudioStats {
    /// 从 audio 通道收到的 Ogg 字节数。
    pub ogg_bytes: AtomicU64,
    /// 收到的帧数。
    pub frames: AtomicU64,
    /// 解码出来的 PCM 字节数（s16le 立体声 48 kHz，192000 B/s）。
    pub pcm_bytes: AtomicU64,
    /// 播放后端写出去的字节数。`--audio-save` 时与 `pcm_bytes` 相等。
    pub played_bytes: AtomicU64,
    /// 解码/播放出错次数。
    pub errors: AtomicU64,
    /// 峰值电平（0..=32767 的绝对值最大值），用来区分「有流但全是静音」。
    pub peak: AtomicU64,
}

impl AudioStats {
    pub fn pcm_seconds(&self) -> f64 {
        self.pcm_bytes.load(Ordering::Relaxed) as f64 / (SAMPLE_RATE as f64 * CHANNELS as f64 * 2.0)
    }

    /// 本机到底收到过音频字节没有。用来把「对面推完了/结束了」与
    /// 「对面说在推、其实一个字节都没到」分开 —— 两者在统计行上不能长一个样。
    pub fn got_bytes(&self) -> bool {
        self.ogg_bytes.load(Ordering::Relaxed) > 0
    }

    /// 一行给人看的总结。
    pub fn summary(&self) -> String {
        format!(
            "音频：收到 {} 帧 / {} 字节 → 解出 {:.1} 秒 PCM（峰值 {}，播放 {} 字节，错误 {}）",
            self.frames.load(Ordering::Relaxed),
            self.ogg_bytes.load(Ordering::Relaxed),
            self.pcm_seconds(),
            self.peak.load(Ordering::Relaxed),
            self.played_bytes.load(Ordering::Relaxed),
            self.errors.load(Ordering::Relaxed),
        )
    }
}

/// 从哪儿出声。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sink {
    /// 实时播放到系统默认输出设备。
    Speaker,
    /// 落成一个 WAV 文件（无人值守验收用；也方便现场取证）。
    Wav(PathBuf),
}

/// 一条音频接收任务。
///
/// `Drop` 会 abort 异步侧；阻塞侧由 `spawn_blocking` 的句柄 abort（它会在下一个
/// 检查点退出，解码器进程 `kill_on_drop`）。
pub struct AudioIn {
    stats: Arc<AudioStats>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl AudioIn {
    pub fn stats(&self) -> Arc<AudioStats> {
        self.stats.clone()
    }

    /// 挂一个跟着这条会话走的任务（会话结束 → `Drop` → abort）。
    ///
    /// 看门狗用它：那条任务既不能比会话活得久（会往已经关掉的通道里发 AudioStop），
    /// 也不能由调用方自己拿着句柄管 —— 会话槽那边没有地方放它。
    pub fn add_task(&mut self, t: tokio::task::JoinHandle<()>) {
        self.tasks.push(t);
    }
}

impl Drop for AudioIn {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// 起一条音频接收。
///
/// `ffmpeg` 为 `None` 时走 PATH。
pub fn start(
    audio: SecReadHalf,
    ffmpeg: Option<PathBuf>,
    sink: Sink,
    stats: Arc<AudioStats>,
    watch: Arc<AudioWatch>,
) -> AudioIn {
    let ffmpeg = ffmpeg.unwrap_or_else(|| PathBuf::from("ffmpeg"));

    // 有界通道：解码跟不上就反压到 socket（宁可让被控端那边编码器等一等，
    // 也不要在内存里堆几百毫秒的声音 —— 远控要的是「现在的」声音）。
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(16);
    let st_in = stats.clone();
    let reader = tokio::spawn(async move {
        let mut audio = audio;
        loop {
            let f = match read_frame(&mut audio).await {
                Ok(f) => f,
                Err(e) => {
                    debug!(error = %e, "音频通道结束");
                    break;
                }
            };
            match f.kind {
                FrameKind::Blob => {
                    let n = f.payload.len();
                    if n == 0 {
                        continue;
                    }
                    st_in.frames.fetch_add(1, Ordering::Relaxed);
                    st_in.ogg_bytes.fetch_add(n as u64, Ordering::Relaxed);
                    if tx.send(f.payload.to_vec()).is_err() {
                        break;
                    }
                }
                FrameKind::Bye => break,
                // 音频通道上只该有 Blob；别的帧忽略（前向兼容）。
                other => debug!(?other, "音频通道上收到意外帧，已忽略"),
            }
        }
    });

    let st_out = stats.clone();
    let player = tokio::task::spawn_blocking(move || {
        play_loop(&ffmpeg, sink, rx, st_out, watch);
    });

    AudioIn { stats, tasks: vec![reader, player] }
}

/// ffmpeg 解码参数：Ogg → s16le 裸 PCM。
fn decode_args() -> Vec<String> {
    vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
        "-f".into(),
        "ogg".into(),
        "-i".into(),
        "pipe:0".into(),
        "-f".into(),
        "s16le".into(),
        "-ar".into(),
        SAMPLE_RATE.to_string(),
        "-ac".into(),
        CHANNELS.to_string(),
        "pipe:1".into(),
    ]
}

/// 阻塞侧：喂字节 → 取 PCM → 写后端。
fn play_loop(
    ffmpeg: &std::path::Path,
    sink: Sink,
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    stats: Arc<AudioStats>,
    watch: Arc<AudioWatch>,
) {
    let mut child = match std::process::Command::new(ffmpeg)
        .args(decode_args())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "音频解码器启动失败");
            stats.errors.fetch_add(1, Ordering::Relaxed);
            // 本机连解码器都起不来 → 记一笔，由会话那边的看门狗发 `C2H::AudioStop`：
            // 再让被控端一路编码下去只是白吃 CPU 与带宽，而这边永远不会有声音。
            watch.note_local_failure(&format!("本机音频解码器起不来：{e}"));
            return;
        }
    };
    let mut din = child.stdin.take().expect("piped stdin");
    let mut dout = child.stdout.take().expect("piped stdout");

    let mut out: Box<dyn Out> = match &sink {
        Sink::Wav(p) => match WavOut::create(p) {
            Ok(w) => Box::new(w),
            Err(e) => {
                warn!(error = %e, path = %p.display(), "打不开录音文件，改为丢弃");
                Box::new(NullOut)
            }
        },
        Sink::Speaker => match speaker() {
            Ok(s) => s,
            Err(e) => {
                // 没有声卡/被占用时不要整条链路跟着死：继续解码，只是不出声。
                warn!(error = %e, "打不开音频输出设备（只解码，不播放）");
                Box::new(NullOut)
            }
        },
    };

    // 喂字节的线程：`din.write_all` 阻塞，正好由它承担反压。
    let feeder = std::thread::spawn(move || {
        while let Ok(chunk) = rx.recv() {
            if din.write_all(&chunk).is_err() {
                break;
            }
        }
        // 关掉 stdin 让 ffmpeg 把缓冲区里剩下的解码完 —— 不关的话最后几十毫秒会丢。
        let _ = din.flush();
    });

    // 主线程读 PCM 并播放。
    let mut pcm = vec![0u8; 8192];
    loop {
        let n = match dout.read(&mut pcm) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        stats.pcm_bytes.fetch_add(n as u64, Ordering::Relaxed);
        // 峰值：s16le 小端，两字节一个样本。
        let mut peak = 0u32;
        for pair in pcm[..n].chunks_exact(2) {
            let v = i16::from_le_bytes([pair[0], pair[1]]).unsigned_abs() as u32;
            if v > peak {
                peak = v;
            }
        }
        let cur = stats.peak.load(Ordering::Relaxed);
        if peak as u64 > cur {
            stats.peak.store(peak as u64, Ordering::Relaxed);
        }
        if out.write(&pcm[..n]).is_err() {
            stats.errors.fetch_add(1, Ordering::Relaxed);
            break;
        }
        stats.played_bytes.fetch_add(n as u64, Ordering::Relaxed);
    }

    let _ = feeder.join();
    out.finish();
    let _ = child.wait();
    info!(pcm = stats.pcm_bytes.load(Ordering::Relaxed), "音频解码结束");
}

trait Out: Send {
    fn write(&mut self, pcm: &[u8]) -> std::io::Result<()>;
    fn finish(&mut self) {}
}

/// 不出声的后端：没有声卡 / 没实现实时播放时用它，解码链路照跑（统计仍然有意义）。
struct NullOut;

impl Out for NullOut {
    fn write(&mut self, _pcm: &[u8]) -> std::io::Result<()> {
        Ok(())
    }
}

/// WAV 落盘。头里的长度先写占位，`finish` 时按实际字节数回填。
///
/// 为什么不是「直接写成一堆裸 PCM」：现场取证要能双击就放出来。
struct WavOut {
    f: std::fs::File,
    data: u64,
}

impl WavOut {
    fn create(path: &std::path::Path) -> std::io::Result<Self> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let mut f = std::fs::File::create(path)?;
        f.write_all(&wav_header(0))?;
        Ok(WavOut { f, data: 0 })
    }
}

impl Out for WavOut {
    fn write(&mut self, pcm: &[u8]) -> std::io::Result<()> {
        self.f.write_all(pcm)?;
        self.data += pcm.len() as u64;
        Ok(())
    }

    fn finish(&mut self) {
        use std::io::Seek;
        self.data = self.f.stream_position().unwrap_or(self.data).saturating_sub(44);
        if self.f.seek(std::io::SeekFrom::Start(0)).is_ok() {
            let _ = self.f.write_all(&wav_header(self.data));
            let _ = self.f.flush();
        }
        info!(bytes = self.data, "音频已写入 WAV");
    }
}

fn wav_header(data_len: u64) -> Vec<u8> {
    let data_len = data_len.min(u32::MAX as u64) as u32;
    let byte_rate = SAMPLE_RATE * CHANNELS as u32 * 2;
    let mut h = Vec::with_capacity(44);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&(36 + data_len).to_le_bytes());
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk 大小
    h.extend_from_slice(&1u16.to_le_bytes()); // PCM
    h.extend_from_slice(&CHANNELS.to_le_bytes());
    h.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    h.extend_from_slice(&byte_rate.to_le_bytes());
    h.extend_from_slice(&(CHANNELS * 2).to_le_bytes()); // block align
    h.extend_from_slice(&16u16.to_le_bytes()); // 位深
    h.extend_from_slice(b"data");
    h.extend_from_slice(&data_len.to_le_bytes());
    h
}

/// 按平台挑实时播放后端。
#[cfg(windows)]
fn speaker() -> Result<Box<dyn Out>, String> {
    Ok(Box::new(winmm::WaveOut::open()?))
}

#[cfg(not(windows))]
fn speaker() -> Result<Box<dyn Out>, String> {
    let dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let mut tried = Vec::new();
    for cand in player_candidates() {
        let Some(found) = find_in_path(cand.program, &dirs) else {
            tried.push(format!("{}（不在 PATH）", cand.program));
            continue;
        };
        match PipeOut::open(&cand, &found) {
            Ok(o) => {
                // 这行是「本机到底有没有出声」唯一可 grep 的证据：统计里那个播放字节数只说明
                // 字节写进了管道，而管道能一声不响地吞掉一整晚。
                info!(program = %cand.program, rate = SAMPLE_RATE, channels = CHANNELS,
                      "实时播放已接上");
                return Ok(Box::new(o));
            }
            Err(e) => tried.push(format!("{}：{}", cand.program, e)),
        }
    }
    Err(format!(
        "这台机器上没能起任何一个播放命令（{}）—— 用 --audio-save 落成 WAV 仍可验收",
        tried.join(" / ")
    ))
}

/// 一个实时播放候选：命令名 + 完整参数（参数里已经钉死采样格式）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerCandidate {
    pub program: &'static str,
    pub args: Vec<String>,
}

/// 候选表。**只列实测过能吃 raw stdin 的命令**：`paplay` 在这台麒麟上明确不行（见模块头），
/// `pw-play` 手边没有 PipeWire 机器可验 —— 写进来就是拿没验证过的东西去抢播放顺序。
///
/// 为什么它是纯函数：参数错一个字符，现场的表现是「流在动、统计在涨、一声不响」，
/// 那是最难查的一类故障，所以两个平台的单测都得能钉住这张表。
pub fn player_candidates() -> Vec<PlayerCandidate> {
    vec![PlayerCandidate {
        program: "aplay",
        // raw 模式必须把格式说全：少了任何一项，ALSA 要么当场报错退出，要么按错的规格播。
        args: vec![
            "-q".into(),
            "-t".into(),
            "raw".into(),
            "-f".into(),
            "S16_LE".into(),
            "-c".into(),
            CHANNELS.to_string(),
            "-r".into(),
            SAMPLE_RATE.to_string(),
            "-".into(),
        ],
    }]
}

/// 按 PATH 的顺序找可执行文件；「算不算可执行」由调用方给，单测因此不碰磁盘。
pub fn find_in_path_with(
    name: &str,
    dirs: &[PathBuf],
    is_exe: impl Fn(&std::path::Path) -> bool,
) -> Option<PathBuf> {
    dirs.iter().find_map(|d| {
        let p = d.join(name);
        if is_exe(&p) {
            Some(p)
        } else {
            None
        }
    })
}

/// 生产入口：拿真实文件系统的答案。
pub fn find_in_path(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    find_in_path_with(name, dirs, |p| is_executable(p))
}

#[cfg(unix)]
fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &std::path::Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
}

/// unix 实时播放：PCM 原样进子进程的 stdin。
///
/// 反压靠子进程本身 —— `write_all` 阻塞在它没读完的时候，正是「能等就别丢字节」
/// （与 Windows 那条 `waveOutWrite` 等缓冲放完同理，也和被控端 PTY 那侧一致）。
#[cfg(not(windows))]
struct PipeOut {
    program: String,
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
}

#[cfg(not(windows))]
impl PipeOut {
    fn open(cand: &PlayerCandidate, program_path: &std::path::Path) -> Result<Self, String> {
        let mut child = std::process::Command::new(program_path)
            .args(&cand.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("{}", e))?;
        let stdin = child.stdin.take().ok_or_else(|| "拿不到 stdin".to_string())?;
        Ok(PipeOut { program: cand.program.into(), child, stdin: Some(stdin) })
    }
}

#[cfg(not(windows))]
impl Out for PipeOut {
    fn write(&mut self, pcm: &[u8]) -> std::io::Result<()> {
        match self.stdin.as_mut() {
            Some(s) => s.write_all(pcm),
            None => Err(std::io::Error::other("播放后端已经关掉了")),
        }
    }

    fn finish(&mut self) {
        // 先关 stdin，让播放器把缓冲里的尾巴放完自己退出；直接 kill 会切掉最后一句。
        self.stdin = None;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(PIPE_FINISH_MS);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = self.child.kill();
                        let _ = self.child.wait();
                        warn!(program = %self.program, "播放命令没自己退出，宽限到点后终止（尾巴可能被切）");
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(e) => {
                    warn!(error = %e, program = %self.program, "问不到播放命令的状态");
                    break;
                }
            }
        }
        info!(program = %self.program, "实时播放已收尾");
    }
}

#[cfg(windows)]
mod winmm {
    //! 手写 waveOut FFI：只用 5 个符号，不值得为它把 windows-sys 的
    //! `Win32_Media_Audio` 特性打开（那会把整个多媒体头文件搬进来）。

    use std::ffi::c_void;

    type Hwaveout = *mut c_void;
    type MmResult = u32;

    const WAVE_MAPPER: u32 = u32::MAX;
    const WAVE_FORMAT_PCM: u16 = 1;
    const CALLBACK_NULL: u32 = 0;
    /// 缓冲准备好了（内核已放完这一块）。`waveOutWrite` 之后轮询它。
    const WHDR_DONE: u32 = 0x0000_0001;
    const WHDR_PREPARED: u32 = 0x0000_0002;

    #[repr(C)]
    struct WaveFormatEx {
        format_tag: u16,
        channels: u16,
        samples_per_sec: u32,
        avg_bytes_per_sec: u32,
        block_align: u16,
        bits_per_sample: u16,
        cb_size: u16,
    }

    #[repr(C)]
    struct WaveHdr {
        data: *mut u8,
        buffer_length: u32,
        bytes_recorded: u32,
        flags: u32,
        loops: u32,
        lp_data: usize,
        reserved: usize,
    }

    #[link(name = "winmm")]
    extern "system" {
        fn waveOutOpen(
            h: *mut Hwaveout,
            device: u32,
            fmt: *const WaveFormatEx,
            callback: usize,
            instance: usize,
            flags: u32,
        ) -> MmResult;
        fn waveOutPrepareHeader(h: Hwaveout, hdr: *mut WaveHdr, size: u32) -> MmResult;
        fn waveOutUnprepareHeader(h: Hwaveout, hdr: *mut WaveHdr, size: u32) -> MmResult;
        fn waveOutWrite(h: Hwaveout, hdr: *mut WaveHdr, size: u32) -> MmResult;
        fn waveOutReset(h: Hwaveout) -> MmResult;
        fn waveOutClose(h: Hwaveout) -> MmResult;
    }

    pub struct WaveOut {
        h: Hwaveout,
        /// 缓冲区必须活到 `waveOutUnprepareHeader` 之后 —— 由我们自己持有，
        /// 先 prepare/write/unprepare 再复用，避免让内核指着一块已经释放的内存。
        buf: Vec<u8>,
        hdr: Box<WaveHdr>,
    }

    // SAFETY: 句柄只在持有它的线程里用（播放循环是单线程），
    // 且 `Out` 要求 Send 只是为了让 Box<dyn Out> 能在线程间移动 ——
    // 实际上它从创建到 drop 都在同一个阻塞线程里。
    unsafe impl Send for WaveOut {}

    impl WaveOut {
        pub fn open() -> Result<Self, String> {
            let fmt = WaveFormatEx {
                format_tag: WAVE_FORMAT_PCM,
                channels: super::CHANNELS,
                samples_per_sec: super::SAMPLE_RATE,
                avg_bytes_per_sec: super::SAMPLE_RATE * super::CHANNELS as u32 * 2,
                block_align: super::CHANNELS * 2,
                bits_per_sample: 16,
                cb_size: 0,
            };
            let mut h: Hwaveout = std::ptr::null_mut();
            // SAFETY: fmt 是合法的栈上结构，h 指向可写的局部变量。
            let rc = unsafe {
                waveOutOpen(&mut h, WAVE_MAPPER, &fmt, 0, 0, CALLBACK_NULL)
            };
            if rc != 0 {
                return Err(format!("waveOutOpen 失败（MMRESULT {rc}）"));
            }
            Ok(WaveOut {
                h,
                buf: vec![0u8; 16 * 1024],
                hdr: Box::new(WaveHdr {
                    data: std::ptr::null_mut(),
                    buffer_length: 0,
                    bytes_recorded: 0,
                    flags: 0,
                    loops: 0,
                    lp_data: 0,
                    reserved: 0,
                }),
            })
        }

        fn write_block(&mut self, pcm: &[u8]) -> std::io::Result<()> {
            let n = pcm.len().min(self.buf.len());
            // 不足一块的尾巴也照样送（waveOut 允许任意长度，只要是 block_align 的整数倍）。
            let n = n - (n % (super::CHANNELS as usize * 2));
            if n == 0 {
                return Ok(());
            }
            self.buf[..n].copy_from_slice(&pcm[..n]);
            self.hdr.data = self.buf.as_mut_ptr();
            self.hdr.buffer_length = n as u32;
            self.hdr.bytes_recorded = 0;
            self.hdr.flags = 0;
            // SAFETY: 头结构、缓冲区都活到 unprepare 之后。
            unsafe {
                let rc = waveOutPrepareHeader(self.h, &mut *self.hdr, std::mem::size_of::<WaveHdr>() as u32);
                if rc != 0 {
                    return Err(std::io::Error::other(format!("waveOutPrepareHeader {rc}")));
                }
                let rc = waveOutWrite(self.h, &mut *self.hdr, std::mem::size_of::<WaveHdr>() as u32);
                if rc != 0 {
                    waveOutUnprepareHeader(self.h, &mut *self.hdr, std::mem::size_of::<WaveHdr>() as u32);
                    return Err(std::io::Error::other(format!("waveOutWrite {rc}")));
                }
            }
            // 等这一块放完再复用缓冲区（约 85 ms；播放本来就是实时的）。
            let mut spins = 0u32;
            while self.hdr.flags & WHDR_DONE == 0 {
                std::thread::sleep(std::time::Duration::from_millis(5));
                spins += 1;
                if spins > 1000 {
                    break;
                }
            }
            let _ = self.hdr.flags & WHDR_PREPARED;
            // SAFETY: 同上。
            unsafe {
                waveOutUnprepareHeader(self.h, &mut *self.hdr, std::mem::size_of::<WaveHdr>() as u32);
            }
            Ok(())
        }
    }

    impl super::Out for WaveOut {
        fn write(&mut self, pcm: &[u8]) -> std::io::Result<()> {
            self.write_block(pcm)
        }

        fn finish(&mut self) {
            // SAFETY: 句柄由我们自己打开，只在 Drop/finish 关闭一次。
            unsafe {
                waveOutReset(self.h);
                waveOutClose(self.h);
                self.h = std::ptr::null_mut();
            }
        }
    }

    impl Drop for WaveOut {
        fn drop(&mut self) {
            if !self.h.is_null() {
                // SAFETY: 同上。
                unsafe {
                    waveOutReset(self.h);
                    waveOutClose(self.h);
                }
            }
        }
    }
}

// ─────────────────── 被控端音频状态（`H2C::AudioState`）的观察器 ───────────────────

/// `C2H::AudioStart` 发出去之后，允许被控端多久一句 `AudioState` 都不回。
///
/// 5 秒够了：被控端起音频是「解析/探测源 → 拉 ffmpeg」，成功那句 `running:true`
/// 通常几百毫秒内到，失败那句 `running:false` 更是立刻到。真过了 5 秒还没回音，
/// 只能是**对面根本不会发这条消息**（版本比它更旧）—— 那就必须说破，
/// 不能让现场把「没声音」理解成「网络慢」。
pub const AUDIO_ACK_GRACE_MS: u64 = 5_000;

/// `detail` 进状态机之前裁到这么多个**字符**（它要上日志与统计行）。
const DETAIL_MAX_CHARS: usize = 160;

/// 把被控端给的诊断文本压成一行。换行必须去掉：现场有人把整段 ffmpeg 的 stderr
/// 塞进配置错误里，一行统计被刷成十行就没法 grep 了。按字符裁而不是按字节 ——
/// 中文详情按字节切会正好切在多字节中间。
fn clip_detail(s: &str) -> String {
    let flat = s.replace('\n', " ").replace('\r', " ");
    if flat.chars().count() <= DETAIL_MAX_CHARS {
        flat
    } else {
        let mut out: String = flat.chars().take(DETAIL_MAX_CHARS).collect();
        out.push('…');
        out
    }
}

/// 这条会话的音频到底处在什么状态。
///
/// 为什么要专门记它：被控端在「音频起不来」的每一条路上都会回一句
/// `AudioState{running:false, detail=原因}`（音频源写错、这台机器没 PulseAudio、
/// ffmpeg 起不来…），而控制端以前**一条都没接** —— `control.rs` 的兜底分支把它丢了。
/// 于是现场只看得见「没声音」，看不见「为什么没声音」：与 2026-10-31 那个
/// 「剪贴板恒报发送 0 却查不出原因」是同一类问题（协议先行、两端未接）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioPhase {
    /// 没要声音（没给 `--audio`）。
    Idle,
    /// 要了声音，但被控端没声明 `Cap::Audio` —— 连 `AudioStart` 都没发出去。
    Unsupported,
    /// 已发 `AudioStart`，还没等到被控端任何一句 `AudioState`。
    Requested,
    /// 被控端确认在推流。
    Running,
    /// 从来没推起来过，被控端就明说不行（原因在 `detail`）。
    Failed,
    /// 推过之后被控端不再推了（编码器退出 / 会话收尾）—— 这不是「起不来」。
    Ended,
    /// 本机主动叫停（本机解码器起不来时发 `C2H::AudioStop`），对面那句
    /// `running:false` 是预期内的确认，不计入失败。
    Stopped,
    /// `AudioStart` 发出去了，被控端**一句都没回**（版本比这条状态消息更旧）。
    Stalled,
}

impl AudioPhase {
    /// 一行中文说法。日志与收工统计共用，免得两处各写一套措辞、grep 规则也要写两套。
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "本端没要声音",
            Self::Unsupported => "被控端没有音频能力（没声卡 / 没 PulseAudio），没请求",
            Self::Requested => "已请求，等被控端回话",
            Self::Running => "被控端正在推流",
            Self::Failed => "被控端音频起不来",
            Self::Ended => "被控端不再推流",
            Self::Stopped => "本机已请求停止",
            Self::Stalled => "被控端没回过音频状态（版本比这条消息更旧？）",
        }
    }
}

#[derive(Debug)]
struct Inner {
    phase: AudioPhase,
    /// 收到过几条 `AudioState`（含内容重复的）。
    states: u64,
    /// 其中「一次都没推起来过就说不行」的条数。
    failures: u64,
    /// 本机叫停**之后**被控端回的确认条数。
    ///
    /// 这一条是「`C2H::AudioStop` 真的被对面处理了」的唯一证据：`phase==Stopped`
    /// 是本机自己记的（发之前就记了），光看它分不清「对面停了」与「对面没理我们」。
    stop_acks: u64,
    requested_at_ms: u64,
    codec: String,
    detail: String,
    /// 本机一侧的失败原因（解码器进程都拉不起来），由看门狗取走一次。
    local_failure: Option<String>,
}

impl Default for Inner {
    fn default() -> Self {
        Inner {
            phase: AudioPhase::Idle,
            states: 0,
            failures: 0,
            stop_acks: 0,
            requested_at_ms: 0,
            codec: String::new(),
            detail: String::new(),
            local_failure: None,
        }
    }
}

/// 音频状态观察器。纯逻辑 + **时钟由调用方给**（`now_ms`），所以每一条相位迁移
/// 都能在 `cargo test` 里钉住 —— 不需要真链路，更不需要真声卡。
#[derive(Debug, Default)]
pub struct AudioWatch {
    inner: Mutex<Inner>,
}

impl AudioWatch {
    /// 进程内单调时钟（毫秒）。与 `control::now_us` 同一套理由：用 `Instant`，
    /// 别用墙上时钟 —— NTP 拽一下就能把「等了 5 秒」变成「等了负 3 秒」。
    pub fn clock_ms() -> u64 {
        use std::sync::OnceLock;
        static T0: OnceLock<std::time::Instant> = OnceLock::new();
        T0.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // 中毒也继续：这里没有跨 panic 的不变量，把原 panic 变成二次 panic 只会更难查。
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 记一次「`AudioStart` 已经发出去了」。返回相位是否真的变了。
    pub fn note_requested(&self, now_ms: u64) -> bool {
        let mut w = self.lock();
        if w.phase == AudioPhase::Running {
            // 已经在推（重连后的第二条会话重复请求）：被控端会回一句「已在运行」，
            // 这边别把一次无害的重复请求记成「重新开始等回音」。
            return false;
        }
        w.phase = AudioPhase::Requested;
        w.requested_at_ms = now_ms;
        w.detail.clear();
        true
    }

    /// 对面压根没声明 `Cap::Audio`：连请求都发不出去。原因要留在状态里，
    /// 不能只留在日志里 —— headless 的收工统计是脚本唯一的观测面。
    pub fn note_unsupported(&self) {
        self.lock().phase = AudioPhase::Unsupported;
    }

    /// 本机主动叫停（发 `C2H::AudioStop` 之前调）。`why` 是本机给的原因。
    pub fn note_stop_requested(&self, why: &str) {
        let mut w = self.lock();
        w.phase = AudioPhase::Stopped;
        w.detail = clip_detail(why);
    }

    /// 记一条被控端来的 `AudioState`。返回 `Some((从, 到))` = 相位变了，
    /// 调用方只在变化时打日志（同一状态每 2 秒重复报一次会把终端刷满）。
    ///
    /// 计数口径是这一轮定下来的，写清楚免得以后被「顺手改回去」：
    /// - 一次都没推起来过就说 `running:false` → **Failed**（`failures += 1`）；
    /// - 推过之后停了 → **Ended**。这是正常收尾，记成故障会让每条会话都以红收场；
    /// - 我们自己叫的停之后那句 `false` → 仍是 **Stopped**，不计失败。
    pub fn on_state(
        &self,
        running: bool,
        codec: &str,
        detail: &str,
    ) -> Option<(AudioPhase, AudioPhase)> {
        let mut w = self.lock();
        w.states += 1;
        let from = w.phase;
        let to = if running {
            AudioPhase::Running
        } else {
            match from {
                AudioPhase::Running => AudioPhase::Ended,
                AudioPhase::Stopped => {
                    w.stop_acks += 1;
                    AudioPhase::Stopped
                }
                _ => {
                    w.failures += 1;
                    AudioPhase::Failed
                }
            }
        };
        if !codec.is_empty() {
            w.codec = codec.to_string();
        }
        // 「我们自己叫的停」之后那句确认**不许**盖掉本机的原因。
        // 顺序是：看门狗先 `note_stop_requested(本机原因)`，再把 AudioStop 发出去，
        // 被控端回的那句只是「知道了」——它说的是我们已经知道的事，而对面那句
        // 「找不到 ffmpeg」才是现场要看的根因。（这一条是被自己的单测抓出来的。）
        if !detail.is_empty() && !(from == AudioPhase::Stopped && to == AudioPhase::Stopped) {
            w.detail = clip_detail(detail);
        }
        w.phase = to;
        if from == to {
            None
        } else {
            Some((from, to))
        }
    }

    /// 「请求了却一句回音都没有」到点了没有。**只在跨界那一次**返回 `true`。
    pub fn check_stalled(&self, now_ms: u64, grace_ms: u64) -> bool {
        let mut w = self.lock();
        if w.phase != AudioPhase::Requested {
            return false;
        }
        if now_ms.saturating_sub(w.requested_at_ms) < grace_ms {
            return false;
        }
        w.phase = AudioPhase::Stalled;
        true
    }

    /// 本机一侧起不来。留给看门狗去发 `C2H::AudioStop`；只记第一句（后面的是同一件事）。
    pub fn note_local_failure(&self, why: &str) {
        let mut w = self.lock();
        if w.local_failure.is_none() {
            w.local_failure = Some(clip_detail(why));
        }
    }

    pub fn take_local_failure(&self) -> Option<String> {
        self.lock().local_failure.take()
    }

    pub fn phase(&self) -> AudioPhase {
        self.lock().phase
    }

    pub fn states(&self) -> u64 {
        self.lock().states
    }

    pub fn failures(&self) -> u64 {
        self.lock().failures
    }

    /// 本机叫停之后，被控端回过几句确认（>0 才算 `AudioStop` 真被处理了）。
    pub fn stop_acks(&self) -> u64 {
        self.lock().stop_acks
    }

    /// 一行可 grep 的总结（收工统计用）。
    ///
    /// `got_bytes` = 本机到底收到过音频字节没有。「被控端说结束了却一字节没到」
    /// 与「放完了」在统计行上必须分得开 —— 这正是现场最容易混的两件事。
    pub fn summary(&self, got_bytes: bool) -> String {
        let w = self.lock();
        let mut s = format!(
            "音频状态 : {}（被控端回了 {} 条 / 起不来 {} 次）",
            w.phase.label(),
            w.states,
            w.failures
        );
        if !w.codec.is_empty() {
            s.push_str(&format!("，编码 {}", w.codec));
        }
        if !w.detail.is_empty() {
            s.push_str(&format!("，最后一句：{}", w.detail));
        }
        if w.phase == AudioPhase::Ended && !got_bytes {
            s.push_str("（本机一个字节都没收到 —— 采集源没起来？）");
        }
        // 「我们叫了停」与「对面确认停了」是两件事，必须分开说：前者只是本机的意图。
        if w.phase == AudioPhase::Stopped {
            s.push_str(if w.stop_acks > 0 {
                "，被控端已确认停止"
            } else {
                "，被控端还没确认"
            });
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 假时钟起点。状态机的时间一律由调用方给，所以这些测试不真等 5 秒。
    const T0: u64 = 1_000;

    /// 解码器吐的是什么，播放命令就得要什么 —— 一字不差。对不上的表现是「ALSA 当场退出」
    /// 或「一声不响但统计在涨」，两种都极难在现场定位，所以这条必须被单测钉住。
    #[test]
    fn player_args_are_exactly_the_decoders_format() {
        let c = player_candidates();
        assert_eq!(c.len(), 1, "候选表里只该有实测过的那一个");
        let a = &c[0].args;
        let val = |flag: &str| a.iter().position(|s| s == flag).map(|i| a[i + 1].clone());
        assert_eq!(val("-t").as_deref(), Some("raw"), "必须走 raw 模式");
        assert_eq!(val("-f").as_deref(), Some("S16_LE"));
        assert_eq!(val("-c").as_deref(), Some(CHANNELS.to_string()).as_deref());
        assert_eq!(val("-r").as_deref(), Some(SAMPLE_RATE.to_string()).as_deref());
        assert_eq!(a.last().map(|s| s.as_str()), Some("-"), "数据必须从 stdin 进");
    }

    /// PATH 的顺序说了算，而且只认真正能执行的那一项；谁都没有时是老实的 None。
    #[test]
    fn find_in_path_follows_path_order_and_executability() {
        // 两边的路径一律用同一个 join 造出来：Windows 上分隔符是 \，写成字面量会假红。
        let first = PathBuf::from("/first").join("aplay");
        let second = PathBuf::from("/second").join("aplay");
        let dirs = vec![PathBuf::from("/first"), PathBuf::from("/second")];
        let table = std::collections::HashMap::from([(first, false), (second.clone(), true)]);
        let hit = find_in_path_with("aplay", &dirs, |p| *table.get(p).unwrap_or(&false));
        assert_eq!(hit, Some(second), "第一个目录里那个不可执行的同名文件不该被选中");
        assert_eq!(find_in_path_with("nope", &dirs, |_| false), None, "找不到不能赌第一个目录");
    }

    #[test]
    fn idle_until_something_is_asked() {
        let w = AudioWatch::default();
        assert_eq!(w.phase(), AudioPhase::Idle);
        assert_eq!(w.states(), 0);
        // 没请求过就永远不会被判成「没回音」。
        assert!(!w.check_stalled(T0 + 60_000, AUDIO_ACK_GRACE_MS));
        assert!(w.summary(false).contains("本端没要声音"));
    }

    #[test]
    fn ack_within_grace_is_not_stalled_and_repeat_logs_once() {
        let w = AudioWatch::default();
        assert!(w.note_requested(T0));
        assert!(w.on_state(true, "opus/ogg 64k 48000Hz 2ch", "lavfi:sine").is_some());
        assert_eq!(w.phase(), AudioPhase::Running);
        // 已经回过话了，「没回音」这条判据就永远不该再触发。
        assert!(!w.check_stalled(T0 + AUDIO_ACK_GRACE_MS + 1, AUDIO_ACK_GRACE_MS));
        // 同一状态再来一条：不产生新日志，但计数要涨（证明消息确实到了这一层）。
        assert!(w.on_state(true, "", "").is_none());
        assert_eq!(w.states(), 2);
        assert_eq!(w.failures(), 0);
        assert!(w.summary(true).contains("正在推流"));
        assert!(w.summary(true).contains("opus/ogg"));
    }

    #[test]
    fn repeated_request_while_running_is_not_re_counted() {
        let w = AudioWatch::default();
        w.note_requested(T0);
        w.on_state(true, "opus/ogg", "pulse:monitor");
        // 第二条会话又发了一次 AudioStart：不该退回「等回话」。
        assert!(!w.note_requested(T0 + 8_000));
        assert_eq!(w.phase(), AudioPhase::Running);
    }

    #[test]
    fn silence_past_grace_is_reported_exactly_once() {
        let w = AudioWatch::default();
        w.note_requested(T0);
        assert!(!w.check_stalled(T0 + AUDIO_ACK_GRACE_MS - 1, AUDIO_ACK_GRACE_MS));
        assert!(w.check_stalled(T0 + AUDIO_ACK_GRACE_MS, AUDIO_ACK_GRACE_MS));
        assert!(!w.check_stalled(T0 + AUDIO_ACK_GRACE_MS * 3, AUDIO_ACK_GRACE_MS));
        assert_eq!(w.phase(), AudioPhase::Stalled);
        assert!(w.summary(false).contains("没回过音频状态"));
    }

    #[test]
    fn never_started_failure_keeps_the_reason() {
        let w = AudioWatch::default();
        w.note_requested(T0);
        let change = w.on_state(false, "", "不支持的音频源 \"alsa:plughw:0,0\"");
        assert_eq!(change, Some((AudioPhase::Requested, AudioPhase::Failed)));
        assert_eq!(w.failures(), 1);
        let s = w.summary(false);
        assert!(s.contains("起不来"), "{s}");
        assert!(s.contains("不支持的音频源"), "{s}");
    }

    #[test]
    fn streamed_then_ended_is_not_a_failure() {
        let w = AudioWatch::default();
        w.note_requested(T0);
        w.on_state(true, "opus/ogg", "pulse:monitor");
        w.on_state(false, "", "音频流结束（12 秒，96000 字节）");
        assert_eq!(w.phase(), AudioPhase::Ended);
        assert_eq!(w.failures(), 0, "正常收尾不能记成故障");
        // 收到过字节 vs 一字节没到，统计行上必须分得开。
        assert!(!w.summary(true).contains("一个字节都没收到"));
        assert!(w.summary(false).contains("一个字节都没收到"));
    }

    #[test]
    fn our_own_stop_ack_is_expected() {
        let w = AudioWatch::default();
        w.note_requested(T0);
        w.on_state(true, "opus/ogg", "lavfi:sine");
        w.note_stop_requested("本机音频解码器起不来：找不到 ffmpeg");
        assert_eq!(w.phase(), AudioPhase::Stopped);
        // 叫停刚发出去、对面还没回话：这只能算「本机意图」，不能算「对面停了」。
        assert_eq!(w.stop_acks(), 0);
        assert!(w.summary(false).contains("被控端还没确认"), "{}", w.summary(false));
        // 被控端回的那句 running:false 是确认，不该变红、也不该记失败。
        assert!(w.on_state(false, "", "控制端关闭了音频").is_none());
        assert_eq!(w.phase(), AudioPhase::Stopped);
        assert_eq!(w.failures(), 0);
        assert_eq!(w.states(), 2);
        assert_eq!(w.stop_acks(), 1, "确认条数才是 AudioStop 被处理的证据");
        assert!(w.summary(false).contains("本机已请求停止"));
        assert!(w.summary(false).contains("被控端已确认停止"));
        assert!(w.summary(false).contains("找不到 ffmpeg"));
    }

    /// `running:true` 不是「确认停止」—— 集成测试以前用 `states>=1` 当判据，
    /// 那样对面只回一句 running:true 也会算通过，等于没测。
    #[test]
    fn a_running_state_is_never_a_stop_ack() {
        let w = AudioWatch::default();
        w.note_requested(T0);
        w.note_stop_requested("本机接不住");
        w.on_state(true, "opus/ogg", "对面还在推");
        assert_eq!(w.stop_acks(), 0);
        assert_eq!(w.phase(), AudioPhase::Running, "对面还在推就该如实显示在推，别停在 Stopped 上自欺");
    }

    #[test]
    fn recovery_after_failure_is_reported() {
        let w = AudioWatch::default();
        w.note_requested(T0);
        w.on_state(false, "", "第一次探测失败");
        assert_eq!(w.phase(), AudioPhase::Failed);
        assert!(w.on_state(true, "opus/ogg", "换了源之后起来了").is_some());
        assert_eq!(w.phase(), AudioPhase::Running);
        assert_eq!(w.failures(), 1, "失败过一次要留痕");
    }

    #[test]
    fn second_session_requests_again() {
        // 断线接回之后是一条新会话：上一轮的原因不能把新一轮钉死。
        let w = AudioWatch::default();
        w.note_requested(T0);
        w.on_state(false, "", "上一轮的原因");
        assert!(w.note_requested(T0 + 4_000));
        assert_eq!(w.phase(), AudioPhase::Requested);
        assert!(w.summary(false).contains("等被控端回话"), "{}", w.summary(false));
    }

    #[test]
    fn unsupported_is_recorded_not_only_logged() {
        let w = AudioWatch::default();
        w.note_unsupported();
        assert_eq!(w.phase(), AudioPhase::Unsupported);
        assert!(w.summary(false).contains("没有音频能力"));
        // 对面没能力 ≠ 被控端故障。
        assert_eq!(w.failures(), 0);
        assert_eq!(w.states(), 0);
    }

    #[test]
    fn long_multiline_detail_stays_one_line_and_valid_utf8() {
        let w = AudioWatch::default();
        w.note_requested(T0);
        let noisy = " ffmpeg 报错\n".repeat(60);
        w.on_state(false, "", &noisy);
        let s = w.summary(false);
        assert!(!s.contains('\n'), "detail 里的换行会把统计行刷成一片");
        assert!(s.chars().count() < 300, "detail 没裁：{} 字符", s.chars().count());
        // 裁切按字符不按字节：中文按字节切正好落在多字节中间。
        assert!(s.ends_with('…'), "裁过的文本要留下痕迹：{s}");
    }

    #[test]
    fn local_failure_is_handed_over_once() {
        let w = AudioWatch::default();
        assert!(w.take_local_failure().is_none());
        w.note_local_failure("解码器启动失败：os error 2");
        w.note_local_failure("第二句不该盖掉第一句");
        assert_eq!(w.take_local_failure().as_deref(), Some("解码器启动失败：os error 2"));
        assert!(w.take_local_failure().is_none(), "同一件事只通知一次");
    }
}
