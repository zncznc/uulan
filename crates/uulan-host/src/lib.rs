//! uulan 被控端。
//!
//! 结构：
//! - [`capture`]：采集源抽象（X11 抓屏 / 合成源）。
//! - [`config`]：本机身份与访问密码的持久化。
//! - [`rv`]：与中继服务器的连接（注册、保活、接收来连）。
//! - [`session`]：一次远程会话 —— 三条通道、握手、鉴权、视频推送、控制消息处理。
//! - [`term`]：终端（PTY）会话服务 —— 状态机与可注入的 PTY 后端。
//!
//! 注意：被控端**不依赖任何 X11 头文件或库**。抓屏是交给 `ffmpeg -f x11grab` 子进程做的，
//! 所以这个 crate 在 Windows 上也能完整编译和跑测试 —— 只是 `x11` 采集源要真的连上 X 才能用。

pub mod capture;
pub mod config;
pub mod input;
pub mod portmap;
pub mod session;
pub mod term;

pub use capture::CaptureInput;
pub use config::{validate_password, HostConfig};
pub use portmap::{PortMap, PortPolicy};
pub use term::{NativePty, PtyFactory};
pub use session::{AuthState, HostState, SessionOutcome, SessionParams, VideoSettings};
