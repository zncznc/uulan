//! uulan 线协议。
//!
//! 三条连接（control / video / bulk）共用同一套帧格式，见 [`frame`]。
//! 控制类消息见 [`msg`]（被控端 ↔ 控制端），会合协议见 [`rv`]（端 ↔ 中继服务器），
//! Windows VK → KeySym 的键位映射见 [`vk`]。
//! bulk 连接上的多路复用（文件传输 / 端口映射的数据流）见 [`bulk`]。

pub mod bulk;
pub mod frame;
pub mod msg;
pub mod rv;
pub mod rvclient;
pub mod vk;

pub use frame::{
    read_frame, write_frame, Frame, FrameKind, MAX_FRAME, VIDEO_AU_TS_LEN,
};
pub use rvclient::{RelaySet, RvConn, RvError};

/// 协议版本。不兼容变更必须递增；服务端/两端在握手时用它拒绝过旧的另一端。
pub const PROTO_VERSION: u32 = 1;

/// 默认中继端口。
pub const DEFAULT_PORT: u16 = 21116;

/// 设备 ID 长度（纯数字，与 UU/TeamViewer 的观感一致）。
pub const DEVICE_ID_LEN: usize = 9;