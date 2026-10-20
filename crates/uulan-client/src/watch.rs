//! 控制端的视频接收与解码。
//!
//! 三条并发的流水线，各自职责单一：
//!
//! ```text
//! 网络 socket ──(pump)──> 有界队列 ──(feed)──> ffmpeg stdin
//!                                               ffmpeg stdout ──(decoded)──> watch(最新一帧)
//! ```
//!
//! 两个刻意的设计：
//!
//! 1. **入队用 `try_send`，满了就丢**。远程桌面要的是「最新画面」，不是「每一帧」。
//!    解码跟不上时宁可丢掉旧帧，也不能让队列无限增长把内存吃光、把延迟越拖越长。
//! 2. **出图用 `watch` 通道**（只保留最新值）。渲染端永远拿最新的一帧，
//!    天然做到「追不上就跳帧」，不需要任何丢弃逻辑。
//!
//! 这套结构 headless 也能跑 —— 所以整条控制端链路在没有显示器、没有窗口系统的机器上
//! 依然可以端到端验证。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

use uulan_media::annexb;
use uulan_media::ffmpeg;
use uulan_media::VideoDecoder;
use uulan_protocol::frame::{read_frame, video, write_frame, FrameKind};
use uulan_protocol::msg::{H2C, HostVideoCfg};

