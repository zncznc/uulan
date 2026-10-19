//! 控制端：连接、握手、鉴权。

use std::time::Duration;

use tokio::net::TcpStream;
use tokio::time::timeout;

use uulan_protocol::frame::{read_frame, write_frame, FrameKind};
use uulan_protocol::msg::{AuthFailReason, C2H, Cap, ClientHello, H2C, HostHelloOk};
use uulan_protocol::rv::Role;
use uulan_protocol::rvclient::{RelaySet, RvConn, RvError};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("会合失败: {0}")]
    Rv(#[from] RvError),
    #[error("协议: {0}")]
    Frame(#[from] uulan_protocol::frame::FrameError),
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
#[derive(Debug)]
pub struct Connected {
    pub host: HostHelloOk,
    pub control: TcpStream,
    pub video: TcpStream,
    /// 文件传输 / 端口映射的数据通道。M4 起使用。
    pub bulk: TcpStream,
    /// 服务端下发的会话票据（排障用）。
    pub session: String,
}

impl Connected {
    /// 本次会话双方共同支持的能力（各自主张的交集）。
    pub fn agreed_caps(&self, mine: &[Cap]) -> Vec<Cap> {
        mine.iter().copied().filter(|c| self.host.caps.contains(c)).collect()
    }
}

/// 连到一台被控端：会合 → 三条通道 → 握手 → 鉴权。
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
    timeout(Duration::from_secs(15), async {
        // 1) Hello
        send_c2h(
            &mut control,
            &C2H::Hello(ClientHello {
                os: std::env::consts::OS.to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                name: client_name.to_string(),
                caps: caps.clone(),
            }),
        )
        .await?;

        // 2) HelloOk
        let host: H2C = match read_h2c(&mut control).await? {
            Some(m) => m,
            None => return Err(ClientError::Unexpected("握手期间被控端断开".into())),
        };
        let host = match host {
            H2C::HelloOk(h) => h,
            H2C::Bye { reason } => return Err(ClientError::Rejected(reason)),
            other => return Err(ClientError::Unexpected(format!("{other:?}"))),
        };

        // 3) Auth
        send_c2h(&mut control, &C2H::Auth { password: password.to_string() }).await?;

        let resp: H2C = match read_h2c(&mut control).await? {
            Some(m) => m,
            None => return Err(ClientError::Unexpected("鉴权期间被控端断开".into())),
        };
        match resp {
            H2C::AuthOk => {}
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
        Ok(host)
    })
    .await
    .map_err(|_| ClientError::Timeout)
    .and_then(|r| r)
    .map(|host| Connected { host, control, video, bulk, session })
}

async fn read_h2c(s: &mut TcpStream) -> Result<Option<H2C>, ClientError> {
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

pub async fn send_c2h(s: &mut TcpStream, m: &C2H) -> Result<(), ClientError> {
    write_frame(s, FrameKind::Json, &serde_json::to_vec(m)?).await?;
    Ok(())
}
