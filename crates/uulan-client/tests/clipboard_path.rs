//! M2.5 验收：**剪贴板双向同步真的接上了**。
//!
//! 链路（除了「读写系统剪贴板」这一环之外，全是生产代码）：
//!
//! ```text
//! 控制端轮询本机剪贴板 → ClipSync → C2H::Clip → 中继盲转发
//!   → 被控端 control_loop → ClipSync → Clipboard::set_text（本测试里是内存剪贴板）
//! 反方向同样一条路，走 H2C::Clip。
//! ```
//!
//! 被替换掉的只有 `Clipboard` 后端（真机上分别是 Win32 与 X11），
//! 因为它需要真的桌面。**这正是本机能验证的最大范围**：所有去重、防回声、
//! 「第一拍不发」的逻辑都在生产代码里跑，不是替身。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::time::timeout;

use uulan_clip::Clipboard;
use uulan_client::connect;
use uulan_client::control::ControlLink;
use uulan_host::capture::CaptureInput;
use uulan_host::config::HostConfig;
use uulan_host::session::{self, HostState, SessionParams, VideoSettings};
use uulan_media::ffmpeg::{self, Encoder};
use uulan_protocol::rv::{Role, ToEndpoint};
use uulan_protocol::rvclient::RvConn;

/// 一块「系统剪贴板」的替身：内容和写入次数。
#[derive(Clone, Default)]
struct Cell {
    text: Arc<Mutex<Option<String>>>,
    writes: Arc<Mutex<u32>>,
}

impl Cell {
    fn with(text: &str) -> Self {
        let c = Self::default();
        c.user_copy(text);
        c
    }

    /// 模拟用户在本机按下 Ctrl+C。
    fn user_copy(&self, text: &str) {
        *self.text.lock().unwrap() = Some(text.to_string());
    }

    fn get(&self) -> Option<String> {
        self.text.lock().unwrap().clone()
    }

    fn writes(&self) -> u32 {
        *self.writes.lock().unwrap()
    }
}

struct FakeClipboard {
    cell: Cell,
}

impl Clipboard for FakeClipboard {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn available(&self) -> bool {
        true
    }

    fn get_text(&mut self) -> Option<String> {
        self.cell.get()
    }

    fn set_text(&mut self, text: &str) -> Result<(), String> {
        *self.cell.writes.lock().unwrap() += 1;
        *self.cell.text.lock().unwrap() = Some(text.to_string());
        Ok(())
    }
}

async fn spawn_server() -> String {
    let server = uulan_server::Server::new(None).expect("建服务器失败");
    let listener = server.bind("127.0.0.1:0").await.expect("绑定失败");
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = server.serve(listener).await;
    });
    addr
}

fn test_config(server: &str) -> HostConfig {
    let mut cfg = HostConfig::default();
    cfg.server = server.to_string();
    cfg.name = "剪贴板测试被控端".into();
    cfg.password = "aB3dE5fG".into();
    cfg.capture = CaptureInput::parse("lavfi:testsrc2=size=320x240:rate=10").unwrap();
    cfg.fps = 10;
    cfg.screen_width = 320;
    cfg.screen_height = 240;
    cfg
}

/// 起一个被控端，剪贴板用给定的内存后端。
async fn spawn_host(server: &str, cfg: HostConfig, cell: Cell) -> String {
    let mut conn = RvConn::connect(server).await.expect("被控端连不上中继");
    let (device_id, _) = conn
        .host_register(None, cfg.name.clone())
        .await
        .expect("注册失败");

    let state = Arc::new(HostState::new(cfg.clone(), None));
    let screen = session::resolve_screen(&cfg.capture, &cfg).await;
    let settings = VideoSettings::from_config(&cfg);
    let addr = conn.server_addr().to_string();
    let ffmpeg = ffmpeg::find_ffmpeg(None).unwrap_or_else(|| std::path::PathBuf::from("ffmpeg"));

    tokio::spawn(async move {
        loop {
            let msg = match conn.recv().await {
                Ok(m) => m,
                Err(_) => break,
            };
            if let ToEndpoint::Incoming { session: sid, client_addr, .. } = msg {
                let params = SessionParams {
                    state: state.clone(),
                    ffmpeg: ffmpeg.clone(),
                    capture: cfg.capture.clone(),
                    screen,
                    encoder: Encoder::X264,
                    settings,
                    clipboard: Some(Box::new(FakeClipboard { cell: cell.clone() })),
                };
                let a = addr.clone();
                tokio::spawn(async move {
                    if let Ok(c) = RvConn::connect(&a).await {
                        if let Ok(relays) = c.relay_open_all(&sid, Role::Host).await {
                            let _ = session::run(params, relays, client_addr).await;
                        }
                    }
                });
            }
        }
    });

    device_id
}

