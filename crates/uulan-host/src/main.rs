//! uulan-host —— 被控端命令行入口。
//!
//! 用法：
//!   uulan-host                               # 用默认配置连中继，常驻待连
//!   uulan-host --server 10.0.0.1:21116
//!   uulan-host --capture lavfi:testsrc2=size=1280x720:rate=30   # 无需 X11，自测/演示
//!   uulan-host --show                        # 只打印设备 ID 与密码
//!   uulan-host --new-temp-code               # 换一个一次性临时验证码
//!   uulan-host --encoder nvenc|vaapi|x264

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tracing::{info, warn};

use uulan_host::capture::{CaptureInput, Region};
use uulan_host::config::{gen_password, HostConfig};
use uulan_host::session::{self, HostState, SessionOutcome, SessionParams, VideoSettings};
use uulan_media::ffmpeg::{self, Encoder};
use uulan_protocol::rv::{Role, ToEndpoint};
use uulan_protocol::shape::Shape;
use uulan_protocol::rvclient::{RelaySet, RvConn};

/// 向中继发心跳的间隔。远小于服务端的设备超时，避免被误判离线。
/// 心跳间隔。10 秒是刻意压低的：中继靠「多久没消息」判半开连接（`DEVICE_STALE_TTL`
/// 45 秒），心跳太稀就会把仍然活着的机器判成离线。一条 Ping 几十字节，10 秒一次
/// 在内网里可以忽略。
const PING_INTERVAL: Duration = Duration::from_secs(10);

/// 一次性临时码的轮换周期（DESIGN 里定的是「每小时」）。
const TEMP_CODE_ROTATE: Duration = Duration::from_secs(3600);

struct Args {
    config: Option<PathBuf>,
    server: Option<String>,
    capture: Option<String>,
    audio_source: Option<String>,
    /// 麦克风转接的落点覆盖（`speaker` / `null`）。不写 = 配置里的值（默认 speaker）。
    mic_sink: Option<String>,
    /// 按流窗口帧数的命令行覆盖（M8）。不写 = 配置里的值（默认 16）。
    bulk_window: Option<String>,
    ffmpeg: Option<PathBuf>,
    encoder: Option<Encoder>,
    name: Option<String>,
    show: bool,
    new_temp_code: bool,
    /// 每人一份独立口令的管理入口。**口令绝不走命令行**（argv 会进 shell 历史、
    /// 进程列表与审计日志），所以只有"自动生成并打印一次"这一条路。
    user_add: Option<String>,
    user_del: Option<String>,
    list_users: bool,
    once: bool,
    diagnose: bool,
    /// 直连监听地址。默认 `0.0.0.0:21117`：**装了就能用 IP 直接连**。
    listen: Option<String>,
    /// 关掉直连监听（敏感现场、或不想多开一个入站端口时用）。
    no_listen: bool,
    /// 打开 PAM 第二因素，并把名单写进配置（这样 systemd 单元不必带参数）。
    pam: bool,
    /// 关掉第二因素（现场要收回这个权限时不用去手改 JSON）。
    no_pam: bool,
    pam_allow: Option<String>,
    auth: Option<String>,
}

