//! 长度前缀帧编解码。
//!
//! ```text
//! +--------+--------+---------------------------+
//! | u32 BE | u8     | payload                   |
//! | length | kind   |                           |
//! +--------+--------+---------------------------+
//! ```
//!
//! 显式长度前缀是刻意的：参考项目在 WebRTC 的 SCTP 上踩过「对端假设 64KB 上限」的坑，
//! 自建帧头没有这种隐式约定。上限 [`MAX_FRAME`] 用来防止对端用一个巨大的 length 把我们 OOM 掉。

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// 单帧 payload 的硬上限。视频关键帧在 4K 下可能到几 MiB，留足余量。
pub const MAX_FRAME: usize = 8 * 1024 * 1024;

/// 帧类型字节。
///
/// **控制类消息一律走 [`FrameKind::Json`]**，靠 payload 里的 `type` 字段区分
/// （包括「画面参数变化」`H2C::VideoCfg`）。
///
/// 曾经这里还有一个 `VideoCfg = 0x03` 帧型，结果被控端把画面参数当 Json 帧发出去，
/// 而控制端却在等 0x03 帧 —— 两边对同一件事有两套表达，控制端就一直忽略那一帧，
/// 直到视频通道 EOF，表现为「连不上画面」。同一件事只允许有一种线上表达。
/// `0x03` 保留不分配，避免以后有人复用它造成同样的错位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    /// UTF-8 JSON 控制消息。小而密，走 control 连接；画面参数也用它。
    Json = 0x01,
    /// 一个 H.264/H.265 访问单元：8 字节小端时间戳(µs) + 码流字节。
    VideoAu = 0x02,
    // 0x03 保留（历史上是 VideoCfg，已废除，见上）。
    /// 文件传输 / 端口映射的裸字节。
    Blob = 0x04,
    /// 心跳。
    Ping = 0x05,
    /// 心跳回应。
    Pong = 0x06,
    /// 主动断开，payload 为 UTF-8 原因（可空）。
    Bye = 0x07,
}

impl FrameKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0x01 => Self::Json,
            0x02 => Self::VideoAu,
            0x04 => Self::Blob,
            0x05 => Self::Ping,
            0x06 => Self::Pong,
            0x07 => Self::Bye,
            _ => return None,
        })
    }
}

/// 视频访问单元前的时间戳长度（微秒，小端 u64）。
pub const VIDEO_AU_TS_LEN: usize = 8;

#[derive(Debug, Clone)]
pub struct Frame {
    pub kind: FrameKind,
    pub payload: Bytes,
}

impl Frame {
    pub fn new(kind: FrameKind, payload: impl Into<Bytes>) -> Self {
        Self { kind, payload: payload.into() }
    }

    pub fn json(kind: FrameKind, value: &impl serde::Serialize) -> Result<Self, FrameError> {
        Ok(Self { kind, payload: Bytes::from(serde_json::to_vec(value)?) })
    }

    pub fn as_str(&self) -> Result<&str, FrameError> {
        Ok(std::str::from_utf8(&self.payload)?)
    }

    pub fn decode_json<T: serde::de::DeserializeOwned>(&self) -> Result<T, FrameError> {
        Ok(serde_json::from_slice(&self.payload)?)
    }
}

/// 视频帧构造/解析辅助。
pub mod video {
    use super::*;

    /// 把时间戳和码流拼成一个 VideoAu 帧的 payload。
    pub fn pack_au(ts_us: u64, au: &[u8]) -> Bytes {
        let mut buf = Vec::with_capacity(VIDEO_AU_TS_LEN + au.len());
        buf.extend_from_slice(&ts_us.to_le_bytes());
        buf.extend_from_slice(au);
        Bytes::from(buf)
    }

    /// 拆开 VideoAu payload。长度不足时返回 None（不是 panic —— 对端可能是坏实现）。
    pub fn unpack_au(payload: &[u8]) -> Option<(u64, &[u8])> {
        if payload.len() < VIDEO_AU_TS_LEN {
            return None;
        }
        let ts = u64::from_le_bytes(payload[..VIDEO_AU_TS_LEN].try_into().ok()?);
        Some((ts, &payload[VIDEO_AU_TS_LEN..]))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("帧长度 {0} 超过上限 {MAX_FRAME}")]
    TooLarge(usize),
    #[error("未知帧类型 0x{0:02x}")]
    UnknownKind(u8),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("utf8: {0}")]
    Utf8(#[from] std::str::Utf8Error),
}

