//! uulan-client —— 控制端。
//!
//! **默认开窗口**：连上被控端、握手、收画面解码，然后把最新一帧画到窗口里。
//! 加 `--headless` 回到命令行模式 —— 那是排障与基准用的路径（可在无显示环境跑，
//! 也是端到端测试用的那条），统计输出的格式和以前完全一致。
//!
//! 用法：
//!   uulan-client --server 10.0.0.1:21116 --device 123456789 --password aB3dE5fG
//!   uulan-client ... --headless --frames 120   # 命令行模式，解出 120 帧后退出
//!   uulan-client ... --headless --json         # 以 JSON 打印统计，便于脚本消费
//!   uulan-client --server 10.0.0.1:21116 --devices   # 只看设备目录，不连任何一台

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tracing::info;

use uulan_client::connect::{self, ClientError};
use uulan_client::session;
use uulan_client::watch::WatchStatsSnapshot;
use uulan_protocol::msg::{Cap, Quality, VideoSet};
use uulan_protocol::rv::DeviceEntry;
use uulan_protocol::rvclient::{RvConn, RvError};

struct Args {
    server: String,
    device: String,
    /// 口令。**别从命令行传**：`--password` 会留在 `ps`、shell 历史、工单截图里。
    /// 推荐 `--password-file`（首行）或环境变量 `UULAN_PASSWORD`，见 `resolve_password`。
    password: String,
    /// 从文件读口令（取首行、去空白）。与 `UULAN_PASSWORD` 二选一即可。
    password_file: Option<PathBuf>,
    name: String,
    frames: Option<u64>,
    seconds: Option<f64>,
    ffmpeg: Option<PathBuf>,
    json: bool,
    save_frame: Option<PathBuf>,
    /// 不开窗口，走原来的命令行模式。
    headless: bool,
    /// 只读模式：不上报键鼠（排障、旁观时用）。
    no_input: bool,
    /// 不参与剪贴板同步（排障 / 不希望本机剪贴板被对面改写时用）。
    no_clipboard: bool,
    /// 文件操作（给了就只干这件事，不开窗口也不收画面）。
    fs_op: Option<uulan_client::fsx::Op>,
    /// 端口映射规则（给了就只干这件事，不开窗口也不收画面）。
    forward: Vec<uulan_client::ForwardSpec>,
    /// 开一个远端终端（交互式），同样不开窗口也不收画面。
    term: bool,
    /// 终端行列（`--term-size 140x40`）。不给就按本机终端大小来。
    term_size: Option<(u16, u16)>,
    /// 远端 shell（不给用被控端配置/环境里的）。
    term_shell: Option<String>,
    /// 脚本化键鼠注入（`--inject`）：只发输入、不收画面，跑完就退出。
    /// 存在的理由是「键鼠真的打进了被控端」必须能机器验证，不能靠人手点。
    inject: Option<String>,
    /// 现场注入（`--inject-live`）：在**正在看画面的这条会话里**按脚本发键鼠。
    /// 与 `--inject` 的区别不只是「顺便收画面」—— 关键是每一步都按**当时**的画幅换算，
    /// 所以它是「被控端中途换分辨率之后落点跟不跟得上」唯一的机器可验路径
    /// （`--inject` 是独立会话 + 握手时的固定尺寸，天生验不了这条）。
    inject_live: Option<String>,
    /// 只列设备目录（`--devices`）：连中继、拉清单、打印、退出。
    /// 不需要 `--device`，也不需要口令 —— 它回答的正是「我该连哪台」。
    devices: bool,
    /// 用本机记住的上一次连接（`--last`）补上空着的 `--server` / `--device`。
    /// **命令行优先**：显式给了哪个就不覆盖哪个。
    last: bool,
    /// 忘掉记住的上一次连接（`--forget`），不需要 `--device` 和口令。
    forget: bool,
    /// `--server` 是否真的出现在命令行里。
    /// 默认值是非空字符串，光看 `server.is_empty()` 分不出「没给」和「给了默认值」。
    server_explicit: bool,
    /// 只看被控端状态（`--status <秒>`）：读控制通道上的 Metrics / SystemState，
    /// 每秒打一行。存在的理由同 `--inject`：负载与锁屏也必须能机器验证。
    status: Option<f64>,
    /// 连接阶段的重试次数（默认 3，0 = 不重试）。**只在「连不上」时重试**，
    /// 口令错/被锁定这类「被拒」绝不重试 —— 那是自己往锁定计数里加数。
    connect_retry: u32,
    /// 会话中途断线后最多连续重连几次（默认 8，0 = 断线即退出）。
    /// 与 `connect_retry` 是两件事：那是「一开始就没连上」，这是「连上了又断」。
    reconnect: u32,
    /// 画质档位（`--quality`）。不给 = 用被控端自己的配置。
    quality: Option<uulan_protocol::msg::Quality>,
    /// 码率上限 kbps（`--bitrate`）。给了就走 `Quality::Custom`。
    bitrate: Option<u32>,
    /// 请求的编码帧率（`--fps`）。0/None = 不动。
    fps: Option<u32>,
    /// 编码缩放倍率（`--scale`）。1.0 = 原始分辨率。
    scale: Option<f32>,
    /// 要声音（`--audio`）。不给就不碰音频通道 —— 被控端那边连编码器都不会起。
    audio: bool,
    /// 把收到的声音落成 WAV 文件（`--audio-save`）。无人值守验收靠它给证据。
    audio_save: Option<PathBuf>,
}

const HELP: &str = "\
uulan-client —— uulan 内网远控的控制端

默认打开窗口显示被控端画面；加 --headless 退回命令行模式。

  --server <addr>     中继地址（默认 127.0.0.1:21116）
  --device <id>       被控端设备 ID（9 位数字）
  --password <pwd>    访问密码或一次性临时验证码（会出现在 ps/历史里，能用别的就用别的）
  --password-file <f> 从文件首行读口令（推荐：不进进程表，也不进 shell 历史）
                      也可以设环境变量 UULAN_PASSWORD；优先级 --password > 文件 > 环境变量
  --name <name>       本机在对方界面上显示的名字
  --frames <N>        解出 N 帧后退出
  --seconds <S>       跑 S 秒后退出
  --ffmpeg <path>     ffmpeg 可执行文件（默认走 PATH）
  --save-frame <path> 退出时把最后一帧存成 PPM（排查「对面到底画的是什么」用）
  --headless          不开窗口，只打印统计（自测 / 无显示环境用）
  --audio             接收被控端的声音（PulseAudio monitor → Opus）
  --audio-save <path> 把声音存成 WAV（同时隐含 --audio；无人值守验收用）
  --no-input          只读模式：不上报键鼠（排障、旁观时用）
  --no-clipboard      不同步剪贴板（不希望本机剪贴板被对面改写时用）
  --json              以 JSON 输出（--headless 的统计 / --devices 的设备目录）
  --connect-retry <N> 连接阶段最多重试 N 次（默认 3；0 = 连一次就报错退出）
                      只在「连不上」时重试；口令错/被锁直接报错，不重试
  --quality <档位>    画质档位：auto|fast|high|hd|bluray（不给 = 用被控端配置）
  --bitrate <kbps>    码率上限，给了就覆盖档位（例：--bitrate 2000）
  --fps <N>           请求的编码帧率（1..144）
  --scale <倍率>      编码缩放，0.1..1.0（例：--scale 0.75 = 四分之三画幅，带宽跟着降）
                      只缩编码、不改被控端显示器分辨率；窗口里看到的是缩放后的画面
  --reconnect <N>     会话中途断线后最多连续重连 N 次（默认 8；0 = 断线即退出）
                      退避 1/2/4/8/15 秒；口令错、协议不匹配这类不重试
                      中继重启（升级）后窗口会自己接回来，不用重开

文件操作（做完就退出，不开窗口、不收画面）：
  --ls <远程路径>            列目录（`:/` = 此电脑，`~/` = 家目录）
  --get <远程路径> <本地路径>  下载
  --put <本地路径> <远程路径>  上传
  --rm <远程路径>            删除文件（目录必须为空）
  --mkdir <远程路径>         新建目录
  --put-tree <本地目录> <远程目录>  整棵本地目录树上传（只读模式/符号链接跳过）
  --get-tree <远程目录> <本地目录>  整棵远端目录树下载（先清单、再逐个校验）

终端（交互式；不开窗口、不收画面；客户端退出码 = 远端 shell 的退出码）：
  --term                      在远端开一个 shell
  --term-size <COLSxROWS>     行列数，例：--term-size 140x40（不写按本机终端大小）
  --term-shell <path>         指定远端 shell，例：--term-shell /bin/bash

