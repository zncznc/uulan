//! uulan 控制端。
//!
//! - [`connect`]：会合 → 三条通道 → 握手 → 鉴权。
//! - [`watch`]：视频接收与解码（headless 也能跑，便于自测和排障）。
//! - [`control`]：控制通道（键鼠上报 + 被控端回报）。
//! - [`input`]：窗口事件 → 协议消息的纯函数（坐标换算、滚轮、键位表）。
//!
//! 窗口（winit + softbuffer）是 [`watch`] 之上的一层，不参与协议。

pub mod connect;
pub mod control;
pub mod input;
pub mod watch;

pub use connect::{connect, ClientError, Connected, HostInfo};
pub use watch::{WatchStats, VideoWatcher};
pub mod window;
