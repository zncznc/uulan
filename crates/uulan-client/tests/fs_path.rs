//! M4 文件传输验收：**两端真的接上了**。
//!
//! 链路（全是生产代码，没有替身）：
//!
//! ```text
//! 控制端 fsx（真命令行逻辑所调用的同一套函数）
//!   → C2H::Fs JSON 帧 → 中继盲转发 → 被控端 control_loop
//!   → uulan_fs::server::handle（真文件、真 sha256）
//!   → Blob 帧 → 中继 → 控制端
//! ```
//!
//! 被控端用 `lavfi` 合成画面（本机没有 X11），文件根指向一个临时目录 ——
//! 这是唯一被替换的东西（`FsRoots`），文件读写与协议路径全是生产代码。
//!
//! 断言的都是**机器可查的量**：落盘字节、sha256、`.uulan-part` 是否残留、
//! 「此电脑」列表非空、越界路径被拒。

use std::path::PathBuf;
use std::time::Duration;

use tokio::time::timeout;

use uulan_client::connect;
use uulan_fs::path::FsRoots;
use uulan_fs::testutil::TempDir;
use uulan_host::capture::CaptureInput;
use uulan_host::config::HostConfig;
use uulan_host::session::{self, HostState, SessionParams, VideoSettings};
use uulan_media::ffmpeg::{self, Encoder};
use uulan_protocol::bulk::{BulkFrame, BulkKind, CHUNK};
use uulan_protocol::frame::{read_frame, write_frame, FrameKind};
use uulan_protocol::msg::{C2H, Cap, FsEntry, FsOp, FsStreamStage, H2C};
use uulan_protocol::rv::{Role, ToEndpoint};
use uulan_protocol::rvclient::RvConn;

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
    cfg.name = "文件测试被控端".into();
    cfg.password = "aB3dE5fG".into();
    cfg.capture = CaptureInput::parse("lavfi:testsrc2=size=320x240:rate=5").unwrap();
    cfg.fps = 5;
    cfg.screen_width = 320;
    cfg.screen_height = 240;
    cfg
}