const HELP: &str = "\
uulan-host —— uulan 内网远控的被控端

  --listen <地址:端口> 直连监听地址（默认 0.0.0.0:21117；控制端用 --direct 直连它）
  --no-listen         不监听任何端口（只走中继）
  --pam               打开第二因素：口令过了之后还要输对**系统登录密码**
  --no-pam            关掉第二因素（改完即退出，不常驻）
  --pam-allow <名单>  允许从远端登录的系统账户，逗号分隔（空名单 = 谁都不许）
  --users             列出本机额外登录身份（口令一律不打印）
  --user-add <名字>   加一个人并现场生成一份独立口令（只打印一次）
  --user-del <名字>   吊销一个人的口令（常驻服务当场生效）
  --config <path>     配置文件路径（默认 ~/.config/uulan/host.json）
  --server <addr>     中继地址，覆盖配置里的值（ip:port）
  --capture <spec>    采集源：x11 / x11::0.0 / x11::0.0@<显示器号(0基)> / lavfi:<合成源>[@帧数]
  --ffmpeg <path>     ffmpeg 可执行文件（默认走 PATH）
  --encoder <name>    x264（默认）/ nvenc / vaapi
  --name <name>       本机显示名
  --show              打印设备 ID、密码、临时码后退出（唯一会打印口令的入口）
  --diagnose          自检：显示 / XTest / ffmpeg / 中继 / 配置可写性，给一行结论 + 建议
                      只读，不注册中继、不改配置；退出码 0 = 能跑，1 = 有失败项
  --audio-source <源> 音频采集源（M7）：`pulse:<设备>` / `lavfi:<表达式>`；
  --bulk-window <帧数> 按流窗口帧数（默认 16，1..=96）：广域网上调大能提吞吐；
                      握手时两端取较小值，所以两端都要调
  --mic-sink <落点> 麦克风转接（M7 上行）本机送到哪儿：`speaker`（默认）/
                      `null`（解码后丢弃，验收台专用）；拼错词服务直接起不来
                      不写 = 自动探测 PulseAudio 默认输出的 monitor
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
        audio_source: None,
        mic_sink: None,
        bulk_window: None,
        ffmpeg: None,
        encoder: None,
        name: None,
        show: false,
        new_temp_code: false,
        user_add: None,
        user_del: None,
        list_users: false,
        once: false,
        diagnose: false,
        listen: None,
        no_listen: false,
        pam: false,
        no_pam: false,
        pam_allow: None,
        auth: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        match x.as_str() {
            "--config" => a.config = Some(PathBuf::from(it.next().unwrap_or_default())),
            "--server" => a.server = Some(it.next().unwrap_or_default()),
            "--capture" => a.capture = Some(it.next().unwrap_or_default()),
            "--audio-source" => a.audio_source = Some(it.next().unwrap_or_default()),
            "--mic-sink" => a.mic_sink = Some(it.next().unwrap_or_default()),
            "--bulk-window" => a.bulk_window = Some(it.next().unwrap_or_default()),
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
            "--diagnose" => a.diagnose = true,
            "--new-temp-code" => a.new_temp_code = true,
            "--user-add" => a.user_add = Some(it.next().unwrap_or_default()),
            "--user-del" => a.user_del = Some(it.next().unwrap_or_default()),
            "--users" => a.list_users = true,
            "--listen" => a.listen = Some(it.next().unwrap_or_default()),
            "--no-listen" => a.no_listen = true,
            "--pam" => a.pam = true,
            "--no-pam" => a.no_pam = true,
            "--pam-allow" => a.pam_allow = Some(it.next().unwrap_or_default()),
            // 认证模式：pake（默认，临时码/口令）| os（用户 + 系统登录密码）
            "--auth" => a.auth = Some(it.next().unwrap_or_default()),
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
        // 日志走 stderr，stdout 留给凭据横幅这类「给人看的数据」。
        .with_writer(std::io::stderr)
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
    if let Some(s) = &args.audio_source {
        let v = if s.trim().is_empty() { None } else { Some(s.clone()) };
        if cfg.audio_source != v {
            cfg.audio_source = v;
            dirty = true;
        }
    }
    if let Some(v) = &args.mic_sink {
        // 拼错词当场死（与 power_allow 同纪律）——别把「没配」与「配错」混成一个。
        let parsed = uulan_host::mic::parse_sink(v).map_err(|e| anyhow::anyhow!(e))?;
        let as_str = match parsed {
            uulan_host::mic::MicSink::Speaker => "speaker",
            uulan_host::mic::MicSink::Null => "null",
        };
        if cfg.mic_sink != as_str {
            cfg.mic_sink = as_str.to_string();
            dirty = true;
        }
    }
    if let Some(v) = &args.bulk_window {
        // 帧数写坏了当场死（同 mic_sink / power_allow 纪律）：夹住会让「我配了 96」查不出来。
        let n: u16 = v
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("--bulk-window 要一个整数帧数（收到 {v:?}）"))?;
        if !(uulan_bulk::WINDOW_MIN..=uulan_bulk::WINDOW_MAX).contains(&n) {
            anyhow::bail!(
                "--bulk-window {n} 超出 {}..={} 帧",
                uulan_bulk::WINDOW_MIN,
                uulan_bulk::WINDOW_MAX
            );
        }
        if cfg.bulk_window_frames != n {
            cfg.bulk_window_frames = n;
            dirty = true;
        }
    }
    if dirty {
        cfg.save(&cfg_path)?;
    }

    if args.diagnose {
        // 自检是「只读」模式：上面那几段命令行覆盖已经落盘（和正常启动一致），
        // 但从这里往下不碰中继、不开会话、不碰 X 的全局状态。
        let report = uulan_host::diagnose::run(
            &cfg,
            &cfg_path,
            args.ffmpeg.as_deref(),
            args.encoder.unwrap_or(Encoder::X264),
        )
        .await;
        print!("{}", report.render());
        // 退出码就是结论：脚本和现场都能直接判。
        std::process::exit(if report.all_ok() { 0 } else { 1 });
    }

    // PAM 开关落盘：这样 systemd 单元里不用带参数，重启也还在。
    // 只给 --pam 没给名单时**保持现有名单不变**（可能已经配过）；开了却没名单会当场说清楚，
    // 而不是让人以为开了、实际谁都被拒。
    if args.pam {
        cfg.pam.enabled = true;
    }
    if args.no_pam {
        cfg.pam.enabled = false;
    }
    if let Some(list) = &args.pam_allow {
        cfg.pam.allow_users = list.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    }
    // 管理参数一律"改完就退出"：`--pam` 若继续往下走，就会把这条命令变成
    // 一个前台被控端（我第一次实测就把自己的 ssh 挂到超时，见里程碑文档）。
    if let Some(mode) = &args.auth {
        // 认证模式：os = 用户名 + 系统登录密码（跳过 PAKE）；pake = 临时码/口令（默认）。
        match mode.as_str() {
            "os" => {
                if !cfg.pam.enabled {
                    // 没有 PAM 就没有校验手段，宁可当场拒绝也不留一个"看起来开了"的空壳。
                    anyhow::bail!("--auth os 要求同时开第二因素（--pam），否则没有校验手段");
                }
                cfg.auth_os = true;
            }
            "pake" => cfg.auth_os = false,
            other => anyhow::bail!("--auth 只认 os / pake，收到 {other}"),
        }
        println!("认证模式：{}", if cfg.auth_os { "os（用户名+系统登录密码，PAKE 退出）" } else { "pake（临时码/口令）" });
    }
    if args.pam || args.no_pam || args.pam_allow.is_some() || args.auth.is_some() {
        if cfg.pam.enabled && cfg.pam.allow_users.is_empty() {
            anyhow::bail!("--pam 已打开，但允许名单是空的（空名单 = 谁都不许登录）。加 --pam-allow 用户名1,用户名2");
        }
        cfg.save(&cfg_path)?;
        if cfg.pam.enabled {
            println!("PAM 第二因素：已开启，允许名单 {:?}", cfg.pam.allow_users);
        } else {
            println!("PAM 第二因素：已关闭（allow_users 仅作保留，未生效）");
        }
        println!("已写进配置并立即生效；常驻服务会自己重读（不需要重启）。");
        return Ok(());
    }

    // 用户表管理：改完就存盘退出，常驻服务会通过 adopt_external_config 立刻看见。
    if let Some(name) = &args.user_add {
        let pw = gen_password();
        cfg.add_user(name, &pw)
            .map_err(|e| anyhow::anyhow!("添加用户失败：{e}"))?;
        cfg.save(&cfg_path)?;
        println!("已添加用户: {}", name.trim());
        println!("该用户的口令: {pw}");
        println!("（只打印这一次；关掉它就等于没口令可用。吊销：uulan-host --user-del {}）", name.trim());
        return Ok(());
    }
    if let Some(name) = &args.user_del {
        if !cfg.del_user(name) {
            println!("没有这个用户：{}（--users 看当前名单）", name.trim());
            return Ok(());
        }
        cfg.save(&cfg_path)?;
        println!("已吊销用户 {} 的口令。常驻服务下一次鉴权就不再接受它。", name.trim());
        return Ok(());
    }
    if args.list_users {
        println!("主人（配置里的访问口令）：已设");
        if cfg.users.is_empty() {
            println!("额外用户：（无）");
        } else {
            for u in &cfg.users {
                // 只报长度，不报值 —— 这个命令的输出会被贴进工单和聊天窗口。
                println!("  {}  口令 {} 位", u.name, u.password.chars().count());
            }
        }
        if let Some(t) = &cfg.temp_code {
            println!("一次性临时码：已有一张（{} 位，用一次即失效）", t.chars().count());
        }
        return Ok(());
    }

    if args.new_temp_code {
        let c = cfg.refresh_temp_code();
        cfg.save(&cfg_path)?;
        println!("新的一次性临时验证码: {c}");
        println!("（用掉一次即失效；配置文件已更新）");
        return Ok(());
    }

    if args.show {
        print_credentials(&cfg, &cfg_path, true);
        return Ok(());
    }

    let ffmpeg = ffmpeg::find_ffmpeg(args.ffmpeg.as_deref()).ok_or_else(|| {
        anyhow::anyhow!("找不到 ffmpeg。被控端靠它抓屏与编码，请先安装，或用 --ffmpeg 指定路径。")
    })?;
    let encoder = args.encoder.unwrap_or(Encoder::X264);

    print_credentials(&cfg, &cfg_path, false);
    info!(
        capture = %cfg.capture.describe(),
        encoder = encoder.label(),
        ffmpeg = %ffmpeg.display(),
        "被控端启动"
    );

    let screen = session::resolve_region(&cfg.capture, &cfg).await;
    info!(x = screen.x, y = screen.y, w = screen.w, h = screen.h, "初始抓取区域已确定");
    let state = Arc::new(HostState::new(cfg.clone(), Some(cfg_path.clone())));

    // Ctrl-C 只装一次：以前它装在「一条中继连接的寿命」里，重连循环会把信号处理
    // 反复装一堆（每个空洞一样地等 ctrl_c，只有第一个会真的返回）。
    tokio::spawn(async {
        let _ = tokio::signal::ctrl_c().await;
        info!("收到 Ctrl-C，退出");
        std::process::exit(0);
    });

    // 直连监听：与中继循环**互不依赖**。中继挂了，坐在同一内网的人照样能用 IP 连进来；
    // 反过来端口被人挡了，中继那条路也不受影响。两条路共用同一份 session::run。
    if !args.no_listen {
        let addr = args.listen.clone().unwrap_or_else(|| "0.0.0.0:21117".to_string());
        match tokio::net::TcpListener::bind(&addr).await {
            Ok(l) => {
                info!(%addr, "直连监听已开启（控制端：uulan-client --direct <本机IP>:21117）");
                let st = state.clone();
                let ff = ffmpeg.clone();
                let enc = encoder;
                tokio::spawn(async move {
                    if let Err(e) = serve_direct(st, ff, enc, l).await {
                        warn!(error = %e, "直连监听退出");
                    }
                });
            }
            Err(e) => {
                // 端口被占/无权限：中继模式仍然可用，所以这不该让被控端整个起不来，
                // 但必须在日志里说清楚，否则现场只会看到「直连连不上」而没人知道原因。
                warn!(%addr, error = %e, "直连监听开不起来（只走中继仍可用；要关它就加 --no-listen）");
            }
        }
    } else {
        info!("按 --no-listen 的要求：不开直连监听，只走中继");
    }

    run_forever(state, ffmpeg, cfg, screen, encoder, args.once).await
}

