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

use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tracing::{debug, warn};

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
    /// 最近一个 AU 的被控端时间戳（微秒）。
    pub last_ts_us: AtomicU64,
    /// 解码流是否已结束。
    pub ended: AtomicBool,
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
    pub last_ts_us: u64,
    pub ended: bool,
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
            last_ts_us: self.last_ts_us.load(Ordering::Relaxed),
            ended: self.ended.load(Ordering::Relaxed),
        }
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
    tasks: Vec<tokio::task::JoinHandle<()>>,
    /// 解码器子进程的宿主。
    ///
    /// **必须由 watcher 持有，不能塞进任何后台任务里**：`VideoDecoder::drop`
    /// 会 kill 掉 ffmpeg。曾经把它当作「保活守卫」放进喂流任务，结果那个任务
    /// 一结束（喂完最后一帧、stdin 关闭）就把 ffmpeg 杀了 —— ffmpeg 还没来得及
    /// 把已解出的帧冲刷到 stdout。表现出来就是「喂入 20 个 AU、解出 0 帧」。
    _decoder: VideoDecoder,
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

        let mut decoder = VideoDecoder::start(&ffmpeg_path, info.w, info.h).await?;
        let (writer, mut reader) = decoder.split()?;
        let stats = Arc::new(WatchStats::default());
        let (latest_tx, latest_rx) = watch::channel::<Option<Frame>>(None);
        let started = Instant::now();

        // 网络 → 有界队列
        let (q_tx, q_rx) = mpsc::channel::<(u64, Vec<u8>)>(8);
        let stats_pump = stats.clone();
        let pump = tokio::spawn(async move {
            pump_video(video, q_tx, stats_pump).await;
        });

        // 队列 → ffmpeg stdin
        let stats_feed = stats.clone();
        let last_ts = Arc::new(AtomicU64::new(0));
        let last_ts_feed = last_ts.clone();
        let mut writer = writer;
        // 参数帧之前攒下的 AU 先喂进去
        for (ts, au) in pending {
            let _ = writer.feed(&au).await;
            last_ts_feed.store(ts, Ordering::Relaxed);
        }
        let feed = tokio::spawn(async move {
            let mut rx = q_rx;
            while let Some((ts, au)) = rx.recv().await {
                if let Err(e) = writer.feed(&au).await {
                    debug!(error = %e, "写解码器失败，喂流结束");
                    break;
                }
                last_ts_feed.store(ts, Ordering::Relaxed);
                stats_feed.fed.fetch_add(1, Ordering::Relaxed);
            }
            // writer 在这里 drop → 关掉 ffmpeg 的 stdin → 它会冲刷出剩余帧再退出。
            // 注意这里**不**持有 `VideoDecoder`：它的 Drop 会 kill 子进程，
            // 那会把还没冲刷出来的帧一起杀掉。宿主是 `VideoWatcher::_decoder`。
        });

        // ffmpeg stdout → watch（最新一帧）
        let stats_dec = stats.clone();
        let last_ts_dec = last_ts.clone();
        let (w, h) = (info.w, info.h);
        let decoded = tokio::spawn(async move {
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
                            width: w,
                            height: h,
                            ts_us: last_ts_dec.load(Ordering::Relaxed),
                            at: Instant::now(),
                        };
                        // watch 只保留最新值：渲染端天然「追不上就跳帧」
                        let _ = latest_tx.send(Some(frame));
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

        Ok(Self {
            info,
            rx: latest_rx,
            stats,
            started,
            tasks: vec![pump, feed, decoded],
            _decoder: decoder,
        })
    }

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

/// 从视频通道收帧，塞进有界队列。满了就丢 —— 宁可丢帧也不要拖延迟。
async fn pump_video(
    mut video: TcpStream,
    tx: mpsc::Sender<(u64, Vec<u8>)>,
    stats: Arc<WatchStats>,
) {
    loop {
        let f = match read_frame(&mut video).await {
            Ok(f) => f,
            Err(uulan_protocol::frame::FrameError::Io(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                debug!("视频通道被对端关闭");
                break;
            }
            Err(e) => {
                debug!(error = %e, "视频通道读取结束");
                break;
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
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                }
            }
            FrameKind::Ping => {
                let _ = write_frame(&mut video, FrameKind::Pong, &f.payload).await;
            }
            FrameKind::Bye => {
                debug!("被控端道别，视频通道结束");
                break;
            }
            // 画面参数变化（分辨率/帧率）到 M2.5 才处理；现在只记一条日志。
            FrameKind::Json => match f.decode_json::<H2C>() {
                Ok(H2C::VideoCfg(c)) => debug!(?c, "画面参数变化（当前实现不重建解码器）"),
                Ok(H2C::Bye { reason }) => {
                    debug!(%reason, "被控端道别，视频通道结束");
                    break;
                }
                Ok(_) => {}
                Err(e) => debug!(error = %e, "视频通道上收到无法解析的 JSON 帧"),
            },
            _ => {}
        }
    }
}
