//! 控制端：连接、握手、鉴权。
//!
//! 顺序（M6 起多了一步端到端握手，见 [`uulan_protocol::crypto`]）：
//!
//! ```text
//! 会合（中继）→ 三条 relay 连接 → CryptoHello 交换 → Hello/HelloOk → Auth/AuthOk
//! ```
//!
//! 握手之后三条连接上的**每个帧都是密文**，中继即使被拿下也看不到画面/键盘/文件。

use std::time::Duration;

use tokio::time::timeout;

use uulan_protocol::crypto::{self, CryptoError, SecConn, SecControl, SessionKeys};
use uulan_protocol::frame::{read_frame, write_frame, FrameKind};
use uulan_protocol::msg::{AuthFailReason, C2H, Cap, ClientHello, H2C, HostHelloOk};
use uulan_protocol::rv::{Channel, Role};
use uulan_protocol::rvclient::{RelaySet, RvConn, RvError};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("会合失败: {0}")]
    Rv(#[from] RvError),
    #[error("协议: {0}")]
    Frame(#[from] uulan_protocol::frame::FrameError),
    #[error("加密: {0}")]
    Crypto(#[from] CryptoError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("被控端拒绝了连接：{0}")]
    Rejected(String),
    #[error("被控端返回了意外的消息：{0}")]
    Unexpected(String),
    #[error("握手超时")]
    Timeout,
}

/// 一台被控端上报的自身信息。
pub use uulan_protocol::msg::HostHelloOk as HostInfo;

/// 一条已建立并已通过鉴权的会话。
///
/// 三个通道都是**已加密的连接**：`into_split()` 出来的两半自己加解密，
/// 上层 `read_frame` / `write_frame` 的用法一字未变。
#[derive(Debug)]
pub struct Connected {
    pub host: HostHelloOk,
    /// 控制通道：握过手的两半（`into_split()` 用法与以前一致）。
    pub control: SecControl,
    pub video: SecConn,
    /// 文件传输 / 端口映射的数据通道。M4 起使用。
    pub bulk: SecConn,
    /// 服务端下发的会话票据（排障用）。
    pub session: String,
}

impl Connected {
    /// 本次会话双方共同支持的能力（各自主张的交集）。
    pub fn agreed_caps(&self, mine: &[Cap]) -> Vec<Cap> {
        mine.iter().copied().filter(|c| self.host.caps.contains(c)).collect()
    }
}

/// 连到一台被控端：会合 → 三条通道 → 端到端握手 → 握手鉴权。
pub async fn connect(
    server: &str,
    device_id: &str,
    password: &str,
    client_name: &str,
    caps: Vec<Cap>,
) -> Result<Connected, ClientError> {
    let mut rv = RvConn::connect(server).await?;
    let (session, back_id, device_name) = rv.client_connect(device_id, client_name).await?;

    let relays = rv.relay_open_all(&session, Role::Client).await?;
    let RelaySet { control, video, bulk } = relays;
    let mut control = control;

    // 握手也要有超时：被控端可能连上了中继但自己的会话线程卡住。
    let (host, keys, ctl) = timeout(Duration::from_secs(15), async {
        // 0) 端到端握手。这一步是**明文**的（临时公钥本来就是公开信息），
        //    但它之后这条连接上每个帧都是密文，口令也从此不再上线。
        let keys: SessionKeys = crypto::client_handshake(&mut control).await?;
        // 握手之后就把控制通道包上加密层：Hello/Auth 也必须是密文，
        // 而且这两半之后要一直用到会话结束（计数器不能归零重来）。
        let mut ctl = SecControl::new(control, &keys.channel(Channel::Control), Role::Client);

        // 1) Hello
        send_c2h(
            &mut ctl.wr,
            &C2H::Hello(ClientHello {
                os: std::env::consts::OS.to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                name: client_name.to_string(),
                caps: caps.clone(),
            }),
        )
        .await?;

        // 2) HelloOk
        let host: H2C = match read_h2c(&mut ctl.rd).await? {
            Some(m) => m,
            None => return Err(ClientError::Unexpected("握手期间被控端断开".into())),
        };
        let host = match host {
            H2C::HelloOk(h) => h,
            H2C::Bye { reason } => return Err(ClientError::Rejected(reason)),
            other => return Err(ClientError::Unexpected(format!("{other:?}"))),
        };

        // 3) Auth：只发口令证明，不发口令本身。
        send_c2h(&mut ctl.wr, &C2H::Auth { proof: keys.client_proof(password) }).await?;

        let resp: H2C = match read_h2c(&mut ctl.rd).await? {
            Some(m) => m,
            None => return Err(ClientError::Unexpected("鉴权期间被控端断开".into())),
        };
        match resp {
            H2C::AuthOk { proof } => {
                // 校验对面的口令证明：不做这一步，中继就能冒充被控端收走所有输入。
                if !keys.verify_host_proof(password, &proof) {
                    return Err(ClientError::Rejected(
                        "被控端没能证明它知道口令（可能是中继在冒充）".to_string(),
                    ));
                }
            }
            H2C::AuthFail { reason, retry_after_ms } => {
                return Err(ClientError::Rejected(match reason {
                    AuthFailReason::BadPassword => "密码不正确".to_string(),
                    AuthFailReason::Locked => {
                        format!("被控端已锁定，请等 {} 秒后再试", retry_after_ms / 1000)
                    }
                    AuthFailReason::NotAllowed => "被控端不允许连接".to_string(),
                    AuthFailReason::Busy => "被控端正在被别人控制，请稍后再试".to_string(),
                }))
            }
            other => return Err(ClientError::Unexpected(format!("{other:?}"))),
        }

        // 4) 无关紧要的差异：设备名以被控端上报的为准
        let _ = (back_id, device_name);
        Ok((host, keys, ctl))
    })
    .await
    .map_err(|_| ClientError::Timeout)
    .and_then(|r| r)?;

    Ok(Connected {
        host,
        control: ctl,
        video: SecConn::new(video, &keys.channel(Channel::Video), Role::Client, Channel::Video),
        bulk: SecConn::new(bulk, &keys.channel(Channel::Bulk), Role::Client, Channel::Bulk),
        session,
    })
}

async fn read_h2c<R: tokio::io::AsyncRead + Unpin>(s: &mut R) -> Result<Option<H2C>, ClientError> {
    let f = match read_frame(s).await {
        Ok(f) => f,
        Err(uulan_protocol::frame::FrameError::Io(e))
            if e.kind() == std::io::ErrorKind::UnexpectedEof =>
        {
            return Ok(None)
        }
        Err(e) => return Err(e.into()),
    };
    if f.kind != FrameKind::Json {
        return Err(ClientError::Unexpected(format!("期望 JSON，收到 {:?}", f.kind)));
    }
    Ok(Some(f.decode_json()?))
}

pub async fn send_c2h<W: tokio::io::AsyncWrite + Unpin>(s: &mut W, m: &C2H) -> Result<(), ClientError> {
    write_frame(s, FrameKind::Json, &serde_json::to_vec(m)?).await?;
    Ok(())
}
