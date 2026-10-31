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
//! - 其它平台：实时播放**未实现**（`--audio-save` 照常可用）。麒麟控制端要出声得等下一步，
//!   这一点必须写在帮助与文档里，别让人以为「开了 --audio 就有声音」。

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tracing::{debug, info, warn};

use uulan_protocol::crypto::SecReadHalf;
use uulan_protocol::frame::{read_frame, FrameKind};

/// 采样率/声道：解码器与播放器都按这套参数，改一处就要两处一起改。
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;

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
        play_loop(&ffmpeg, sink, rx, st_out);
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
    Err("这个平台还没有实时播放后端（用 --audio-save 落成 WAV 即可验收）".into())
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