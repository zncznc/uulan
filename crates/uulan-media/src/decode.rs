//! 解码侧：H.264 访问单元 → BGRA 原始帧。
//!
//! 启用 `builtin-h264` 特性时（Windows 控制端默认开启），使用进程内 OpenH264 解码，
//! 不依赖任何外部 `ffmpeg` 可执行文件；未启用时回退到外部 `ffmpeg` 子进程解码
//! （Linux 离线包已同目录自带静态 `ffmpeg`）。
//!
//! 解码器是**读写两半可以拆开**的：真实控制端里，收包的异步任务往写端喂 AU，
//! 渲染线程从读端取帧，两者必须并发跑 —— 串行的话通道一满就死锁。

#[cfg(feature = "builtin-h264")]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread::JoinHandle;

    use openh264::decoder::{DecodedYUV, Decoder};
    use openh264::formats::YUVSource;
    use tokio::sync::mpsc;

    use crate::ffmpeg;
    use crate::MediaError;

    const AU_QUEUE_CAP: usize = 16;
    const FRAME_QUEUE_CAP: usize = 4;

    pub struct DecoderWriter {
        tx: mpsc::Sender<Vec<u8>>,
        fed: u64,
        fed_bytes: u64,
    }

    impl DecoderWriter {
        pub async fn feed(&mut self, au: &[u8]) -> Result<(), MediaError> {
            self.tx.send(au.to_vec()).await.map_err(|_| MediaError::Exited)?;
            self.fed += 1;
            self.fed_bytes += au.len() as u64;
            Ok(())
        }

        pub fn fed(&self) -> u64 {
            self.fed
        }

        pub fn fed_bytes(&self) -> u64 {
            self.fed_bytes
        }
    }

    pub struct DecoderReader {
        rx: mpsc::Receiver<Vec<u8>>,
        width: u32,
        height: u32,
        frame_bytes: usize,
        eof: bool,
        decoded: u64,
        truncated: u64,
    }

    impl DecoderReader {
        pub fn dimensions(&self) -> (u32, u32) {
            (self.width, self.height)
        }

        pub fn frame_bytes(&self) -> usize {
            self.frame_bytes
        }

        pub fn decoded(&self) -> u64 {
            self.decoded
        }

        pub fn truncated(&self) -> u64 {
            self.truncated
        }

        pub fn is_eof(&self) -> bool {
            self.eof
        }

        pub async fn next_frame_into(&mut self, out: &mut Vec<u8>) -> Result<bool, MediaError> {
            if self.eof {
                return Ok(false);
            }
            match self.rx.recv().await {
                Some(frame) => {
                    if frame.len() != self.frame_bytes {
                        self.truncated += 1;
                        tracing::warn!(
                            target: "lanlink::decode",
                            "解码帧字节数不符（{}/{}），跳过",
                            frame.len(),
                            self.frame_bytes
                        );
                        out.clear();
                        return Ok(false);
                    }
                    *out = frame;
                    self.decoded += 1;
                    Ok(true)
                }
                None => {
                    self.eof = true;
                    out.clear();
                    Ok(false)
                }
            }
        }
    }

    fn yuv_to_bgra(yuv: &DecodedYUV<'_>, expect_w: u32, expect_h: u32) -> Option<Vec<u8>> {
        let (w, h) = yuv.dimensions();
        if w != expect_w as usize || h != expect_h as usize {
            tracing::debug!(
                target: "lanlink::decode",
                got_w = w,
                got_h = h,
                expect_w,
                expect_h,
                "解码帧尺寸与当前期望不一致，忽略该帧"
            );
            return None;
        }
        let mut bgra = vec![0u8; w * h * 4];
        yuv.write_rgba8(&mut bgra);
        for px in bgra.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        Some(bgra)
    }

    pub struct VideoDecoder {
        stop: Arc<AtomicBool>,
        worker: Option<JoinHandle<()>>,
        writer: Option<DecoderWriter>,
        reader: Option<DecoderReader>,
    }

    impl VideoDecoder {
        pub async fn start_auto(
            _ffmpeg: Option<&std::path::Path>,
            width: u32,
            height: u32,
        ) -> Result<Self, MediaError> {
            let mut decoder =
                Decoder::new().map_err(|e| MediaError::Decoder(format!("初始化失败: {e}")))?;

            let (au_tx, mut au_rx) = mpsc::channel::<Vec<u8>>(AU_QUEUE_CAP);
            let (frame_tx, frame_rx) = mpsc::channel::<Vec<u8>>(FRAME_QUEUE_CAP);
            let stop = Arc::new(AtomicBool::new(false));
            let stop_bg = stop.clone();

            let worker = std::thread::Builder::new()
                .name("lanlink-h264-dec".to_string())
                .spawn(move || {
                    while let Some(au) = au_rx.blocking_recv() {
                        if stop_bg.load(Ordering::Relaxed) {
                            return;
                        }
                        let mut emitted = false;
                        for nal in openh264::nal_units(&au) {
                            if stop_bg.load(Ordering::Relaxed) {
                                return;
                            }
                            match decoder.decode(nal) {
                                Ok(Some(yuv)) => {
                                    emitted = true;
                                    if let Some(bgra) = yuv_to_bgra(&yuv, width, height) {
                                        if frame_tx.blocking_send(bgra).is_err() {
                                            return;
                                        }
                                    }
                                }
                                Ok(None) => {}
                                Err(e) => {
                                    tracing::debug!(
                                        target: "lanlink::decode",
                                        error = %e,
                                        "OpenH264 解码单条 NAL 出错，跳过继续"
                                    );
                                }
                            }
                        }
                        if !emitted && !au.is_empty() {
                            const TRAILING_AUD: [u8; 6] = [0, 0, 0, 1, 0x09, 0x10];
                            if let Ok(Some(yuv)) = decoder.decode(&TRAILING_AUD) {
                                if let Some(bgra) = yuv_to_bgra(&yuv, width, height) {
                                    if frame_tx.blocking_send(bgra).is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                    }

                    if !stop_bg.load(Ordering::Relaxed) {
                        if let Ok(rest) = decoder.flush_remaining() {
                            for yuv in rest {
                                if stop_bg.load(Ordering::Relaxed) {
                                    return;
                                }
                                if let Some(bgra) = yuv_to_bgra(&yuv, width, height) {
                                    if frame_tx.blocking_send(bgra).is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                    }
                })
                .map_err(MediaError::Io)?;

            Ok(Self {
                stop,
                worker: Some(worker),
                writer: Some(DecoderWriter { tx: au_tx, fed: 0, fed_bytes: 0 }),
                reader: Some(DecoderReader {
                    rx: frame_rx,
                    width,
                    height,
                    frame_bytes: ffmpeg::frame_bytes(width, height),
                    eof: false,
                    decoded: 0,
                    truncated: 0,
                }),
            })
        }

        pub async fn start(
            ffmpeg: &std::path::Path,
            width: u32,
            height: u32,
        ) -> Result<Self, MediaError> {
            Self::start_auto(Some(ffmpeg), width, height).await
        }

        pub fn split(&mut self) -> Result<(DecoderWriter, DecoderReader), MediaError> {
            match (self.writer.take(), self.reader.take()) {
                (Some(w), Some(r)) => Ok((w, r)),
                _ => Err(MediaError::Exited),
            }
        }

        pub async fn kill(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            self.writer.take();
            self.reader.take();
            if let Some(handle) = self.worker.take() {
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = handle.join();
                })
                .await;
            }
        }
    }

    impl Drop for VideoDecoder {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            self.writer.take();
            self.reader.take();
        }
    }
}

#[cfg(not(feature = "builtin-h264"))]
mod imp {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::process::{Child, ChildStdin, ChildStdout};

    use crate::ffmpeg;
    use crate::MediaError;

    pub struct DecoderWriter {
        stdin: ChildStdin,
        fed: u64,
        fed_bytes: u64,
    }

    impl DecoderWriter {
        pub async fn feed(&mut self, au: &[u8]) -> Result<(), MediaError> {
            self.stdin.write_all(au).await?;
            self.stdin.flush().await?;
            self.fed += 1;
            self.fed_bytes += au.len() as u64;
            Ok(())
        }

        pub fn fed(&self) -> u64 {
            self.fed
        }

        pub fn fed_bytes(&self) -> u64 {
            self.fed_bytes
        }
    }

    pub struct DecoderReader {
        stdout: ChildStdout,
        width: u32,
        height: u32,
        frame_bytes: usize,
        eof: bool,
        decoded: u64,
        truncated: u64,
    }

    impl DecoderReader {
        pub fn dimensions(&self) -> (u32, u32) {
            (self.width, self.height)
        }

        pub fn frame_bytes(&self) -> usize {
            self.frame_bytes
        }

        pub fn decoded(&self) -> u64 {
            self.decoded
        }

        pub fn truncated(&self) -> u64 {
            self.truncated
        }

        pub fn is_eof(&self) -> bool {
            self.eof
        }

        pub async fn next_frame_into(&mut self, out: &mut Vec<u8>) -> Result<bool, MediaError> {
            if self.eof {
                return Ok(false);
            }
            out.clear();
            out.resize(self.frame_bytes, 0);

            let mut filled = 0usize;
            while filled < self.frame_bytes {
                let n = self.stdout.read(&mut out[filled..]).await?;
                if n == 0 {
                    self.eof = true;
                    if filled == 0 {
                        return Ok(false);
                    }
                    out.clear();
                    self.truncated += 1;
                    tracing::warn!(
                        target: "lanlink::ffmpeg",
                        "解码输出在帧中途结束（{filled}/{} 字节），丢弃半帧",
                        self.frame_bytes
                    );
                    return Ok(false);
                }
                filled += n;
            }
            self.decoded += 1;
            Ok(true)
        }
    }

    pub struct VideoDecoder {
        child: Child,
        writer: Option<DecoderWriter>,
        reader: Option<DecoderReader>,
    }

    impl VideoDecoder {
        pub async fn start_auto(
            ffmpeg: Option<&std::path::Path>,
            width: u32,
            height: u32,
        ) -> Result<Self, MediaError> {
            let path = ffmpeg::find_ffmpeg(ffmpeg).ok_or_else(|| {
                MediaError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "找不到 ffmpeg（控制端靠它解码画面）",
                ))
            })?;
            Self::start(&path, width, height).await
        }

        pub async fn start(
            ffmpeg: &std::path::Path,
            width: u32,
            height: u32,
        ) -> Result<Self, MediaError> {
            let mut child = ffmpeg::spawn_decoder(ffmpeg, width, height)?;
            let stdin = child.stdin.take().ok_or(MediaError::NoPipe)?;
            let stdout = child.stdout.take().ok_or(MediaError::NoPipe)?;
            if let Some(err) = child.stderr.take() {
                ffmpeg::drain_stderr(err, "decoder");
            }
            Ok(Self {
                child,
                writer: Some(DecoderWriter { stdin, fed: 0, fed_bytes: 0 }),
                reader: Some(DecoderReader {
                    stdout,
                    width,
                    height,
                    frame_bytes: ffmpeg::frame_bytes(width, height),
                    eof: false,
                    decoded: 0,
                    truncated: 0,
                }),
            })
        }

        pub fn split(&mut self) -> Result<(DecoderWriter, DecoderReader), MediaError> {
            match (self.writer.take(), self.reader.take()) {
                (Some(w), Some(r)) => Ok((w, r)),
                _ => Err(MediaError::Exited),
            }
        }

        pub async fn kill(&mut self) {
            let _ = self.child.start_kill();
            let _ = self.child.wait().await;
        }
    }

    impl Drop for VideoDecoder {
        fn drop(&mut self) {
            let _ = self.child.start_kill();
        }
    }
}

pub use imp::{DecoderReader, DecoderWriter, VideoDecoder};