键鼠注入（脚本化，做完就退出；不开窗口、不收画面）：
  --devices           列中继知道的设备（ID / 名称 / 在线），不连任何一台
                      不需要 --device 和口令；加 --json 输出机器可读格式
  --status <秒>       只读被控端状态（CPU/内存/锁屏），每秒一行，跑完退出
  --inject \"<脚本>\"           沿真实链路把键鼠打进被控端，用来机器验证「真的打进去了」
                             步骤用 ; 分隔：move:X,Y（屏幕像素）/ movn:X,Y（0..1）
                             click:left|right|middle / down:left / up:left
                             wheel:up|down|left|right[:N] / tap:VK / keydown:VK / keyup:VK
                             type:文本 / wait:毫秒
                             例：--inject \"move:960,540; click:left; type:hello\"
                             退出码 0 = 全部注入成功；3 = 被控端报告注入失败
  --inject-live \"<脚本>\"  在**正在看的这条会话里**发键鼠（画面照常收，窗口 / headless 都行）
                             步骤写法与 --inject 一样，区别有两条：
                               · 画幅是**每一步现取**的（被控端中途换分辨率，落点跟着换）
                               · 跑完不退出，会话按 --frames / --seconds 收尾
                             例：--headless --seconds 40 --inject-live
                                   \"wait:4000; move:960,540; wait:12000; move:640,360\"
                             每发一条移动打一行「注入换算 : 画幅=… → 归一化=…」，
                             验收脚本据此把「控制端按新尺寸换算」与「被控端按新尺寸落点」对上号

上次连接（本机记一个文件，**不存口令**）：
  --last              用上次连成功的那台补上空着的 --server / --device
                      例：uulan-client --last --password-file ~/.uulan-pw
                      命令行显式给的 --server / --device 优先，不会被覆盖
  --forget            忘掉记住的那台（删掉 ~/.config/uulan/client.json）

端口映射（做完就退出，不开窗口、不收画面；可写多次）：
  --forward [绑定地址:]本地端口:目标主机:目标端口
                             例：--forward 2222:127.0.0.1:22
                             目标地址站在被控端的立场解析，白名单由被控端判定
                             不加 --seconds 就一直转发，Ctrl-C 结束
  -h, --help          显示本帮助

退出码：0 正常；1 被控端拒绝（密码错/离线）；2 用法错；4 断线后重连放弃。
";

fn parse_args() -> Result<Args> {
    let mut a = Args {
        server: format!("127.0.0.1:{}", uulan_protocol::DEFAULT_PORT),
        device: String::new(),
        password: String::new(),
        password_file: None,
        name: default_client_name(),
        frames: None,
        seconds: None,
        ffmpeg: None,
        json: false,
        save_frame: None,
        headless: false,
        no_input: false,
        no_clipboard: false,
        fs_op: None,
        forward: Vec::new(),
        term: false,
        term_size: None,
        term_shell: None,
        inject: None,
        inject_live: None,
        status: None,
        devices: false,
        last: false,
        forget: false,
        server_explicit: false,
        connect_retry: DEFAULT_CONNECT_RETRY,
        reconnect: DEFAULT_RECONNECT,
        quality: None,
        bitrate: None,
        fps: None,
        scale: None,
        audio: false,
        audio_save: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        match x.as_str() {
            "--server" => {
                a.server = it.next().unwrap_or_default();
                a.server_explicit = true;
            }
            "--device" => a.device = it.next().unwrap_or_default(),
            "--password" => a.password = it.next().unwrap_or_default(),
            "--password-file" => {
                a.password_file = Some(PathBuf::from(
                    it.next().context("--password-file 需要一个文件路径")?,
                ))
            }
            "--name" => a.name = it.next().unwrap_or_default(),
            "--frames" => {
                a.frames = Some(it.next().unwrap_or_default().parse().context("--frames 要数字")?)
            }
            "--seconds" => {
                a.seconds = Some(it.next().unwrap_or_default().parse().context("--seconds 要数字")?)
            }
            "--ffmpeg" => a.ffmpeg = Some(PathBuf::from(it.next().unwrap_or_default())),
            "--save-frame" => a.save_frame = Some(PathBuf::from(it.next().unwrap_or_default())),
            "--json" => a.json = true,
            "--headless" => a.headless = true,
            "--audio" => a.audio = true,
            "--audio-save" => {
                a.audio_save = Some(PathBuf::from(it.next().context("--audio-save 需要一个路径")?));
                // 只要落盘就是「要声音」，否则用户以为给了路径就会有文件。
                a.audio = true;
            }
            "--no-input" => a.no_input = true,
            "--no-clipboard" => a.no_clipboard = true,
            "--forward" => {
                let spec = it.next().context("--forward 需要 [绑定地址:]本地端口:目标主机:目标端口")?;
                a.forward.push(uulan_client::ForwardSpec::parse(&spec)?);
            }
            "--devices" => a.devices = true,
            "--last" => a.last = true,
            "--forget" => a.forget = true,
            "--term" => a.term = true,
            "--term-size" => {
                let v = it.next().context("--term-size 需要 COLSxROWS，例：140x40")?;
                a.term_size = Some(parse_size(&v)?);
            }
            "--term-shell" => a.term_shell = Some(it.next().context("--term-shell 需要一个路径")?),
            "--status" => {
                a.status = Some(
                    it.next()
                        .context("--status 需要一个秒数，例如 --status 8")?
                        .parse()
                        .context("--status 的秒数不是数字")?,
                )
            }
            "--inject" => {
                a.inject = Some(it.next().context("--inject 需要一个脚本，例如 \"click:left\"")?)
            }
            "--inject-live" => {
                a.inject_live = Some(
                    it.next()
                        .context("--inject-live 需要一个脚本，例如 \"wait:8000; move:640,360\"")?,
                )
            }
            "--quality" => {
                let v = it.next().context("--quality 需要一个档位：auto|fast|high|hd|bluray")?;
                a.quality = Some(parse_quality(&v)?);
            }
            "--bitrate" => {
                let v: u32 = it
                    .next()
                    .context("--bitrate 需要 kbps，例如 --bitrate 2000")?
                    .parse()
                    .context("--bitrate 的数值不是数字")?;
                if v == 0 {
                    bail!("--bitrate 必须大于 0（想「不设上限」就别给这个参数）");
                }
                a.bitrate = Some(v);
            }
            "--fps" => {
                let v: u32 = it
                    .next()
                    .context("--fps 需要帧率，例如 --fps 20")?
                    .parse()
                    .context("--fps 的数值不是数字")?;
                if !(1..=144).contains(&v) {
                    bail!("--fps 要在 1..=144 之间");
                }
                a.fps = Some(v);
            }
            "--scale" => {
                let v: f32 = it
                    .next()
                    .context("--scale 需要倍率，例如 --scale 0.75")?
                    .parse()
                    .context("--scale 的数值不是数字")?;
                if !(0.1..=1.0).contains(&v) {
                    bail!("--scale 要在 0.1..=1.0 之间（不放大，远控放大没有意义）");
                }
                a.scale = Some(v);
            }
            "--reconnect" => {
                a.reconnect = it
                    .next()
                    .context("--reconnect 需要一个次数，例如 --reconnect 5")?
                    .parse()
                    .context("--reconnect 的次数不是数字")?
            }
            "--connect-retry" => {
                a.connect_retry = it
                    .next()
                    .context("--connect-retry 需要一个次数，例如 --connect-retry 5")?
                    .parse()
                    .context("--connect-retry 的次数不是数字")?
            }
            "--ls" => {
                let p = it.next().context("--ls 需要一个远程路径")?;
                a.fs_op = Some(uulan_client::fsx::Op::Ls(p));
            }
            "--rm" => {
                let p = it.next().context("--rm 需要一个远程路径")?;
                a.fs_op = Some(uulan_client::fsx::Op::Rm(p));
            }
            "--mkdir" => {
                let p = it.next().context("--mkdir 需要一个远程路径")?;
                a.fs_op = Some(uulan_client::fsx::Op::Mkdir(p));
            }
            "--get" => {
                let remote = it.next().context("--get 需要 <远程路径> <本地路径>")?;
                let local = it.next().context("--get 需要 <远程路径> <本地路径>")?;
                a.fs_op = Some(uulan_client::fsx::Op::Get { remote, local: PathBuf::from(local) });
            }
            "--put" => {
                let local = it.next().context("--put 需要 <本地路径> <远程路径>")?;
                let remote = it.next().context("--put 需要 <本地路径> <远程路径>")?;
                a.fs_op = Some(uulan_client::fsx::Op::Put { local: PathBuf::from(local), remote });
            }
            "--put-tree" => {
                let local = it.next().context("--put-tree 需要 <本地目录> <远程目录>")?;
                let remote = it.next().context("--put-tree 需要 <本地目录> <远程目录>")?;
                a.fs_op = Some(uulan_client::fsx::Op::PutTree {
                    local: PathBuf::from(local),
                    remote,
                });
            }
            "--get-tree" => {
                let remote = it.next().context("--get-tree 需要 <远程目录> <本地目录>")?;
                let local = it.next().context("--get-tree 需要 <远程目录> <本地目录>")?;
                a.fs_op = Some(uulan_client::fsx::Op::GetTree {
                    remote,
                    local: PathBuf::from(local),
                });
            }
            "-h" | "--help" => {
                println!("{HELP}");
                std::process::exit(0);
            }
            other => bail!("未知参数 {other}（用 --help 查看用法）"),
        }
    }
    if a.server.is_empty() {
        bail!("--server 不能为空");
    }
    // `--forget` 只删本机的一个小文件：不连中继、不需要设备 ID、也不需要口令。
    if a.forget {
        return Ok(a);
    }
    // `--devices` 是「先看看有哪些机器」的那一步，用户这时候当然还不知道设备 ID。
    if a.devices {
        return Ok(a);
    }
    // `--last`：用本机记住的上一次连接补空缺。命令行给的一律不动（见 `recent::apply`）。
    if a.last {
        match uulan_client::recent::load_at(&recent_path()) {
            Some(r) => {
                let now = uulan_client::recent::now_unix();
                let took_device = a.device.trim().is_empty();
                let (server, device) =
                    uulan_client::recent::apply(&r, &a.server, a.server_explicit, &a.device);
                // 说的话必须和真正用的参数一致：命令行已经明确指了另一台时，
                // 绝不能印「按 --last 用上次连过的」—— 那会让人以为连的是记忆里那台。
                // 走 stderr：`--term` 模式下 stdout 就是远端 PTY 的字节流，一个字都不能掺。
                if took_device {
                    eprintln!(
                        "按 --last 用上次连过的设备：{}",
                        uulan_client::recent::describe(&r, now)
                    );
                } else {
                    eprintln!(
                        "上次连过的是 {}；本次按命令行给的 --device 连。",
                        uulan_client::recent::describe(&r, now)
                    );
                }
                a.server = server;
                a.device = device;
            }
            None => bail!(
                "还没有记住任何设备（{} 不存在或读不出来）\n\
                 先完整连一次成功（连上就会记住），或者用 --devices 看有哪些机器",
                recent_path().display()
            ),
        }
    }
    if a.device.is_empty() {
        bail!(
            "必须给出 --device（被控端界面上显示的 9 位设备 ID），或者用 --last 连上次那台"
        );
    }
    // 画面参数只对「看画面」的会话有意义：`--inject`、文件、终端、端口映射、`--status`
    // 都不开视频通道，给了也不会有任何效果。宁可当场报错，也别让人以为自己压了带宽。
    let has_video_flag =
        a.quality.is_some() || a.bitrate.is_some() || a.fps.is_some() || a.scale.is_some();
    let non_video_mode = a.inject.is_some()
        || a.fs_op.is_some()
        || a.term
        || !a.forward.is_empty()
        || a.status.is_some();
    if has_video_flag && non_video_mode {
        bail!(
            "--quality/--bitrate/--fps/--scale 只对看画面的会话有效：--inject、文件操作、--term、--forward、--status 都不开视频通道"
        );
    }
    // 现场注入必须**跟着一条有画面的会话**。它和 --inject 是两种东西：含糊地两个都给，
    // 用户以为在验的其实是另一条路（独立会话、按握手时的固定尺寸换算）。
    if a.inject_live.is_some() {
        if a.inject.is_some() {
            bail!("--inject 与 --inject-live 只能选一个：前者是独立会话、后者跟着画面会话跑");
        }
        if a.no_input {
            bail!("--inject-live 与 --no-input 矛盾：只读模式一条键鼠都不发，脚本一步也出不去");
        }
        if a.fs_op.is_some() || a.term || !a.forward.is_empty() || a.status.is_some() {
            bail!("--inject-live 只在有画面的会话里有效：文件操作、--term、--forward、--status 都不开视频通道，没有「当时的画幅」可换算");
        }
    }
    let env_pw = std::env::var("UULAN_PASSWORD").ok();
    a.password = resolve_password(&a.password, a.password_file.as_deref(), env_pw.as_deref())?;
    if a.password.is_empty() {
        bail!(
            "没有口令：用 --password、--password-file <文件>、或环境变量 UULAN_PASSWORD \
             （被控端的访问密码或一次性临时码）"
        );
    }
    Ok(a)
}

