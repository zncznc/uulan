//! uulan-host —— 被控端命令行入口。
//!
//! 用法：
//!   uulan-host                               # 用默认配置连中继，常驻待连
//!   uulan-host --server 10.0.0.1:21116
//!   uulan-host --capture lavfi:testsrc2=size=1280x720:rate=30   # 无需 X11，自测/演示
//!   uulan-host --show                        # 只打印设备 ID 与密码
//!   uulan-host --new-temp-code               # 换一个一次性临时验证码
//!   uulan-host --encoder nvenc|vaapi|x264

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tracing::{info, warn};

use uulan_host::capture::CaptureInput;
use uulan_host::config::HostConfig;
use uulan_host::session::{self, HostState, SessionOutcome, SessionParams, VideoSettings};
use uulan_media::ffmpeg::{self, Encoder};
use uulan_protocol::msg::Screen;
use uulan_protocol::rv::{Role, ToEndpoint};
use uulan_protocol::rvclient::{RelaySet, RvConn};

/// 向中继发心跳的间隔。远小于服务端的设备超时，避免被误判离线。
const PING_INTERVAL: Duration = Duration::from_secs(30);

struct Args {
    config: Option<PathBuf>,
    server: Option<String>,
    capture: Option<String>,
    ffmpeg: Option<PathBuf>,
    encoder: Option<Encoder>,
    name: Option<String>,
    show: bool,
    new_temp_code: bool,
    once: bool,
}

const HELP: &str = "\
uulan-host —— uulan 内网远控的被控端

  --config <path>     配置文件路径（默认 ~/.config/uulan/host.json）
  --server <addr>     中继地址，覆盖配置里的值（ip:port）
  --capture <spec>    采集源：x11 / x11::0.0 / lavfi:<合成源>[@帧数]
  --ffmpeg <path>     ffmpeg 可执行文件（默认走 PATH）
  --encoder <name>    x264（默认）/ nvenc / vaapi
  --name <name>       本机显示名
  --show              打印设备 ID、密码、临时码后退出
  --new-temp-code     生成新的一次性临时验证码，落盘后退出
  --once              收到一次会话并结束后就退出（自测用）
  -h, --help          显示本帮助

首次运行会自动生成 12 位访问密码并写入配置文件（Unix 下权限 0600）。
把「设备 ID + 密码」给到控制端的人即可让他连上。
";

