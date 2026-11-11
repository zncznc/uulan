//! 「这条消息是哪一种」—— 只回答**类名**，绝不带任何字段值。
//!
//! 为什么要有这一层：`H2C` / `C2H` / `ToEndpoint` 都 `#[derive(Debug)]`，而它们里面躺着
//! 用户内容与凭据：
//!
//! - `H2C::Clip { text }` —— 剪贴板原文；
//! - `H2C::TermOut { data }` —— 终端里的字（可能是口令、可能是别人的文档内容）；
//! - `H2C::FsStream { .. }` / `FsResult { .. }` —— 文件正文与路径；
//! - `ToEndpoint::Incoming { session }` / `ClientReady { session }` —— **会话票据原值**
//!   （32 hex，60 秒内能拨起中继通道）。
//!
//! 于是任何一行 `debug!(?msg, ...)` 在 `RUST_LOG=debug/trace` 下就是把这些东西写进 journal。
//! 而 journal 默认永久保留、工单里贴日志不会先脱敏 —— M6 的「日志只记字节数」这条规矩，
//! 靠人自觉守不住（2026-10-31 就抓到中继 sweep 把票据原值打出来过一次）。
//!
//! **规矩：要把一条消息写进日志，只能写 [`Shape::shape`]。**
//! 实现是 exhaustive match，新增变体不补这里就编译不过 —— 这是刻意的设计，
//! 让「新消息忘了考虑脱敏」变成构建期错误，而不是等真机日志里搜出来。
//!
//! 已知不需要这层的地方：`FrameKind`（纯帧型枚举，没有载荷）、`C2H` 的兜底日志
//! （`session.rs` 那里用的是 `std::mem::discriminant`，同样不带载荷，只是可读性差一点）。

use crate::msg::H2C;
use crate::rv::ToEndpoint;

/// 一条消息的**类名**。返回值是 `'static` 的字面量，不可能包含载荷。
pub trait Shape {
    fn shape(&self) -> &'static str;
}

impl Shape for H2C {
    fn shape(&self) -> &'static str {
        match self {
            Self::HelloOk(..) => "HelloOk",
            Self::AuthOk { .. } => "AuthOk",
            Self::AuthFail { .. } => "AuthFail",
            Self::VideoCfg(..) => "VideoCfg",
            Self::VideoError { .. } => "VideoError",
            Self::Metrics(..) => "Metrics",
            Self::Clip { .. } => "Clip",
            Self::ClipOffer { .. } => "ClipOffer",
            Self::SystemState { .. } => "SystemState",
            Self::InputError { .. } => "InputError",
            Self::FsResult { .. } => "FsResult",
            Self::FsStream { .. } => "FsStream",
            Self::TermOut { .. } => "TermOut",
            Self::TermExit { .. } => "TermExit",
            Self::TermSessions { .. } => "TermSessions",
            Self::TermError { .. } => "TermError",
            Self::PortMapOk { .. } => "PortMapOk",
            Self::PortMapErr { .. } => "PortMapErr",
            Self::Pong { .. } => "Pong",
            Self::AudioState { .. } => "AudioState",
            Self::Bye { .. } => "Bye",
        }
    }
}

impl Shape for ToEndpoint {
    fn shape(&self) -> &'static str {
        match self {
            Self::HostWelcome { .. } => "HostWelcome",
            Self::Incoming { .. } => "Incoming",
            Self::ClientReady { .. } => "ClientReady",
            Self::RelayReady { .. } => "RelayReady",
            Self::QueryResult { .. } => "QueryResult",
            Self::DeviceList { .. } => "DeviceList",
            Self::Pong { .. } => "Pong",
            Self::Error { .. } => "Error",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 哨兵：**能出现在 Debug 里、绝不能出现在 shape 里**的东西。
    const CANARY_CLIP: &str = "CANARY-clip-clipboard-原文";
    const CANARY_TERM: &str = "CANARY-term-ls -la /etc-passwd";
    const CANARY_TICKET: &str = "0123456789abcdef0123456789abcdef";

    /// 反面事实：`Debug` 派生确实会把载荷带出来。这条断言不是「测出来有问题」，
    /// 而是把**为什么必须有这一层**钉住 —— 哪天有人图省事写 `debug!(?msg)`，
    /// 这条测试就是证据。
    #[test]
    fn debug_derivation_really_does_leak_payloads() {
        let clip = H2C::Clip { text: CANARY_CLIP.into() };
        assert!(format!("{clip:?}").contains(CANARY_CLIP));
        let term = H2C::TermOut { id: 1, data: CANARY_TERM.into() };
        assert!(format!("{term:?}").contains(CANARY_TERM));
        let ready = ToEndpoint::ClientReady {
            session: CANARY_TICKET.into(),
            device_id: "805747189".into(),
            device_name: "麒麟真机".into(),
        };
        assert!(format!("{ready:?}").contains(CANARY_TICKET));
    }

    /// 正面事实：shape 只给类名，四类秘密一个都不带。
    #[test]
    fn shape_carries_only_the_variant_name() {
        let cases: Vec<(&str, String, H2C)> = vec![
            ("Clip", CANARY_CLIP.into(), H2C::Clip { text: CANARY_CLIP.into() }),
            (
                "TermOut",
                CANARY_TERM.into(),
                H2C::TermOut { id: 1, data: CANARY_TERM.into() },
            ),
            ("Bye", "对面挂了".into(), H2C::Bye { reason: "对面挂了".into() }),
        ];
        for (want, canary, m) in &cases {
            assert_eq!(m.shape(), *want);
            assert!(
                !m.shape().contains(canary),
                "shape() 不该带载荷：{}",
                m.shape()
            );
        }
        let ready = ToEndpoint::ClientReady {
            session: CANARY_TICKET.into(),
            device_id: "805747189".into(),
            device_name: "麒麟真机".into(),
        };
        assert_eq!(ready.shape(), "ClientReady");
        assert!(!ready.shape().contains(CANARY_TICKET));
        assert!(!ready.shape().contains("805747189"));
        let incoming = ToEndpoint::Incoming {
            session: CANARY_TICKET.into(),
            client_name: "张三的电脑".into(),
            client_addr: "192.168.223.7:51234".into(),
        };
        assert_eq!(incoming.shape(), "Incoming");
        assert!(!incoming.shape().contains(CANARY_TICKET));
    }

    /// 类名必须是 ASCII 标识符 —— 排障时能 grep，也不会把中文原文混进字段值。
    #[test]
    fn shape_names_are_greppable_identifiers() {
        for name in ["Clip", "TermOut", "ClientReady", "Incoming", "DeviceList"] {
            assert!(name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_'));
        }
    }
}
