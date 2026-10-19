//! 编码侧：抓屏（或任何输入）→ H.264 → 一路带时间戳的访问单元（AU）。
//!
//! 这一层刻意**不碰 X11**：输入段由调用方给（生产环境是 `-f x11grab`，
//! 测试里是 `-f lavfi` 合成源）。好处是被控端的整条编码链路在开发机上就能端到端验证，
//! 不必等一台真的麒麟机器。

use std::collections::VecDeque;
use std::path::Path;
use std::time::Instant;

use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStdout};

use crate::annexb::{self, AuSplitter};
use crate::ffmpeg::{self, CaptureOptions};
use crate::MediaError;

/// 一个可发往外部的访问单元。
#[derive(Debug, Clone)]
pub struct Au {
    /// 相对视频源启动时刻的微秒数。控制端用它做延迟统计与节流。
    pub ts_us: u64,
    /// 完整 Annex-B 访问单元字节。
    pub data: Vec<u8>,
    /// 是否关键帧（首条 VCL NAL 是 IDR）。控制端据此判断能否立刻出图。
    pub keyframe: bool,
}

/// 正在运行的编码源。
pub struct VideoSource {
    child: Child,
    stdout: ChildStdout,
    splitter: AuSplitter,
    ready: VecDeque<Au>,
    buf: Vec<u8>,
    started: Instant,
    eof: bool,
    flushed: bool,
    emitted: u64,
    keyframes: u64,
    bytes: u64,
    pub opts: CaptureOptions,
}

impl VideoSource {
    /// 用给定的输入段启动。生产环境请用 [`VideoSource::start`]。
    pub async fn start_with_input(
        ffmpeg: &Path,
        opts: CaptureOptions,
        input: &[String],
    ) -> Result<Self, MediaError> {
        let mut child = ffmpeg::spawn_encoder_with_input(ffmpeg, &opts, input)?;
        let stdout = child.stdout.take().ok_or(MediaError::NoPipe)?;
        if let Some(err) = child.stderr.take() {
            ffmpeg::drain_stderr(err, "encoder");
        }
        Ok(Self {
            child,
            stdout,
            splitter: AuSplitter::new(),
            ready: VecDeque::new(),
            buf: vec![0u8; 64 * 1024],
            started: Instant::now(),
            eof: false,
            flushed: false,
            emitted: 0,
            keyframes: 0,
            bytes: 0,
            opts,
        })
    }

    /// 生产路径：抓 X11 屏幕。
    pub async fn start(ffmpeg: &Path, opts: CaptureOptions) -> Result<Self, MediaError> {
        let input = ffmpeg::x11grab_input(&opts);
        Self::start_with_input(ffmpeg, opts, &input).await
    }

    /// 已产出的 AU 数。
    pub fn emitted(&self) -> u64 {
        self.emitted
    }

    /// 已产出的关键帧数。
    pub fn keyframes(&self) -> u64 {
        self.keyframes
    }

    /// 已读到的 H.264 字节数（压缩后）。
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// 取下一个 AU。流结束返回 `None`。
    ///
    /// 语义是「尽快返回」：一旦切分器攒够一个完整 AU 就立刻交出，不等下一帧的字节。
    /// 这是低延迟的前提 —— 等整帧收完再发，一帧的编码时间就白等了。
    pub async fn next_au(&mut self) -> Result<Option<Au>, MediaError> {
        loop {
            if let Some(au) = self.ready.pop_front() {
                return Ok(Some(au));
            }
            if self.eof {
                if !self.flushed {
                    self.flushed = true;
                    if let Some(last) = self.splitter.flush() {
                        let ts = self.started.elapsed().as_micros() as u64;
                        return Ok(Some(self.record(last, ts)));
                    }
                }
                return Ok(None);
            }

            let n = self.stdout.read(&mut self.buf).await?;
            if n == 0 {
                self.eof = true;
                continue;
            }
            self.bytes += n as u64;
            let ts = self.started.elapsed().as_micros() as u64;
            for au in self.splitter.push(&self.buf[..n]) {
                let ready = self.record(au, ts);
                self.ready.push_back(ready);
            }
        }
    }

    fn record(&mut self, data: Vec<u8>, ts_us: u64) -> Au {
        let keyframe = annexb::is_keyframe_au(&data);
        self.emitted += 1;
        if keyframe {
            self.keyframes += 1;
        }
        Au { ts_us, data, keyframe }
    }

    /// 主动结束：杀掉 ffmpeg 并回收。
    ///
    /// 用 kill 而不是等它自然退出 —— 抓屏进程不会自己结束。
    pub async fn stop(&mut self) -> Result<(), MediaError> {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
        Ok(())
    }
}

impl Drop for VideoSource {
    fn drop(&mut self) {
        // spawn 时设了 kill_on_drop，这里只是兜底确保不留孤儿 ffmpeg。
        let _ = self.child.start_kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyframe_detection_matches_first_vcl_nal() {
        let mk = |typs: &[u8]| {
            let mut v = Vec::new();
            for t in typs {
                v.extend_from_slice(&[0, 0, 0, 1, *t & 0x1f]);
                v.extend_from_slice(&[0xAA, 0xBB]);
            }
            v
        };
        assert!(annexb::is_keyframe_au(&mk(&[9, 7, 8, 6, 5, 5])));
        assert!(!annexb::is_keyframe_au(&mk(&[9, 1])));
        assert!(!annexb::is_keyframe_au(&mk(&[9, 6, 1])));
        // 同一 AU 里 IDR 之后又跟了 P（非规范但不应崩）
        assert!(annexb::is_keyframe_au(&mk(&[9, 5, 1])));
    }
}
