//! M4 电源动作验收（`--power` / `H2C::PowerState`）：两端真的接上了。
//!
//! 为什么这条能在本机验：闸门前半（`power_allow` 的解析与「先拒绝后执行」的顺序）
//! 与回话组装全是被单测钉过的纯函数；这条真链路补的是**它们之间那根线** ——
//! 真 TCP + 真协议 + 真被控端任务模型，并真 spawn 一个**无害**子进程
//! （Windows 用 `where.exe lock`、Linux 用 `/bin/false`，都不碰电源），
//! 断言「子进程的退出码原样回到了控制端屏幕上」。
//! 「采纳（退出 0）」那条线上路经在 `scripts/verify-power-e2e.sh`（假执行 exe，
//! 工作区外跑）—— 本机 cargo 测试造不出可控退出码的真 exe，不用纯函数冒充。

use std::sync::Arc;
use std::time::Duration;

use tokio::time::timeout;

use uulan_client::connect;
use uulan_client::power;
use uulan_host::capture::CaptureInput;
use uulan_host::config::HostConfig;
use uulan_host::session::{self, HostState, SessionParams, VideoSettings};
use uulan_media::ffmpeg::{self, Encoder};
use uulan_protocol::msg::{Cap, PowerAction};
use uulan_protocol::rv::{Role, ToEndpoint};
use uulan_protocol::rvclient::RvConn;

const PW: &str = "aB3dE5fG";

