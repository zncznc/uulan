//! uulan 媒体层。
//!
//! - [`annexb`]：H.264 Annex-B 码流的 NAL 扫描与访问单元（AU）切分。
//!   被控端从 FFmpeg 的 stdout 里读到的是一串裸字节，必须切成「一帧一个 AU」
//!   才能带上时间戳发给控制端。
//! - [`ffmpeg`]：FFmpeg 命令行编排（采集/编码参数、解码参数）。
//! - [`source`]：`VideoSource` —— 编码侧，抓屏/合成源 → 带时间戳的 AU。
//! - [`decode`]：`VideoDecoder` —— 解码侧，AU → BGRA 原始帧；读写两端可拆开并发使用。
//!
//! 两侧都只跟管道打交道，不依赖 X11、也不依赖窗口系统，所以能在任何平台上端到端测。

pub mod annexb;
pub mod decode;
pub mod ffmpeg;
pub mod source;

pub use annexb::{nal, AuSplitter, Nal};
pub use decode::{DecoderReader, DecoderWriter, VideoDecoder};
pub use source::{Au, VideoSource};

/// 媒体层的统一错误。
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("ffmpeg 子进程已结束")]
    Exited,
    #[error("编码进程没有 stdout / 解码进程没有 stdout")]
    NoPipe,
}
