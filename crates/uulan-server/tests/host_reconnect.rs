//! 被控端**重连**的回归测试：中继断过之后，同一台机器必须拿回**同一个设备 ID**。
//!
//! 真机上踩到的路径：中继升级/被 KILL 后拉起 → 被控端重连 → 服务端那边旧注册有时还在
//! `devices` 表里（旧 TCP 还没被读端回收），于是「这个 ID 已经有人注册」被当成「被别人占了」，
//! 给这台机器**分配一个新 ID**，而 `uulan-host` 会把它写回 `host.json`：
//! 现场手里那个设备 ID 当场失效，旧 ID 的注册还挂着（指向一个已死的推送通道）。
//!
//! 现在的判定是「注册令牌」（`ToServer::HostRegister.reg_token`）：同一台机器重连时出示
//! 同一个令牌 → 照收并替换旧注册；令牌不符或没给 → 不许顶掉在线设备
//! （这条安全边界由 `tests/rvclient.rs` 的 `live_duplicate_registration_does_not_steal_the_id`
//! 钉住）。所以这里锁三件事：
//! 1. 令牌一致的重连拿回同一个 ID；
//! 2. 旧注册被**替换**掉（旧连接被服务端关闭，新会话只推给新连接）；
//! 3. 旧连接的收尾不能把新注册踢掉（注册流水号）。

use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::net::TcpStream;
use tokio::time::timeout;
use uulan_protocol::frame::{read_frame, write_frame, FrameKind};
use uulan_protocol::rv::{ErrCode, ToEndpoint, ToServer};
use uulan_protocol::PROTO_VERSION;

const T: Duration = Duration::from_secs(5);
/// 两台不同「机器」的注册令牌。
const TOKEN_A: &str = "token-of-machine-a";
const TOKEN_B: &str = "token-of-machine-b";

async fn send(stream: &mut TcpStream, msg: &ToServer) -> Result<()> {
    write_frame(stream, FrameKind::Json, &serde_json::to_vec(msg)?).await?;
    Ok(())
}

async fn recv(stream: &mut TcpStream) -> Result<ToEndpoint> {
    let f = timeout(T, read_frame(stream)).await.context("读帧超时")??;
    if f.kind != FrameKind::Json {
        bail!("期望 json 帧，收到 {:?}", f.kind);
    }
    Ok(serde_json::from_slice(&f.payload)?)
}

async fn spawn_server() -> Result<String> {
    let server = uulan_server::Server::new(None)?;
    let listener = server.bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?.to_string();
    tokio::spawn(async move { server.serve(listener).await });
    Ok(addr)
}

/// 用给定的 device_id（None = 全新）与令牌注册一台假被控端。
async fn register_with(
    addr: &str,
    device_id: Option<String>,
    token: Option<&str>,
) -> Result<(TcpStream, String)> {
    let mut ctl = TcpStream::connect(addr).await?;
    send(
        &mut ctl,
        &ToServer::HostRegister {
            device_id,
            name: "假麒麟".into(),
            proto: PROTO_VERSION,
            reg_token: token.map(|t| t.to_string()),
        },
    )
    .await?;
    match recv(&mut ctl).await? {
        ToEndpoint::HostWelcome { device_id, .. } => Ok((ctl, device_id)),
        other => bail!("期望 host_welcome，收到 {other:?}"),
    }
}