/// 等剪贴板变成指定内容。超时会带上实际内容，免得只看到一句「超时」。
async fn wait_text(cell: &Cell, want: &str, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if cell.get().as_deref() == Some(want) {
            return;
        }
        if Instant::now() > deadline {
            panic!("{what} 超时：期望 {want:?}，实际 {:?}", cell.get());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn clipboard_syncs_both_ways_without_echo() {
    let addr = spawn_server().await;
    let cfg = test_config(&addr);
    let host_cell = Cell::default();
    let client_cell = Cell::default();
    let device_id = spawn_host(&addr, cfg.clone(), host_cell.clone()).await;

    let c = timeout(
        Duration::from_secs(20),
        connect::connect(&addr, &device_id, &cfg.password, "剪贴板测试控制端", vec![]),
    )
    .await
    .expect("连接超时")
    .expect("连接失败");
    drop(c.video);
    drop(c.bulk);

    let link = ControlLink::start_with_clipboard(
        c.control,
        Some(Box::new(FakeClipboard { cell: client_cell.clone() })),
    );
    let _input = link.input();

    // 等「基线」读完再模拟复制：连接这一刻本机剪贴板里的内容按设计不算新内容
    // （见 session_start_does_not_push_pre_existing_clipboard），所以这里要跨过那一瞬。
    tokio::time::sleep(Duration::from_millis(150)).await;

    // ① 控制端复制 → 被控端粘得出来
    let out = "从控制端复制的：中文、\n换行、还有 emoji 🚀";
    client_cell.user_copy(out);
    wait_text(&host_cell, out, "控制端 → 被控端").await;

    // ② 被控端复制 → 控制端粘得出来（反方向必须同样通）
    let back = "被控端复制的内容 / host → client";
    host_cell.user_copy(back);
    wait_text(&client_cell, back, "被控端 → 控制端").await;

    // ③ 不许回声：两端各自只被「对面」写过一次。
    //    跑过若干个轮询周期再看，任何多写都是回声在刷。
    tokio::time::sleep(Duration::from_millis(1600)).await;
    assert_eq!(host_cell.writes(), 1, "被控端剪贴板被写了 {} 次（回声）", host_cell.writes());
    assert_eq!(client_cell.writes(), 1, "控制端剪贴板被写了 {} 次（回声）", client_cell.writes());
    // ② 之后被控端本机剪贴板就是用户刚复制的那段（`user_copy` 直接改，不经过 set_text），
    // 控制端已经同步成了同一段内容。
    assert_eq!(host_cell.get().as_deref(), Some(back));
    assert_eq!(client_cell.get().as_deref(), Some(back));

    let s = link.stats().snapshot();
    assert!(s.clips_sent >= 1, "控制端应当至少上报过一条剪贴板内容：{s:?}");
    assert!(s.clips_recv >= 1, "控制端应当至少收到过一条剪贴板内容：{s:?}");
    assert_eq!(s.clip_errors, 0, "内存后端不该写失败：{s:?}");

    link.close("测试结束").await;
}

/// 会话刚建立时，本机剪贴板里的旧内容**不准推给对面** —— 否则一连接就把
/// 对方正在用的剪贴板换掉，用户会以为对面乱粘东西。
#[tokio::test]
async fn session_start_does_not_push_pre_existing_clipboard() {
    let addr = spawn_server().await;
    let cfg = test_config(&addr);
    let host_cell = Cell::with("被控端连上之前就有的内容");
    let client_cell = Cell::with("控制端连上之前就有的内容");
    let device_id = spawn_host(&addr, cfg.clone(), host_cell.clone()).await;

    let c = timeout(
        Duration::from_secs(20),
        connect::connect(&addr, &device_id, &cfg.password, "剪贴板测试控制端", vec![]),
    )
    .await
    .expect("连接超时")
    .expect("连接失败");
    drop(c.video);
    drop(c.bulk);

    let link = ControlLink::start_with_clipboard(
        c.control,
        Some(Box::new(FakeClipboard { cell: client_cell.clone() })),
    );
    let _input = link.input();

    // 两个轮询周期（500ms × 2）之后，两边都必须还是自己原来的内容。
    tokio::time::sleep(Duration::from_millis(1400)).await;
    assert_eq!(host_cell.get().as_deref(), Some("被控端连上之前就有的内容"));
    assert_eq!(client_cell.get().as_deref(), Some("控制端连上之前就有的内容"));
    assert_eq!(host_cell.writes(), 0, "被控端剪贴板不该被会话初始化改写");
    assert_eq!(client_cell.writes(), 0, "控制端剪贴板不该被会话初始化改写");

    // 之后用户真的复制了新内容，同步必须照常工作（不能因为「没发」就卡死）。
    client_cell.user_copy("会话开始之后用户复制的新内容");
    wait_text(&host_cell, "会话开始之后用户复制的新内容", "初始化之后的新内容").await;

    link.close("测试结束").await;
}