impl From<FrameError> for std::io::Error {
    fn from(e: FrameError) -> Self {
        match e {
            FrameError::Io(io) => io,
            other => std::io::Error::new(std::io::ErrorKind::InvalidData, other.to_string()),
        }
    }
}

pub async fn write_frame<W>(w: &mut W, kind: FrameKind, payload: &[u8]) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
{
    if payload.len() > MAX_FRAME {
        return Err(FrameError::TooLarge(payload.len()));
    }
    let mut hdr = [0u8; 5];
    hdr[..4].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    hdr[4] = kind as u8;
    w.write_all(&hdr).await?;
    if !payload.is_empty() {
        w.write_all(payload).await?;
    }
    // 已 flush：控制帧是延迟敏感的，不能攒在缓冲区里等下一个写。
    w.flush().await?;
    Ok(())
}

pub async fn read_frame<R>(r: &mut R) -> Result<Frame, FrameError>
where
    R: AsyncRead + Unpin,
{
    let mut hdr = [0u8; 5];
    r.read_exact(&mut hdr).await?;
    let len = u32::from_be_bytes(hdr[..4].try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    let kind = FrameKind::from_u8(hdr[4]).ok_or(FrameError::UnknownKind(hdr[4]))?;
    let mut buf = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut buf).await?;
    }
    Ok(Frame { kind, payload: Bytes::from(buf) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn roundtrip_all_kinds() {
        let (mut a, mut b) = duplex(1 << 16);
        for kind in [
            FrameKind::Json,
            FrameKind::VideoAu,
            FrameKind::Blob,
            FrameKind::Ping,
            FrameKind::Pong,
            FrameKind::Bye,
        ] {
            write_frame(&mut a, kind, b"hello").await.unwrap();
            let f = read_frame(&mut b).await.unwrap();
            assert_eq!(f.kind, kind);
            assert_eq!(&f.payload[..], b"hello");
        }
    }

    #[tokio::test]
    async fn empty_payload_roundtrip() {
        let (mut a, mut b) = duplex(1024);
        write_frame(&mut a, FrameKind::Ping, b"").await.unwrap();
        let f = read_frame(&mut b).await.unwrap();
        assert_eq!(f.kind, FrameKind::Ping);
        assert!(f.payload.is_empty());
    }

    #[tokio::test]
    async fn oversized_length_is_rejected_without_allocating() {
        let (mut a, mut b) = duplex(1024);
        // 谎报 4 GiB 长度，只跟 5 字节帧头。
        let hdr = [0xffu8, 0xff, 0xff, 0xff, FrameKind::Blob as u8];
        a.write_all(&hdr).await.unwrap();
        let err = read_frame(&mut b).await.unwrap_err();
        assert!(matches!(err, FrameError::TooLarge(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn unknown_kind_is_an_error_not_a_panic() {
        let (mut a, mut b) = duplex(1024);
        a.write_all(&[0, 0, 0, 0, 0x7f]).await.unwrap();
        let err = read_frame(&mut b).await.unwrap_err();
        assert!(matches!(err, FrameError::UnknownKind(0x7f)), "got {err:?}");
    }

    #[tokio::test]
    async fn truncated_stream_errors() {
        let (mut a, mut b) = duplex(1024);
        a.write_all(&[0, 0, 0, 10, FrameKind::Json as u8]).await.unwrap();
        drop(a);
        assert!(read_frame(&mut b).await.is_err());
    }

    #[tokio::test]
    async fn json_helpers() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct T {
            a: u32,
        }
        let f = Frame::json(FrameKind::Json, &T { a: 7 }).unwrap();
        let back: T = f.decode_json().unwrap();
        assert_eq!(back, T { a: 7 });
        assert_eq!(f.as_str().unwrap(), r#"{"a":7}"#);
    }

    #[test]
    fn video_pack_roundtrip() {
        let p = video::pack_au(1234567, b"\x00\x00\x00\x01");
        assert_eq!(p.len(), VIDEO_AU_TS_LEN + 4);
        let (ts, au) = video::unpack_au(&p).unwrap();
        assert_eq!(ts, 1234567);
        assert_eq!(au, b"\x00\x00\x00\x01");
    }

    #[test]
    fn video_unpack_rejects_short_payload() {
        assert!(video::unpack_au(&[0u8; 3]).is_none());
    }
}