/// 口令的来路有三条，按优先级取：命令行 > 文件首行 > 环境变量。
///
/// 为什么非要后两条：`--password xxx` 在 Linux 上对同机任何用户都是 `ps` 里可读的，
/// 在 Windows 上会进 PowerShell 历史、还会被截图贴进工单。而一次性临时码恰恰是
/// 「发给人、让人赶紧连一次」的东西 —— 它最经不起这么抄。
///
/// 纯函数（三个入参都给全），所以能在本机用 `cargo test` 钉住优先级与边界情况。
fn resolve_password(
    from_cli: &str,
    file: Option<&std::path::Path>,
    from_env: Option<&str>,
) -> Result<String> {
    if !from_cli.is_empty() {
        return Ok(from_cli.to_string());
    }
    if let Some(p) = file {
        let raw = std::fs::read_to_string(p)
            .with_context(|| format!("读不了 --password-file：{}", p.display()))?;
        // 只取首行：口令文件往往顺手写成 `口令\n` 或多行备忘，多出来的内容不该混进口令。
        let line = raw.lines().next().unwrap_or("").trim().to_string();
        if line.is_empty() {
            bail!("--password-file 的第一行是空的：{}", p.display());
        }
        return Ok(line);
    }
    if let Some(v) = from_env {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return Ok(v);
        }
    }
    Ok(String::new())
}

/// 解析 `COLSxROWS`（也收大写 `X` 和 `*`，省得为了一个分隔符去查文档）。
/// 画质档位名 → 枚举。名字同时就是命令行与日志里用的词，别搞第二套说法。
fn parse_quality(v: &str) -> Result<Quality> {
    Ok(match v.trim().to_ascii_lowercase().as_str() {
        "auto" => Quality::Auto,
        "fast" => Quality::Fast,
        "high" => Quality::High,
        "hd" => Quality::Hd,
        "bluray" | "original" => Quality::Bluray,
        _ => bail!("不认识的画质档位 {v}（可用：auto|fast|high|hd|bluray）"),
    })
}

/// 把 `--quality/--bitrate/--fps/--scale` 折成一条 `VideoSet`。
///
/// 一个都没给 = `None`（被控端按自己的配置来，一个字节都不多发）。
/// 给了 `--bitrate` 就以码率为准（`Quality::Custom`）；没给画质档位时用 `Quality::Keep`
/// ——「这一项我不表态」，免得只想降帧率的用户把对面配好的高清档悄悄改成自动。
fn video_set(a: &Args) -> Option<VideoSet> {
    make_video_set(a.quality, a.bitrate, a.fps, a.scale)
}

/// 同上，但只接受四个纯值（便于单测：`Args` 有二十来个字段，构造它跟测语义无关）。
fn make_video_set(
    quality: Option<Quality>,
    bitrate: Option<u32>,
    fps: Option<u32>,
    scale: Option<f32>,
) -> Option<VideoSet> {
    if quality.is_none() && bitrate.is_none() && fps.is_none() && scale.is_none() {
        return None;
    }
    // 给了码率就以码率为准：`Custom` 的语义就是「上限由我这条消息里的数字决定」。
    let quality = if bitrate.is_some() {
        Quality::Custom
    } else {
        quality.unwrap_or(Quality::Keep)
    };
    Some(VideoSet {
        fps: fps.unwrap_or(0),
        max_bitrate_kbps: bitrate.unwrap_or(0),
        quality,
        scale: scale.unwrap_or(1.0),
        chroma: "420".into(),
        encoder: "auto".into(),
    })
}

fn parse_size(s: &str) -> Result<(u16, u16)> {
    let norm = s.replace(['X', '*', '×'], "x");
    let (c, r) = norm
        .split_once('x')
        .with_context(|| format!("--term-size 要 COLSxROWS，例：140x40，收到 {s:?}"))?;
    let cols: u16 = c.trim().parse().with_context(|| format!("列数不对：{c:?}"))?;
    let rows: u16 = r.trim().parse().with_context(|| format!("行数不对：{r:?}"))?;
    if cols == 0 || rows == 0 {
        bail!("--term-size 的列/行都必须大于 0");
    }
    Ok((cols, rows))
}

/// 连接阶段默认重试几次。
///
/// 现场最常见的「连不上」不是口令错，而是中继正在升级重启、被控端刚被拉起 ——
/// 那两种情况几秒内就恢复。以前控制端一撞上就直接 exit 1，用户只能手点第二次。
const DEFAULT_CONNECT_RETRY: u32 = 3;

/// 会话中途断线后默认最多连续重连几次。
///
/// 8 次按 1/2/4/8/15 秒退避算下来 ≈ 2 分钟：足够跨过「中继重启 + 被控端重新注册」
/// （真机实测 2–3 秒），又不至于让一台已经关机的机器把窗口挂一整天。
const DEFAULT_RECONNECT: u32 = 8;

/// 连接阶段的重试间隔（秒）：1、2、4、8，之后一直是 8。
///
/// 和会话中途的「自动接回」不是一回事：这里只是把「多试几次」的等待定死。
fn connect_retry_delay(attempt: u32) -> Duration {
    const STEPS: [u64; 4] = [1, 2, 4, 8];
    let i = (attempt.saturating_sub(1) as usize).min(STEPS.len() - 1);
    Duration::from_secs(STEPS[i])
}

/// 这个错误值不值得再试一次？
///
/// 只有「**没连上**」才值得：中继正在重启、被控端刚被拉起、网络还没通 —— 几秒后就好。
/// 其余一律不重试，理由分两类：
///
/// * **被拒**（口令错 / 被锁 / 被控端说忙）：重试等于自己往被控端的失败计数器里加数，
///   把「一个字打错」升级成「等一分钟」。
/// * **说得清的结果**（设备 ID 不在线 / ID 不合法 / 协议版本不匹配 / 帧解析错）：
///   再试一百次也是同一个答案，只会让人多等 7 秒才看到真话。
fn is_retryable(e: &ClientError) -> bool {
    match e {
        ClientError::Rv(RvError::Io(_)) | ClientError::Rv(RvError::Timeout(_)) => true,
        // 会话级握手超时：被控端可能正被别的会话占着 CPU，给一次机会。
        ClientError::Timeout => true,
        _ => false,
    }
}

