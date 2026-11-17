//! 共享的实时播放后端（M7）：Ogg 字节流 → ffmpeg 解码 s16le PCM → 本机播放命令。
//!
//! 控制端播「被控端推来的声音」（`audioin`），被控端播「控制端送来的麦克风」
//! （host 的 `mic`）—— 两端做的是**同一件事**。2026-11-17 那晚在真麒麟上逐条实测
//! 出来的纪律全在这里，只留一份副本：
//!
//! - `paplay`（PulseAudio 13.99）吃不了 raw stdin，候选表里只有实测能用的 `aplay -t raw`；
//! - 收尾先关子进程 stdin 让播放器把尾巴放完，1500 ms 宽限才 kill（直接 kill = 切尾音）；
//! - 播放侧先死必须**先杀解码器**再 join 喂字节的线程，否则三方互等、整个进程挂死
//!   （反例见 docs/milestones/麒麟实时播放-真机-2026-11-17.txt）；
//! - 「播放 N 字节」只证明字节进了管道，两句「实时播放已接上 / 已收尾」才是可 grep 的证据。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;

use tracing::{info, warn};

/// 采样率/声道：解码器与播放器都按这套参数，改一处就要两处一起改。
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;

/// 收尾时给播放命令的宽限：先把缓冲里的尾巴放完，超时才 kill。
#[cfg(not(windows))]
const PIPE_FINISH_MS: u64 = 1500;

/// 一条播放链路的计数。控制端下行（audioin）与被控端上行（mic）共用同一套字段名，
/// 收工日志因此可以互相对照 —— 「送了多少 / 收了多少 / 播了多少」三段账要能接得上。
#[derive(Debug, Default)]
pub struct Counters {
    /// 从通道收到的 Ogg 字节数。
    pub ogg_bytes: AtomicU64,
    /// 收到的帧数。
    pub frames: AtomicU64,
    /// 解码出来的 PCM 字节数（s16le 立体声 48 kHz，192000 B/s）。
    pub pcm_bytes: AtomicU64,
    /// 播放后端写出去的字节数。
    pub played_bytes: AtomicU64,
    /// 解码/播放出错次数。
    pub errors: AtomicU64,
    /// 峰值电平（0..=32767 的绝对值最大值），用来区分「有流但全是静音」。
    pub peak: AtomicU64,
}

impl Counters {
    pub fn pcm_seconds(&self) -> f64 {
        self.pcm_bytes.load(Ordering::Relaxed) as f64 / (SAMPLE_RATE as f64 * CHANNELS as f64 * 2.0)
    }

    /// 到底收到过音频字节没有。用来把「对面推完了/结束了」与
    /// 「对面说在推、其实一个字节都没到」分开 —— 两者在统计行上不能长一个样。
    pub fn got_bytes(&self) -> bool {
        self.ogg_bytes.load(Ordering::Relaxed) > 0
    }

