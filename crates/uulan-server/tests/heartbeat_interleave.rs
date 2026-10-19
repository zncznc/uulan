//! 回归测试：**心跳和「有人要连你」同时到达时，被控端不能自己判自己违规。**
//!
//! 真机上出过一次：控制端一发起连接，被控端立刻打
//! `心跳失败 error=期望 Pong，收到 Incoming {...}` 然后退出进程。
//!
//! 根因有两层，两层都被这里的断言覆盖：
//!
//! 1. **语义层**：被控端上线后，同一条会合连接上既有服务端主动推的 `Incoming`，
//!    也有心跳的 `Pong`。所以「发 Ping 就死等 Pong，来了别的就报错」这个模型
//!    本身就是错的 —— 报错是必然的，只是早晚的问题。
//! 2. **并发层**：旧的实现把这个连接放进 `tokio::select!`，一边等 `recv()`
//!    一边等心跳定时器。`select!` 会丢弃没被选中的 future，于是读到一半的帧
//!    被砍断、已进缓冲的字节永久丢失。`read_exact` 不是取消安全的，
//!    这个坑没法靠「加容错」补，只能把读和写彻底分开。
//!
//! 因此本测试锁的是 [`RvConn::into_split`] 出来的读写两半的行为：
//! **读端按到达顺序交出所有消息、永不因「不是 Pong」而报错**；
//! **写端只写不读，发心跳不会抢走读端的字节**。

use std::time::Duration;

use anyhow::Result;
use tokio::net::TcpListener;

use uulan_protocol::frame::{read_frame, write_frame, FrameKind};
use uulan_protocol::rv::{ToEndpoint, ToServer};
use uulan_protocol::rvclient::RvConn;

/// 起一个假中继：注册成功之后，按给定顺序把这些消息推给被控端。
///
/// 用假中继而不是真服务器，是因为这里要精确控制「Pong 与 Incoming 的先后」，
/// 真服务器的时间点不受测试摆布。
async fn fake_relay(push_after_register: Vec<ToEndpoint>) -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?.to_string();

    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept 失败");

        // 收 HostRegister
        let f = read_frame(&mut sock).await.expect("读注册帧失败");
        let reg: ToServer = serde_json::from_slice(&f.payload).expect("解析注册帧失败");
        assert!(
            matches!(reg, ToServer::HostRegister { .. }),
            "第一条应该是 HostRegister，实际 {reg:?}"
        );

        // 回 HostWelcome
        let welcome = serde_json::to_vec(&ToEndpoint::HostWelcome {
            device_id: "123456789".into(),
            name: "麒麟-01".into(),
            server_proto: uulan_protocol::PROTO_VERSION,
        })
        .unwrap();
        write_frame(&mut sock, FrameKind::Json, &welcome).await.unwrap();

        // 按脚本推消息
        for m in push_after_register {
            let b = serde_json::to_vec(&m).unwrap();
            write_frame(&mut sock, FrameKind::Json, &b).await.expect("推送失败");
        }

        // 持续排空读方向，直到对端关闭。
        //
        // **不能只读一帧就走人**：被控端会往这条连接上发心跳，如果这里留着
        // 未读的字节就 close，内核会发 RST，对端缓冲区里还没被应用读走的消息
        // （比如刚刚推过去的 Incoming）会一并被丢弃，表现为「莫名其妙断连」。
        loop {
            if read_frame(&mut sock).await.is_err() {
                break;
            }
        }
    });

    Ok(addr)
}

/// 注册后连着来 Pong→Incoming→Pong：读端必须原样按序交出三条，一条都不能报错。
#[tokio::test]
async fn reader_delivers_pong_and_incoming_in_order_without_error() -> Result<()> {
    let incoming = ToEndpoint::Incoming {
        session: "sess-abc".into(),
        client_name: "Windows控制端".into(),
        client_addr: "127.0.0.1:5555".into(),
    };
    let addr = fake_relay(vec![
        ToEndpoint::Pong { server_time_ms: 1 },
        incoming.clone(),
        ToEndpoint::Pong { server_time_ms: 2 },
    ])
    .await?;

    let mut conn = RvConn::connect(&addr).await?;
    let (id, _) = conn.host_register(None, "麒麟-01".into()).await?;
    assert_eq!(id, "123456789");

    let (mut reader, mut writer) = conn.into_split();

    // 读端逐个取。注意：**没有任何一步要求「下一条必须是 Pong」**，
    // 这正是旧实现错的地方。
    match reader.recv().await? {
        ToEndpoint::Pong { server_time_ms } => assert_eq!(server_time_ms, 1),
        other => panic!("第 1 条应是 Pong，得到 {other:?}"),
    }
    match reader.recv().await? {
        ToEndpoint::Incoming { session, client_name, .. } => {
            // 收到 Incoming 必须被当成正常事件，而不是「协议违规」
            assert_eq!(session, "sess-abc");
            assert_eq!(client_name, "Windows控制端");
        }
        other => panic!("第 2 条应是 Incoming，得到 {other:?}"),
    }
    match reader.recv().await? {
        ToEndpoint::Pong { server_time_ms } => assert_eq!(server_time_ms, 2),
        other => panic!("第 3 条应是 Pong，得到 {other:?}"),
    }

    // 写端发心跳不应该影响读端已经收到的顺序
    writer.ping().await?;
    Ok(())
}

/// 心跳（写）与收推送（读）可以**同时**进行：写端发 Ping 不会吃掉读端的字节。
///
/// 这条对着旧的 `select!` 实现是过不了的 —— 那里的「发 Ping 并等 Pong」
/// 会直接从同一个 socket 读，把本该交给读端的消息抢走。
#[tokio::test]
async fn heartbeat_write_does_not_steal_the_readers_bytes() -> Result<()> {
    let addr = fake_relay(vec![ToEndpoint::Incoming {
        session: "sess-xyz".into(),
        client_name: "控制端".into(),
        client_addr: "127.0.0.1:6666".into(),
    }])
    .await?;

    let mut conn = RvConn::connect(&addr).await?;
    conn.host_register(None, "麒麟-01".into()).await?;
    let (mut reader, mut writer) = conn.into_split();

    // 先连发几次心跳（只写），再读 —— 读端应当仍能拿到完整的那一条 Incoming。
    for _ in 0..3 {
        writer.ping().await?;
    }
    tokio::time::sleep(Duration::from_millis(80)).await;

    match reader.recv().await? {
        ToEndpoint::Incoming { session, .. } => assert_eq!(session, "sess-xyz"),
        other => panic!("应收到 Incoming，得到 {other:?}"),
    }
    Ok(())
}

/// 服务端推 `Error` 时，读端把它当**一条消息**交出来，而不是让整个进程报错退出。
///
/// 旧的 ping 路径用 `recv_ok()`，会把 Error 转成 Err 并冒泡到主循环，
/// 于是一条「某次请求失败」的通知就能把常驻的被控端打死。
#[tokio::test]
async fn error_push_is_reported_as_a_message_not_a_fatal_error() -> Result<()> {
    let addr = fake_relay(vec![ToEndpoint::Error {
        code: uulan_protocol::rv::ErrCode::Internal,
        message: "测试用错误".into(),
    }])
    .await?;

    let mut conn = RvConn::connect(&addr).await?;
    conn.host_register(None, "麒麟-01".into()).await?;
    let (mut reader, _writer) = conn.into_split();

    match reader.recv().await? {
        ToEndpoint::Error { message, .. } => assert_eq!(message, "测试用错误"),
        other => panic!("应收到 Error，得到 {other:?}"),
    }
    Ok(())
}