/// 连被控端，连不上就按 [`connect_retry_delay`] 重试，最多 `--connect-retry` 次。
///
/// 什么算「连不上」见 [`is_retryable`]。
async fn connect_with_retry(
    args: &Args,
    caps: &[Cap],
) -> Result<connect::Connected, ClientError> {
    let mut attempt: u32 = 0;
    loop {
        match connect::connect(&args.server, &args.device, &args.password, &args.name, caps.to_vec())
            .await
        {
            Ok(c) => return Ok(c),
            Err(e) => {
                if !is_retryable(&e) || attempt >= args.connect_retry {
                    return Err(e);
                }
                attempt += 1;
                let wait = connect_retry_delay(attempt);
                // 走 stderr：`--term` 模式下 stdout 是远端字节流，一个字都不能掺。
                eprintln!(
                    "连接失败：{e}（{} 秒后重试，第 {attempt}/{} 次）",
                    wait.as_secs(),
                    args.connect_retry
                );
                tokio::time::sleep(wait).await;
            }
        }
    }
}

/// 把设备目录排成人看的样子。
///
/// 纯函数，所以「空目录怎么说话 / 离线条目怎么标」能在本机用 `cargo test` 钉住 ——
/// 这两句最容易在改动里悄悄变形（比如把离线写成「空闲 0s」，读起来像刚心跳过）。
///
/// `last` 是本机记住的上次连过的设备 ID：命中的那一行后面标「← 上次」。
/// 只标看得见的那一行，**不动 JSON** —— 脚本要的是中继原样给的目录，不是本机的私货。
fn format_device_list(server: &str, devices: &[DeviceEntry], last: Option<&str>) -> String {
    if devices.is_empty() {
        return format!("中继 {server}：设备目录是空的（还没有被控端注册过）\n");
    }
    let online = devices.iter().filter(|d| d.online).count();
    let mut out = format!("中继 {server}：{} 台设备（在线 {online}）\n", devices.len());
    // 按字符数补空格对齐：中文一个字占两格显示宽，但这里的目标是「能扫」不是排版，
    // 不值得为此引入 unicode-width。
    let id_w = devices.iter().map(|d| d.device_id.chars().count()).max().unwrap_or(0);
    let name_w = devices.iter().map(|d| d.name.chars().count()).max().unwrap_or(0);
    for d in devices {
        let state = match (d.online, d.idle_s) {
            (true, Some(s)) => format!("在线  空闲 {s}s"),
            (true, None) => "在线".to_string(),
            (false, _) => "离线".to_string(),
        };
        // 只标本机记住的那台。目录里没有它就什么也不标 —— 不猜、不编。
        let mark = if last == Some(d.device_id.as_str()) { "  ← 上次" } else { "" };
        out.push_str(&format!(
            "  {:id_w$}  {:name_w$}  {}{mark}\n",
            d.device_id,
            d.name,
            state,
            id_w = id_w,
            name_w = name_w,
        ));
    }
    out
}

/// 本机「上次连的是哪台」那个文件的位置（`$UULAN_CLIENT_CONFIG` 可覆盖）。
///
/// 单独包一层是为了让测试和文档只说一个名字：真机上就是 `~/.config/uulan/client.json`。
fn recent_path() -> PathBuf {
    uulan_client::recent::default_path()
}

