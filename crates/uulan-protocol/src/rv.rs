//! 会合（rendezvous）协议：被控端 / 控制端 ↔ 中继服务器。
//!
//! 中继服务器是**盲转发**的：它只按 `session` + `channel` 把两条 TCP 连接对接起来，
//! 然后 `copy_bidirectional`。它不持有设备密码，也不需要理解控制/视频/文件内容。
//! 访问控制全部由被控端自己做（见 [`crate::msg::C2H::Auth`]）。
//!
//! ## 时序
//!
//! ```text
//! host                              server                        client
//!  |-- HostRegister(device_id?) --->  |
//!  |<-- HostWelcome(device_id) -----  |      (首次连接时下发 9 位 ID，被控端自己存)
//!  |                                  |  <---- ClientConnect(device_id) --
//!  |<-- Incoming(session) ----------- |  ---- ClientReady(session) ----->
//!  |-- RelayOpen(session,"control") -> |  <--- RelayOpen(session,"control") --
//!  |<=========== 服务器盲转发，后续全是 uulan 帧 ===========>|
//! ```
//!
//! 被控端收到 `Incoming` 后才拨出 relay 连接 —— 平时只有一条控制连接，不占带宽。

use serde::{Deserialize, Serialize};

/// 一条会话里被拆开的逻辑通道。三条连接都由两端主动拨出，不需要任何入站端口。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    /// 输入事件、剪贴板、终端 I/O、RPC。小而密，必须低延迟。
    Control,
    /// H.264 访问单元。高带宽，可丢弃，拥塞时只影响画面。
    Video,
    /// 文件传输、端口映射的数据。突发大流量，绝不能和输入抢 TCP 队列。
    Bulk,
    /// 音频（二期）。先占位，服务端按名字配对，不需要改代码。
    Audio,
}

/// 端点 → 服务器。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToServer {
    /// 被控端上线自报家门。`device_id` 为 None 表示首次连接，请服务端分配。
    HostRegister {
        #[serde(skip_serializing_if = "Option::is_none")]
        device_id: Option<String>,
        name: String,
        proto: u32,
    },
    /// 控制端请求连到某个设备。
    ClientConnect { device_id: String, client_name: String, proto: u32 },
    /// 两端各自拨出一条通道连接，用 session 配对。
    RelayOpen { session: String, channel: Channel, role: Role },
    /// 客户端查询设备在线状态（不发起会话）。
    Query { device_id: String },
    Ping,
    /// 被控端更新自己的显示名 / 能力。
    HostUpdate {
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Host,
    Client,
}

