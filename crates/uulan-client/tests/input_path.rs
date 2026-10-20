//! M2.5 验收：**两端真的接上了**。
//!
//! 链路（除了 X11 的 syscall 之外，全是生产代码，没有替身）：
//!
//! ```text
//! 控制端的键/光标决策 (uulan_client::input)
//!   → C2H JSON 帧 → 中继盲转发 → 被控端 control_loop (session.rs)
//!   → Mapper 状态机 (uulan_host::input) → Inject 指令 → InputSink
//! ```
//!
//! 被控端这一端用一个「记录型 sink」替掉 XTest —— **这是唯一被替换的一环**，
//! 因为它需要真 X11。测试断言的是「被控端打算注入什么」，这正是本机能验证的最大范围；
//! XTest 之后的行为必须在真麒麟上验（见 docs/milestones/M2.5-input.txt）。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::timeout;

use uulan_client::connect;
use uulan_client::control::ControlLink;
use uulan_client::input::{fit_rect, key_action, key_messages, line_units, norm_point, vk_from_key, Mods};
use uulan_host::capture::CaptureInput;
use uulan_host::config::HostConfig;
use uulan_host::input::{Inject, InputSink};
use uulan_host::session::{self, HostState, SessionParams, VideoSettings};
use uulan_media::ffmpeg::{self, Encoder};
use uulan_protocol::msg::C2H;
use uulan_protocol::rv::{Role, ToEndpoint};
use uulan_protocol::rvclient::RvConn;
use winit::keyboard::KeyCode;

/// 把被控端「打算注入」的东西记下来。
struct RecordingSink {
    out: Arc<Mutex<Vec<Inject>>>,
}

impl InputSink for RecordingSink {
    fn name(&self) -> &'static str {
        "recording"
    }

    fn apply(&mut self, batch: &[Inject]) -> Result<(), String> {
        self.out.lock().unwrap().extend_from_slice(batch);
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
    cfg.name = "输入测试被控端".into();
    cfg.password = "aB3dE5fG".into();
    cfg.capture = CaptureInput::parse("lavfi:testsrc2=size=320x240:rate=10").unwrap();
    cfg.fps = 10;
    cfg.screen_width = 320;
    cfg.screen_height = 240;
    cfg
}

/// 起一个被控端；每个会话都用同一个记录器。
async fn spawn_host(server: &str, cfg: HostConfig, out: Arc<Mutex<Vec<Inject>>>) -> String {
    let mut conn = RvConn::connect(server).await.expect("被控端连不上中继");
    let (device_id, _) = conn
        .host_register(None, cfg.name.clone())
        .await
        .expect("注册失败");

    let state = Arc::new(HostState::new(cfg.clone(), None));
    let screen = session::resolve_screen(&cfg.capture, &cfg).await;
    let settings = VideoSettings::from_config(&cfg);
    let addr = conn.server_addr().to_string();
    // 没有 ffmpeg 也能测输入：视频任务只会自己失败退出，控制通道照常工作。
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
                };
                let a = addr.clone();
                let rec = out.clone();
                tokio::spawn(async move {
                    if let Ok(c) = RvConn::connect(&a).await {
                        if let Ok(relays) = c.relay_open_all(&sid, Role::Host).await {
                            let _ = session::run_with_sink(
                                params,
                                relays,
                                client_addr,
                                Box::new(RecordingSink { out: rec }),
                            )
                            .await;
                        }
                    }
                });
            }
        }
    });

    device_id
}