/// 重连退避：1、2、4、8、15、30 秒，之后一直是 30 秒。
///
/// 上限 30 秒是有意的：中继升级/重启只要几秒，退避太狠会让现场白等；
/// 而无限指数增长在网络长期不通时会变成「几小时才试一次」，等于再也回不来。
/// 内网里被控端数量少、单机一条连接，所以不加抖动（抖动只让测试变不确定）。
fn reconnect_delay(attempt: u32) -> Duration {
    const STEPS: [u64; 6] = [1, 2, 4, 8, 15, 30];
    let i = (attempt.saturating_sub(1) as usize).min(STEPS.len() - 1);
    Duration::from_secs(STEPS[i])
}

/// 口令能不能落到 stdout？
///
/// 只有「stdout 是终端」或用户显式 `--show` 才打印。systemd 的 journal、
/// 重定向到文件、管道都算「会被人拿走的地方」，一律打占位符。
/// 这不是洁癖：`journalctl --user -u uulan-host` 是现场第一个会被贴进工单的东西。
fn secrets_allowed(explicit: bool) -> bool {
    explicit || std::io::stdout().is_terminal()
}

fn print_credentials(cfg: &HostConfig, cfg_path: &std::path::Path, explicit: bool) {
    let reveal = secrets_allowed(explicit);
    println!("──────────────────────────────────────────────");
    match &cfg.device_id {
        Some(id) => println!("  设备 ID : {id}"),
        None => println!("  设备 ID : （首次连上中继后分配）"),
    }
    if reveal {
        println!("  访问密码: {}", cfg.password);
        if let Some(t) = &cfg.temp_code {
            println!("  临时验证码: {t}   （用掉一次即失效）");
        }
    } else {
        println!("  访问密码: （已隐藏 —— 输出不是终端；在终端里跑 `uulan-host --show` 查看）");
        if cfg.temp_code.is_some() {
            println!("  临时验证码: （已隐藏，同上）");
        }
    }
    println!("  中继地址: {}", cfg.server);
    println!("  采集源  : {}", cfg.capture.describe());
    println!("  配置文件: {}", cfg_path.display());
    println!("──────────────────────────────────────────────");
}

