//! `uulan-fs` —— 文件传输。
//!
//! 分三层，和键鼠/剪贴板同样的取舍：
//!
//! | 层 | 模块 | 能不能在本机（Windows）测 |
//! |---|---|---|
//! | 路径解析（`:/`、`~/`、绝对路径、`..` 不许越界） | [`path`] | 能，纯函数 |
//! | 「此电脑」条目（盘符 / 挂载点） | [`mounts`] | 能，解析函数收文本 |
//! | 文件服务（列/取/送/建/删/改名/哈希 + bulk 传输） | [`server`] | 能，收发只依赖 `AsyncRead/AsyncWrite` |
//!
//! 也就是说：**M4 的文件传输这一项，本机能给出真链路的证据**（真 socket、真文件、真 sha256），
//! 不需要麒麟在场。麒麟上要验的只有「挂载点列表长得对不对」和真实路径权限。

pub mod mounts;
pub mod path;
pub mod server;

/// 测试辅助（建临时目录 + 可写性探测）。生产代码不该用它。
#[doc(hidden)]
pub mod testutil;

pub use path::{FsRoots, PathError, Target};
pub use server::{Bulk, BulkPeer, PART_SUFFIX};