#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error("协议: {0}")]
    Frame(#[from] uulan_protocol::frame::FrameError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("媒体: {0}")]
    Media(#[from] uulan_media::MediaError),
    #[error("被控端在报送画面参数前就断开了")]
    NoVideoCfg,
    #[error("被控端拒绝推流：{0}")]
    HostRefused(String),
}

/// 累计统计。用原子量，让三个任务不必互相加锁。
#[derive(Debug, Default)]
pub struct WatchStats {
    /// 从网络收到的访问单元数。
    pub aus: AtomicU64,
    /// 其中关键帧数。
    pub keyframes: AtomicU64,
    /// 收到的压缩字节数。
    pub bytes: AtomicU64,
    /// 因解码跟不上而丢掉的 AU 数。
    pub dropped: AtomicU64,
    /// 实际喂进解码器的 AU 数。
    pub fed: AtomicU64,
    /// 已解出的完整帧数。
    pub decoded: AtomicU64,
    /// 从会话开始到第一帧画面的毫秒数。
    pub first_frame_ms: AtomicU64,
    /// 被控端上报的**实测编码帧率 ×10**（0 = 还没测出来）。
    ///
    /// 为什么要 ×10 存整数：这是跨线程共享的原子量，浮点没法原子地存。
    /// 为什么不数控制端自己解出多少帧：解码端丢帧/跳帧都会让它偏离真实编码帧率，
    /// 用户想知道的是「对面到底产了多少帧」。
    pub host_fps_x10: AtomicU64,
    /// 最近一个 AU 的被控端时间戳（微秒）。
    pub last_ts_us: AtomicU64,
    /// 解码流是否已结束。
    pub ended: AtomicBool,
    /// 因为被控端换分辨率而重建解码器的次数。
    pub resizes: AtomicU64,
    /// 最新一条 `VideoCfg` 报的画幅宽 / 高（换过分辨率之后就是新值）。
    pub cfg_w: AtomicU64,
    pub cfg_h: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WatchStatsSnapshot {
    pub aus: u64,
    pub keyframes: u64,
    pub bytes: u64,
    pub dropped: u64,
    pub fed: u64,
    pub decoded: u64,
    pub first_frame_ms: u64,
    pub host_fps_x10: u64,
    pub last_ts_us: u64,
    pub ended: bool,
    pub resizes: u64,
    pub cfg_w: u32,
    pub cfg_h: u32,
}

impl WatchStats {
    pub fn snapshot(&self) -> WatchStatsSnapshot {
        WatchStatsSnapshot {
            aus: self.aus.load(Ordering::Relaxed),
            keyframes: self.keyframes.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            fed: self.fed.load(Ordering::Relaxed),
            decoded: self.decoded.load(Ordering::Relaxed),
            first_frame_ms: self.first_frame_ms.load(Ordering::Relaxed),
            host_fps_x10: self.host_fps_x10.load(Ordering::Relaxed),
            last_ts_us: self.last_ts_us.load(Ordering::Relaxed),
            ended: self.ended.load(Ordering::Relaxed),
            resizes: self.resizes.load(Ordering::Relaxed),
            cfg_w: self.cfg_w.load(Ordering::Relaxed) as u32,
            cfg_h: self.cfg_h.load(Ordering::Relaxed) as u32,
        }
    }

    /// 被控端**当前**画幅（最新 `VideoCfg` 报的）。换过分辨率后就是新值。
    pub fn size(&self) -> (u32, u32) {
        (
            self.cfg_w.load(Ordering::Relaxed) as u32,
            self.cfg_h.load(Ordering::Relaxed) as u32,
        )
    }

    /// 被控端上报的实测编码帧率（fps）。0 表示「还没测出来」。
    pub fn host_fps(&self) -> f32 {
        self.host_fps_x10.load(Ordering::Relaxed) as f32 / 10.0
    }

    /// 平均码率（kbps）。`elapsed` 是从会话开始到现在的时长。
    pub fn bitrate_kbps(&self, elapsed: Duration) -> f64 {
        let s = elapsed.as_secs_f64();
        if s <= 0.0 {
            return 0.0;
        }
        (self.bytes.load(Ordering::Relaxed) as f64 * 8.0) / s / 1000.0
    }
}

/// 收下一条 `VideoCfg`：更新实测帧率与当前画幅；返回「分辨率变了」。
///
/// 必须分得清这两种情况：**只报帧率**是每 2 秒一次的常规消息，绝不能因此重建解码器；
/// 分辨率变了才需要重建（返回 `true`，调用方据此重开 ffmpeg 解码进程）。
pub fn apply_video_cfg(stats: &WatchStats, c: &HostVideoCfg, expect: (u32, u32)) -> bool {
    let x10 = (c.actual_fps.max(0.0) * 10.0).round() as u64;
    stats.host_fps_x10.store(x10, Ordering::Relaxed);
    stats.cfg_w.store(c.w as u64, Ordering::Relaxed);
    stats.cfg_h.store(c.h as u64, Ordering::Relaxed);
    c.w != expect.0 || c.h != expect.1
}

/// 一帧已解码的 BGRA 画面。
#[derive(Debug, Clone)]
pub struct Frame {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// 被控端给这个 AU 打的时间戳（微秒）。
    pub ts_us: u64,
    /// 解码器产出这一帧的本地时刻。
    pub at: Instant,
}

pub struct VideoWatcher {
    info: HostVideoCfg,
    rx: watch::Receiver<Option<Frame>>,
    stats: Arc<WatchStats>,
    started: Instant,
    /// 收帧/喂流/重建解码器的总管任务句柄。
    ///
    /// **解码器子进程由这个任务持有，不再由 watcher 直接持有**：换分辨率时要先杀掉
    /// 旧解码器再起新的，而「同一时刻只该有一个解码器活着」这条约束放在一个任务里
    /// 最容易保证。`Drop` 会 abort 它，解码器随之被 kill（`VideoDecoder::drop`）。
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl VideoWatcher {
    /// 等被控端报出画面参数，然后拉起整条解码流水线。
    ///
    /// `ffmpeg` 为 `None` 时走 PATH。
    pub async fn start(
        mut video: TcpStream,
        ffmpeg: Option<PathBuf>,
    ) -> Result<Self, WatchError> {
        // 先同步等到 VideoCfg —— 没有它就不知道解码输出的帧尺寸。
        let mut pending: Vec<(u64, Vec<u8>)> = Vec::new();
        let info = loop {
            let f = read_frame(&mut video).await?;
            debug!(kind = ?f.kind, len = f.payload.len(), "视频通道收到帧（等画面参数）");
            match f.kind {
                // 画面参数是以 Json 帧发过来的（payload 里的 type 是 video_cfg）。
                // 早先这里匹配的是 FrameKind::VideoCfg（0x03），那个帧型根本不存在 ——
                // 于是这一帧被静默忽略，客户端一直等到 EOF。
                FrameKind::Json => {
                    let m: H2C = f.decode_json()?;
                    match m {
                        H2C::VideoCfg(c) => {
                            debug!(?c, "收到画面参数");
                            break c;
                        }
                        H2C::Bye { reason } => return Err(WatchError::HostRefused(reason)),
                        other => {
                            debug!(?other, "视频通道上的其他控制消息，忽略");
                        }
                    }
                }
                FrameKind::VideoAu => {
                    // 罕见但合法：被控端可能在参数帧之前就推了一帧。先攒着。
                    if let Some((ts, au)) = video::unpack_au(&f.payload) {
                        pending.push((ts, au.to_vec()));
                    }
                }
                FrameKind::Ping => {
                    let _ = write_frame(&mut video, FrameKind::Pong, &f.payload).await;
                }
                FrameKind::Bye => {
                    // 把被控端给的原因原样带出去。这样用户看到的是
                    // 「被控端无法开始采集：xxx」而不是一个光秃秃的 early eof。
                    let reason = f
                        .decode_json::<H2C>()
                        .ok()
                        .and_then(|m| match m {
                            H2C::Bye { reason } => Some(reason),
                            _ => None,
                        })
                        .unwrap_or_else(|| "（对端未说明原因）".to_string());
                    return Err(WatchError::HostRefused(reason));
                }
                _ => {}
            }
        };

        if info.w == 0 || info.h == 0 {
            return Err(WatchError::NoVideoCfg);
        }

        let ffmpeg_path = ffmpeg::find_ffmpeg(ffmpeg.as_deref()).ok_or_else(|| {
            WatchError::Media(uulan_media::MediaError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "找不到 ffmpeg（控制端靠它解码画面）",
            )))
        })?;

        // 第一代解码器在这里起：起不来就当场返回错误，别让调用方拿到一个永远不出帧的 watcher。
        let first = VideoDecoder::start(&ffmpeg_path, info.w, info.h).await?;
        let stats = Arc::new(WatchStats::default());
        // 首帧的 VideoCfg 里也带实测帧率（刚开播通常是 0，编码器重启时可能已经有值）。
        apply_video_cfg(&stats, &info, (info.w, info.h));
        let (latest_tx, latest_rx) = watch::channel::<Option<Frame>>(None);
        let started = Instant::now();

        let (read_half, write_half) = video.into_split();
        let sup = tokio::spawn(supervise(
            read_half,
            write_half,
            info.clone(),
            pending,
            ffmpeg_path,
            first,
            stats.clone(),
            latest_tx,
            started,
        ));

        Ok(Self {
            info,
            rx: latest_rx,
            stats,
            started,
            tasks: vec![sup],
        })
    }

    /// 首帧的画面参数。**分辨率可能随后变化**，当前值看 `stats().size()`。
    pub fn info(&self) -> &HostVideoCfg {
        &self.info
    }

    pub fn stats(&self) -> Arc<WatchStats> {
        self.stats.clone()
    }

    pub fn snapshot(&self) -> WatchStatsSnapshot {
        self.stats.snapshot()
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// 订阅最新帧。返回的接收端可以直接用在窗口渲染循环里。
    pub fn subscribe(&self) -> watch::Receiver<Option<Frame>> {
        self.rx.clone()
    }

    /// 等下一帧（headless 用）。返回 `None` 表示流水线已结束且没有新帧。
    pub async fn next_frame(&mut self) -> Option<Frame> {
        loop {
            if self.rx.changed().await.is_err() {
                return None;
            }
            let v = self.rx.borrow_and_update().clone();
            if let Some(f) = v {
                return Some(f);
            }
        }
    }

    /// 结束所有后台任务。
    pub fn shutdown(&mut self) {
        for t in self.tasks.drain(..) {
            t.abort();
        }
    }
}

impl Drop for VideoWatcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// 一代收帧任务的结局。
enum PumpEnd {
    /// 被控端换了画幅：socket 交回来，由总管重建解码器。
    Resize(HostVideoCfg),
    /// 通道关闭（Bye / EOF / 读错误）。
    Closed,
}

/// 总管：一代解码流水线跑完 → 若要换分辨率，杀旧解码器、起新解码器，接着跑。
///
/// ```text
/// loop {
///   ffmpeg 解码器(本代尺寸) ← feed ← 有界队列 ← pump(socket 读半)
///                     └→ watch(最新一帧)
///   pump 报「分辨率变了」→ kill 解码器 → 下一代（socket 两个半端原样交回）
/// }
/// ```
///
/// 为什么要分代而不是「一个解码器吃到死」：ffmpeg 的 rawvideo 输出尺寸在进程启动时
/// 就定死了（`-video_size` + 每帧字节数），被控端换了分辨率之后继续喂，读出来的
/// 就是错位的字节流 —— 画出来是一堆斜条纹，而且永远不会自愈。
async fn supervise(
    r: OwnedReadHalf,
    w: OwnedWriteHalf,
    mut info: HostVideoCfg,
    mut pending: Vec<(u64, Vec<u8>)>,
    ffmpeg: PathBuf,
    first: VideoDecoder,
    stats: Arc<WatchStats>,
    latest_tx: watch::Sender<Option<Frame>>,
    started: Instant,
) {
    let mut reuse = Some(first);
    // socket 的两个半端在换代之间交接：pump 任务结束后把所有权带回来。
    let mut halves = Some((r, w));
    loop {
        let (fw, fh) = (info.w, info.h);
        let mut decoder = match reuse.take() {
            Some(d) => d,
            None => match VideoDecoder::start(&ffmpeg, fw, fh).await {
                Ok(d) => d,
                Err(e) => {
                    // 新尺寸的解码器起不来：留着旧尺寸继续解码只会画出错位画面，
                    // 所以老实收尾，让上层看到「视频结束」而不是一屏花屏。
                    warn!(error = %e, w = fw, h = fh, "按新分辨率启动解码器失败，视频结束");
                    stats.ended.store(true, Ordering::Relaxed);
                    return;
                }
            },
        };
        let (mut writer, mut reader) = match decoder.split() {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "解码器两端取不出来（已经 split 过？），视频结束");
                stats.ended.store(true, Ordering::Relaxed);
                return;
            }
        };

        // 网络 → 有界队列
        let (q_tx, mut q_rx) = mpsc::channel::<(u64, Vec<u8>)>(8);

        // 参数帧之前攒下的 AU 先喂进去
        for (ts, au) in pending.drain(..) {
            if writer.feed(&au).await.is_err() {
                break;
            }
            stats.fed.fetch_add(1, Ordering::Relaxed);
            stats.last_ts_us.store(ts, Ordering::Relaxed);
        }

        // 队列 → ffmpeg stdin
        let stats_feed = stats.clone();
        let feed = tokio::spawn(async move {
            while let Some((ts, au)) = q_rx.recv().await {
                if let Err(e) = writer.feed(&au).await {
                    debug!(error = %e, "写解码器失败，喂流结束");
                    break;
                }
                stats_feed.last_ts_us.store(ts, Ordering::Relaxed);
                stats_feed.fed.fetch_add(1, Ordering::Relaxed);
            }
            // writer 在这里 drop → 关掉 ffmpeg 的 stdin → 它会冲刷出剩余帧再退出。
            // **不**持有解码器本体：Drop 会 kill 子进程，把还没冲刷的帧一起杀掉。
        });

        // ffmpeg stdout → watch（最新一帧）
        let stats_dec = stats.clone();
        let frame_tx = latest_tx.clone();
        let mut decoded = tokio::spawn(async move {
            let mut buf: Vec<u8> = Vec::new();
            loop {
                match reader.next_frame_into(&mut buf).await {
                    Ok(true) => {
                        let n = stats_dec.decoded.fetch_add(1, Ordering::Relaxed);
                        if n == 0 {
                            stats_dec
                                .first_frame_ms
                                .store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                        }
                        let frame = Frame {
                            pixels: buf.clone(),
                            width: fw,
                            height: fh,
                            ts_us: stats_dec.last_ts_us.load(Ordering::Relaxed),
                            at: Instant::now(),
                        };
                        // watch 只保留最新值：渲染端天然「追不上就跳帧」
                        let _ = frame_tx.send(Some(frame));
                    }
                    Ok(false) => break,
                    Err(e) => {
                        debug!(error = %e, "解码读取结束");
                        break;
                    }
                }
            }
            stats_dec.ended.store(true, Ordering::Relaxed);
        });

        let (r0, w0) = halves.take().expect("socket 两半应当在每一代开头交回");
        let mut pump = tokio::spawn(pump_generation(r0, w0, q_tx, stats.clone(), (fw, fh)));

        let end = tokio::select! {
            j = &mut pump => match j {
                Ok((nr, nw, end)) => {
                    halves = Some((nr, nw));
                    end
                }
                Err(e) => {
                    debug!(error = %e, "收帧任务异常退出");
                    PumpEnd::Closed
                }
            },
            // 解码先停（码流断了 / ffmpeg 崩了）：没有新帧可放，换尺寸也救不回来。
            _ = &mut decoded => {
                debug!("解码流水线先结束，视频通道收尾");
                pump.abort();
                stats.ended.store(true, Ordering::Relaxed);
                return;
            }
        };

        match end {
            PumpEnd::Resize(cfg) => {
                // 旧的一代整个丢掉：旧解码器的输出尺寸已经不对，留着只会出斜条纹。
                feed.abort();
                decoded.abort();
                let _ = decoder.kill().await;
                stats.resizes.fetch_add(1, Ordering::Relaxed);
                info!(w = cfg.w, h = cfg.h, from_w = fw, from_h = fh, "被控端分辨率变了，解码器已重建");
                info = cfg;
                stats.ended.store(false, Ordering::Relaxed);
                // 新码流以关键帧开头（被控端重启编码源必然先出 IDR），不需要再补旧帧
                continue;
            }
            PumpEnd::Closed => {
                // 让 ffmpeg 把已解出的帧冲刷出来再收工：不 abort feed ——
                // 它会在队列排空、stdin 关闭后自然退出。
                let _ = tokio::time::timeout(Duration::from_secs(2), &mut decoded).await;
                feed.abort();
                let _ = decoder.kill().await;
                stats.ended.store(true, Ordering::Relaxed);
                return;
            }
        }
    }
}

