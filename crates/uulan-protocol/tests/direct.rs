//! 直连传输（不经中继）的回路测试。
//!
//! 为什么值得单独立一个文件：直连最容易犯的错不是连不上，而是**连错了通道** ——
//! 四条 TCP 连接靠各自开头的声明归位，一旦哪天声明与装配对不上，症状是
//! "画面黑屏/文件传坏"，现场极难归因到这里。这里用真 TCP 回环把映射钉死。

use std::time::Duration;

use tokio::net::TcpListener;
use uulan_protocol::frame::{read_frame, write_frame, FrameKind};
use uulan_protocol::rv::{Channel, Role};
use uulan_protocol::rvclient::{direct_accept_all, direct_dial_all};

#[tokio::test]
async fn four_channels_land_in_the_right_slots() {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();

    let host = tokio::spawn(async move { direct_accept_all(&l).await });
    let mut dial = direct_dial_all(&addr).await.unwrap();

    let (mut got, peer) = host.await.unwrap().unwrap();
    assert!(peer.ip().is_loopback(), "对端地址要能记进日志：{peer}");

    // 四条通道逐条验映射：控制端在第 k 条上写的标记，必须出现在被控端的第 k 条上。
    for (name, c, v) in [
        ("control", &mut dial.control, &mut got.control),
        ("video", &mut dial.video, &mut got.video),
        ("bulk", &mut dial.bulk, &mut got.bulk),
        ("audio", &mut dial.audio, &mut got.audio),
    ] {
        write_frame(c, FrameKind::Json, name.as_bytes()).await.unwrap();
        let f = read_frame(v).await.unwrap();
        assert_eq!(
            f.payload,
            name.as_bytes(),
            "{name} 通道串到了别的通道上 —— 直连的通道映射错了"
        );
    }
}

#[tokio::test]
async fn half_dialed_peer_fails_fast_instead_of_hanging_the_listener() {
    // 只拨两条就断掉：监听侧必须**当场报错**并回去接下一个，而不是攥着半套连接等下去。
    // 没有这一条，任何人拨两条晾着就能把直连端口占死 —— 自己造的拒绝服务。
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let host = tokio::spawn(async move { direct_accept_all(&l).await });

    let mut s1 = tokio::net::TcpStream::connect(&addr).await.unwrap();
    write_frame(
        &mut s1,
        FrameKind::Json,
        &serde_json::to_vec(&uulan_protocol::rv::ToServer::DirectOpen {
            channel: Channel::Control,
            role: Role::Client,
        })
        .unwrap(),
    )
    .await
    .unwrap();
    // 读完被控端的回执再消失，模拟"连了一条就跑"。
    let _ = read_frame(&mut s1).await.unwrap();
    drop(s1);

    // 这里断言的是**上界**：监听侧最迟在收集超时（5 秒）之后放手，绝不永久攥着。
    let r = tokio::time::timeout(std::time::Duration::from_secs(9), host)
        .await
        .expect("收集超时之后必须放手（永久攥着半套连接就是自己造的拒绝服务）")
        .unwrap();
    assert!(r.is_err(), "半套通道不该被当成一次完整会话");
}

#[tokio::test]
async fn a_non_uulan_peer_is_rejected_at_the_first_frame() {
    // 有人把这个端口当普通服务连（或端口扫描）：第一帧不是 direct_open 就必须立刻失败。
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let host = tokio::spawn(async move { direct_accept_all(&l).await });

    let mut s = tokio::net::TcpStream::connect(&addr).await.unwrap();
    write_frame(&mut s, FrameKind::Json, b"GET / HTTP/1.1").await.unwrap();
    drop(s);

    let r = tokio::time::timeout(std::time::Duration::from_secs(5), host)
        .await
        .expect("不该永久卡住")
        .unwrap();
    assert!(r.is_err(), "非 uulan 的第一帧必须被拒");
}

#[tokio::test]
async fn an_idle_port_is_not_a_half_dialed_visitor() {
    // 两件事一起钉住，因为它们是同一个实现选择的两个后果：
    // ① 没人来连时**不许**判「半途而废」——旧实现每 5 秒编一条假事件，
    //    真机日志里就是这么刷的（会话进行中每 5 秒一条），真事件反而被埋掉；
    // ② 晚来的控制端必须拿到**完整的**收齐窗口 —— 旧实现的期限从 accept() 起算，
    //    恰好在窗口末尾连进来只剩零点几秒，那是真丢连接。
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let host = tokio::spawn(async move { direct_accept_all(&l).await });

    // 空闲 6 秒：旧实现在第 5 秒就已经超时返回了（这条断言就是回归判据）。
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(
        !host.is_finished(),
        "没人来连却判成「半途而废」：把「没人来」说成了「有人来又走」"
    );

    // 等到这一刻才拨号（已经过了 5 秒）：期限必须从这一条起算，四条照常收齐。
    let mut dial = direct_dial_all(&addr).await.expect("晚来的控制端必须连得上");
    let (mut got, _peer) = host.await.unwrap().expect("四条到齐就该成一次会话");
    write_frame(&mut dial.control, FrameKind::Json, b"late").await.unwrap();
    let f = read_frame(&mut got.control).await.unwrap();
    assert_eq!(&f.payload[..], &b"late"[..], "晚来的会话通道映射也必须准");
}
