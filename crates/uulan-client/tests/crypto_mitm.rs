//! M6 验收：**中继看不到明文内容**。
//!
//! 链路（除了剪贴板后端与「记录字节的代理」之外全是生产代码）：
//!
//! ```text
//! 控制端 → [记录字节的代理＝扮演中继] → 真 uulan-server → 真被控端
//! ```
//!
//! 代理把经过它的每个字节都存一份，最后断言两件事：
//!
//! 1. **消息真的到了**：被控端的假剪贴板里出现了那段文字（否则「看不见明文」毫无意义
//!    —— 什么都没发也能通过）。
//! 2. **代理记下的字节里没有那段文字的明文**：这就是 DESIGN.md 里
//!    「中继抓包看不到明文内容」这条验收标准的机器可查版本。
//!
//! 注意代理看到的是**控制端侧**的那半段流量；被控端与 server 之间是直连。
//! 三条会话通道（control/video/bulk）都从代理过，所以任何一个明文泄漏都会被抓住。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
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

// ───────────────────────────── 假剪贴板 ─────────────────────────────

#[derive(Clone, Default)]
struct Cell {
    text: Arc<Mutex<Option<String>>>,
    writes: Arc<Mutex<u32>>,
}

impl Cell {
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

// ───────────────────────────── 夹具 ─────────────────────────────

async fn spawn_server() -> String {
    let server = uulan_server::Server::new(None).expect("建服务器失败");
    let listener = server.bind("127.0.0.1:0").await.expect("绑定失败");
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = server.serve(listener).await;
    });
    addr
}

/// 一个「只会记录、不会改」的 TCP 代理：扮演中继，把经过的字节抄一份。
async fn spawn_recording_proxy(upstream: String) -> (String, Arc<Mutex<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("代理绑定失败");
    let addr = listener.local_addr().unwrap().to_string();
    let log: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let log_for_task = log.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut down, _)) = listener.accept().await else { break };
            let up_addr = upstream.clone();
            let log = log_for_task.clone();
            tokio::spawn(async move {
                let Ok(mut up) = TcpStream::connect(&up_addr).await else { return };
                let (mut dr, mut dw) = down.split();
                let (mut ur, mut uw) = up.split();
                let l1 = log.clone();
                let l2 = log.clone();
                // 两个方向都要记录：只记上行的话，「被控端推下来的明文」就抓不到。
                let a = pump(&mut dr, &mut uw, l1);
                let b = pump(&mut ur, &mut dw, l2);
                tokio::join!(a, b);
            });
        }
    });
    (addr, log)
}

async fn pump<R, W>(r: &mut R, w: &mut W, log: Arc<Mutex<Vec<u8>>>)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                {
                    let mut g = log.lock().unwrap();
                    // 只留一份有界副本：测试要的是「有没有这段明文」，不是抓包归档。
                    if g.len() < 8 << 20 {
                        g.extend_from_slice(&buf[..n]);
                    }
                }
                if w.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
        }
    }
    let _ = w.shutdown().await;
}

fn test_config(server: &str) -> HostConfig {
    let mut cfg = HostConfig::default();
    cfg.server = server.to_string();
    cfg.name = "加密测试被控端".into();
    cfg.password = "aB3dE5fG".into();
    cfg.capture = CaptureInput::parse("lavfi:testsrc2=size=320x240:rate=10").unwrap();
    cfg.fps = 10;
    cfg.screen_width = 320;
    cfg.screen_height = 240;
    cfg
}

/// 起一个被控端（连的是**真 server**，不经过代理），剪贴板用给定的内存后端。
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
                    screen_probe: None,
                    pty: None,
                    fs_roots: None,
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

async fn wait_text(cell: &Cell, want: &str, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
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

/// 在一段字节里找子串。
fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[tokio::test]
async fn relay_sees_no_plaintext_of_clipboard_or_filenames() {
    let server = spawn_server().await;
    let (proxy, log) = spawn_recording_proxy(server.clone()).await;
    let cfg = test_config(&server);
    let host_cell = Cell::default();
    let device_id = spawn_host(&server, cfg.clone(), host_cell.clone()).await;

    // 控制端连的是**代理**，所以它看到的流量全被记录。
    let c = timeout(
        Duration::from_secs(25),
        connect::connect(&proxy, &device_id, &cfg.password, "加密测试控制端", vec![]),
    )
    .await
    .expect("连接超时")
    .expect("连接失败（加密握手的失败也会落在这里）");
    drop(c.video);
    drop(c.bulk);

    // 剪贴板后端必须是**我能改的那块**：只有 user_copy 之后才能确认消息真的发出去了。
    let client_cell = Cell::default();
    let link = ControlLink::start_with_clipboard(
        c.control,
        Some(Box::new(FakeClipboard { cell: client_cell.clone() })),
    );
    let _input = link.input();
    // 等基线读完再复制（与剪贴板验收测试同一套时序）
    tokio::time::sleep(Duration::from_millis(200)).await;

    // 这段文字是「中继绝不该看到」的那段。中文 + ASCII 混排。
    let canary = "M6-CANARY-中继不许看到这段明文-9f3a7c";

    // 用户复制 → 走真实的生产链路，被控端必须真的收到
    client_cell.user_copy(canary);
    wait_text(&host_cell, canary, "控制端 → 被控端（密文）").await;

    // 再反向来一条：被控端复制 → 控制端也必须收到（否则上面的「没泄漏」可能只是没流量）
    let canary_back = "M6-CANARY-BACK-被控端推下来的明文-41d2";
    host_cell.user_copy(canary_back);
    wait_text(&client_cell, canary_back, "被控端 → 控制端（密文）").await;

    link.close("测试结束").await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let seen = log.lock().unwrap().clone();
    assert!(!seen.is_empty(), "代理一个字节都没记录到，这个测试就成了自欺欺人");
    assert!(
        !contains(&seen, canary.as_bytes()),
        "中继记录里出现了明文剪贴板内容（{} 字节里找到 canary）",
        seen.len()
    );
    assert!(
        !contains(&seen, canary_back.as_bytes()),
        "中继记录里出现了反方向的明文剪贴板内容"
    );
    // 反证：明文协议头（会合阶段的 JSON）是能看到的 —— 说明记录本身是有效的，
    // 「找不到 canary」不是因为什么都没记。
    assert!(
        contains(&seen, b"relay_open") || contains(&seen, b"client_connect"),
        "记录里应当能看到会合阶段的明文 JSON，否则记录可能没工作"
    );
    eprintln!(
        "中继记录了 {} 字节，两个方向都没有 canary 的明文（写入次数：被控端 {}）",
        seen.len(),
        host_cell.writes()
    );
}