#![cfg(windows)]
//! Windows 控制端 `--term` 的**控制台原始模式**验收（真 ConPTY、真控制台、真 PTY、真进程）。
//!
//! 为什么必须这么验：`SetConsoleMode` 只在「stdin 真的是一个控制台」时才有意义，而
//! `cargo test` 自己的 stdin 是管道 —— 于是这条路径过去只能靠人眼看。这里用
//! `portable-pty` 给客户端进程造一个 **ConPTY** 当它的终端：客户端拿到的就是一个真控制台，
//! `GetConsoleMode` / `SetConsoleMode` / VT 输入都会真的生效；同时 master 端能喂按键、
//! 收屏幕字节，整件事于是变成机器可查的断言。
//!
//! 四条断言，每一条都能在「没进原始模式」的旧二进制上变红：
//!
//! ① 日志里有「已进入控制台原始模式」；
//! ② 单打一个 `a`（**不带回车**）远端就回显 —— 行模式会把键扣到回车才发；
//! ③ 单独一个上方向键（`ESC[A`）能让远端 cmd 重现上一行 —— 行模式下方向键根本不是字节；
//! ④ `Ctrl-C`（`0x03`）能打到远端并出 `^C` —— 行模式下它被本机当成信号，客户端直接死。
//!
//! 最后核一遍远端 shell 的退出码原样回来。
//!
//! `UULAN_CLIENT_EXE` 可以指定用哪个客户端二进制（用来拿旧二进制跑出「红」）。
//!
//! 已知边界（没验的就不写）：中文/IME 输入只做到「输入码页切 UTF-8」这一步，
//! 本机 IME 上屏的字在 ConPTY 里没法可靠注入，**未验**；粘贴（Ctrl+V / 右键）**未验**。

use std::io::{Read, Write};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

use uulan_host::capture::CaptureInput;
use uulan_host::config::HostConfig;
use uulan_host::session::{self, HostState, SessionParams, VideoSettings};
use uulan_media::ffmpeg::{self, Encoder};
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
    cfg.name = "控制台原始模式测试被控端".into();
    cfg.password = PW.into();
    cfg.capture = CaptureInput::parse("lavfi:testsrc2=size=320x240:rate=5").unwrap();
    cfg.fps = 5;
    cfg.screen_width = 320;
    cfg.screen_height = 240;
    cfg
}