/// 无害的「必然非零退出」子进程：证执行机制，不证成功路径。
///
/// Windows 上**别用 `where.exe`**：它会扫整条 PATH，机器被 `cargo test --workspace`
/// 压满时启动能超过被控端 3 秒宽限期，于是回话变成「命令仍在执行」—— 判据假红
/// （2026-11-20 夜批实测：单独跑 0.28 s 全绿，整仓并发跑时这条红）。
/// `whoami.exe <动作名>` 不扫 PATH、46 ms 退 1，语义一样（真 spawn、真退出码）。
fn failing_program() -> String {
    if cfg!(windows) {
        "C:/Windows/System32/whoami.exe".to_string()
    } else {
        "/bin/false".to_string()
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
    cfg.name = "电源测试被控端".into();
    cfg.password = PW.into();
    cfg.capture = CaptureInput::parse("lavfi:testsrc2=size=320x240:rate=5").unwrap();
    cfg.fps = 5;
    cfg.screen_width = 320;
    cfg.screen_height = 240;
    cfg
}

/// 起一个被控端。返回设备 ID。（骨架与 terms_path 同一套。）
async fn spawn_host(server: &str, cfg: HostConfig) -> String {
    let mut conn = RvConn::connect(server).await.expect("被控端连不上中继");
    let (device_id, _) = conn
        .host_register(None, cfg.name.clone(), None)
        .await
        .expect("注册失败");

    let state = Arc::new(HostState::new(cfg.clone(), None));
    let screen = session::resolve_region(&cfg.capture, &cfg).await;
    let settings = VideoSettings::from_config(&cfg);
    let addr = conn.server_addr().to_string();
    let ffmpeg = ffmpeg::find_ffmpeg(None).unwrap_or_else(|| std::path::PathBuf::from("ffmpeg"));

    tokio::spawn(async move {
        loop {
            let msg = match conn.recv().await {
                Ok(m) => m,
                Err(_) => break,
            };
            if let ToEndpoint::Incoming {
                session: sid,
                client_addr,
                ..
            } = msg
            {
                let params = SessionParams {
                    pam_check: None,
                    mic_sink: uulan_host::mic::MicSink::Speaker,
                    state: state.clone(),
                    ffmpeg: ffmpeg.clone(),
                    capture: cfg.capture.clone(),
                    screen,
                    encoder: Encoder::X264,
                    settings,
                    clipboard: Some(Box::new(uulan_clip::NoClipboard::new("电源测试"))),
                    screen_probe: None,
                    fs_roots: None,
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

/// 默认配置：闸门必须**死在 spawn 之前** —— 连无害假程序都不许起。
#[tokio::test]
async fn an_unlisted_power_action_is_denied_on_the_wire_with_the_fix() {
    let srv = spawn_server().await;
    let device = spawn_host(&srv, test_config(&srv)).await;

    let c = timeout(
        Duration::from_secs(15),
        connect::connect(&srv, &device, PW, "power-cli", vec![],
            None,  // 集成测试都走中继
),
    )
    .await
    .expect("连接超时")
    .expect("连接失败");
    drop(c.video);

    let (ok, detail) = timeout(
        Duration::from_secs(12),
        power::ask(c.control, PowerAction::Reboot),
    )
    .await
    .expect("回话超时（Power 没接上？）")
    .expect("收发失败");
    assert!(!ok, "默认配置下 reboot 必须被拒");
    assert!(detail.contains("power_allow"), "拒绝理由要指向配置：{detail}");
}

/// 放行名单 + 无害假程序：真 spawn、真退出码、真回上线。
#[tokio::test]
async fn an_allowed_power_action_really_executes_and_reports_the_child() {
    let srv = spawn_server().await;
    let mut cfg = test_config(&srv);
    cfg.power_allow = vec!["lock".to_string()];
    cfg.power_test_program = Some(failing_program());
    let device = spawn_host(&srv, cfg).await;

    let c = timeout(
        Duration::from_secs(15),
        connect::connect(&srv, &device, PW, "power-cli2", vec![],
            None,  // 集成测试都走中继
),
    )
    .await
    .expect("二次连接超时")
    .expect("二次连接失败");
    drop(c.video);

    // 1) 名单里 + 起得来的程序：退出码回到线上。where lock / /bin/false 都是退出 1。
    let (ok, detail) = timeout(
        Duration::from_secs(12),
        power::ask(c.control, PowerAction::Lock),
    )
    .await
    .expect("回话超时")
    .expect("收发失败");
    assert!(!ok, "非零退出不能算采纳：{detail}");
    assert!(detail.contains("退出码 1"), "子进程退出码必须原样回来：{detail}");

    // 2) 名单外（上面只放了 lock）：拒绝理由要带「当前放行名单」，照着就能改。
    let c2 = timeout(
        Duration::from_secs(15),
        connect::connect(&srv, &device, PW, "power-cli3", vec![],
            None,  // 集成测试都走中继
),
    )
    .await
    .expect("三次连接超时")
    .expect("三次连接失败");
    drop(c2.video);
    let (ok, detail) = timeout(
        Duration::from_secs(12),
        power::ask(c2.control, PowerAction::Logout),
    )
    .await
    .expect("回话超时")
    .expect("收发失败");
    assert!(!ok);
    assert!(detail.contains("lock"), "拒绝理由要列出当前名单：{detail}");
    assert!(
        !detail.contains("退出码"),
        "名单外绝不能走到执行那一步：{detail}"
    );
}

/// 老口径不回归：没有 Power 相关能力声明要求 —— 上面两条走的就是裸 caps 连接。
#[tokio::test]
async fn power_needs_no_capability_claim_to_answer() {
    let srv = spawn_server().await;
    let device = spawn_host(&srv, test_config(&srv)).await;
    let c = timeout(
        Duration::from_secs(15),
        connect::connect(&srv, &device, PW, "power-cli4", vec![Cap::Files],
            None,  // 集成测试都走中继
),
    )
    .await
    .expect("四次连接超时")
    .expect("连接失败");
    drop(c.video);
    let (ok, _) = timeout(
        Duration::from_secs(12),
        power::ask(c.control, PowerAction::Lock),
    )
    .await
    .expect("回话超时")
    .expect("收发失败");
    assert!(!ok, "没放行就是没放行，与声明了什么能力无关");
}