/// 起一个被控端，文件根指向 `home`。
async fn spawn_host(server: &str, cfg: HostConfig, home: PathBuf) -> String {
    let mut conn = RvConn::connect(server).await.expect("被控端连不上中继");
    let (device_id, _) = conn.host_register(None, cfg.name.clone()).await.expect("注册失败");

    let state = std::sync::Arc::new(HostState::new(cfg.clone(), None));
    let screen = session::resolve_screen(&cfg.capture, &cfg).await;
    let settings = VideoSettings::from_config(&cfg);
    let addr = conn.server_addr().to_string();
    let ffmpeg = ffmpeg::find_ffmpeg(None).unwrap_or_else(|| PathBuf::from("ffmpeg"));

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
                    clipboard: Some(Box::new(uulan_clip::NoClipboard::new("文件链路测试"))),
                    screen_probe: None,
                    fs_roots: Some(FsRoots::new(home.clone(), true)),
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

/// 一条测试用的控制端会话：控制通道 + bulk 通道。
struct Cli {
    ctl_rd: tokio::net::tcp::OwnedReadHalf,
    ctl_wr: tokio::net::tcp::OwnedWriteHalf,
    // Option 只是为了「测中断上传」时能把两半取走丢掉（不留半个连接挂着）。
    bulk_rd: Option<tokio::net::tcp::OwnedReadHalf>,
    bulk_wr: Option<tokio::net::tcp::OwnedWriteHalf>,
}

impl Cli {
    async fn connect(server: &str, device: &str) -> Cli {
        let c = timeout(
            Duration::from_secs(15),
            connect::connect(server, device, "aB3dE5fG", "cli", vec![Cap::Files]),
        )
        .await
        .expect("连接超时")
        .expect("连接失败");
        let (ctl_rd, ctl_wr) = c.control.into_split();
        let (bulk_rd, bulk_wr) = c.bulk.into_split();
        Cli { ctl_rd, ctl_wr, bulk_rd: Some(bulk_rd), bulk_wr: Some(bulk_wr) }
    }

    async fn send(&mut self, op: FsOp) {
        let f = C2H::Fs(op).to_frame().unwrap();
        write_frame(&mut self.ctl_wr, f.kind, &f.payload).await.unwrap();
    }

    /// 等下一条与 `id` 相关的控制消息（跳过别的）。
    async fn step(&mut self, id: u32) -> Step {
        loop {
            let f = timeout(Duration::from_secs(20), read_frame(&mut self.ctl_rd))
                .await
                .expect("等控制帧超时")
                .expect("读控制帧失败");
            if f.kind != FrameKind::Json {
                continue;
            }
            let m: H2C = match f.decode_json() {
                Ok(m) => m,
                Err(_) => continue,
            };
            match m {
                H2C::FsResult { id: rid, ok, error, entries, path, hash } if rid == id => {
                    return Step::Result { ok, error, entries, path, hash }
                }
                H2C::FsStream { id: rid, stage, total, done } if rid == id => {
                    return match stage {
                        FsStreamStage::Begin => Step::Begin { total },
                        FsStreamStage::Done => Step::Done { done },
                        FsStreamStage::Failed => Step::Failed { done },
                    }
                }
                H2C::Bye { reason } => panic!("被控端道别：{reason}"),
                _ => continue,
            }
        }
    }

    async fn result(&mut self, id: u32) -> Step {
        loop {
            match self.step(id).await {
                r @ Step::Result { .. } => return r,
                Step::Done { done } => eprintln!("（对端报告传输完成，{done} 字节）"),
                Step::Failed { done } => eprintln!("（对端报告传输失败，已到 {done} 字节）"),
                Step::Begin { .. } => {}
            }
        }
    }

    async fn bulk_send(&mut self, id: u32, data: &[u8]) {
        let payload = BulkFrame::data(id, bytes::Bytes::copy_from_slice(data)).encode();
        write_frame(self.bulk_wr.as_mut().expect("bulk 已关闭"), FrameKind::Blob, &payload)
            .await
            .unwrap();
    }

    async fn bulk_end(&mut self, id: u32) {
        write_frame(self.bulk_wr.as_mut().expect("bulk 已关闭"), FrameKind::Blob, &BulkFrame::end(id).encode())
            .await
            .unwrap();
    }

    async fn bulk_recv(&mut self) -> BulkFrame {
        let rd = self.bulk_rd.as_mut().expect("bulk 已关闭");
        let f = timeout(Duration::from_secs(20), read_frame(rd))
            .await
            .expect("等 bulk 帧超时")
            .expect("读 bulk 帧失败");
        assert_eq!(f.kind, FrameKind::Blob);
        BulkFrame::decode(&f.payload).unwrap()
    }
}

#[derive(Debug)]
enum Step {
    Begin { total: Option<u64> },
    Done { done: u64 },
    Failed { done: u64 },
    Result {
        ok: bool,
        error: Option<String>,
        entries: Option<Vec<FsEntry>>,
        path: Option<String>,
        hash: Option<String>,
    },
}

fn pseudo(n: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(n);
    let mut x: u32 = 0xdead_beef;
    for _ in 0..n {
        x = x.wrapping_mul(1664525).wrapping_add(1013904223);
        v.push((x >> 24) as u8);
    }
    v
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

#[tokio::test]
async fn fs_roundtrip_over_the_real_wire() {
    let Some(tmp) = TempDir::new("fs-e2e") else {
        eprintln!("跳过：这台机器没有可写目录");
        return;
    };
    let server = spawn_server().await;
    let cfg = test_config(&server);
    let home = tmp.path().to_path_buf();
    let device = spawn_host(&server, cfg, home.clone()).await;
    let mut cli = Cli::connect(&server, &device).await;

    // ① 「此电脑」：至少一个条目（Windows 上是 C:，Linux 上是 /）。
    cli.send(FsOp::List { id: 1, path: ":/".into() }).await;
    match cli.result(1).await {
        Step::Result { ok: true, entries: Some(e), .. } => {
            assert!(!e.is_empty(), "「此电脑」不该是空的");
        }
        other => panic!("列「此电脑」失败：{other:?}"),
    }

    // ② 上传一个跨多个 chunk 的文件，逐字节 + sha256 校验。
    let data = pseudo(200 * 1024 + 3);
    let remote = home.join("dir-not-exist").join("x.bin");
    cli.send(FsOp::Put { id: 2, path: remote.display().to_string(), size: data.len() as u64 })
        .await;
    // 上级目录不存在 → 必须明确拒绝，而不是悄悄失败
    match cli.result(2).await {
        Step::Result { ok: false, error: Some(e), .. } => {
            assert!(e.contains("上级目录不存在"), "got {e}")
        }
        other => panic!("期望「上级目录不存在」，实际 {other:?}"),
    }

    // ③ 建目录 → 再上传
    let dir = home.join("sub");
    cli.send(FsOp::Mkdir { id: 3, path: dir.display().to_string() }).await;
    match cli.result(3).await {
        Step::Result { ok: true, .. } => {}
        other => panic!("mkdir 失败：{other:?}"),
    }
    let remote = dir.join("payload.bin");
    cli.send(FsOp::Put { id: 4, path: remote.display().to_string(), size: data.len() as u64 })
        .await;
    match cli.step(4).await {
        Step::Begin { .. } => {}
        other => panic!("上传没有被放行：{other:?}"),
    }
    for chunk in data.chunks(CHUNK) {
        cli.bulk_send(4, chunk).await;
    }
    cli.bulk_end(4).await;
    match cli.result(4).await {
        Step::Result { ok: true, hash: Some(h), .. } => assert_eq!(h, sha256_hex(&data)),
        other => panic!("上传失败：{other:?}"),
    }
    assert_eq!(std::fs::read(&remote).unwrap(), data, "被控端落盘内容必须一致");
    assert!(!dir.join("payload.bin.uulan-part").exists(), "不该残留临时文件");

    // ④ 下载回来，逐字节比对。
    cli.send(FsOp::Get { id: 5, path: remote.display().to_string() }).await;
    match cli.step(5).await {
        Step::Begin { total } => assert_eq!(total, Some(data.len() as u64)),
        other => panic!("下载没有开始：{other:?}"),
    }
    let mut got = Vec::new();
    loop {
        let f = cli.bulk_recv().await;
        assert_eq!(f.id, 5);
        if f.kind == BulkKind::End {
            break;
        }
        got.extend_from_slice(&f.data);
    }
    match cli.result(5).await {
        Step::Result { ok: true, hash: Some(h), .. } => assert_eq!(h, sha256_hex(&got)),
        other => panic!("下载失败：{other:?}"),
    }
    assert_eq!(got, data, "下载内容必须与上传内容一致");

    // ⑤ 列目录：能看到刚建的文件，且目录在前。
    cli.send(FsOp::List { id: 6, path: "~/".into() }).await;
    match cli.result(6).await {
        Step::Result { ok: true, entries: Some(e), path, .. } => {
            assert_eq!(path.as_deref(), Some(home.display().to_string().as_str()));
            assert_eq!(e[0].name, "sub");
            assert!(e[0].dir);
        }
        other => panic!("列家目录失败：{other:?}"),
    }

    // ⑥ 越界路径必须被拒（`~/../..`）。
    cli.send(FsOp::List { id: 7, path: "~/../..".into() }).await;
    match cli.result(7).await {
        Step::Result { ok: false, error: Some(e), .. } => assert!(e.contains("逃出"), "got {e}"),
        other => panic!("期望越界被拒，实际 {other:?}"),
    }

    // ⑦ 删除，再确认不在了。
    cli.send(FsOp::Remove { id: 8, path: remote.display().to_string() }).await;
    match cli.result(8).await {
        Step::Result { ok: true, .. } => {}
        other => panic!("删除失败：{other:?}"),
    }
    assert!(!remote.exists());
}

/// 一条大文件的实测（8 MiB）：确认分块、无丢字节、sha256 一致。
///
/// 之所以单独一条：它要跑几秒，混在上面那条里会让「协议走通」的信号变钝。
#[tokio::test]
async fn eight_mib_file_survives_the_wire() {
    let Some(tmp) = TempDir::new("fs-big") else {
        eprintln!("跳过：这台机器没有可写目录");
        return;
    };
    let server = spawn_server().await;
    let cfg = test_config(&server);
    let home = tmp.path().to_path_buf();
    let device = spawn_host(&server, cfg, home.clone()).await;
    let mut cli = Cli::connect(&server, &device).await;

    let data = pseudo(8 * 1024 * 1024);
    let remote = home.join("big.bin");
    cli.send(FsOp::Put { id: 1, path: remote.display().to_string(), size: data.len() as u64 })
        .await;
    match cli.step(1).await {
        Step::Begin { .. } => {}
        other => panic!("{other:?}"),
    }
    for chunk in data.chunks(CHUNK) {
        cli.bulk_send(1, chunk).await;
    }
    cli.bulk_end(1).await;
    match cli.result(1).await {
        Step::Result { ok: true, hash: Some(h), .. } => assert_eq!(h, sha256_hex(&data)),
        other => panic!("{other:?}"),
    }

    // 用 Hash 走另一条路径核对一遍（不经过传输）。
    cli.send(FsOp::Hash { id: 2, path: remote.display().to_string() }).await;
    match cli.result(2).await {
        Step::Result { ok: true, hash: Some(h), .. } => assert_eq!(h, sha256_hex(&data)),
        other => panic!("{other:?}"),
    }

    // 下载回来再比一次长度与哈希。
    cli.send(FsOp::Get { id: 3, path: remote.display().to_string() }).await;
    let mut got = Vec::new();
    match cli.step(3).await {
        Step::Begin { .. } => loop {
            let f = cli.bulk_recv().await;
            if f.kind == BulkKind::End {
                break;
            }
            got.extend_from_slice(&f.data);
        },
        other => panic!("{other:?}"),
    }
    assert_eq!(got.len(), data.len());
    assert_eq!(sha256_hex(&got), sha256_hex(&data));
}

/// 上传中途断开：目标文件不该出现（宁可没有，也不能有半截）。
#[tokio::test]
async fn aborted_upload_leaves_no_target_file() {
    let Some(tmp) = TempDir::new("fs-abort") else {
        eprintln!("跳过：这台机器没有可写目录");
        return;
    };
    let server = spawn_server().await;
    let cfg = test_config(&server);
    let home = tmp.path().to_path_buf();
    let device = spawn_host(&server, cfg, home.clone()).await;
    let mut cli = Cli::connect(&server, &device).await;

    let remote = home.join("broken.bin");
    cli.send(FsOp::Put { id: 1, path: remote.display().to_string(), size: 1_000_000 }).await;
    match cli.step(1).await {
        Step::Begin { .. } => {}
        other => panic!("{other:?}"),
    }
    cli.bulk_send(1, &pseudo(1000)).await;
    // 不发 END，直接把 bulk 连接断掉 → 被控端必须清理临时文件并报失败。
    drop(cli.bulk_wr.take());
    drop(cli.bulk_rd.take());

    // 服务端会在 bulk EOF 时返回 FsResult(ok=false)。控制通道仍在。
    match cli.result(1).await {
        Step::Result { ok: false, error: Some(e), .. } => {
            assert!(e.contains("断") || e.contains("bulk"), "got {e}")
        }
        other => panic!("期望失败，实际 {other:?}"),
    }
    assert!(!remote.exists(), "中断的上传绝不能留下目标文件");
    assert!(!home.join("broken.bin.uulan-part").exists(), "临时文件也要清掉");
}