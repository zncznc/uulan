//! 被控端 ↔ 控制端 的控制消息（JSON，走 control 连接）。
//!
//! 设计要点：
//! - 方向分开成 [`C2H`]（控制端→被控端）和 [`H2C`]（被控端→控制端）两个枚举，
//!   两端各自 `match` 时不需要处理对方才能收到的消息。
//! - 键位用 **Windows VK 码**（或 IME 合成后的字符）：控制端（含麒麟端）只需要一张表，
//!   VK→KeySym→KeyCode 的映射只在被控端做一次。
//! - 鼠标坐标归一化到 `0..1`：分辨率/缩放变化时不需要重新协商。
//! - 终端输出走 base64（可能含任意字节，甚至半截 UTF-8 序列）。
//! - 新增字段一律 `#[serde(default)]` + 可省略，保证新旧两端在灰度期内能互通。

use serde::{Deserialize, Serialize};

use crate::vk::KeyTarget;

/// 一次会话的能力位。两端取交集后才能启用对应工具。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cap {
    Clipboard,
    Files,
    Terminal,
    PortMap,
    Audio,
    Metrics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Screen {
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientHello {
    pub os: String,
    pub version: String,
    pub name: String,
    pub caps: Vec<Cap>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostHelloOk {
    pub device_id: String,
    pub os: String,
    pub version: String,
    pub name: String,
    pub screen: Screen,
    pub caps: Vec<Cap>,
    /// 被控端是否有图形会话。无头机器（`UU_DESKTOP_BACKEND=none` 那种）只有终端/文件/端口映射。
    pub has_desktop: bool,
}

// ───────────────────────────── 控制端 → 被控端 ─────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum C2H {
    Hello(ClientHello),

    /// 密码校验。走到这一步之前，被控端不应处理任何输入消息。
    Auth { password: String },

    // ── 输入 ──
    MouseMoveAbs { x: f32, y: f32 },
    MouseMoveRel { dx: i32, dy: i32 },
    MouseButton { button: MouseButton, down: bool },
    /// `dx/dy` 以"一格滚轮 = 120"为单位（Windows WHEEL_DELTA）。
    MouseWheel { dx: i32, dy: i32 },
    /// 物理键（VK）。`repeat=false` 表示这是物理按下（长按连发由控制端在操作系统层产生，
    /// 被控端不再叠加自己的 autorepeat）。
    Key { vk: u16, down: bool, repeat: bool },
    /// 字符（输入法合成 / 软键盘 / 快捷键冲突时）。见 [`KeyTarget`]。
    KeyChar(KeyInput),

    // ── 画面设置 ──
    VideoSet(VideoSet),

    // ── 剪贴板 ──
    Clip { text: String },
    /// 只声明"我这里有新的剪贴板内容"，不携带正文（大内容时省一次往返）。
    ClipOffer { len: u32 },

    // ── 文件 ──
    Fs(FsOp),

    // ── 终端 ──
    Term(TermOp),

    // ── 端口映射 ──
    PortMap(PortMapOp),

    // ── 系统 ──
    Power { action: PowerAction },
    /// 调整画面里某块区域的质量（AE 风格：控制端只关心"看到"的区域）。
    VideoFocus { x: f32, y: f32, w: f32, h: f32 },
    /// 会话保持 / RTT 探测。`seq` 原样带回。
    Ping { seq: u32, sent_ms: u64 },

    /// 控制端主动道别。
    Bye { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyInput {
    pub target: KeyTarget,
    pub down: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerAction {
    Lock,
    Reboot,
    Shutdown,
    Logout,
}

/// 画面质量档位。语义对齐参考项目：静止桌面靠 CQ 决定观感，所以档位同时改码率上限和 CQ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    /// 自动：CRF 恒定质量。
    Auto,
    /// 流畅（低带宽弱机优先）。
    Fast,
    /// 高清 2Mbps 档。
    High,
    /// 超清 8Mbps 档。
    Hd,
    /// 原画 20Mbps 档。
    Bluray,
    /// 自定义码率，配合 `max_bitrate_kbps`。
    Custom,
}

/// 一个码率档位 → (CQ, 码率上限 kbps, 编码器预设)。0 表示不设上限。
pub fn quality_preset(q: Quality) -> (u8, u32) {
    match q {
        Quality::Auto => (23, 0),
        Quality::Fast => (32, 1_000),
        Quality::High => (28, 2_000),
        Quality::Hd => (23, 8_000),
        Quality::Bluray => (18, 20_000),
        Quality::Custom => (23, 0),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoSet {
    /// 0 = 保持被控端当前值。
    pub fps: u32,
    /// 0 = 由档位决定。
    pub max_bitrate_kbps: u32,
    pub quality: Quality,
    /// 编码器缩放倍率。1.0 = 原始分辨率。
    /// **只作用于编码，不改被控端显示器分辨率**（参考项目同款行为）。
    pub scale: f32,
    /// 色度："420" | "444"。老客户端不传，默认 420。
    #[serde(default = "default_chroma")]
    pub chroma: String,
    /// 优先的编码器：`auto|cpu|nvenc|vaapi`。仅当被控端确实可用时才生效。
    #[serde(default = "default_encoder")]
    pub encoder: String,
}

fn default_chroma() -> String {
    "420".into()
}
fn default_encoder() -> String {
    "auto".into()
}

impl Default for VideoSet {
    fn default() -> Self {
        Self {
            fps: 30,
            max_bitrate_kbps: 0,
            quality: Quality::Auto,
            scale: 1.0,
            chroma: default_chroma(),
            encoder: default_encoder(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum FsOp {
    /// 列目录。`path` 支持 `:/`（此电脑，列出挂载点）、`~/`（家目录）、绝对路径。
    List { id: u32, path: String },
    /// 开始下载：被控端把数据写到 bulk 连接上，带这个 id。
    Get { id: u32, path: String },
    /// 开始上传：控制端随后把数据写到 bulk 连接上，被控端收够 `size` 字节后落盘。
    Put { id: u32, path: String, size: u64 },
    Mkdir { id: u32, path: String },
    Remove { id: u32, path: String },
    Rename { id: u32, from: String, to: String },
    /// 读取单个文件的哈希，用来校验传输完整性。
    Hash { id: u32, path: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TermOp {
    Open { id: u32, cols: u16, rows: u16, shell: Option<String> },
    Input { id: u32, data: String },
    Resize { id: u32, cols: u16, rows: u16 },
    Close { id: u32 },
    /// 列出当前所有会话（重连后接回旧会话时用）。
    List,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PortMapOp {
    /// 被控端连到 `target_host:target_port`，之后双向裸字节走 bulk 连接，用 `id` 区分流。
    Open { id: u32, target_host: String, target_port: u16 },
    Close { id: u32 },
}

// ───────────────────────────── 被控端 → 控制端 ─────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum H2C {
    HelloOk(HostHelloOk),

    AuthOk,
    AuthFail { reason: AuthFailReason, retry_after_ms: u64 },

    /// 编码器已（重）启动，参数以此为准。
    VideoCfg(HostVideoCfg),
    /// 编码器出错（回退到 CPU / 重启）。控制端据此提示用户，而不是黑屏干等。
    VideoError { message: String },

    /// 系统指标，被控端每 2 秒推一次。
    Metrics(Metrics),

    /// 被控端剪贴板变化。
    Clip { text: String },
    ClipOffer { len: u32 },

    /// 锁屏状态变化。锁屏后控制端应停止发送输入并提示用户。
    SystemState { locked: bool, has_desktop: bool },

    /// 输入注入出错（比如 XTest 被拦截）。不致命，但要让人知道。
    InputError { message: String },

    FsResult {
        id: u32,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        entries: Option<Vec<FsEntry>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        hash: Option<String>,
    },

    /// 文件传输开始/进度/结束通知（数据本体在 bulk 连接上）。
    FsStream {
        id: u32,
        stage: FsStreamStage,
        #[serde(skip_serializing_if = "Option::is_none")]
        total: Option<u64>,
        #[serde(default, skip_serializing_if = "is_zero")]
        done: u64,
    },

    TermOut { id: u32, data: String },
    TermExit { id: u32, code: i32 },
    TermSessions { sessions: Vec<TermSession> },

    PortMapOk { id: u32 },
    PortMapErr { id: u32, error: String },

    /// RTT 探测回应。
    Pong { seq: u32, sent_ms: u64, host_ms: u64 },

    /// 被控端主动道别（比如用户点了「断开」）。
    Bye { reason: String },
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthFailReason {
    BadPassword,
    Locked,
    NotAllowed,
    /// 已经有人在控制这台机器（第一版默认拒绝第二个人）。
    Busy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostVideoCfg {
    pub w: u32,
    pub h: u32,
    /// "h264" | "h265"
    pub codec: String,
    pub fps: u32,
    /// libx264 | h264_nvenc | h264_vaapi | libx265 ...
    pub encoder: String,
    /// 实际编码的色度："420" | "444"
    pub chroma: String,
    /// 实测帧率（每秒一次的滑动平均），便于控制端显示。
    #[serde(default)]
    pub actual_fps: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metrics {
    /// 0..100
    pub cpu: f32,
    pub mem_used: u64,
    pub mem_total: u64,
    /// 字节/秒
    pub net_rx: u64,
    pub net_tx: u64,
    /// 控制端可见的往返延迟（毫秒），由 PING/PONG 测得。
    pub rtt_ms: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsEntry {
    pub name: String,
    pub dir: bool,
    pub size: u64,
    /// Unix 秒。
    pub mtime: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsStreamStage {
    Begin,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TermSession {
    pub id: u32,
    pub title: String,
}

// ───────────────────────────── 编解码辅助 ─────────────────────────────

impl C2H {
    pub fn to_frame(&self) -> Result<crate::Frame, crate::frame::FrameError> {
        crate::Frame::json(crate::FrameKind::Json, self)
    }

    pub fn from_frame(f: &crate::Frame) -> Result<Self, crate::frame::FrameError> {
        f.decode_json()
    }
}

impl H2C {
    pub fn to_frame(&self) -> Result<crate::Frame, crate::frame::FrameError> {
        crate::Frame::json(crate::FrameKind::Json, self)
    }

    pub fn from_frame(f: &crate::Frame) -> Result<Self, crate::frame::FrameError> {
        f.decode_json()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt_c2h(m: C2H) {
        let s = serde_json::to_string(&m).unwrap();
        let back: C2H = serde_json::from_str(&s).unwrap();
        assert_eq!(s, serde_json::to_string(&back).unwrap(), "roundtrip changed json");
    }

    fn rt_h2c(m: H2C) {
        let s = serde_json::to_string(&m).unwrap();
        let back: H2C = serde_json::from_str(&s).unwrap();
        assert_eq!(s, serde_json::to_string(&back).unwrap(), "roundtrip changed json");
    }

    #[test]
    fn wire_shape_is_flat_and_stable() {
        // 这些字符串是协议的一部分，改了就是破坏兼容 —— 故意断言死。
        let f = C2H::MouseMoveAbs { x: 0.5, y: 0.25 };
        assert_eq!(serde_json::to_string(&f).unwrap(), r#"{"type":"mouse_move_abs","x":0.5,"y":0.25}"#);

        let f = C2H::Key { vk: 65, down: true, repeat: false };
        assert_eq!(serde_json::to_string(&f).unwrap(), r#"{"type":"key","vk":65,"down":true,"repeat":false}"#);

        let f = C2H::MouseButton { button: MouseButton::Right, down: false };
        assert_eq!(serde_json::to_string(&f).unwrap(), r#"{"type":"mouse_button","button":"right","down":false}"#);

        let f = H2C::AuthFail { reason: AuthFailReason::BadPassword, retry_after_ms: 1000 };
        assert_eq!(
            serde_json::to_string(&f).unwrap(),
            r#"{"type":"auth_fail","reason":"bad_password","retry_after_ms":1000}"#
        );

        let f = C2H::KeyChar(KeyInput { target: KeyTarget::Char { ch: '中' }, down: true });
        assert_eq!(
            serde_json::to_string(&f).unwrap(),
            r#"{"type":"key_char","target":{"kind":"char","ch":"中"},"down":true}"#
        );
    }

    #[test]
    fn video_set_defaults_are_sane() {
        let v = VideoSet::default();
        assert_eq!(v.fps, 30);
        assert_eq!(v.scale, 1.0);
        assert_eq!(v.quality, Quality::Auto);
        assert_eq!(v.chroma, "420");
    }

    /// 老控制端发的 video_set 没有 chroma/encoder，也必须能解析。
    #[test]
    fn old_video_set_without_new_fields_still_parses() {
        let s = r#"{"fps":60,"max_bitrate_kbps":3000,"quality":"hd","scale":1.0}"#;
        let v: VideoSet = serde_json::from_str(s).unwrap();
        assert_eq!(v.fps, 60);
        assert_eq!(v.chroma, "420");
        assert_eq!(v.encoder, "auto");
    }

    #[test]
    fn quality_presets_have_bitrate_caps_where_expected() {
        assert_eq!(quality_preset(Quality::High).1, 2_000);
        assert_eq!(quality_preset(Quality::Hd).1, 8_000);
        assert_eq!(quality_preset(Quality::Bluray).1, 20_000);
        assert_eq!(quality_preset(Quality::Auto).1, 0);
        // 档位越高质量要求越高（CQ 越小）
        assert!(quality_preset(Quality::Bluray).0 < quality_preset(Quality::Fast).0);
    }

    #[test]
    fn roundtrip_every_c2h_variant() {
        rt_c2h(C2H::Hello(ClientHello {
            os: "windows".into(),
            version: "0.1.0".into(),
            name: "ZNC-7G4D".into(),
            caps: vec![Cap::Clipboard, Cap::Files, Cap::Terminal],
        }));
        rt_c2h(C2H::Auth { password: "aB3dE5fG".into() });
        rt_c2h(C2H::MouseMoveAbs { x: 1.0, y: 0.0 });
        rt_c2h(C2H::MouseMoveRel { dx: -5, dy: 7 });
        rt_c2h(C2H::MouseButton { button: MouseButton::X2, down: true });
        rt_c2h(C2H::MouseWheel { dx: 0, dy: -120 });
        rt_c2h(C2H::Key { vk: 0x5b, down: true, repeat: true });
        rt_c2h(C2H::KeyChar(KeyInput { target: KeyTarget::plain(0x61), down: false }));
        rt_c2h(C2H::VideoSet(VideoSet { fps: 60, max_bitrate_kbps: 8000, quality: Quality::Hd, scale: 0.5, ..Default::default() }));
        rt_c2h(C2H::Clip { text: "中文 clipboard\n".into() });
        rt_c2h(C2H::ClipOffer { len: 1234 });
        rt_c2h(C2H::Fs(FsOp::List { id: 1, path: ":/".into() }));
        rt_c2h(C2H::Fs(FsOp::Put { id: 2, path: "~/a.bin".into(), size: 1 << 40 }));
        rt_c2h(C2H::Term(TermOp::Open { id: 1, cols: 120, rows: 30, shell: None }));
        rt_c2h(C2H::Term(TermOp::List));
        rt_c2h(C2H::PortMap(PortMapOp::Open { id: 1, target_host: "127.0.0.1".into(), target_port: 22 }));
        rt_c2h(C2H::Power { action: PowerAction::Reboot });
        rt_c2h(C2H::VideoFocus { x: 0.1, y: 0.2, w: 0.3, h: 0.4 });
        rt_c2h(C2H::Ping { seq: 9, sent_ms: 1_700_000_000_000 });
        rt_c2h(C2H::Bye { reason: "user closed window".into() });
    }

    #[test]
    fn roundtrip_every_h2c_variant() {
        rt_h2c(H2C::HelloOk(HostHelloOk {
            device_id: "123456789".into(),
            os: "kylin".into(),
            version: "0.1.0".into(),
            name: "KIRIN-01".into(),
            screen: Screen { w: 1920, h: 1080 },
            caps: vec![Cap::Clipboard, Cap::Metrics],
            has_desktop: true,
        }));
        rt_h2c(H2C::AuthOk);
        rt_h2c(H2C::AuthFail { reason: AuthFailReason::Busy, retry_after_ms: 60000 });
        rt_h2c(H2C::VideoCfg(HostVideoCfg {
            w: 1920,
            h: 1080,
            codec: "h264".into(),
            fps: 30,
            encoder: "libx264".into(),
            chroma: "420".into(),
            actual_fps: 29.7,
        }));
        rt_h2c(H2C::VideoError { message: "nvenc 初始化失败，已回退 libx264".into() });
        rt_h2c(H2C::Metrics(Metrics { cpu: 12.5, mem_used: 1 << 30, mem_total: 1 << 32, net_rx: 1, net_tx: 2, rtt_ms: 8 }));
        rt_h2c(H2C::Clip { text: "x".into() });
        rt_h2c(H2C::ClipOffer { len: 99 });
        rt_h2c(H2C::SystemState { locked: false, has_desktop: true });
        rt_h2c(H2C::InputError { message: "XTest 被拒绝".into() });
        rt_h2c(H2C::FsResult {
            id: 1,
            ok: true,
            error: None,
            entries: Some(vec![FsEntry { name: "a".into(), dir: false, size: 1, mtime: 0 }]),
            path: Some("~/".into()),
            hash: None,
        });
        rt_h2c(H2C::FsStream { id: 3, stage: FsStreamStage::Begin, total: Some(100), done: 0 });
        rt_h2c(H2C::FsStream { id: 3, stage: FsStreamStage::Done, total: Some(100), done: 100 });
        rt_h2c(H2C::TermOut { id: 1, data: "bHMK".into() });
        rt_h2c(H2C::TermExit { id: 1, code: 130 });
        rt_h2c(H2C::TermSessions { sessions: vec![TermSession { id: 1, title: "bash".into() }] });
        rt_h2c(H2C::PortMapOk { id: 1 });
        rt_h2c(H2C::PortMapErr { id: 1, error: "refused".into() });
        rt_h2c(H2C::Pong { seq: 1, sent_ms: 5, host_ms: 8 });
        rt_h2c(H2C::Bye { reason: "host quit".into() });
    }

    #[test]
    fn optional_fields_are_omitted_not_null() {
        let m = H2C::FsResult { id: 1, ok: false, error: Some("denied".into()), entries: None, path: None, hash: None };
        let s = serde_json::to_string(&m).unwrap();
        assert!(!s.contains("null"), "null 字段应该被省略: {s}");
        assert!(s.contains(r#""error":"denied""#));

        // done=0 也不该出现（老控制端不认识这个字段）
        let m = H2C::FsStream { id: 1, stage: FsStreamStage::Begin, total: None, done: 0 };
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(s, r#"{"type":"fs_stream","id":1,"stage":"begin"}"#);
    }
}
