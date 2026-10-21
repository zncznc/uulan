//! M4 终端验收：**两端真的接上了**，而且走的是真 TCP + 真协议。
//!
//! 链路（除了 PTY 本身，全是生产代码）：
//!
//! ```text
//! 控制端 TermLink（真 base64 编解码、真 JSON 帧）
//!   → C2H::Term → 中继盲转发 → 被控端 control_loop → term::run（真注册表/切帧/收尾）
//!   → PtyFactory → 假 shell（回显 + 认 exit/burst/size） → H2C::TermOut/TermExit
//! ```
//!
//! 为什么用假 shell 而不是真 PTY：这台开发机没有能跑的 PTY 路径（`portable-pty` 的 unix 后端
//! 要 `/dev/ptmx`，ConPTY 又只有 Windows 才有）。假后端换掉的只是「谁来产生这些字节」，
//! **协议、切帧、id 注册表、退出码上报、收尾这几块是真的**。真 PTY 的 termios / 信号行为
//! 只能上麒麟验 —— 这一点在 DESIGN 里单列，不混为一谈。
//!
//! 被控端用 `lavfi` 合成画面（本机没有 X11），和文件/端口映射测试相同。

use std::sync::Arc;
use std::time::Duration;

use tokio::time::timeout;

use uulan_client::connect;
use uulan_client::termx::{Ev, Options, TermLink};
use uulan_host::capture::CaptureInput;
use uulan_host::config::HostConfig;
use uulan_host::session::{self, HostState, SessionParams, VideoSettings};
use uulan_host::term::testing::FakeShellPty;
use uulan_media::ffmpeg::{self, Encoder};
use uulan_protocol::msg::Cap;
use uulan_protocol::rv::{Role, ToEndpoint};
use uulan_protocol::rvclient::RvConn;

const PW: &str = "aB3dE5fG";

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
    cfg.name = "终端测试被控端".into();
    cfg.password = PW.into();
    cfg.capture = CaptureInput::parse("lavfi:testsrc2=size=320x240:rate=5").unwrap();
    cfg.fps = 5;
    cfg.screen_width = 320;
    cfg.screen_height = 240;
    cfg
}

/// 起一个被控端，终端后端换成假 shell。返回设备 ID。
async fn spawn_host(server: &str, cfg: HostConfig) -> String {
    let mut conn = RvConn::connect(server).await.expect("被控端连不上中继");
    let (device_id, _) = conn.host_register(None, cfg.name.clone()).await.expect("注册失败");

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
                    clipboard: Some(Box::new(uulan_clip::NoClipboard::new("终端测试"))),
                    screen_probe: None,
                    fs_roots: None,
                    pty: Some(Box::new(FakeShellPty::new())),
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

/// 起 server + host，并开一条已鉴权的会话，返回一个 TermLink。
async fn harness(opts: Options) -> TermLink {
    let srv = spawn_server().await;
    let device = spawn_host(&srv, test_config(&srv)).await;
    let c = timeout(
        Duration::from_secs(15),
        connect::connect(&srv, &device, PW, "term-cli", vec![Cap::Terminal]),
    )
    .await
    .expect("连接超时")
    .expect("连接失败");
    assert!(
        c.host.caps.contains(&Cap::Terminal),
        "被控端必须声明终端能力（否则控制端会认为它不支持）"
    );
    // 终端会话不看画面：把视频通道丢掉，被控端会自己停编码器。
    drop(c.video);
    timeout(Duration::from_secs(10), TermLink::open(c.control, opts))
        .await
        .expect("开终端超时")
        .expect("开终端失败")
}

/// 收到 `needle` 之前的全部输出（按字节拼起来再看）。
async fn output_until(link: &mut TermLink, needle: &str) -> String {
    let mut acc = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        match timeout(Duration::from_secs(5), link.next_event()).await {
            Ok(Some(Ev::Output(b))) => {
                acc.push_str(&String::from_utf8_lossy(&b));
                if acc.contains(needle) {
                    return acc;
                }
            }
            Ok(Some(Ev::Exit(c))) => panic!("会话提前结束（退出码 {c}），已收到 {acc:?}"),
            Ok(Some(Ev::Error(e))) => panic!("被控端报错：{e}"),
            Ok(None) => panic!("连接断了，已收到 {acc:?}"),
            Err(_) => {}
        }
    }
    panic!("等 {needle:?} 超时，已收到 {acc:?}");
}