    pub fn add_frame(&self, bytes: usize) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.ogg_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    fn bump_errors(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    /// 一行给人看的总结（措辞与 grep 规则自 2026-11-15 起不变）。
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

/// 一个 PCM 去处。实现者自己保证「写入阻塞 = 反压」（能等就别丢字节）。
pub trait PcmSink: Send {
    fn write(&mut self, pcm: &[u8]) -> std::io::Result<()>;
    fn finish(&mut self) {}
}

/// 不出声的后端：没有声卡 / 验收台专用。解码链路照跑（统计仍然有意义）。
pub struct NullSink;

impl PcmSink for NullSink {
    fn write(&mut self, _pcm: &[u8]) -> std::io::Result<()> {
        Ok(())
    }
}

/// ffmpeg 解码参数：Ogg → s16le 裸 PCM。两端共用，只有这一份。
pub fn decode_args() -> Vec<String> {
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

/// 一个实时播放候选：命令名 + 完整参数（参数里已经钉死采样格式）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerCandidate {
    pub program: &'static str,
    pub args: Vec<String>,
}

/// 候选表。**只列实测过能吃 raw stdin 的命令**：`paplay` 在真麒麟上明确不行（见模块头），
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
    is_exe: impl Fn(&Path) -> bool,
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
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
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
    fn open(cand: &PlayerCandidate, program_path: &Path) -> Result<Self, String> {
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
impl PcmSink for PipeOut {
    fn write(&mut self, pcm: &[u8]) -> std::io::Result<()> {
        let Some(s) = self.stdin.as_mut() else {
            return Err(std::io::Error::other("播放后端已经关掉了"));
        };
        match s.write_all(pcm) {
            Ok(()) => Ok(()),
            Err(e) => {
                // io 错误本身只会说 broken pipe —— 现场要看的是子进程到底是死了还是还活着。
                let why = match self.child.try_wait() {
                    Ok(Some(st)) => format!("播放进程已退出（{st}）"),
                    Ok(None) => "播放进程仍在跑".to_string(),
                    Err(q) => format!("问不到播放进程的状态（{q}）"),
                };
                Err(std::io::Error::other(format!("{e}；{why}")))
            }
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

/// 找一个能用的播放命令并拉起来。找不到时错误里列出**每一条为什么不行**
/// （现场分不清「没装 aplay」和「装了但设备被占」）。
#[cfg(not(windows))]
pub fn pipe_speaker() -> Result<Box<dyn PcmSink>, String> {
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

/// Windows：控制端自己的实时播放走 winmm `waveOut`（在 `uulan-client` 里，那是
/// 只有控制端会站到的位置）；共享层在 Windows 上给不出播放后端 —— 被控端目前
/// 只有麒麟/Linux，这里如实报错，别装作播了。
#[cfg(windows)]
pub fn pipe_speaker() -> Result<Box<dyn PcmSink>, String> {
    Err("这个平台的共享播放后端还没实现（控制端在 Windows 用 waveOut，与被控端无关）".into())
}

/// `pump` 的结局。每一端把它接成自己那句「为什么结束了」。
#[derive(Debug, Clone)]
pub enum PumpEnd {
    /// 上游字节流结束（对端停了 / 写半端关了），解码器把尾巴冲刷完了。
    Ended,
    /// ffmpeg 解码器起不来。
    DecoderGone(String),
    /// 播放后端半路坏了（aplay 退了）。PCM 不再往下送。
    SinkGone(String),
}

/// 阻塞侧主循环：`rx`（Ogg 分块）→ ffmpeg 解码 → `sink`（PCM）。
///
/// 喂字节的线程由 `rx` 承担反压（`sync_channel` 有界 → 满了阻塞异步侧）；
/// `on_ready` 在「解码器起来了」的那一刻被调用一次 —— 被控端用它回
/// `MicState{running:true}`，让对面知道播放在本机真的落地了。
///
/// `on_end` 在**每一条返回路径上、真正 return 之前**被调用（含 DecoderGone 的早退）。
/// 为什么必须有它：调用方那个 async 任务可能已经被 `abort`（控制端整个进程先退出是
/// 常态），而 `spawn_blocking` 的这里**杀不到** —— 收尾的回话与账目只有落在这个
/// 线程里才保证说得出口。2026-11-18 真机 night 批就是「玩家释放了、账却没落」暴露的。
pub fn pump(
    ffmpeg: &Path,
    rx: Receiver<Vec<u8>>,
    mut sink: Box<dyn PcmSink>,
    stats: &Arc<Counters>,
    on_ready: Box<dyn FnOnce() + Send>,
    on_end: Box<dyn FnOnce(&PumpEnd, &Arc<Counters>) + Send>,
) -> PumpEnd {
    let mut child = match std::process::Command::new(ffmpeg)
        .args(decode_args())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let why = format!("{e}");
            warn!(error = %why, "音频解码器启动失败");
            stats.bump_errors();
            let end = PumpEnd::DecoderGone(why);
            on_end(&end, stats);
            return end;
        }
    };
    let Some(mut din) = child.stdin.take() else {
        let end = PumpEnd::DecoderGone("拿不到 ffmpeg 的 stdin".into());
        on_end(&end, stats);
        return end;
    };
    let Some(mut dout) = child.stdout.take() else {
        let end = PumpEnd::DecoderGone("拿不到 ffmpeg 的 stdout".into());
        on_end(&end, stats);
        return end;
    };
    on_ready();

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
    // died_early：播放后端坏了或读 PCM 出错 —— 与「解码器把这一路放完了」是两回事，
    // 收尾动作不同（见下面 kill 那段）。
    let mut early_err: Option<String> = None;
    let mut pcm = vec![0u8; 8192];
    loop {
        let n = match dout.read(&mut pcm) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                early_err = Some(format!("读解码器输出失败：{e}"));
                break;
            }
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
        if let Err(e) = sink.write(&pcm[..n]) {
            stats.bump_errors();
            // 原因必须落日志：只加计数就 break，现场分不清是播放器退了、管道被抢、还是声卡没了
            // （与剪贴板那条教训同族：「少了 N 个」要能说清是谁拒的）。
            warn!(error = %e, "播放后端写入失败，本机出声到此中断（解码与统计照常收尾）");
            early_err = Some(format!("播放后端写入失败：{e}"));
            break;
        }
        stats.played_bytes.fetch_add(n as u64, Ordering::Relaxed);
    }

    // 播放侧提前死了必须先杀解码器，否则这里会**永久挂住**：ffmpeg 卡在写 stdout（没人读了），
    // 喂字节的线程卡在写它的 stdin，`feeder.join()` 就永远不返回；而 spawn_blocking 的任务不结束，
    // tokio 运行时的收尾会一直等它 —— 表现是整个进程不退出、统计一个字都没打出来。
    // 2026-11-17 夜批用「ALSA 配置指坏 → aplay 当场退出」这个反例在真麒麟上抓到过这条。
    let ended_early = early_err.is_some();
    if ended_early {
        let _ = child.kill();
    }
    let _ = feeder.join();
    sink.finish();
    let _ = child.wait();
    info!(pcm = stats.pcm_bytes.load(Ordering::Relaxed), "音频解码结束");
    let end = match early_err {
        Some(why) => PumpEnd::SinkGone(why),
        None => PumpEnd::Ended,
    };
    on_end(&end, stats);
    end
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// 解码输出的格式与播放命令要的格式共用同一组常量 —— 这条钉的是「别改一端」。
    #[test]
    fn decode_args_emit_the_same_format_the_player_wants() {
        let a = decode_args();
        let val = |flag: &str| a.iter().position(|s| s == flag).map(|i| a[i + 1].clone());
        assert_eq!(val("-f").as_deref(), Some("ogg"), "输入必须是 ogg");
        assert_eq!(a.iter().filter(|s| *s == "-f").count(), 2, "输入输出各钉一次 -f");
        assert_eq!(val("-ar").as_deref(), Some(SAMPLE_RATE.to_string()).as_deref());
        assert_eq!(val("-ac").as_deref(), Some(CHANNELS.to_string()).as_deref());
        assert_eq!(a.last().map(|s| s.as_str()), Some("pipe:1"));
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
    fn counters_summary_keeps_the_established_wording() {
        // 2026-11-15 起脚本 grep 的就是这句话，措辞变了验收会假红。
        let c = Counters::default();
        c.add_frame(100);
        let s = c.summary();
        assert!(s.contains("音频：收到 1 帧 / 100 字节"), "{s}");
        assert!(s.contains("峰值 0"), "默认没有峰值：{s}");
        assert!(c.got_bytes(), "收到过 100 字节就该说收到过");
    }
}