#[tokio::test]
async fn reconnecting_host_keeps_its_device_id_and_replaces_the_old_registration() {
    let addr = spawn_server().await.unwrap();

    let (mut old_ctl, id) = register_with(&addr, None, Some(TOKEN_A)).await.unwrap();
    assert_eq!(id.len(), 9);

    // 关键场景：**旧连接还活着**（服务端还没发现它断了）时，同一台机器带着自己的 ID + 令牌回来。
    let (mut new_ctl, id2) = register_with(&addr, Some(id.clone()), Some(TOKEN_A)).await.unwrap();
    assert_eq!(id2, id, "重连必须拿回原来的设备 ID；拿回 {id2} 说明服务端把它当成了新机器");

    // 旧注册要被替换：服务端丢弃旧推送通道 → 旧连接读到 EOF（而不是继续挂着）。
    match timeout(T, read_frame(&mut old_ctl)).await {
        Ok(Ok(_)) => panic!("旧连接不该还能收到帧"),
        Ok(Err(_)) => {}
        Err(_) => panic!("旧连接没有被服务端关闭（等了 {T:?}）"),
    }

    // 新注册是唯一在线的那个：查询在线，而且新会话只推给新连接。
    let mut q = TcpStream::connect(&addr).await.unwrap();
    send(&mut q, &ToServer::Query { device_id: id.clone() }).await.unwrap();
    match recv(&mut q).await.unwrap() {
        ToEndpoint::QueryResult { online, .. } => assert!(online, "重连后应仍在线"),
        other => panic!("期望 query_result，收到 {other:?}"),
    }

    let mut cc = TcpStream::connect(&addr).await.unwrap();
    send(
        &mut cc,
        &ToServer::ClientConnect {
            device_id: id.clone(),
            client_name: "控制端".into(),
            proto: PROTO_VERSION,
        },
    )
    .await
    .unwrap();
    assert!(matches!(recv(&mut cc).await.unwrap(), ToEndpoint::ClientReady { .. }));
    match recv(&mut new_ctl).await.unwrap() {
        ToEndpoint::Incoming { .. } => {}
        other => panic!("新注册应收到 incoming，收到 {other:?}"),
    }

    // 旧连接的收尾（读到 EOF 自行退出）**不能**把新注册从表里摘掉。
    drop(old_ctl);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut q2 = TcpStream::connect(&addr).await.unwrap();
    send(&mut q2, &ToServer::Query { device_id: id.clone() }).await.unwrap();
    match recv(&mut q2).await.unwrap() {
        ToEndpoint::QueryResult { online, .. } => {
            assert!(online, "旧连接收尾后把新注册也摘掉了（设备会显示离线）")
        }
        other => panic!("期望 query_result，收到 {other:?}"),
    }
}

#[tokio::test]
async fn same_id_with_a_different_token_still_gets_a_new_id() {
    // 安全边界：知道别人的设备 ID 也顶不掉它（令牌不符 → 分配新 ID）。
    let addr = spawn_server().await.unwrap();
    let (_ctl_a, id) = register_with(&addr, None, Some(TOKEN_A)).await.unwrap();

    let (_ctl_b, id_b) = register_with(&addr, Some(id.clone()), Some(TOKEN_B)).await.unwrap();
    assert_ne!(id_b, id, "令牌不符的注册不该拿走在线设备 ID");

    // 不带令牌（老版本被控端）同样不许顶。
    let (_ctl_c, id_c) = register_with(&addr, Some(id.clone()), None).await.unwrap();
    assert_ne!(id_c, id, "没有令牌的注册不该拿走在线设备 ID");

    // 原来那台还在线。
    let mut q = TcpStream::connect(&addr).await.unwrap();
    send(&mut q, &ToServer::Query { device_id: id.clone() }).await.unwrap();
    match recv(&mut q).await.unwrap() {
        ToEndpoint::QueryResult { online, .. } => assert!(online),
        other => panic!("期望 query_result，收到 {other:?}"),
    }
}

#[tokio::test]
async fn illegal_device_id_is_still_replaced_and_proto_mismatch_still_refused() {
    // 原有的两条边界不能被「重连照收」带掉。
    let addr = spawn_server().await.unwrap();
    let (_a, _id) = register_with(&addr, None, Some(TOKEN_A)).await.unwrap();

    let (_ctl, id) = register_with(&addr, Some("012345678".into()), Some(TOKEN_A)).await.unwrap();
    assert_ne!(id, "012345678");
    assert!(id.bytes().all(|b| b.is_ascii_digit()));

    let mut bad = TcpStream::connect(&addr).await.unwrap();
    send(
        &mut bad,
        &ToServer::HostRegister {
            device_id: None,
            name: "x".into(),
            proto: 999,
            reg_token: None,
        },
    )
    .await
    .unwrap();
    match recv(&mut bad).await.unwrap() {
        ToEndpoint::Error { code, .. } => assert_eq!(code, ErrCode::ProtoMismatch),
        other => panic!("期望 proto_mismatch，收到 {other:?}"),
    }
}