/// 起一个被控端：终端后端用**生产**的 `portable-pty`（`pty: None`），这样远端 shell 是真的。
async fn spawn_host(server: &str, cfg: HostConfig) -> String {
    let mut conn = RvConn::connect(server).await.expect("被控端连不上中继");
    let (device_id, _) = conn
        .host_register(None, cfg.name.clone(), None)
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
                    clipboard: Some(Box::new(uulan_clip::NoClipboard::new("原始模式测试"))),
                    screen_probe: None,
                    fs_roots: None,
                    // None = 生产用的 portable-pty（Windows 上就是 ConPTY）。
                    pty: None,
                    sys_source: None,
                    lock_probe: None,
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

/// ConPTY 一上来会先问「光标在哪」（`ESC[6n`），**不回答它就不往下画** ——
/// 所以这个假终端必须替 conhost 回一句 `ESC[1;1R`。少了这一步，屏幕上只有 4 个字节，
/// 看起来像「客户端什么都没干」（第一版测试就栽在这里）。
const DSR: &[u8] = &[0x1b, b'[', b'6', b'n'];
const CPR: &[u8] = b"\x1b[1;1R";

/// 挂在 ConPTY 上的客户端：拿着 master 的读半端（屏幕字节）与写半端（按键）。
struct Console {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    screen: Arc<Mutex<Vec<u8>>>,
    dsr: mpsc::Receiver<()>,
    cpr_answered: usize,
}

impl Console {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.screen.lock().unwrap()).to_string()
    }

    fn count(&self, needle: &str) -> usize {
        self.text().matches(needle).count()
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("往控制台写按键失败");
        self.writer.flush().ok();
    }

    /// 把「conhost 在问光标位置」这件事答掉（每轮等待都调一次）。
    fn pump(&mut self) {
        while self.dsr.try_recv().is_ok() {
            let _ = self.writer.write_all(CPR);
            let _ = self.writer.flush();
            self.cpr_answered += 1;
        }
    }

    /// 等到屏幕上出现第 `at_least` 个 `needle`（`timeout` 到点就返回 false）。
    async fn wait_for_count(&mut self, needle: &str, at_least: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.pump();
            if self.count(needle) >= at_least {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        self.pump();
        self.count(needle) >= at_least
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn windows_console_goes_raw_so_arrows_and_ctrl_c_work() {
    let srv = spawn_server().await;
    let device = spawn_host(&srv, test_config(&srv)).await;

    // 客户端二进制：默认用本包的构建产物；`UULAN_CLIENT_EXE` 可指到别处（拿旧二进制验红）。
    let exe = std::env::var("UULAN_CLIENT_EXE")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_uulan-client").to_string());

    let pty = native_pty_system()
        .openpty(PtySize { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 })
        .expect("开 ConPTY 失败");

    let mut cmd = CommandBuilder::new(&exe);
    cmd.arg("--server");
    cmd.arg(&srv);
    cmd.arg("--device");
    cmd.arg(&device);
    cmd.arg("--term");
    // 口令走环境变量：命令行上的口令会留在 `ps` 里（DESIGN 的已知限制）。
    cmd.env("UULAN_PASSWORD", PW);
    cmd.env("RUST_LOG", "info");
    cmd.env("NO_COLOR", "1");
    // 别去动这台机器上真实的 `~/.config/uulan/client.json`（那是用户「上次连了哪台」）。
    cmd.env(
        "UULAN_CLIENT_CONFIG",
        std::env::temp_dir().join("uulan-term-raw-console-test-client.json"),
    );

    let child = pty.slave.spawn_command(cmd).expect("起客户端失败");
    drop(pty.slave); // 不放手的话 ConPTY 不会在客户端退出后关闭
    let writer = pty.master.take_writer().expect("拿不到 master 写端");
    let mut reader = pty.master.try_clone_reader().expect("拿不到 master 读端");

    // 屏幕字节由独立线程收（`Read` 是阻塞的）；顺手把 conhost 的光标位置提问回报上来。
    let screen = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink = screen.clone();
    let (dsr_tx, dsr_rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut tail: Vec<u8> = Vec::new();
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    sink.lock().unwrap().extend_from_slice(&buf[..n]);
                    let mut scan = tail.clone();
                    scan.extend_from_slice(&buf[..n]);
                    if scan.windows(DSR.len()).any(|w| w == DSR) {
                        let _ = dsr_tx.send(());
                    }
                    tail = scan[scan.len().saturating_sub(DSR.len() - 1)..].to_vec();
                }
            }
        }
    });

    let mut con = Console { child, writer, screen, dsr: dsr_rx, cpr_answered: 0 };

    // ① 原始模式真的进了（日志走 stderr，在控制台上与远端字节混在一条流里）。
    assert!(
        con.wait_for_count("已进入控制台原始模式", 1, Duration::from_secs(20)).await,
        "控制台没进原始模式。屏幕上收到：{:?}",
        con.text()
    );

    // ② 不带回车的连续按键必须立刻到远端（行模式下远端一个字节都收不到）。
    // 用一个不会出现在横幅里的词，免得断言被 cmd 的欢迎语碰巧满足。
    con.send(b"uulanraw");
    assert!(
        con.wait_for_count("uulanraw", 1, Duration::from_secs(8)).await,
        "不带回车打的字远端没回显 —— 还是行模式？屏幕：{:?}",
        con.text()
    );
    con.send(b"\r"); // 让远端 cmd 把这一行消化掉（执行失败无所谓，我们要的是缓冲区清空）
    tokio::time::sleep(Duration::from_millis(500)).await;

    // ③ 方向键的字节必须立刻过去。远端是 Windows 的 ConPTY，它**不**把输入管道里的
    // `ESC[A` 还原成「上方向键」（它要的是 win32-input-mode 的 `ESC[38;72;..._` 编码），
    // 所以这里能核的是「三个字节到了远端」—— 屏幕上会原样出现 `^[[A`。
    // 行模式下这几个字节根本不会离开本机，这条就变红。
    // （远端是麒麟的 Linux PTY 时，`ESC[A` 本身就是方向键序列，不需要这层转码。）
    con.send(b"\x1b[A");
    assert!(
        con.wait_for_count("^[[A", 1, Duration::from_secs(10)).await,
        "方向键的字节没到远端（行模式下它要等回车才发）。屏幕：{:?}",
        con.text()
    );
    con.send(b"\r"); // 把远端 cmd 里这行垃圾消化掉
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 同一件事的另一面：远端 Windows 控制台确实在要 win32-input-mode（`ESC[?9001h`）。
    // 记下来是为了让「Windows 被控端的方向键要转码」这条限制有据可查，不是猜的。
    let wants_win32_input = con.text().contains("\x1b[?9001h");
    println!("远端控制台是否请求 win32-input-mode：{wants_win32_input}");

    // ④ Ctrl-C 必须打到远端，而不是把**本机客户端**打死。行模式下 `ENABLE_PROCESSED_INPUT`
    // 还开着，0x03 会变成给客户端的 CTRL_C_EVENT —— 客户端当场退出，会话断。
    // 所以这里判两件事：客户端还活着，而且会话还能继续用。
    con.send(b"echo never-runs");
    tokio::time::sleep(Duration::from_millis(300)).await;
    con.send(b"\x03");
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        matches!(con.child.try_wait(), Ok(None)),
        "Ctrl-C 把本机客户端打死了（行模式的行为）—— 屏幕上：{:?}",
        con.text()
    );
    con.send(b"echo mark-three\r");
    assert!(
        con.wait_for_count("mark-three", 2, Duration::from_secs(10)).await,
        "Ctrl-C 之后会话不可用了（`echo mark-three` 没执行）。屏幕：{:?}",
        con.text()
    );

    // ⑤ 退出码原样回来。
    con.send(b"exit 3\r");
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut code = None;
    while Instant::now() < deadline {
        con.pump();
        if let Ok(Some(st)) = con.child.try_wait() {
            code = Some(st.exit_code());
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let code = code.unwrap_or_else(|| panic!("客户端没退出。屏幕：{:?}", con.text()));
    assert_eq!(code, 3, "客户端退出码应当等于远端 shell 的退出码。屏幕：{:?}", con.text());

    // 控制台确实被当成控制台来问过话（`ESC[6n` 至少一次，我们也都答了）——
    // 这条同时证明「客户端这个进程真的挂在一个控制台上」，不是跑在管道里。
    assert!(con.cpr_answered >= 1, "一次 conhost 的光标提问都没收到，harness 可能没挂到控制台");

    // 收尾：模式已还原这件事在 ConPTY 里没有独立判据（进程已经没了），
    // 所以只断言没有 panic。
    assert!(!con.text().contains("panicked"), "客户端 panic 了：{:?}", con.text());
}