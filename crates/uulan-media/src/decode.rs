//! 解码侧：H.264 访问单元 → BGRA 原始帧。
//!
//! 解码器是**读写两半可以拆开**的：真实控制端里，收包的异步任务往 stdin 喂 AU，
//! 渲染线程从 stdout 取帧，两者必须并发跑 —— 串行的话管道一满就死锁。
//! 所以这里不提供「一个 `&mut self` 同时喂和读」的接口，避免使用者写出死锁代码。

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout};

use crate::ffmpeg;
use crate::MediaError;

/// 解码器的写端：喂 H.264 访问单元。
pub struct DecoderWriter {
    stdin: ChildStdin,
    fed: u64,
    fed_bytes: u64,
}

impl DecoderWriter {
    /// 喂一个访问单元。解码器自己缓冲到凑齐可解码的一帧。
    pub async fn feed(&mut self, au: &[u8]) -> Result<(), MediaError> {
        self.stdin.write_all(au).await?;
        self.stdin.flush().await?;
        self.fed += 1;
        self.fed_bytes += au.len() as u64;
        Ok(())
    }

    /// 已喂入的 AU 数。
    pub fn fed(&self) -> u64 {
        self.fed
    }

    /// 已喂入的字节数。
    pub fn fed_bytes(&self) -> u64 {
        self.fed_bytes
    }
}

/// 解码器的读端：取 BGRA 原始帧。
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

    /// 已解出的完整帧数。
    pub fn decoded(&self) -> u64 {
        self.decoded
    }

    /// 因为流中途断掉而丢掉的半帧数。持续增长说明码流被损坏。
    pub fn truncated(&self) -> u64 {
        self.truncated
    }

    pub fn is_eof(&self) -> bool {
        self.eof
    }

    /// 读一帧 BGRA 到 `out`（会被填成恰好 `frame_bytes` 字节）。
    ///
    /// 返回 `Ok(false)` 表示流结束。**绝不返回半帧** —— 宁可报结束，
    /// 也不要把半张图交给渲染层（那会画出撕裂的画面）。
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
                // 半帧：丢弃。这通常意味着码流被截断或解码器崩了。
                out.clear();
                self.truncated += 1;
                tracing::warn!(
                    target: "uulan::ffmpeg",
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

/// 控制端的 ffmpeg 解码进程。
///
/// 典型用法：
/// ```ignore
/// let mut dec = VideoDecoder::start(&ffmpeg, w, h).await?;
/// let (mut w, mut r) = dec.split()?;
/// tokio::spawn(async move { while let Some(au) = rx.recv().await { w.feed(&au).await?; } });
/// let mut buf = Vec::new();
/// while r.next_frame_into(&mut buf).await? { render(&buf); }
/// ```
pub struct VideoDecoder {
    child: Child,
    writer: Option<DecoderWriter>,
    reader: Option<DecoderReader>,
}

impl VideoDecoder {
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

    /// 取出读写两端。**只能取一次** —— 第二次返回 `Err`，
    /// 因为「同时持有两半」正是这里要防的死锁来源。
    pub fn split(&mut self) -> Result<(DecoderWriter, DecoderReader), MediaError> {
        match (self.writer.take(), self.reader.take()) {
            (Some(w), Some(r)) => Ok((w, r)),
            _ => Err(MediaError::Exited),
        }
    }

    /// 杀掉解码进程。
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
