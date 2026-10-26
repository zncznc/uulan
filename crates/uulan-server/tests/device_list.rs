//! 设备目录（`ToServer::ListDevices` → `ToEndpoint::DeviceList`）的端到端测试。
//!
//! 只碰会合层：起真服务器、真 TCP、真 `RvConn`，不碰 X11 / FFmpeg / 会话。
//! 钉住四件现场会出问题的事：
//!   1. 谁在线、谁只是台账里的旧记录；
//!   2. 同一台机器不能因为「既在注册表又在台账里」出现两次；
//!   3. 被控端断开之后条目还在（名字查得到），但状态必须变离线；
//!   4. `--no-list` 时控制端拿到的是 `Denied`，不是「空目录」——
//!      后者会让人以为「一台设备都没有」，前者才说得清是策略。

use std::time::Duration;

use anyhow::{bail, Context, Result};
use uulan_protocol::rv::{DeviceEntry, ErrCode};
use uulan_protocol::rvclient::{RvConn, RvError};

/// 起一台服务器，返回它监听的地址。
async fn spawn(allow_list: bool) -> Result<String> {
    let server = uulan_server::Server::new_with_list(None, allow_list)?;
    let listener = server.bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?.to_string();
    tokio::spawn(async move {
        let _ = server.serve(listener).await;
    });
    Ok(addr)
}

/// 一台假被控端：注册后把会合连接**留在手里**（掉了就等于离线）。
struct FakeHost {
    /// 只是「拿在手里别掉」：这条连接一断，服务端就把这台设备判离线。
    _rv: RvConn,
    pub id: String,
}

impl FakeHost {
    async fn register(addr: &str, wanted: Option<&str>, name: &str) -> Result<Self> {
        let mut rv = RvConn::connect(addr).await?;
        let (id, _fresh) = rv.host_register(wanted.map(|s| s.to_string()), name.to_string(), None).await?;
        Ok(Self { _rv: rv, id })
    }
}

/// 拉一次目录。`RvConn` 用完就丢 —— 每次查询一条独立连接。
async fn list(addr: &str) -> Result<Vec<DeviceEntry>> {
    let mut rv = RvConn::connect(addr).await?;
    Ok(rv.list_devices().await?)
}

fn find<'a>(list: &'a [DeviceEntry], id: &str) -> &'a DeviceEntry {
    list.iter().find(|d| d.device_id == id).unwrap_or_else(|| panic!("目录里没有 {id}：{list:?}"))
}

#[tokio::test]
async fn empty_relay_reports_an_empty_directory() -> Result<()> {
    let addr = spawn(true).await?;
    let l = list(&addr).await.context("拉设备目录失败")?;
    assert!(l.is_empty(), "还没人注册就应该是空的，实际 {l:?}");
    Ok(())
}

#[tokio::test]
async fn registered_hosts_show_up_sorted_and_online() -> Result<()> {
    let addr = spawn(true).await?;
    let a = FakeHost::register(&addr, Some("805747111"), "麒麟-二号").await?;
    let b = FakeHost::register(&addr, Some("805747189"), "麒麟-一号").await?;

    let l = list(&addr).await?;
    assert_eq!(l.len(), 2, "{l:?}");
    // 按 device_id 升序：111 在 189 前面，跟注册顺序无关。
    assert_eq!(l[0].device_id, "805747111");
    assert_eq!(l[1].device_id, "805747189");
    for d in &l {
        assert!(d.online, "刚注册的设备必须在线：{d:?}");
        // 刚注册 → 空闲秒数是 0 或 1（不是编出来的大数）。
        assert!(matches!(d.idle_s, Some(0) | Some(1)), "空闲秒数异常：{d:?}");
    }
    assert_eq!(find(&l, "805747111").name, "麒麟-二号");
    assert_eq!(find(&l, "805747189").name, "麒麟-一号");

    // 拿到手的 ID 必须和请求的一致（服务端没换 ID）。
    assert_eq!(a.id, "805747111");
    assert_eq!(b.id, "805747189");
    Ok(())
}

#[tokio::test]
async fn a_disconnected_host_stays_listed_but_goes_offline() -> Result<()> {
    let addr = spawn(true).await?;
    let host = FakeHost::register(&addr, Some("805747189"), "会断的那台").await?;
    assert!(find(&list(&addr).await?, &host.id).online);

    // 断开：丢掉 RvConn，服务端那条控制连接读到 EOF → 摘掉注册。
    let id = host.id.clone();
    drop(host);

    // 服务端的清理是「读循环结束之后」做的，给几拍时间；别只 sleep 一次就断言，
    // 那在慢机器上会偶发假红，看起来像「离线判定坏了」。
    let mut entry = None;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(40)).await;
        let l = list(&addr).await?;
        let e = find(&l, &id).clone();
        if !e.online {
            entry = Some(e);
            break;
        }
    }
    let e = entry.context("断开 2 秒后仍显示在线")?;
    // 关键：台账里还留着名字 —— 现场「这台机器叫什么」正是靠它找回来的。
    assert_eq!(e.name, "会断的那台");
    assert_eq!(e.idle_s, None, "离线条目不该带空闲秒数");
    Ok(())
}

#[tokio::test]
async fn listing_can_be_switched_off_by_the_relay() -> Result<()> {
    let addr = spawn(false).await?;
    let mut rv = RvConn::connect(&addr).await?;
    match rv.list_devices().await {
        Err(RvError::Server { code, message }) => {
            assert_eq!(code, ErrCode::Denied);
            assert!(message.contains("--no-list"), "错误信息要说清怎么开：{message}");
        }
        Ok(l) => bail!("--no-list 下不该给出目录，实际 {l:?}"),
        Err(e) => bail!("期望 Server{{Denied}}，得到 {e:?}"),
    }
    // 关掉目录不影响别的查询：单台 Query 照旧（一条连接只答一条查询，这是既有约定，
    // 服务端答完就关 —— 所以这里必须新开一条，不能复用上面那条已经收到 Denied 的连接）。
    let mut q = RvConn::connect(&addr).await?;
    let (online, _name) = q.query("805747189").await?;
    assert!(!online);
    Ok(())
}

#[tokio::test]
async fn the_directory_never_carries_a_password() -> Result<()> {
    // 这条是「别把凭据塞进目录」的护栏：DeviceEntry 里只有 ID/名字/在线/空闲，
    // 一旦有人给它加了 `password` 字段，这里的 JSON 断言就会红。
    let addr = spawn(true).await?;
    let _host = FakeHost::register(&addr, Some("805747189"), "麒麟").await?;
    let l = list(&addr).await?;
    let json = serde_json::to_string(&l)?;
    for banned in ["password", "temp_code", "reg_token", "secret"] {
        assert!(!json.contains(banned), "设备目录里出现了 {banned}：{json}");
    }
    Ok(())
}