/// 连中继、注册、常驻待连；**中继断了就退避重连，不退出进程**。
///
/// 以前这里是一条命：中继一断就 `return Err`，全靠 systemd 的 `Restart=always` 把进程
/// 重新拉起来。手工 `run.sh host` 跑的时候没有 systemd，中继重启一次被控端就再也不上线；
/// 而且「重启进程」会重新走一遍注册 —— 那一步正好撞上服务端还没回收旧注册，
/// 于是设备 ID 被改掉（见 `uulan-server` 的 handle_host_register）。
async fn run_forever(
    state: Arc<HostState>,
    ffmpeg: PathBuf,
    cfg: HostConfig,
    screen: Region,
    encoder: Encoder,
    once: bool,
) -> Result<()> {
    let mut attempt: u32 = 0;
    loop {
        match serve_relay(&state, &ffmpeg, &cfg, screen, encoder, once, &mut attempt).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                if once {
                    // --once 是自测语义：一轮结束就退，别把脚本吊在重连里。
                    return Err(e);
                }
                attempt = attempt.saturating_add(1);
                let wait = reconnect_delay(attempt);
                warn!(
                    error = %e,
                    step = attempt,
                    retry_in_s = wait.as_secs_f32(),
                    "与中继的连接断了，稍后重连（进程不退出，设备 ID 不变）"
                );
                tokio::time::sleep(wait).await;
            }
        }
    }
}

