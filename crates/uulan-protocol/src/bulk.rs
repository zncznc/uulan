//! bulk 连接上的多路复用帧：文件传输与端口映射共用。
//!
//! ```text
//! +-----------+--------+-----------------+
//! | u32 LE id | u8 flg | data            |
//! +-----------+--------+-----------------+
//! ```
//!
//! - `id` 由控制通道上的请求分配（`FsOp::id` / `PortMapOp::id`），一条 bulk 连接上
//!   可以同时存在多个 id 的流，接收方按 id 分派。
//! - `flg` 第 0 位是 `END`：该 id 的数据到此结束，`data` 必须为空。
//!
//! **为什么结束标记在数据流里、而不是只靠控制通道的 `FsStream::Done`**：
//! control 和 bulk 是两条独立的 TCP 连接，谁先到没有保证。数据的结束只能由数据流自己
//! 表达，否则接收方就得在「等数据」和「等控制消息」之间猜，猜错就是丢尾巴或死等。
//! 控制通道上的 `FsStream` 只负责「进度/通知」，不承担同步职责。
//!
//! 反过来也一样：`END` 之后**必须**再有一条控制通道的 `FsResult`（带 sha256），
//! 一份传输只有在哈希对得上时才算成功 —— 见 `uulan-fs`。

use bytes::Bytes;

use crate::frame::{Frame, FrameKind};

/// `id(4) + flags(1)`。
pub const HEADER_LEN: usize = 5;

/// 流结束标志位。其余位保留，收到就报错（比静默忽略安全）。
pub const FLAG_END: u8 = 0x01;

/// 单帧建议负载大小。64 KiB 是「一帧头开销可以忽略」与「不把 TCP 队列堵死」之间的折中。
pub const CHUNK: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BulkKind {
    Data,
    /// 该 id 的数据结束。**之后同一个 id 不应再出现任何帧。**
    End,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BulkFrame {
    pub id: u32,
    pub kind: BulkKind,
    pub data: Bytes,
}

impl BulkFrame {
    pub fn data(id: u32, data: impl Into<Bytes>) -> Self {
        Self { id, kind: BulkKind::Data, data: data.into() }
    }

    pub fn end(id: u32) -> Self {
        Self { id, kind: BulkKind::End, data: Bytes::new() }
    }

    /// 编码成 `Blob` 帧的 payload。
    pub fn encode(&self) -> Bytes {
        let n = self.data.len();
        let mut v = Vec::with_capacity(HEADER_LEN + n);
        v.extend_from_slice(&self.id.to_le_bytes());
        v.push(match self.kind {
            BulkKind::Data => 0,
            BulkKind::End => FLAG_END,
        });
        v.extend_from_slice(&self.data);
        Bytes::from(v)
    }

    /// 整个帧（`FrameKind::Blob`），直接喂给 [`crate::frame::write_frame`]。
    pub fn to_frame(&self) -> Frame {
        Frame::new(FrameKind::Blob, self.encode())
    }

    pub fn decode(payload: &[u8]) -> Result<Self, BulkError> {
        if payload.len() < HEADER_LEN {
            return Err(BulkError::TooShort(payload.len()));
        }
        let id = u32::from_le_bytes(payload[..4].try_into().unwrap());
        let flags = payload[4];
        if flags & !FLAG_END != 0 {
            return Err(BulkError::UnknownFlags(flags));
        }
        let body = Bytes::copy_from_slice(&payload[HEADER_LEN..]);
        if flags & FLAG_END != 0 {
            if !body.is_empty() {
                return Err(BulkError::EndWithData(body.len()));
            }
            Ok(Self { id, kind: BulkKind::End, data: body })
        } else {
            Ok(Self { id, kind: BulkKind::Data, data: body })
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BulkError {
    #[error("bulk 帧太短：{0} 字节（至少 {HEADER_LEN} 字节）")]
    TooShort(usize),
    #[error("bulk 帧标志位非法：0x{0:02x}（只认识 END=0x01）")]
    UnknownFlags(u8),
    #[error("END 帧不允许带数据（带了 {0} 字节）")]
    EndWithData(usize),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_frame_roundtrip() {
        let f = BulkFrame::data(0x1234_5678, &b"hello"[..]);
        let back = BulkFrame::decode(&f.encode()).unwrap();
        assert_eq!(back, f);
        assert_eq!(back.id, 0x1234_5678);
        assert_eq!(&back.data[..], b"hello");
    }

    #[test]
    fn empty_data_frame_is_legal() {
        // 空数据帧不是「结束」，两者不能混：接收方靠 kind 判断，不靠长度。
        let f = BulkFrame::data(7, &b""[..]);
        let back = BulkFrame::decode(&f.encode()).unwrap();
        assert_eq!(back.kind, BulkKind::Data);
        assert!(back.data.is_empty());
    }

    #[test]
    fn end_frame_has_no_body_and_is_five_bytes() {
        let f = BulkFrame::end(9);
        let enc = f.encode();
        assert_eq!(enc.len(), HEADER_LEN);
        let back = BulkFrame::decode(&enc).unwrap();
        assert_eq!(back.kind, BulkKind::End);
        assert_eq!(back.id, 9);
    }

    #[test]
    fn frame_kind_is_blob() {
        let fr = BulkFrame::data(1, &b"x"[..]).to_frame();
        assert_eq!(fr.kind, FrameKind::Blob);
        assert_eq!(BulkFrame::decode(&fr.payload).unwrap().id, 1);
    }

    #[test]
    fn short_payload_is_rejected() {
        assert_eq!(BulkFrame::decode(&[0, 0, 0]), Err(BulkError::TooShort(3)));
        assert_eq!(BulkFrame::decode(&[]), Err(BulkError::TooShort(0)));
    }

    #[test]
    fn unknown_flags_are_rejected() {
        let mut enc = BulkFrame::data(1, &b"a"[..]).encode().to_vec();
        enc[4] = 0x80;
        assert_eq!(BulkFrame::decode(&enc), Err(BulkError::UnknownFlags(0x80)));
    }

    #[test]
    fn end_with_data_is_rejected() {
        // 手搓一个「END 但带数据」的帧：不能当成普通结束静默吃掉。
        let mut enc = Vec::new();
        enc.extend_from_slice(&5u32.to_le_bytes());
        enc.push(FLAG_END);
        enc.extend_from_slice(b"xx");
        assert_eq!(BulkFrame::decode(&enc), Err(BulkError::EndWithData(2)));
    }

    #[test]
    fn large_chunk_roundtrip() {
        let payload: Vec<u8> = (0..CHUNK).map(|i| (i % 251) as u8).collect();
        let f = BulkFrame::data(42, payload.clone());
        let back = BulkFrame::decode(&f.encode()).unwrap();
        assert_eq!(back.data.len(), CHUNK);
        assert_eq!(&back.data[..], &payload[..]);
    }

    #[test]
    fn ids_survive_the_full_u32_range() {
        for id in [0u32, 1, u32::MAX] {
            assert_eq!(BulkFrame::decode(&BulkFrame::end(id).encode()).unwrap().id, id);
        }
    }
}