#[tokio::test]
async fn terminal_round_trips_input_and_reports_the_exit_code() {
    let mut link = harness(Options { shell: Some("/bin/bash".into()), cols: 100, rows: 40 }).await;

    link.input(b"echo hello-uulan\n").await.expect("发输入失败");
    let seen = output_until(&mut link, "echo hello-uulan").await;
    assert!(seen.contains("echo hello-uulan"), "远端 shell 应当回显输入，实际 {seen:?}");

    link.input(b"exit 7\n").await.expect("发输入失败");
    let code = loop {
        match timeout(Duration::from_secs(10), link.next_event()).await {
            Ok(Some(Ev::Exit(code))) => break code,
            Ok(Some(Ev::Output(_))) => continue,
            Ok(Some(Ev::Error(e))) => panic!("被控端报错：{e}"),
            Ok(None) => panic!("连接断了，没等到退出码"),
            Err(_) => panic!("等退出码超时"),
        }
    };
    assert_eq!(code, 7, "远端 shell 的退出码必须原样上报");
    link.finish().await;
}

/// 一次大输出必须被切成多帧，而且字节一个不少（这是「`cat 大文件` 会不会堵住控制通道」的量）。
#[tokio::test]
async fn big_output_is_chunked_and_complete() {
    let mut link = harness(Options { shell: None, cols: 80, rows: 24 }).await;
    let want = uulan_host::term::OUT_CHUNK * 2 + 777;
    link.input(format!("burst {want}\n").as_bytes()).await.expect("发输入失败");

    let mut xs = 0usize;
    let mut frames = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while xs < want && tokio::time::Instant::now() < deadline {
        match timeout(Duration::from_secs(10), link.next_event()).await {
            Ok(Some(Ev::Output(b))) => {
                frames += 1;
                assert!(b.len() <= uulan_host::term::OUT_CHUNK, "单帧超过上限：{}", b.len());
                xs += b.iter().filter(|c| **c == b'x').count();
            }
            Ok(Some(Ev::Exit(_))) => break,
            Ok(Some(Ev::Error(e))) => panic!("被控端报错：{e}"),
            Ok(None) => break,
            Err(_) => break,
        }
    }
    assert_eq!(xs, want, "输出字节必须完整（少一个字节都可能让上一次命令的输出看起来对不上）");
    assert!(frames >= 3, "这么大一块输出必须切成多帧，实际 {frames} 帧");
    link.finish().await;
}

/// 改窗口大小要真的落到 PTY 上（不是「发了个消息就算完」）。
#[tokio::test]
async fn resize_reaches_the_pty() {
    let mut link = harness(Options { shell: None, cols: 80, rows: 24 }).await;
    link.resize(140, 40).await.expect("发 resize 失败");
    link.input(b"size\n").await.expect("发输入失败");
    let seen = output_until(&mut link, "140x40").await;
    assert!(seen.contains("140x40"), "假 shell 报的窗口大小应当是新值，实际 {seen:?}");
    link.finish().await;
}

/// 主动关闭要收到退出事件（不然控制端会以为会话还活着，窗口永远不关）。
#[tokio::test]
async fn closing_reports_the_exit() {
    let mut link = harness(Options { shell: None, cols: 80, rows: 24 }).await;
    link.close().await.expect("发关闭失败");
    let code = loop {
        match timeout(Duration::from_secs(10), link.next_event()).await {
            Ok(Some(Ev::Exit(code))) => break code,
            Ok(Some(Ev::Output(_))) => continue,
            Ok(Some(Ev::Error(e))) => panic!("关会话不该报错：{e}"),
            Ok(None) => panic!("连接断了，没等到退出码"),
            Err(_) => panic!("等退出码超时"),
        }
    };
    // 被 SIGHUP 杀掉 = 128+1，和真 shell 的惯例一致。
    assert_eq!(code, 129, "被我们关掉的会话应当报 128+SIGHUP");
    link.finish().await;
}