//! uulan 控制端。
//!
//! - [`connect`]：会合 → 三条通道 → 握手 → 鉴权。
//! - [`watch`]：视频接收与解码（headless 也能跑，便于自测和排障）。
//!
//! 窗口（winit + softbuffer）是 [`watch`] 之上的一层，不参与协议。

pub mod connect;
pub mod watch;

pub use connect::{connect, ClientError, Connected, HostInfo};
pub use watch::{WatchStats, VideoWatcher};