fn parse_args() -> Result<Args> {
    let mut a = Args {
        config: None,
        server: None,
        capture: None,
        ffmpeg: None,
        encoder: None,
        name: None,
        show: false,
        new_temp_code: false,
        once: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        match x.as_str() {
            "--config" => a.config = Some(PathBuf::from(it.next().unwrap_or_default())),
            "--server" => a.server = Some(it.next().unwrap_or_default()),
            "--capture" => a.capture = Some(it.next().unwrap_or_default()),
            "--ffmpeg" => a.ffmpeg = Some(PathBuf::from(it.next().unwrap_or_default())),
            "--name" => a.name = Some(it.next().unwrap_or_default()),
            "--encoder" => {
                let v = it.next().unwrap_or_default();
                a.encoder = Some(match v.as_str() {
                    "x264" | "cpu" | "libx264" => Encoder::X264,
                    "nvenc" | "h264_nvenc" => Encoder::Nvenc,
                    "vaapi" | "h264_vaapi" => Encoder::Vaapi,
                    other => bail!("未知编码器 {other}（可用 x264 / nvenc / vaapi）"),
                });
            }
            "--show" => a.show = true,
            "--new-temp-code" => a.new_temp_code = true,
            "--once" => a.once = true,
            "-h" | "--help" => {
                println!("{HELP}");
                std::process::exit(0);
            }
            other => bail!("未知参数 {other}（用 --help 查看用法）"),
        }
    }
    Ok(a)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = parse_args()?;

    let cfg_path = args.config.clone().unwrap_or_else(HostConfig::default_path);
    let mut cfg = HostConfig::load_or_create(&cfg_path)
        .with_context(|| format!("读取配置 {} 失败", cfg_path.display()))?;
    cfg.check().map_err(|e| anyhow::anyhow!("配置不合法: {e}"))?;

    // 命令行覆盖（覆盖值也写回配置，免得下次又要敲一遍）
    let mut dirty = false;
    if let Some(s) = &args.server {
        if cfg.server != *s {
            cfg.server = s.clone();
            dirty = true;
        }
    }
    if let Some(n) = &args.name {
        if cfg.name != *n {
            cfg.name = n.clone();
            dirty = true;
        }
    }
    if let Some(c) = &args.capture {
        let parsed = CaptureInput::parse(c).map_err(|e| anyhow::anyhow!(e))?;
        if cfg.capture != parsed {
            cfg.capture = parsed;
            dirty = true;
        }
    }
    if dirty {
        cfg.save(&cfg_path)?;
    }

    if args.new_temp_code {
        let c = cfg.refresh_temp_code();
        cfg.save(&cfg_path)?;
        println!("新的一次性临时验证码: {c}");
        println!("（用掉一次即失效；配置文件已更新）");
        return Ok(());
    }

    if args.show {
        print_credentials(&cfg, &cfg_path);
        return Ok(());
    }

    let ffmpeg = ffmpeg::find_ffmpeg(args.ffmpeg.as_deref()).ok_or_else(|| {
        anyhow::anyhow!("找不到 ffmpeg。被控端靠它抓屏与编码，请先安装，或用 --ffmpeg 指定路径。")
    })?;
    let encoder = args.encoder.unwrap_or(Encoder::X264);

    print_credentials(&cfg, &cfg_path);
    info!(
        capture = %cfg.capture.describe(),
        encoder = encoder.label(),
        ffmpeg = %ffmpeg.display(),
        "被控端启动"
    );

    let screen = session::resolve_screen(&cfg.capture, &cfg).await;
    let state = Arc::new(HostState::new(cfg.clone(), Some(cfg_path.clone())));

    run_forever(state, ffmpeg, cfg, screen, encoder, args.once).await
}

fn print_credentials(cfg: &HostConfig, cfg_path: &std::path::Path) {
    println!("──────────────────────────────────────────────");
    match &cfg.device_id {
        Some(id) => println!("  设备 ID : {id}"),
        None => println!("  设备 ID : （首次连上中继后分配）"),
    }
    println!("  访问密码: {}", cfg.password);
    if let Some(t) = &cfg.temp_code {
        println!("  临时验证码: {t}   （用掉一次即失效）");
    }
    println!("  中继地址: {}", cfg.server);
    println!("  采集源  : {}", cfg.capture.describe());
    println!("  配置文件: {}", cfg_path.display());
    println!("──────────────────────────────────────────────");
}