async fn wait_for(out: &Arc<Mutex<Vec<Inject>>>, n: usize) -> Vec<Inject> {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let v = out.lock().unwrap().clone();
        if v.len() >= n {
            return v;
        }
        if std::time::Instant::now() > deadline {
            panic!("等到 {n} 条注入指令超时，只收到 {} 条：{v:?}", v.len());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn keys_and_mouse_reach_the_host_injector() {
    let addr = spawn_server().await;
    let cfg = test_config(&addr);
    let out = Arc::new(Mutex::new(Vec::<Inject>::new()));
    let device_id = spawn_host(&addr, cfg.clone(), out.clone()).await;

    let c = timeout(
        Duration::from_secs(20),
        connect::connect(&addr, &device_id, &cfg.password, "输入测试控制端", vec![]),
    )
    .await
    .expect("连接超时")
    .expect("连接失败");

    // 视频/数据通道本次不用（关掉，让被控端的视频任务自己收尾）
    drop(c.video);
    drop(c.bulk);

    let link = ControlLink::start(c.control);
    let input = link.input();

    // ① 光标：控制端窗口 640x480 显示 320x240 的画面（整数倍 2x，正好铺满）。
    //    点上 (160,120) 就是画面正中 → 归一化 (0.25, 0.25)？不是：160/640 = 0.25 是左边四分之一处。
    //    取正中：(320,240) → 0.5, 0.5 → 被控端 320x240 上应为 (160, 120) 附近。
    let rect = fit_rect(320, 240, 640, 480);
    assert_eq!(rect, uulan_client::input::Rect { ox: 0, oy: 0, w: 640, h: 480 });
    let (nx, ny) = norm_point(320.0, 240.0, rect).unwrap();
    assert!((nx - 0.5).abs() < 0.01 && (ny - 0.5).abs() < 0.01, "{nx},{ny}");
    assert!(input.send(C2H::MouseMoveAbs { x: nx, y: ny }));

    // ② 左键按下 + 松开
    assert!(input.send(C2H::MouseButton { button: uulan_protocol::msg::MouseButton::Left, down: true }));
    assert!(input.send(C2H::MouseButton { button: uulan_protocol::msg::MouseButton::Left, down: false }));

    // ③ 物理键 a（走控制端真正的查表路径）
    let mods = Mods::default();
    let a = key_action(vk_from_key(KeyCode::KeyA), Some("a"), true, false, &mods);
    for m in key_messages(&a) {
        assert!(input.send(m));
    }
    let a_up = key_action(vk_from_key(KeyCode::KeyA), Some("a"), false, false, &mods);
    for m in key_messages(&a_up) {
        assert!(input.send(m));
    }

    // ④ 滚轮向上一格
    let (dx, dy) = line_units(0.0, 1.0);
    assert_eq!((dx, dy), (0, 120));
    assert!(input.send(C2H::MouseWheel { dx, dy }));

    // ⑤ 输入法上屏的中文（走字符路径，不是物理键）
    for m in key_messages(&key_action(None, Some("中"), true, false, &mods)) {
        assert!(input.send(m));
    }

    // 8 条注入指令：移动 / 按下 / 松开 / 键按下 / 键松开 / 滚轮 / 中文字符按下+松开
    let got = wait_for(&out, 8).await;
    assert_eq!(
        &got[..8],
        &[
            Inject::MoveAbs { x: 160, y: 120 },
            Inject::Button { button: 1, down: true },
            Inject::Button { button: 1, down: false },
            Inject::Key { keysym: 0x61, down: true },
            Inject::Key { keysym: 0x61, down: false },
            Inject::Wheel { button: 4, count: 1 },
            Inject::Key { keysym: 0x0100_4e2d, down: true },
            Inject::Key { keysym: 0x0100_4e2d, down: false },
        ],
        "被控端注入的指令与预期不符：{got:?}"
    );

    // 控制端自己的计数：8 条输入消息（3 条移动/键/按钮 + ... 按类型分）
    let s = link.stats().snapshot();
    assert_eq!(s.moves, 1, "移动 {s:?}");
    assert_eq!(s.buttons, 2, "点击 {s:?}");
    assert_eq!(s.keys, 2, "物理键 {s:?}");
    assert_eq!(s.wheels, 1, "滚轮 {s:?}");
    assert_eq!(s.chars, 2, "字符 {s:?}");
    assert_eq!(s.dropped, 0, "不该有没送出去的消息");
    assert_eq!(s.host_errors, 0, "被控端不该报输入错误");

    link.close("测试结束").await;
}

#[tokio::test]
async fn unknown_vk_and_sub_notch_wheel_inject_nothing() {
    let addr = spawn_server().await;
    let cfg = test_config(&addr);
    let out = Arc::new(Mutex::new(Vec::<Inject>::new()));
    let device_id = spawn_host(&addr, cfg.clone(), out.clone()).await;

    let c = timeout(
        Duration::from_secs(20),
        connect::connect(&addr, &device_id, &cfg.password, "输入测试控制端", vec![]),
    )
    .await
    .expect("连接超时")
    .expect("连接失败");
    drop(c.video);
    drop(c.bulk);

    let link = ControlLink::start(c.control);
    let input = link.input();

    // 不认识的 VK：被控端必须**什么都不注入**（绝不能把 VK 数值当 keycode 乱敲）
    assert!(input.send(C2H::Key { vk: 0x0100, down: true, repeat: false }));
    // 不足一格的滚轮：噪声不能被放大成滚动
    assert!(input.send(C2H::MouseWheel { dx: 0, dy: 60 }));
    // 没按下过的松开：忽略
    assert!(input.send(C2H::Key { vk: 0x41, down: false, repeat: false }));

    // 随后发一条真的能注入的，用来确认上面的消息已经被处理过（而不是还在路上）
    assert!(input.send(C2H::MouseMoveAbs { x: 0.0, y: 0.0 }));

    let _ = wait_for(&out, 1).await;
    // 再等一会儿，把可能晚到的多余注入也收上来 —— 「没有多余」本身就是断言。
    tokio::time::sleep(Duration::from_millis(300)).await;
    let got = out.lock().unwrap().clone();
    assert_eq!(
        got,
        vec![Inject::MoveAbs { x: 0, y: 0 }],
        "无效输入不该产生任何注入指令：{got:?}"
    );

    link.close("测试结束").await;
}