fn default_client_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "uulan-client".to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        // 日志一律走 stderr：stdout 只放「数据」（终端字节、目录列表、统计 JSON）。
        // 否则 `RUST_LOG=info uulan-client --term > out.bin` 会把 INFO 行混进 PTY 字节流。
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let args = parse_args()?;

    // `--forget`：只删本机那个小文件。放在最前面 —— 它不该去连任何东西，
    // 也不该要求口令（「忘掉那台」正是发生在「口令已经不想留」的时候）。
    if args.forget {
        let p = recent_path();
        match uulan_client::recent::forget_at(&p) {
            Ok(true) => println!("已忘掉上次连接的设备（{}）", p.display()),
            Ok(false) => println!("本来就没记住任何设备（{}）", p.display()),
            Err(e) => {
                eprintln!("删不掉 {}：{e}", p.display());
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    // 设备目录：只问中继「有哪些机器」，不建立会话，也不需要口令。
    // 放在最前面 —— 它的用途正是「我还不知道设备 ID」的那个时候。
    if args.devices {
        let mut rv = match RvConn::connect(&args.server).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("连不上中继 {}：{e}", args.server);
                std::process::exit(1);
            }
        };
        let list = match rv.list_devices().await {
            Ok(l) => l,
            Err(RvError::Server { code: uulan_protocol::rv::ErrCode::Denied, message }) => {
                eprintln!("{message}");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("拉设备目录失败：{e}");
                std::process::exit(1);
            }
        };
        if args.json {
            println!("{}", serde_json::to_string_pretty(&list)?);
        } else {
            // 顺手标一下本机记住的那台：现场常见的下一句是「上次那台叫什么来着」。
            let last = uulan_client::recent::load_at(&recent_path()).map(|r| r.device);
            print!(
                "{}",
                format_device_list(&args.server, &list, last.as_deref())
            );
        }
        return Ok(());
    }

    // 能力列表必须如实：声明了没实现的能力，被控端会准备好对应通道，
    // 而控制端点了没反应 —— 那种 bug 最难查。剪贴板只在真有后端时才声明。
    //
    // 这里这个后端只用来**探一下能不能用**（比如没有 DISPLAY 的 X11 就不可用），
    // 探完就丢：真正的后端每条会话各建一个（见 `session::open`）。跨会话复用会把
    // 「上一次会话还是剪贴板所有者」的状态带进新会话。
    let clip_ok = if args.no_clipboard || args.no_input {
        if args.no_clipboard {
            println!("剪贴板同步已按 --no-clipboard 关闭");
        }
        false
    } else {
        // 声明能力前**真开一次剪贴板**：只问「平台支持吗」是不够的 ——
        // 无人值守/受限会话里整条会话都可能被拒（连 clip.exe 都 拒绝访问），
        // 那时旧写法照样声明 Cap::Clipboard，结果统计永远是「发送 0」而查不出原因。
        let mut cb = uulan_clip::default_clipboard(None);
        match cb.probe() {
            Ok(()) => true,
            Err(why) => {
                println!("剪贴板不可用：{why}");
                println!("  → 本次会话不同步剪贴板（其它功能照常）");
                false
            }
        }
    };
    let mut caps: Vec<Cap> = vec![];
    if clip_ok {
        caps.push(Cap::Clipboard);
    }
    if args.fs_op.is_some() {
        // 只有真的要传文件才声明这个能力：声明了不实现，被控端会准备好通道而控制端不用。
        caps.push(Cap::Files);
    }
    if !args.forward.is_empty() {
        // 端口映射同样只声明、不实现就别声明：这里是真要用。
        caps.push(Cap::PortMap);
    }
    if args.term {
        caps.push(Cap::Terminal);
    }
    // 系统指标：控制端总要能显示（窗口标题 / --status），所以总是声明。
    caps.push(Cap::Metrics);

    info!(server = %args.server, device = %args.device, "正在连接");
    let c = match connect_with_retry(&args, &caps).await {
        Ok(c) => c,
        Err(ClientError::Rejected(why)) => {
            eprintln!("被控端拒绝：{why}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("连接失败：{e}");
            std::process::exit(1);
        }
    };

    // `--term` 模式下 stdout 就是远端的 PTY 字节流，**不能**往里掺任何一行自己的话：
    // 掺进去就变成「shell 里凭空多出一行中文」，管道里重定向出来的内容也脏了。
    if !args.term {
        println!(
            "已连上「{}」（{} {}，屏幕 {}x{}，编码器 {}）",
            c.host.name,
            c.host.os,
            c.host.version,
            c.host.screen.w,
            c.host.screen.h,
            c.host.caps.len()
        );
    }
    info!(session = %uulan_protocol::redact::tag(&c.session), os = %c.host.os, "鉴权通过");

    // 记住「上次连的是哪台」：**只在鉴权成功之后写**。TCP 连上中继不算 ——
    // 设备 ID 打错、口令打错时也会连上中继，把那种参数记下来，下次 `--last` 就是
    // 记下一个错答案。写不进去只是「下次还得手抄 ID」，绝不能因此影响这条已建立的会话。
    let rec = recent_path();
    if let Err(e) = uulan_client::recent::remember_at(&rec, &args.server, &args.device, &c.host.name)
    {
        info!(path = %rec.display(), "记住上次连接失败：{e}");
    }

    // 文件操作：不开窗口、不收画面，干完就退出。视频通道连上就丢掉（被控端看到 EOF
    // 会自己停编码器），这样下载大文件时不会和视频抢带宽。
    if let Some(op) = args.fs_op.clone() {
        drop(c.video);
        uulan_client::fsx::run(c.control, c.bulk, op).await.context("文件操作失败")?;
        return Ok(());
    }

    // 终端：不开窗口、不收画面，把本机终端接到远端 PTY 上。
    // 退出码原样传出去 —— 脚本里 `uulan-client --term` 的退出码就是远端 shell 的退出码。
    if args.term {
        drop(c.video);
        if !c.host.caps.contains(&Cap::Terminal) {
            eprintln!("被控端不支持终端（对面版本太旧，或没实现这项能力）");
            std::process::exit(2);
        }
        let opts = uulan_client::termx::Options {
            shell: args.term_shell.clone(),
            cols: args.term_size.map(|s| s.0).unwrap_or(0),
            rows: args.term_size.map(|s| s.1).unwrap_or(0),
        };
        // 用户显式给了 --term-size 就钉死，不再跟随本机窗口（脚本化验证要的是确定的行列）。
        let follow = args.term_size.is_none();
        let code = uulan_client::termx::run(c.control, opts, follow).await?;
        std::process::exit(code);
    }

    // 被控端状态：只读，不注入、不收画面。跑 N 秒、每秒一行，收尾打汇总。
    //
    // 这是 `H2C::Metrics` / `H2C::SystemState` 唯一的机器可验路径：窗口标题里的
    // 「CPU 12% / 已锁屏」靠人眼看，而验收要的是能 grep 出来的数字。
    if let Some(secs) = args.status {
        drop(c.video);
        let link = uulan_client::control::ControlLink::start(c.control);
        let st = link.status();
        let deadline = Instant::now() + Duration::from_secs_f64(secs);
        println!("被控端状态：每秒一行（{secs} 秒）");
        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let s = st.snapshot();
            // `rtt=` 是被控端报的那个（恒 0，它在协议里如实填 0），`rtt_local=` 才是这条
            // 链路真的往返 —— 控制端自己发 Ping 量的。还没量到时打 `-`，不写假的 0.0。
            let rtt_local = if s.rtt_samples > 0 {
                format!("{:.1}ms", s.rtt_local_us as f64 / 1000.0)
            } else {
                "-".to_string()
            };
            println!(
                "locked={} has_state={} cpu={:.1}% mem={}/{}MB rtt={}ms rtt_local={} metrics={} changes={}",
                s.locked,
                s.known,
                s.cpu_pct_x100 as f64 / 100.0,
                s.mem_used_mb,
                s.mem_total_mb,
                s.rtt_ms,
                rtt_local,
                s.metrics_recv,
                s.state_changes
            );
        }
        let s = st.snapshot();
        println!(
            "状态汇总：指标 {} 条 / 状态变化 {} 次 / 锁屏丢弃键鼠 {} 条",
            s.metrics_recv, s.state_changes, s.dropped_locked
        );
        link.close("状态查看结束").await;
        return Ok(());
    }

    // 键鼠注入：只发输入，跑完就退。这是「XTest 真的把键鼠打进去了」的唯一机器可验路径 ——
    // 窗口模式要人手点，验不出东西。见 crates/uulan-client/src/inject.rs。
    if let Some(script) = args.inject.clone() {
        drop(c.video);
        if !c.host.has_desktop {
            eprintln!("被控端没有图形会话，没有可注入的桌面（采集源是合成画面？）");
            std::process::exit(2);
        }
        let steps = uulan_client::inject::parse(&script).context("--inject 脚本解析失败")?;
        let errors =
            uulan_client::inject::run(c.control, &steps, c.host.screen.w, c.host.screen.h)
                .await
                .context("键鼠注入失败")?;
        // 退出码是给脚本看的：被控端报错（XTest 被拦、VK 没映射）不能伪装成成功。
        std::process::exit(if errors == 0 { 0 } else { 3 });
    }

    // 端口映射：同样不开窗口、不收画面 —— 只在本机起监听，把字节经被控端转出去。
    if !args.forward.is_empty() {
        drop(c.video);
        if !c.host.caps.contains(&Cap::PortMap) {
            eprintln!("被控端不支持端口映射（对面版本太旧，或没实现这个能力）");
            std::process::exit(2);
        }
        let fwd = uulan_client::Forwarder::start(c.control, c.bulk);
        for spec in &args.forward {
            let addr = fwd.listen(spec.clone()).await.context("本机监听失败")?;
            println!(
                "端口映射：本机 {addr} → 被控端 {}:{}",
                spec.target_host, spec.target_port
            );
        }
        // 这一行是给脚本看的「就绪信号」：后面才可能出现真实转发。
        println!("转发已就绪（{} 条）", args.forward.len());
        match args.seconds {
            Some(s) => tokio::time::sleep(Duration::from_secs_f64(s)).await,
            None => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
        let (conns, up, down) = fwd.stats();
        println!("端口映射结束：连接 {conns} / 上行 {up} 字节 / 下行 {down} 字节");
        fwd.shutdown();
        return Ok(());
    }

    // 现场注入：脚本现在就解析好（写错要当场退出，而不是画面跑完才发现白等一场），
    // 并先确认对面真有个可注入的桌面 —— 合成采集源没有桌面，打进去也没人看见。
    let live_steps: Option<Vec<uulan_client::inject::Step>> = match &args.inject_live {
        Some(script) => {
            if !c.host.has_desktop {
                eprintln!("被控端没有图形会话，没有可注入的桌面（采集源是合成画面？）");
                std::process::exit(2);
            }
            Some(uulan_client::inject::parse(script).context("--inject-live 脚本解析失败")?)
        }
        None => None,
    };
    let started = Instant::now();

    // 会话配置：窗口模式与 headless 共用同一份 —— 重连就是「拿同一份参数再握一次手」。
    let plan = session::Plan {
        server: args.server.clone(),
        device: args.device.clone(),
        password: args.password.clone(),
        client_name: args.name.clone(),
        caps: caps.clone(),
        ffmpeg: args.ffmpeg.clone(),
        clipboard: clip_ok,
        no_input: args.no_input,
        max_reconnects: args.reconnect,
        video: video_set(&args),
        audio: if args.audio {
            Some(match &args.audio_save {
                Some(p) => uulan_client::audioin::Sink::Wav(p.clone()),
                None => uulan_client::audioin::Sink::Speaker,
            })
        } else {
            None
        },
        audio_stats: std::sync::Arc::new(uulan_client::audioin::AudioStats::default()),
    };

    // ── 窗口模式 ──────────────────────────────────────────────────────────
    // winit 的事件循环**必须占住主线程**，所以画面接收/解码/重连全扔到 tokio 的 worker 上。
    // 窗口与那条会话之间只隔着 `session::Live` 这一个槽：断线重连时窗口不重建、不闪，
    // 也不需要知道握手又走了一遍。
    //
    // `#[tokio::main]` 的 async 主体本来就跑在主线程，所以这里**直接同步调用**事件循环，
    // 不要再 spawn 线程（winit 0.30 会硬性拒绝在其他线程创建事件循环）。
    if !args.headless {
        let live = Arc::new(session::Live::new());
        let overlay = uulan_client::window::Overlay {
            host_name: format!("{}({})", c.host.name, c.host.os),
            screen: (c.host.screen.w, c.host.screen.h),
            ..Default::default()
        };
        println!(
            "已连上「{}」（{} {}，屏幕 {}x{}）—— 正在打开窗口…",
            c.host.name, c.host.os, c.host.version, c.host.screen.w, c.host.screen.h
        );

        // 现场注入挂在**会话槽**上：每一步现取 `live.input()` 与 `live.stats().size()`，
        // 所以换代之后按新尺寸换算；重连期间取不到通道就等（那是常态，不算失败）。
        let inj_handle = match &live_steps {
            Some(steps) => Some(tokio::spawn(inject_via_live(
                live.clone(),
                steps.clone(),
                (c.host.screen.w, c.host.screen.h),
            ))),
            None => None,
        };
        let sup = tokio::spawn(session::drive(live.clone(), plan.clone(), c));
        let r = uulan_client::window::run(live.clone(), overlay, args.seconds);
        // 窗口关了（用户点的 / 到了 --seconds / 不再重连）：会话循环连同解码器一起收掉。
        sup.abort();
        // 现场注入的收尾必须在这：它和下面的「输入上报」共用同一份 InputStats，
        // 先等它发完，那行统计才包含脚本发出去的条数。
        if let Some(h) = inj_handle {
            match await_injection(h).await {
                Some(out) => println!(
                    "现场注入：发送 {} 条 / 用到过的画幅 {}（逐步换算见「注入换算」行）",
                    out.sent,
                    out.sizes
                        .iter()
                        .map(|(w, h)| format!("{}x{}", w, h))
                        .collect::<Vec<_>>()
                        .join(" → ")
                ),
                None => eprintln!("现场注入没跑完（见上面的原因）"),
            }
        }

        // 收工统计在 abort 之后读：abort 只杀任务，原子量里的计数还在。
        let si = live.input_stats().snapshot();
        println!(
            "输入上报：移动 {} / 按键 {} / 文本 {} / 点击 {} / 滚轮 {}（未送出 {}；被控端拒绝 {}）",
            si.moves, si.keys, si.chars, si.buttons, si.wheels, si.dropped, si.host_errors
        );
        println!(
            "剪贴板同步：发送 {} / 接收 {}（本机写入失败 {} / 本机读取失败 {}）",
            si.clips_sent, si.clips_recv, si.clip_errors, si.clip_read_errors
        );
        let hs = live.status().snapshot();
        println!(
            "被控端状态：指标 {} 条 / 状态变化 {} 次 / 锁屏时丢弃键鼠 {} 条{}",
            hs.metrics_recv,
            hs.state_changes,
            hs.dropped_locked,
            if hs.known && hs.locked { "（当前：已锁屏）" } else { "" }
        );
        println!(
            "会话：建立 {} 条 / 断线接回 {} 次{}",
            live.sessions(),
            live.reconnects(),
            if live.phase() == session::Phase::Finished {
                format!("（已停止重连：{}）", live.note())
            } else {
                String::new()
            }
        );
        let t = live.totals_now();
        let last = live.stats().snapshot();
        println!(
            "窗口已关闭。收到 AU {} / 关键帧 {} / 解出帧 {} / 丢帧 {} / 首帧 {}ms / 被控端实测 {:.0} fps / 重建解码器 {} 次",
            t.aus,
            t.keyframes,
            t.decoded,
            t.dropped,
            last.first_frame_ms,
            last.host_fps_x10 as f32 / 10.0,
            last.resizes
        );
        r.context("窗口退出异常")?;
        if live.phase() == session::Phase::Finished {
            // 连不回来了：非 0 退出，脚本/自动化才不会把「画面冻着」当成成功。
            eprintln!("会话没能接回：{}", live.note());
            std::process::exit(4);
        }
        return Ok(());
    }

    // ── headless ──────────────────────────────────────────────────────────
    run_headless(&args, &plan, c, started, live_steps).await
}

/// 一个会话的统计（收工报告用）。
struct Report {
    snap: WatchStatsSnapshot,
    elapsed: Duration,
    kbps: f64,
    video: uulan_protocol::msg::HostVideoCfg,
}

/// 收一个会话的帧，直到画面断开或到时限。
///
/// 返回 `(统计, 是否该收工)`：`true` 表示「到了 `--frames` / `--seconds`」——
/// 那是用户要求的结束，不该再去重连。
async fn watch_session(
    args: &Args,
    deadline: Option<Instant>,
    o: &mut session::Open,
    count: &mut u64,
    last_frame: &mut Option<uulan_client::watch::Frame>,
    show_cfg: bool,
) -> (Report, bool) {
    if show_cfg && !args.json {
        let info = o.watcher.info();
        println!(
            "画面：{}x{} {} {} fps（编码器 {}）",
            info.w, info.h, info.codec, info.fps, info.encoder
        );
    }
    let mut done = false;
    // 只留最后一帧在内存里，退出时落盘 —— 每帧都写盘的话 1080p 一帧 6 MB，
    // 30fps 下就是 180 MB/s 的无用 IO。
    loop {
        if let Some(n) = args.frames {
            if *count >= n {
                done = true;
                break;
            }
        }
        if let Some(d) = deadline {
            if Instant::now() >= d {
                done = true;
                break;
            }
        }
        let sleep = match deadline {
            Some(d) => {
                let left = d.saturating_duration_since(Instant::now());
                tokio::time::sleep(left.min(Duration::from_secs(5)))
            }
            None => tokio::time::sleep(Duration::from_secs(3600)),
        };
        tokio::select! {
            f = o.watcher.next_frame() => match f {
                Some(frame) => {
                    *count += 1;
                    if args.save_frame.is_some() {
                        *last_frame = Some(frame.clone());
                    }
                    if !args.json && (*count == 1 || *count % 30 == 0) {
                        let s = o.watcher.snapshot();
                        println!(
                            "第 {} 帧：{}x{} 首帧 {}ms 丢包 {} 已解 {} 码率 {:.0} kbps",
                            *count,
                            frame.width,
                            frame.height,
                            s.first_frame_ms,
                            s.dropped,
                            s.decoded,
                            o.watcher.stats().bitrate_kbps(o.watcher.elapsed()),
                        );
                    }
                }
                None => {
                    if !args.json {
                        println!("画面流已结束");
                    }
                    break;
                }
            },
            _ = sleep => {
                // 这里只是「醒来再核一次 deadline」，**不能**因为醒了就当成跑完了：
                // 原来写成 `if deadline.is_some() { done = true }`，于是任何 5 秒没有新帧
                // （半开连接、被控端冻住）都会被当成「到点了、正常收工」—— 既不重连、
                // 也不说原因，`--seconds` 一给就静默截断。
                if let Some(d) = deadline {
                    if Instant::now() >= d {
                        done = true;
                        break;
                    }
                }
            }
        }
    }
    let snap = o.watcher.snapshot();
    let elapsed = o.watcher.elapsed();
    let kbps = o.watcher.stats().bitrate_kbps(elapsed);
    let video = o.watcher.info().clone();
    (Report { snap, elapsed, kbps, video }, done)
}

/// 把「现场注入」挂到会话槽上：输入通道与画幅都从 [`session::Live`] **现取**。
///
/// 为什么窗口模式走 `Live` 而不是直接攥着 `ControlLink`：会话槽跨重连，脚本发的每一步
/// 都要落在「当下这条会话」上 —— 断线期间 `live.input()` 是 `None`，那是等待而不是失败。
async fn inject_via_live(
    live: Arc<session::Live>,
    steps: Vec<uulan_client::inject::Step>,
    fallback: (u32, u32),
) -> Result<uulan_client::inject::Outcome> {
    let status = live.status();
    let stats = live.input_stats();
    let wstats = live.stats();
    uulan_client::inject::run_with(
        uulan_client::inject::Hooks {
            port: Box::new(move || live.input()),
            size: Box::new(move || {
                let (w, h) = wstats.size();
                if w == 0 || h == 0 {
                    fallback
                } else {
                    (w, h)
                }
            }),
            status,
            stats,
            port_wait: uulan_client::inject::PORT_WAIT,
        },
        &steps,
        true,
    )
    .await
}

/// 等现场注入收尾（上限 20 秒）。
///
/// 脚本写得比 `--seconds` 长、或会话早就断了，都不能把整个进程挂住：如实打印原因，
/// 别让它伪装成「注入成功」。取不到 `Outcome` 就返回 `None`，收工统计里不再有那一行。
async fn await_injection(
    h: tokio::task::JoinHandle<Result<uulan_client::inject::Outcome>>,
) -> Option<uulan_client::inject::Outcome> {
    match tokio::time::timeout(Duration::from_secs(20), h).await {
        Ok(Ok(Ok(out))) => Some(out),
        Ok(Ok(Err(e))) => {
            eprintln!("现场注入没跑完：{e}");
            None
        }
        Ok(Err(e)) => {
            eprintln!("现场注入任务崩了：{e}");
            None
        }
        Err(_) => {
            eprintln!("现场注入超过 20 秒还没收尾（脚本比 --seconds 长？），已放弃等待");
            None
        }
    }
}
/// headless 会话循环：和窗口模式同一套「断了就退避重连」，只是没有窗口可画。
async fn run_headless(
    args: &Args,
    plan: &session::Plan,
    first: connect::Connected,
    started: Instant,
    live_steps: Option<Vec<uulan_client::inject::Step>>,
) -> Result<()> {
    let deadline = args.seconds.map(|s| started + Duration::from_secs_f64(s));
    let host = first.host.clone();
    let mut count = 0u64;
    let mut last_frame: Option<uulan_client::watch::Frame> = None;
    let mut rc = session::Reconnect::new(plan.max_reconnects);
    let mut next = Some(first);
    let mut sessions = 0u32;
    let mut reconnects = 0u32;
    let mut totals = session::Totals::default();
    let mut report: Option<Report> = None;
    // 现场注入只在**第一条**会话上跑一遍脚本：重连之后不重放，否则验收脚本的计数会翻倍，
    // 而「同一个动作打进两次」在真机上根本分不清是哪一次落的点。
    let mut inj_handle: Option<tokio::task::JoinHandle<Result<uulan_client::inject::Outcome>>> =
        None;
    let mut live_outcome: Option<uulan_client::inject::Outcome> = None;
    // 输入/剪贴板计数跨会话累加：headless 的收工统计也要报这两行 ——
    // `scripts/verify-clipboard-cross.sh` 与夜批判「剪贴板到底同没同步」看的就是它们，
    // 而以前只有窗口模式会打（脚本在 headless 下永远抓不到，等于断言写死成失败）。
    let mut ins = uulan_client::control::InputStatsSnapshot::default();
    // 重连次数用尽 = 这条会话再也不会回来。窗口模式**已经在**这条路径上 `exit(4)`；
    // headless 一直到 2026-10-30 都还是「打一句不再重连、照常出统计、退出码 0」——
    // 脚本/夜批分不清「跑完了」和「画面早冻住了」，所以这里也要按失败收尾。
    let mut gave_up: Option<String> = None;

    loop {
        let mut finished = false;
        let mut opened = false;
        let mut err: Option<ClientError> = None;
        let mut fresh_frames = false;

        // 1) 拿一条连接。第一条是 main 已经握好的那只手。
        let conn = match next.take() {
            Some(c) => Some(c),
            None => match connect::connect(
                &plan.server,
                &plan.device,
                &plan.password,
                &plan.client_name,
                plan.caps.clone(),
            )
            .await
            {
                Ok(c) => {
                    reconnects += 1;
                    if !args.json {
                        println!("已接回被控端（第 {reconnects} 次重连）");
                    }
                    Some(c)
                }
                Err(e) => {
                    eprintln!("接回失败：{e}");
                    err = Some(e);
                    None
                }
            },
        };

        // 2) 跑这一条会话
        if let Some(c) = conn {
            match session::open(None, plan, c).await {
                Ok(mut o) => {
                    sessions += 1;
                    opened = true;
                    if inj_handle.is_none() && live_steps.is_some() {
                        let sender = o.link.input();
                        let status = o.link.status();
                        let istats = o.link.stats();
                        let wstats = o.watcher.stats();
                        let fallback = (host.screen.w, host.screen.h);
                        let steps = live_steps.clone().unwrap_or_default();
                        inj_handle = Some(tokio::spawn(async move {
                            uulan_client::inject::run_with(
                                uulan_client::inject::Hooks {
                                    port: Box::new(move || Some(sender.clone())),
                                    size: Box::new(move || {
                                        let (w, h) = wstats.size();
                                        // 还没有任何 VideoCfg 就用握手时报的尺寸兜一下 ——
                                        // 真到了 0x0，run_with 里还有一层「沿用上一次换算」的保护。
                                        if w == 0 || h == 0 {
                                            fallback
                                        } else {
                                            (w, h)
                                        }
                                    }),
                                    status,
                                    stats: istats,
                                    port_wait: uulan_client::inject::PORT_WAIT,
                                },
                                &steps,
                                true,
                            )
                            .await
                        }));
                    }
                    let (rep, done) =
                        watch_session(args, deadline, &mut o, &mut count, &mut last_frame, sessions == 1)
                            .await;
                    // 现场注入同样要在 close 之前收尾：close 会吃掉控制通道。
                    if let Some(h) = inj_handle.take() {
                        live_outcome = await_injection(h).await;
                    }
                    // 必须在 close 之前取：close 会把 link 吃掉，之后的计数就没了。
                    ins.accumulate(&o.link.stats().snapshot());
                    o.link.close("控制端准备重连").await;
                    totals.add(&rep.snap);
                    fresh_frames = rep.snap.decoded > 0;
                    if !args.json {
                        println!(
                            "本次会话：解出 {} 帧 / 丢帧 {}{} / 首帧 {}ms",
                            rep.snap.decoded,
                            rep.snap.dropped,
                            if rep.snap.silent_s > 0 {
                                format!(" / 静默 {}s（半开？）", rep.snap.silent_s)
                            } else {
                                String::new()
                            },
                            rep.snap.first_frame_ms
                        );
                    }
                    report = Some(rep);
                    finished = done;
                }
                Err(e) if !e.retryable() => {
                    // 对面明确拒绝：重连没用，直接把话原样交给用户。
                    bail!("{}", e.why());
                }
                Err(e) => eprintln!("{}", e.why()),
            }
        }

        if finished {
            break;
        }
        // 3) 接得回去吗
        if fresh_frames {
            rc.recovered();
        }
        match rc.retry(err.as_ref()) {
            Some(d) => {
                eprintln!(
                    "{} 秒后尝试接回（第 {}/{} 次）",
                    d.as_secs(),
                    rc.used(),
                    plan.max_reconnects
                );
                tokio::time::sleep(d).await;
            }
            None => {
                if !opened && report.is_none() {
                    bail!("没能建立起会话（见上面的错误）");
                }
                eprintln!("不再重连");
                gave_up = Some(match &err {
                    Some(e) => format!("连续重连 {} 次都没接回（最后一次：{e}）", plan.max_reconnects),
                    None => format!(
                        "会话结束后连续重连 {} 次都没接回（被控端一直没上线或中继不可达）",
                        plan.max_reconnects
                    ),
                });
                break;
            }
        }
    }

    if let Some(path) = &args.save_frame {
        match &last_frame {
            Some(f) => {
                write_ppm(path, f).with_context(|| format!("写出最后一帧失败 {}", path.display()))?;
                if !args.json {
                    println!("最后一帧已存到 {}（{}x{}）", path.display(), f.width, f.height);
                }
            }
            None => eprintln!("一帧都没收到，没能存图"),
        }
    }

    let Some(rep) = report else {
        bail!("没能建立起会话（见上面的错误）");
    };
    let s = rep.snap;
    let elapsed = rep.elapsed;
    // 总时长按**整次运行**算（`started` 是进程进会话前那一刻），不是最后一条会话的时长：
    // 接回之后就变成「总量 / 最后一段」，实测帧率会凭空翻倍（真机上真出现过 47.0 fps，
    // 而被控端自报 29.3、实际跑了 900 秒、26853 帧）。码率同理 —— 分子是全过程的字节。
    let wall = started.elapsed();
    let wall_s = wall.as_secs_f64();
    let kbps = if wall_s > 0.0 {
        (totals.bytes as f64 * 8.0) / wall_s / 1000.0
    } else {
        0.0
    };

    if args.json {
        println!(
            "{}",
            serde_json::json!({
                "host": { "name": host.name, "os": host.os, "version": host.version,
                          "screen": { "w": rep.video.w, "h": rep.video.h } },
                "video": { "w": rep.video.w, "h": rep.video.h, "codec": rep.video.codec,
                           "fps": rep.video.fps, "encoder": rep.video.encoder },
                "stats": { "aus": totals.aus, "keyframes": totals.keyframes, "bytes": totals.bytes,
                           "dropped": totals.dropped, "decoded": totals.decoded,
                           "first_frame_ms": s.first_frame_ms, "ended": s.ended,
                           "resizes": s.resizes,
                           "silent_s": s.silent_s,
                           "host_fps": s.host_fps_x10 as f32 / 10.0 },
                "sessions": sessions,
                "reconnects": reconnects,
                "rendered": count,
                "elapsed_s": wall_s,
                "last_session_s": elapsed.as_secs_f64(),
                "bitrate_kbps": kbps,
            })
        );
    } else {
        println!("────────────── 统计 ──────────────");
        println!("  收到 AU    : {}", totals.aus);
        println!("  关键帧     : {}", totals.keyframes);
        println!("  解出帧     : {}（本进程渲染 {}）", totals.decoded, count);
        println!("  解码丢帧   : {}", totals.dropped);
        println!("  首帧延迟   : {} ms", s.first_frame_ms);
        println!("  压缩字节   : {}", totals.bytes);
        println!("  平均码率   : {:.0} kbps", kbps);
        println!("  持续时长   : {:.1} s（整次运行，含连接与重连间隔）", wall_s);
        if wall_s > 0.0 {
            println!(
                "  实测帧率   : {:.1} fps（控制端解出，按整次运行算）",
                totals.decoded as f64 / wall_s
            );
        }
        // 多条会话时补一行「最后一段」：调退化时看的是这一段，别让它被总平均埋掉。
        if sessions > 1 {
            let last_s = elapsed.as_secs_f64();
            println!(
                "  最后会话   : {:.1} s / {} 帧{}",
                last_s,
                s.decoded,
                if last_s > 0.0 {
                    format!(" / {:.1} fps / {:.0} kbps", s.decoded as f64 / last_s, rep.kbps)
                } else {
                    String::new()
                }
            );
        }
        println!("  被控端帧率 : {:.2} fps（对面自己数的编码帧率）", s.host_fps_x10 as f32 / 10.0);
        // 缩放编码时两行都要打：屏幕尺寸决定点击落点，编码画幅决定解码字节数。
        // 只打一个数的话，现场看到「画面是 960x540」而统计写 1920x1080 会以为是 bug。
        if (s.enc_w, s.enc_h) != (s.cfg_w, s.cfg_h) {
            println!(
                "  当前画幅   : {}x{}（屏幕）/ {}x{}（编码，已缩放）",
                s.cfg_w, s.cfg_h, s.enc_w, s.enc_h
            );
        } else {
            println!("  当前画幅   : {}x{}（最新一条画面参数）", s.cfg_w, s.cfg_h);
        }
        if s.cfg_fps > 0 {
            println!("  请求帧率   : {} fps（被控端当前编码帧率）", s.cfg_fps);
        }
        println!("  解码器重建 : {} 次（被控端换过分辨率）", s.resizes);
        if let Some(out) = &live_outcome {
            println!(
                "  现场注入   : 发送 {} 条 / 用到过的画幅 {}（逐步换算见「注入换算」行）",
                out.sent,
                out.sizes
                    .iter()
                    .map(|(w, h)| format!("{}x{}", w, h))
                    .collect::<Vec<_>>()
                    .join(" → ")
            );
        }
        // 这两行以前只有窗口模式才有：headless 是脚本/夜批唯一的观测面，
        // 少了它们「剪贴板发送 0」在日志里就无迹可寻。
        println!(
            "  输入上报   : 移动 {} / 按键 {} / 文本 {} / 点击 {} / 滚轮 {}（未送出 {}；被控端拒绝 {}）",
            ins.moves, ins.keys, ins.chars, ins.buttons, ins.wheels, ins.dropped, ins.host_errors
        );
        println!(
            "  剪贴板同步 : 发送 {} / 接收 {}（本机写入失败 {} / 本机读取失败 {}）",
            ins.clips_sent, ins.clips_recv, ins.clip_errors, ins.clip_read_errors
        );
        // 音频：只在要过声音时打。「有流但峰值 0」= 对面在放静音（不是断了），
        // 这是现场区分「没声音」与「没连上」的唯一判据。
        if plan.audio.is_some() {
            println!("  {}", plan.audio_stats.summary());
        }
        println!("  会话       : 建立 {} 条 / 断线接回 {} 次", sessions, reconnects);
    }

    if let Some(why) = &gave_up {
        // 非 0 退出：脚本/夜批才不会把「画面早就冻住了」当成跑完（与窗口模式同一个退出码）。
        eprintln!("会话没能接回：{why}");
        std::process::exit(4);
    }
    Ok(())
}

/// 把一帧 BGRA 写成 PPM（P6）。
///
/// 选 PPM 是因为它**零依赖**（不引任何图像库就能写）、而且 ffmpeg / ImageMagick /
/// 大部分看图工具都能直接读，转换一下就是 PNG。
/// 它的用途只有一个：把「对面现在到底画的是什么」变成一张能看的图，
/// 而不是靠猜统计数字。
fn write_ppm(path: &std::path::Path, f: &uulan_client::watch::Frame) -> Result<()> {
    use std::io::Write;

    let expect = (f.width as usize) * (f.height as usize) * 4;
    if f.pixels.len() != expect {
        bail!(
            "帧数据长度不对：{} 字节，{}x{} BGRA 应为 {} 字节（解码器输出可能不是 BGRA）",
            f.pixels.len(),
            f.width,
            f.height,
            expect
        );
    }

    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    write!(out, "P6\n{} {}\n255\n", f.width, f.height)?;
    let mut rgb = Vec::with_capacity((f.width as usize) * (f.height as usize) * 3);
    for px in f.pixels.chunks_exact(4) {
        rgb.push(px[2]); // R
        rgb.push(px[1]); // G
        rgb.push(px[0]); // B
    }
    out.write_all(&rgb)?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpfile(name: &str, content: &str) -> PathBuf {
        // 放在 exe 旁边（target/debug/deps）而不是系统临时目录：
        // 这台开发机对某些目录有写盘限制，见 skill 里的「坑 2」。
        let dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));
        let p = dir.join(format!("pwtest-{}-{}", std::process::id(), name));
        std::fs::write(&p, content).expect("写临时口令文件");
        p
    }

    /// 连接阶段的重试间隔：先小步、再封顶，且极值不溢出。
    /// 没给任何画面参数 = 一个字节都不发（不能悄悄把对面的配置改掉）。
    #[test]
    fn no_video_flags_means_no_video_set_message() {
        assert!(make_video_set(None, None, None, None).is_none());
    }

    /// 只调帧率/缩放时，画质必须是 `Keep`（「这一项我不表态」）而不是 `Auto`。
    /// 写成 `Auto` 的话，用户只想把帧率降到 15，结果把被控端配好的高清档悄悄改成自动。
    #[test]
    fn fps_or_scale_alone_keeps_the_hosts_quality() {
        let v = make_video_set(None, None, Some(15), Some(0.5)).expect("应当有消息");
        assert_eq!(v.fps, 15);
        assert!((v.scale - 0.5).abs() < f32::EPSILON);
        assert_eq!(v.quality, Quality::Keep, "没表态的画质不能变成 Auto");
        assert_eq!(v.max_bitrate_kbps, 0);
    }

    /// 给了码率 → `Custom` + 那个数字；同时给的档位让位（码率是更具体的表态）。
    #[test]
    fn explicit_bitrate_wins_over_named_quality() {
        let v = make_video_set(Some(Quality::Hd), Some(1500), None, None).expect("应当有消息");
        assert_eq!(v.quality, Quality::Custom);
        assert_eq!(v.max_bitrate_kbps, 1500);
        assert_eq!(v.fps, 0, "0 = 不动被控端的帧率");
        assert!((v.scale - 1.0).abs() < f32::EPSILON, "1.0 = 不缩放");
    }

    #[test]
    fn quality_names_are_the_ones_in_help() {
        for (name, want) in [
            ("auto", Quality::Auto),
            ("FAST", Quality::Fast),
            ("high", Quality::High),
            ("hd", Quality::Hd),
            ("bluray", Quality::Bluray),
        ] {
            assert_eq!(parse_quality(name).unwrap(), want, "{name}");
        }
        assert!(parse_quality("ultra").is_err(), "不认识的档位要报错，不能静默当 auto");
    }

    #[test]
    fn connect_retry_backoff_starts_small_and_caps() {
        let seq: Vec<u64> = (1..=6).map(|i| connect_retry_delay(i).as_secs()).collect();
        assert_eq!(seq, vec![1, 2, 4, 8, 8, 8]);
        assert!(seq.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(connect_retry_delay(u32::MAX).as_secs(), 8);
        // 默认值必须是「会重试」而不是 0 —— --connect-retry 默认不重试的话，
        // 这个改动在现场等于没做（中继重启那几秒还是会报错退出）。
        assert!(DEFAULT_CONNECT_RETRY >= 1);
    }

    /// 重试分类：只有「连不上」才重试。这条钉住的是**用户等待时间**：
    /// 设备 ID 打错、口令打错，都不该让人多等 7 秒才看到真话。
    #[test]
    fn only_transport_failures_are_retryable() {
        use uulan_protocol::rv::ErrCode;
        assert!(is_retryable(&ClientError::Rv(RvError::Io(std::io::Error::other("refused")))));
        assert!(is_retryable(&ClientError::Timeout));
        // 能说清原因的，一律不重试
        assert!(!is_retryable(&ClientError::Rejected("密码不正确".into())));
        assert!(!is_retryable(&ClientError::Rv(RvError::Server {
            code: ErrCode::Offline,
            message: "设备不在线".into()
        })));
        assert!(!is_retryable(&ClientError::Unexpected("握手期间被控端断开".into())));
    }

    fn entry(id: &str, name: &str, online: bool, idle_s: Option<u64>) -> DeviceEntry {
        DeviceEntry { device_id: id.into(), name: name.into(), online, idle_s }
    }

    #[test]
    fn device_list_says_how_many_are_online() {
        let s = format_device_list(
            "10.0.0.1:21116",
            &[
                entry("805747189", "麒麟-mis-pc", true, Some(3)),
                entry("805747000", "旧机器", false, None),
            ],
            None,
        );
        assert!(s.starts_with("中继 10.0.0.1:21116：2 台设备（在线 1）\n"), "{s}");
        assert!(s.contains("805747189") && s.contains("麒麟-mis-pc"));
        assert!(s.contains("在线  空闲 3s"), "{s}");
        // 离线就是离线：绝不能出现「空闲 0s」那种看着像刚心跳过的字样。
        let off_line = s.lines().find(|l| l.contains("805747000")).expect("离线条目不见了");
        assert!(off_line.trim_end().ends_with("离线"), "{off_line}");
        assert!(!off_line.contains("空闲"), "{off_line}");
    }

    #[test]
    fn empty_device_list_reads_like_a_sentence_not_a_table_header() {
        let s = format_device_list("10.0.0.1:21116", &[], None);
        assert!(s.contains("设备目录是空的"), "{s}");
        assert!(!s.contains("在线 0"), "空目录不该印一个表头出来：{s}");
    }

    /// 「上次连的那台」只标在那一条上，且目录里没有它时**不编**一个出来。
    #[test]
    fn the_remembered_device_is_marked_on_its_own_line_only() {
        let devs = [
            entry("805747189", "麒麟-mis-pc", true, Some(1)),
            entry("805747000", "旧机器", false, None),
        ];
        let s = format_device_list("10.0.0.1:21116", &devs, Some("805747189"));
        let marked: Vec<&str> = s.lines().filter(|l| l.contains("← 上次")).collect();
        assert_eq!(marked.len(), 1, "只能标一行：{s}");
        assert!(marked[0].contains("805747189"), "{marked:?}");
        // 别的行一个标记都不能有
        let other = s.lines().find(|l| l.contains("805747000")).unwrap();
        assert!(!other.contains("上次"), "{other}");

        // 记住的那台已经不在目录里（换机器/台账清了）：不标、也不额外编一行
        let s = format_device_list("10.0.0.1:21116", &devs, Some("999999999"));
        assert!(!s.contains("上次"), "{s}");
        assert_eq!(s.lines().count(), 3, "{s}");
    }

    #[test]
    fn cli_password_wins_over_file_and_env() {
        let f = tmpfile("a", "from-file\n");
        let got = resolve_password("from-cli", Some(&f), Some("from-env")).unwrap();
        assert_eq!(got, "from-cli");
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn file_wins_over_env_and_is_trimmed() {
        // 口令文件常带尾随空白/CRLF（有人从 Windows 记事本存下来）
        let f = tmpfile("b", "  s3cret-PW  \r\n第二行不该被读到\r\n");
        let got = resolve_password("", Some(&f), Some("from-env")).unwrap();
        assert_eq!(got, "s3cret-PW");
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn env_is_the_last_resort() {
        assert_eq!(resolve_password("", None, Some(" from-env ")).unwrap(), "from-env");
        assert_eq!(resolve_password("", None, Some("   ")).unwrap(), "");
        assert_eq!(resolve_password("", None, None).unwrap(), "");
    }

    #[test]
    fn empty_or_missing_password_file_is_an_error_not_silence() {
        let empty = tmpfile("c", "\ndeadbeef\n"); // 首行是空的就必须报错，不能拿第二行顶上
        assert!(resolve_password("", Some(&empty), Some("from-env")).is_err());
        let _ = std::fs::remove_file(&empty);

        let missing = std::path::Path::new("D:/definitely-not-here/uulan-pw.txt");
        assert!(resolve_password("", Some(missing), Some("from-env")).is_err());
    }
}
