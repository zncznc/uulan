//! uulan 线协议。
//!
//! 三条连接（control / video / bulk）共用同一套帧格式，见 [`frame`]。
//! 控制类消息见 [`msg`]（被控端 ↔ 控制端），会合协议见 [`rv`]（端 ↔ 中继服务器），
//! Windows VK → KeySym 的键位映射见 [`vk`]。
//! bulk 连接上的多路复用（文件传输 / 端口映射的数据流）见 [`bulk`]。

pub mod bulk;
pub mod crypto;
pub mod frame;
pub mod msg;
pub mod pake;
pub mod redact;
pub mod rv;
pub mod rvclient;
pub mod vk;

pub use frame::{
    read_frame, write_frame, Frame, FrameKind, MAX_FRAME, VIDEO_AU_TS_LEN,
};
pub use rvclient::{RelaySet, RvConn, RvError};

/// 协议版本。不兼容变更必须递增；服务端/两端在握手时用它拒绝过旧的另一端。
///
/// v2（M6）：三条会话连接上强制端到端加密，`Auth` 改成口令证明、`AuthOk` 带被控端证明。
/// 旧端（v1）不会做加密握手，被控端等不到 `CryptoHello` 就会超时收尾 ——
/// 这正是我们要的：宁可连不上，也不要悄悄退回明文。
///
/// v3（M6 加固）：加密握手之后多一轮 PAKE，`Auth` 变成**一组**证明（每个候选凭据一份）、
/// `AuthOk` 带上命中序号，会话密钥里混进 PAKE 共享点。旧端在 `CryptoHello` 阶段就会被
/// `BadVersion` 挡掉（加密层版本也一起递增了），不会出现「一半新一半旧」的半加密状态。
///
/// v4（M7 音频）：会话从三条通道变**四条**，多一条 `audio`。这不能向后兼容 ——
/// 新控制端会等第四条配对，老被控端根本不会拨它，双方就卡在 `relay_open_all` 里。
/// 所以版本号必须跟着动：宁可 `ProtoMismatch` 明确报错，也不要挂在那里让人以为「连上了没声音」。
pub const PROTO_VERSION: u32 = 4;

/// 默认中继端口。
pub const DEFAULT_PORT: u16 = 21116;

/// 设备 ID 长度（纯数字，与 UU/TeamViewer 的观感一致）。
pub const DEVICE_ID_LEN: usize = 9;