/// 连中继、注册、常驻待连。
async fn run_forever(
    state: Arc<HostState>,
    ffmpeg: PathBuf,
    cfg: HostConfig,
    screen: Screen,
    encoder: Encoder,
    once: bool,
) -> Result<()> {
    let mut conn = RvConn::connect(&cfg.server).await.with_context(|| {
        format!("连不上中继 {}（确认 uulan-server 在跑、地址和防火墙都对）", cfg.server)
    })?;

    let stored = state.device_id();
    let (device_id, fresh) = conn.host_register(stored.clone(), cfg.name.clone()).await?;
    if fresh {
        info!(device_id = %device_id, "中继分配了新的设备 ID，已保存");
    } else {
        info!(device_id = %device_id, "已用原有设备 ID 上线");
    }
    state.set_device_id(&device_id);

    let settings = VideoSettings::from_config(&state.snapshot());
    // 上面的横幅是在连中继**之前**打的，首次运行时还不知道设备 ID（要连上才分配），
    // 于是会出现「设备 ID:（首次连上中继后分配）」这种先吊胃口的提示。
    // 这里补一条完整的凭据块，用户只需要看这一处。
    println!("──────────────────────────────────────────────");
    println!("  已上线，等待控制端连接");
    println!("  设备 ID : {device_id}");
    println!("  访问密码: {}", state.snapshot().password);
    println!("──────────────────────────────────────────────");

    // Ctrl-C 单独一个任务：不能用 select! 和读端一起等，否则读端会被反复取消。
    tokio::spawn(async {
        let _ = tokio::signal::ctrl_c().await;
        info!("收到 Ctrl-C，退出");
        std::process::exit(0);
    });

    // 读写彻底分开。
    //
    // 以前这里是一个 `select!`，同时等 `conn.recv()` 和 ping 定时器，结果有两个
    // 致命问题（真机上被「控制端一连接、被控端就退出」暴露出来）：
    //   1. select! 丢弃未选中的 future，正在读的帧被砍断，字节永久丢失；
    //   2. ping 要等 Pong 就得独占读取权，必然撞上插进来的 Incoming 并判违规。
    // 现在读端（下面的 loop）只有一个读者且从不被取消；写端独立成任务。
    let (mut reader, mut writer) = conn.into_split();

    // 心跳任务：只发 Ping，不等回包。Pong 由读端消费。
    // 写失败说明连接已经废了，停下即可 —— 读端随后会读到 EOF 并退出。
    let heartbeat = tokio::spawn(async move {
        let mut iv = tokio::time::interval(PING_INTERVAL);
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        iv.tick().await;
        loop {
            iv.tick().await;
            if let Err(e) = writer.ping().await {
                warn!(error = %e, "心跳发送失败，停止心跳");
                return;
            }
        }
    });

    loop {
        // 唯一的读者。这里不套 select!、不套 timeout ——
        // 读帧不是取消安全的，一旦被砍断就会丢字节。
        let msg = match reader.recv().await {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "与中继的连接断了");
                heartbeat.abort();
                return Err(e.into());
            }
        };
        match msg {
            ToEndpoint::Incoming { session, client_name, client_addr } => {
                info!(%session, %client_name, %client_addr, "有人要连进来");
                let params = SessionParams {
                    state: state.clone(),
                    ffmpeg: ffmpeg.clone(),
                    capture: state.snapshot().capture,
                    screen,
                    encoder,
                    settings,
                };
                let conn_addr = state.snapshot().server.clone();
                tokio::spawn(async move {
                    match build_relay(&conn_addr, &session).await {
                        Ok(relays) => {
                            let outcome = session::run(params, relays, client_addr).await;
                            match &outcome {
                                SessionOutcome::Closed(why) => info!(%why, "会话结束"),
                                SessionOutcome::Unauthenticated(p) => {
                                    warn!(peer = %p, "会话因鉴权未通过被拒")
                                }
                                SessionOutcome::Error(e) => warn!(error = %e, "会话出错"),
                            }
                        }
                        Err(e) => warn!(error = %e, "拨通会话通道失败"),
                    }
                    if once {
                        // 自测用：一轮结束就把进程退掉，方便脚本收尾
                        std::process::exit(0);
                    }
                });
            }
            ToEndpoint::Pong { server_time_ms } => {
                tracing::debug!(server_time_ms, "心跳正常");
            }
            ToEndpoint::Error { code, message } => {
                // 单个请求出错不该要了进程的命，记下来继续待连。
                warn!(?code, %message, "中继报错");
            }
            other => tracing::debug!(?other, "中继推送"),
        }
    }
}

/// 为一次会话拨齐三条通道。
async fn build_relay(addr: &str, session_id: &str) -> Result<RelaySet> {
    let c = RvConn::connect(addr).await?;
    Ok(c.relay_open_all(session_id, Role::Host).await?)
}