/// 服务器 → 端点。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToEndpoint {
    /// 被控端注册成功。`device_id` 是它在内网里的身份，应当持久化到本机配置。
    HostWelcome { device_id: String, name: String, server_proto: u32 },
    /// 有人要连你，请拨出三条 relay 连接。
    Incoming { session: String, client_name: String, client_addr: String },
    /// 控制端拿到会话票据，接着拨 relay 连接。
    ClientReady { session: String, device_id: String, device_name: String },
    /// 配对成功，这条连接后面就是纯字节流了（服务器不再发 JSON）。
    RelayReady { channel: Channel },
    QueryResult { device_id: String, online: bool, #[serde(skip_serializing_if = "Option::is_none")] name: Option<String> },
    Pong { server_time_ms: u64 },
    Error { code: ErrCode, message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrCode {
    /// 设备未注册 / 不在线。
    Offline,
    /// 设备 ID 格式非法。
    BadDeviceId,
    /// 会话票据不存在或已过期（默认 60 秒）。
    BadSession,
    /// 协议版本不匹配。
    ProtoMismatch,
    /// 同一个 channel 重复拨号。
    DuplicateChannel,
    /// 服务端内部错误。
    Internal,
}

impl ToServer {
    pub fn to_frame(&self) -> Result<crate::Frame, crate::frame::FrameError> {
        crate::Frame::json(crate::FrameKind::Json, self)
    }

    pub fn from_frame(f: &crate::Frame) -> Result<Self, crate::frame::FrameError> {
        f.decode_json()
    }
}

impl ToEndpoint {
    pub fn to_frame(&self) -> Result<crate::Frame, crate::frame::FrameError> {
        crate::Frame::json(crate::FrameKind::Json, self)
    }

    pub fn from_frame(f: &crate::Frame) -> Result<Self, crate::frame::FrameError> {
        f.decode_json()
    }
}

/// 生成一个 9 位数字设备 ID（首位非 0）。
pub fn gen_device_id() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let mut s = String::with_capacity(crate::DEVICE_ID_LEN);
    s.push(char::from_digit(rng.gen_range(1..=9), 10).unwrap());
    for _ in 1..crate::DEVICE_ID_LEN {
        s.push(char::from_digit(rng.gen_range(0..=9), 10).unwrap());
    }
    s
}

/// 校验设备 ID 格式：必须是 [`crate::DEVICE_ID_LEN`] 位纯数字且首位非 0。
///
/// 刻意严格 —— 用户从别人那里抄来的 ID 多一位少一位要立刻报错，
/// 而不是当成一个「查不到的设备」再去远程查一圈。
pub fn is_valid_device_id(s: &str) -> bool {
    s.len() == crate::DEVICE_ID_LEN
        && s.bytes().all(|b| b.is_ascii_digit())
        && !s.starts_with('0')
}

/// 生成会话票据。32 个十六进制字符，只在服务器内存里活 60 秒。
pub fn gen_session() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_shape() {
        for _ in 0..200 {
            let id = gen_device_id();
            assert_eq!(id.len(), crate::DEVICE_ID_LEN);
            assert!(is_valid_device_id(&id), "{id}");
            assert!(!id.starts_with('0'));
        }
    }

    #[test]
    fn device_id_validation_rejects_junk() {
        assert!(!is_valid_device_id("12345678"));    // 太短
        assert!(!is_valid_device_id("1234567890"));  // 太长
        assert!(!is_valid_device_id("012345678"));   // 首位 0
        assert!(!is_valid_device_id("12345678a"));   // 非数字
        assert!(!is_valid_device_id(""));
        assert!(is_valid_device_id("100000000"));
        assert!(is_valid_device_id("999999999"));
    }

    #[test]
    fn session_is_unique_and_hex() {
        let a = gen_session();
        let b = gen_session();
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn wire_shapes() {
        assert_eq!(
            serde_json::to_string(&ToServer::HostRegister { device_id: None, name: "K".into(), proto: 1 }).unwrap(),
            r#"{"type":"host_register","name":"K","proto":1}"#
        );
        assert_eq!(
            serde_json::to_string(&ToServer::RelayOpen {
                session: "ab".into(),
                channel: Channel::Video,
                role: Role::Host
            })
            .unwrap(),
            r#"{"type":"relay_open","session":"ab","channel":"video","role":"host"}"#
        );
        assert_eq!(
            serde_json::to_string(&ToEndpoint::Error { code: ErrCode::Offline, message: "设备不在线".into() }).unwrap(),
            r#"{"type":"error","code":"offline","message":"设备不在线"}"#
        );
    }

    #[test]
    fn roundtrip_all_variants() {
        for m in [
            ToServer::HostRegister { device_id: Some("123456789".into()), name: "n".into(), proto: 1 },
            ToServer::HostRegister { device_id: None, name: "n".into(), proto: 1 },
            ToServer::ClientConnect { device_id: "123456789".into(), client_name: "c".into(), proto: 1 },
            ToServer::RelayOpen { session: "s".into(), channel: Channel::Control, role: Role::Client },
            ToServer::RelayOpen { session: "s".into(), channel: Channel::Bulk, role: Role::Host },
            ToServer::RelayOpen { session: "s".into(), channel: Channel::Audio, role: Role::Host },
            ToServer::Query { device_id: "123456789".into() },
            ToServer::Ping,
            ToServer::HostUpdate { name: Some("x".into()) },
            ToServer::HostUpdate { name: None },
        ] {
            let s = serde_json::to_string(&m).unwrap();
            let back: ToServer = serde_json::from_str(&s).unwrap();
            assert_eq!(s, serde_json::to_string(&back).unwrap());
        }

        for m in [
            ToEndpoint::HostWelcome { device_id: "123456789".into(), name: "n".into(), server_proto: 1 },
            ToEndpoint::Incoming { session: "s".into(), client_name: "c".into(), client_addr: "10.0.0.5:1234".into() },
            ToEndpoint::ClientReady { session: "s".into(), device_id: "123456789".into(), device_name: "n".into() },
            ToEndpoint::RelayReady { channel: Channel::Control },
            ToEndpoint::QueryResult { device_id: "123456789".into(), online: true, name: Some("n".into()) },
            ToEndpoint::QueryResult { device_id: "123456789".into(), online: false, name: None },
            ToEndpoint::Pong { server_time_ms: 12345 },
            ToEndpoint::Error { code: ErrCode::BadSession, message: "m".into() },
        ] {
            let s = serde_json::to_string(&m).unwrap();
            let back: ToEndpoint = serde_json::from_str(&s).unwrap();
            assert_eq!(s, serde_json::to_string(&back).unwrap());
        }
    }
}