/// 一条中继连接的寿命：连接 → 注册 → 待连 → 有人连进来就开会话 → 直到连接断掉。
///
/// 断掉就把错误交回 [`run_forever`] 去退避重连。`attempt` 在**注册成功后清零**，
/// 否则一次长时间在线的连接之后再断，第一次退避就是 30 秒。
async fn serve_relay(
    state: &Arc<HostState>,
    ffmpeg: &PathBuf,
    cfg: &HostConfig,
    mut screen: Region,
    encoder: Encoder,
    once: bool,
    attempt: &mut u32,
) -> Result<()> {
    // 上一代注册成功过（计数被清零）就说明这是重连，别再把凭据块打一遍。
    let first = *attempt == 0;
    let mut conn = RvConn::connect(&cfg.server).await.with_context(|| {
        format!("连不上中继 {}（确认 uulan-server 在跑、地址和防火墙都对）", cfg.server)
    })?;

    let stored = state.device_id();
    // 带上注册令牌：重连时中继凭它把**同一个** device_id 还给这台机器
    // （没令牌就只能给个新的，现场手里的 ID 会失效）。
    let (device_id, fresh) = conn
        .host_register(stored.clone(), cfg.name.clone(), Some(cfg.reg_token.clone()))
        .await?;
    if fresh {
        info!(device_id = %device_id, "中继分配了新的设备 ID，已保存");
    } else {
        info!(device_id = %device_id, "已用原有设备 ID 上线");
    }
    state.set_device_id(&device_id);
    // 注册成功 = 中继通了：退避计数归零。放在这里而不是连接成功时，
    // 是因为「连上但注册被拒」那种情况不适合当已恢复。
    *attempt = 0;

    let settings = VideoSettings::from_config(&state.snapshot());
    // 上面的横幅是在连中继**之前**打的，首次运行时还不知道设备 ID（要连上才分配），
    // 于是会出现「设备 ID:（首次连上中继后分配）」这种先吊胃口的提示。
    // 这里补一条完整的凭据块，用户只需要看这一处。
    //
    // 只在**第一条**连接上打：重连是常态（中继升级、网络抖一下），每次都刷一屏凭据块
    // 会把 journal 里真正有用的东西挤走。重连只留一行 info。
    if first {
        println!("──────────────────────────────────────────────");
        println!("  已上线，等待控制端连接");
        println!("  设备 ID : {device_id}");
        if secrets_allowed(false) {
            println!("  访问密码: {}", state.snapshot().password);
        } else {
            println!("  访问密码: （已隐藏 —— 输出不是终端；在终端里跑 `uulan-host --show` 查看）");
        }
        println!("──────────────────────────────────────────────");
    } else {
        info!(device_id = %device_id, "已重新上线（重连成功），等待控制端连接");
    }

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

    // M6：一次性临时码按小时轮换。没有临时码时什么都不做 ——
    // 绝不能因为「定时器到了」就凭空给这台机器造一个可连入的码。
    let rotate_state = state.clone();
    let rotator = tokio::spawn(async move {
        let mut iv = tokio::time::interval(TEMP_CODE_ROTATE);
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        iv.tick().await; // 第一拍立即返回，跳过（否则一启动就换码）
        loop {
            iv.tick().await;
            if rotate_state.rotate_temp_code().is_some() {
                info!("一次性临时验证码已按小时轮换（新码不写日志，用 uulan-host --show 查看）");
            }
        }
    });

    loop {
        // 唯一的读者。这里不套 select!、不套 timeout ——
        // 读帧不是取消安全的，一旦被砍断就会丢字节。
        let msg = match reader.recv().await {
            Ok(m) => m,
            Err(e) => {
                // 不用在这里再 warn：外层重连循环会带着同一个 error 打一条带退避时间的。
                // 两条并排出现只是把 journal 弄脏（真机日志里一眼能看到重复）。
                tracing::debug!(error = %e, "与中继的连接断了");
                heartbeat.abort();
                rotator.abort();
                return Err(e.into());
            }
        };
        match msg {
            ToEndpoint::Incoming { session, client_name, client_addr } => {
                // 票据只在内存/线上有意义，日志里留不可逆标签（与服务端那三条能对上）。
                info!(session = %uulan_protocol::redact::tag(&session), %client_name, %client_addr, "有人要连进来");
                // 每个会话开头重探一次屏幕尺寸：进程启动时缓存的那个值可能已经过期
                // （有人改了分辨率 / 换了显示器 / 缩放了虚拟机窗口）。过期时的代价很具体：
                // Hello 把旧尺寸报给控制端，编码器又按旧尺寸抓一块不存在的区域当场退出
                // （真机日志就是「视频源结束 frames=0」）。探不到就沿用缓存。
                let snap = state.snapshot();
                screen = session::refresh_region(&snap.capture, &snap, screen).await;
                let params = SessionParams {
                    // 启动时 check() 已验过词表；这里解析失败只可能是有人绕过启动直改
                    // 内存快照 —— 那是 bug，就让它 panic 出声，不许静默退回 speaker。
                    mic_sink: uulan_host::mic::parse_sink(&snap.mic_sink).expect("mic_sink 已在启动时校验"),
                    state: state.clone(),
                    ffmpeg: ffmpeg.clone(),
                    capture: snap.capture,
                    screen,
                    encoder,
                    settings,
                    // None = 按采集源自动挑（X11 采集 → X11 剪贴板）。
                    clipboard: None,
                    screen_probe: None,
                    fs_roots: None,
                    // None = 生产用的 portable-pty。
                    pty: None,
                    // None = 读 /proc（生产）；测试可注入假来源。
                    pam_check: None,
                    sys_source: None,
                    // None = 按采集源自动决定（X11 → 读焦点窗口名）。
                    lock_probe: None,
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
            other => tracing::debug!(shape = other.shape(), "中继推送"),
        }
    }
}

/// 直连服务循环：等人用 IP 拨进来，走与中继**同一段**会话代码。
///
/// 参数装配是照 `serve_relay` 的 `Incoming` 分支抄的，不是重构出来的公共函数 ——
/// 这里刻意留两份：两条路必须逐项一致（剪贴板/终端/端口映射/锁屏判定都由这些参数决定），
/// 一旦哪天只改了其中一处，「走中继能用、走直连不能用」就是最难查的现场问题。
/// 改这里必须同时改那边。
async fn serve_direct(
    state: Arc<HostState>,
    ffmpeg: PathBuf,
    encoder: Encoder,
    listener: tokio::net::TcpListener,
) -> Result<()> {
    loop {
        let (relays, peer) = match uulan_protocol::rvclient::direct_accept_all(&listener).await {
            Ok(x) => x,
            Err(e) => {
                // 只可能有两种来路：端口扫描（第一帧就不是 direct_open），或者
                // 「拨了第一条、另外三条迟迟不到」（5 秒期限）。**没人来连不会走到这里** ——
                // 第一条通道是无限等的（见 rvclient::direct_accept_all 的注释），
                // 所以这条日志出现就意味着「真有人来了但没拨齐」，值得记。
                tracing::debug!(error = %e, "直连半途而废");
                continue;
            }
        };
        let snap = state.snapshot();
        let screen = session::resolve_region(&snap.capture, &snap).await;
        let params = SessionParams {
            mic_sink: uulan_host::mic::parse_sink(&snap.mic_sink).expect("mic_sink 已在启动时校验"),
            state: state.clone(),
            ffmpeg: ffmpeg.clone(),
            capture: snap.capture,
            screen,
            encoder,
            settings: VideoSettings::from_config(&state.snapshot()),
            clipboard: None,
            screen_probe: None,
            fs_roots: None,
            pty: None,
            pam_check: None,
            sys_source: None,
            lock_probe: None,
        };
        info!(%peer, "有人从直连拨进来");
        tokio::spawn(async move {
            // 中继路径里的 client_addr 本来就是字符串（中继报过来的人类可读地址），
            // 直连这里给 SocketAddr 的文本形式，两条路的日志形状一致。
            match session::run(params, relays, peer.to_string()).await {
                SessionOutcome::Closed(why) => info!(%why, "直连会话结束"),
                SessionOutcome::Unauthenticated(p) => warn!(peer = %p, "直连会话因鉴权未通过被拒"),
                SessionOutcome::Error(e) => warn!(error = %e, "直连会话出错"),
            }
        });
    }
}

/// 为一次会话拨齐三条通道。
async fn build_relay(addr: &str, session_id: &str) -> Result<RelaySet> {
    let c = RvConn::connect(addr).await?;
    Ok(c.relay_open_all(session_id, Role::Host).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 退避序列必须是「先小步、再封顶」：中继只是重启几秒，第一次不该等太久；
    /// 长期不通时也不能退到几小时一次。
    #[test]
    fn reconnect_backoff_starts_small_and_caps() {
        let seq: Vec<u64> = (1..=8).map(|i| reconnect_delay(i).as_secs()).collect();
        assert_eq!(seq, vec![1, 2, 4, 8, 15, 30, 30, 30]);
        // 单调不减（防止以后改表时写乱）
        assert!(seq.windows(2).all(|w| w[0] <= w[1]));
        // 极端值：attempt 从 1 起算，但 u32::MAX 也不能溢出/回绕成 0 秒
        assert_eq!(reconnect_delay(u32::MAX).as_secs(), 30);
        assert!(reconnect_delay(1) > Duration::ZERO);
    }
}