//! 编码侧：抓屏（或任何输入）→ H.264 → 一路带时间戳的访问单元（AU）。
//!
//! Linux 生产环境走 `ffmpeg -f x11grab`；Windows 生产环境（启用 `builtin-h264`）走
//! 原生 GDI 抓屏 + 内置 OpenH264 编码（零 `ffmpeg.exe` 依赖）；测试走 `-f lavfi` 合成源。

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

enum SourceBackend {
    Ffmpeg {
        child: Child,
        stdout: ChildStdout,
        splitter: AuSplitter,
        ready: VecDeque<Au>,
        buf: Vec<u8>,
        started: Instant,
        eof: bool,
        flushed: bool,
    },
    #[cfg(all(windows, feature = "builtin-h264"))]
    WinGdi {
        rx: tokio::sync::mpsc::Receiver<Au>,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        worker: Option<std::thread::JoinHandle<()>>,
    },
}

/// 正在运行的编码源。
pub struct VideoSource {
    backend: SourceBackend,
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
            backend: SourceBackend::Ffmpeg {
                child,
                stdout,
                splitter: AuSplitter::new(),
                ready: VecDeque::new(),
                buf: vec![0u8; 64 * 1024],
                started: Instant::now(),
                eof: false,
                flushed: false,
            },
            emitted: 0,
            keyframes: 0,
            bytes: 0,
            opts,
        })
    }

    /// Windows 生产路径：GDI 抓屏 + 内置 OpenH264 编码，不依赖 `ffmpeg.exe`。
    #[cfg(all(windows, feature = "builtin-h264"))]
    pub async fn start_windows_gdi(opts: CaptureOptions) -> Result<Self, MediaError> {
        let (rx, stop, worker) = crate::win_gdi::spawn_windows_source(opts.clone())?;
        Ok(Self {
            backend: SourceBackend::WinGdi { rx, stop, worker: Some(worker) },
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
    pub async fn next_au(&mut self) -> Result<Option<Au>, MediaError> {
        match &mut self.backend {
            SourceBackend::Ffmpeg {
                stdout,
                splitter,
                ready,
                buf,
                started,
                eof,
                flushed,
                ..
            } => loop {
                if let Some(au) = ready.pop_front() {
                    return Ok(Some(au));
                }
                if *eof {
                    if !*flushed {
                        *flushed = true;
                        if let Some(last) = splitter.flush() {
                            let ts = started.elapsed().as_micros() as u64;
                            let keyframe = annexb::is_keyframe_au(&last);
                            self.emitted += 1;
                            if keyframe {
                                self.keyframes += 1;
                            }
                            return Ok(Some(Au { ts_us: ts, data: last, keyframe }));
                        }
                    }
                    return Ok(None);
                }

                let n = stdout.read(buf).await?;
                if n == 0 {
                    *eof = true;
                    continue;
                }
                self.bytes += n as u64;
                let ts = started.elapsed().as_micros() as u64;
                for data in splitter.push(&buf[..n]) {
                    let keyframe = annexb::is_keyframe_au(&data);
                    self.emitted += 1;
                    if keyframe {
                        self.keyframes += 1;
                    }
                    ready.push_back(Au { ts_us: ts, data, keyframe });
                }
            },
            #[cfg(all(windows, feature = "builtin-h264"))]
            SourceBackend::WinGdi { rx, .. } => match rx.recv().await {
                Some(au) => {
                    self.emitted += 1;
                    self.bytes += au.data.len() as u64;
                    if au.keyframe {
                        self.keyframes += 1;
                    }
                    Ok(Some(au))
                }
                None => Ok(None),
            },
        }
    }

    /// 主动结束：杀掉编码子进程或停止编码线程并回收。
    pub async fn stop(&mut self) -> Result<(), MediaError> {
        match &mut self.backend {
            SourceBackend::Ffmpeg { child, .. } => {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
            #[cfg(all(windows, feature = "builtin-h264"))]
            SourceBackend::WinGdi { stop, worker, rx } => {
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
                rx.close();
                if let Some(h) = worker.take() {
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = h.join();
                    })
                    .await;
                }
            }
        }
        Ok(())
    }
}

impl Drop for VideoSource {
    fn drop(&mut self) {
        match &mut self.backend {
            SourceBackend::Ffmpeg { child, .. } => {
                let _ = child.start_kill();
            }
            #[cfg(all(windows, feature = "builtin-h264"))]
            SourceBackend::WinGdi { stop, rx, .. } => {
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
                rx.close();
            }
        }
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
        assert!(annexb::is_keyframe_au(&mk(&[9, 5, 1])));
    }
}