/// 收一代的帧：socket 读半 → 有界队列。满了就丢 —— 宁可丢帧也不要拖延迟。
///
/// **返回值把 socket 的两个半端原样交回去**：换分辨率要重建解码器，但 TCP 连接
/// 不能断，所以读半端必须能从一个 pump 交到下一个 pump（`JoinHandle` 的返回值
/// 正好就是这条交接通道；用 `Arc<Mutex<TcpStream>>` 反而会把并发读写成串行）。
async fn pump_generation(
    mut r: OwnedReadHalf,
    mut w: OwnedWriteHalf,
    tx: mpsc::Sender<(u64, Vec<u8>)>,
    stats: Arc<WatchStats>,
    size: (u32, u32),
) -> (OwnedReadHalf, OwnedWriteHalf, PumpEnd) {
    let end = loop {
        let f = match read_frame(&mut r).await {
            Ok(f) => f,
            Err(uulan_protocol::frame::FrameError::Io(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                debug!("视频通道被对端关闭");
                break PumpEnd::Closed;
            }
            Err(e) => {
                debug!(error = %e, "视频通道读取结束");
                break PumpEnd::Closed;
            }
        };

        match f.kind {
            FrameKind::VideoAu => {
                let Some((ts, au)) = video::unpack_au(&f.payload) else {
                    warn!("收到长度不足的视频帧，丢弃");
                    continue;
                };
                stats.aus.fetch_add(1, Ordering::Relaxed);
                stats.bytes.fetch_add(f.payload.len() as u64, Ordering::Relaxed);
                if annexb::is_keyframe_au(au) {
                    stats.keyframes.fetch_add(1, Ordering::Relaxed);
                }
                stats.last_ts_us.store(ts, Ordering::Relaxed);
                match tx.try_send((ts, au.to_vec())) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        // 解码跟不上：丢掉这一帧。远程桌面画面上「跳一帧」无感，
                        // 但排队会让延迟一路涨上去，那才是致命的。
                        stats.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => break PumpEnd::Closed,
                }
            }
            FrameKind::Ping => {
                let _ = write_frame(&mut w, FrameKind::Pong, &f.payload).await;
            }
            FrameKind::Bye => {
                debug!("被控端道别，视频通道结束");
                break PumpEnd::Closed;
            }
            // 画面参数/实测帧率的周期性上报。只改帧率时**绝不能**动解码器。
            FrameKind::Json => match f.decode_json::<H2C>() {
                Ok(H2C::VideoCfg(c)) => {
                    if apply_video_cfg(&stats, &c, size) {
                        break PumpEnd::Resize(c);
                    }
                }
                Ok(H2C::Bye { reason }) => {
                    debug!(%reason, "被控端道别，视频通道结束");
                    break PumpEnd::Closed;
                }
                Ok(_) => {}
                Err(e) => debug!(error = %e, "视频通道上收到无法解析的 JSON 帧"),
            },
            _ => {}
        }
    };
    (r, w, end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(w: u32, h: u32, fps: f32) -> HostVideoCfg {
        HostVideoCfg {
            w,
            h,
            codec: "h264".into(),
            fps: 30,
            encoder: "libx264".into(),
            chroma: "420".into(),
            actual_fps: fps,
        }
    }

    #[test]
    fn video_cfg_updates_fps_without_flagging_a_resize() {
        let s = WatchStats::default();
        // 每 2 秒一次的常规上报：只带帧率，分辨率没变 —— 绝不能因此重建解码器
        assert!(!apply_video_cfg(&s, &cfg(1280, 720, 29.6), (1280, 720)));
        assert!((s.host_fps() - 29.6).abs() < 0.01, "{}", s.host_fps());
        assert_eq!(s.snapshot().host_fps_x10, 296);

        // 0 表示「还没测出来」，不是「测得是 0」
        assert!(!apply_video_cfg(&s, &cfg(1280, 720, 0.0), (1280, 720)));
        assert_eq!(s.host_fps(), 0.0);
    }

    #[test]
    fn video_cfg_flags_a_real_resolution_change() {
        let s = WatchStats::default();
        assert!(apply_video_cfg(&s, &cfg(1920, 1080, 30.0), (1280, 720)));
        assert!(!apply_video_cfg(&s, &cfg(1920, 1080, 30.0), (1920, 1080)));
    }

    #[test]
    fn negative_fps_is_clamped_not_wrapped() {
        // 陌生对端发负数：取绝对值会算出天文数字，必须夹到 0
        let s = WatchStats::default();
        apply_video_cfg(&s, &cfg(100, 100, -5.0), (100, 100));
        assert_eq!(s.host_fps(), 0.0);
    }
}
