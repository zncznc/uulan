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

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tracing::info;

use uulan_client::connect::{self, ClientError};
use uulan_client::watch::VideoWatcher;
use uulan_protocol::msg::Cap;

struct Args {
    server: String,
    device: String,
    password: String,
    name: String,
    frames: Option<u64>,
    seconds: Option<f64>,
    ffmpeg: Option<PathBuf>,
    json: bool,
    save_frame: Option<PathBuf>,
    /// 不开窗口，走原来的命令行模式。
    headless: bool,
}

const HELP: &str = "\
uulan-client —— uulan 内网远控的控制端

默认打开窗口显示被控端画面；加 --headless 退回命令行模式。

  --server <addr>     中继地址（默认 127.0.0.1:21116）
  --device <id>       被控端设备 ID（9 位数字）
  --password <pwd>    访问密码或一次性临时验证码
  --name <name>       本机在对方界面上显示的名字
  --frames <N>        解出 N 帧后退出
  --seconds <S>       跑 S 秒后退出
  --ffmpeg <path>     ffmpeg 可执行文件（默认走 PATH）
  --save-frame <path> 退出时把最后一帧存成 PPM（排查「对面到底画的是什么」用）
  --headless          不开窗口，只打印统计（自测 / 无显示环境用）
  --json              以 JSON 输出统计（仅 --headless 下生效）
  -h, --help          显示本帮助

退出码：0 正常；1 被控端拒绝（密码错/离线）；2 用法错。
";

fn parse_args() -> Result<Args> {
    let mut a = Args {
        server: format!("127.0.0.1:{}", uulan_protocol::DEFAULT_PORT),
        device: String::new(),
        password: String::new(),
        name: default_client_name(),
        frames: None,
        seconds: None,
        ffmpeg: None,
        json: false,
        save_frame: None,
        headless: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        match x.as_str() {
            "--server" => a.server = it.next().unwrap_or_default(),
            "--device" => a.device = it.next().unwrap_or_default(),
            "--password" => a.password = it.next().unwrap_or_default(),
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
    if a.device.is_empty() {
        bail!("必须给出 --device（被控端界面上显示的 9 位设备 ID）");
    }
    if a.password.is_empty() {
        bail!("必须给出 --password（被控端的访问密码或一次性临时码）");
    }
    Ok(a)
}

fn default_client_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "uulan-client".to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let args = parse_args()?;

    // M2 只实现了「看画面」。能力列表必须如实：声明了没实现的能力，
    // 被控端会准备好对应通道，而控制端点了没反应 —— 那种 bug 最难查。
    let caps: Vec<Cap> = vec![];

    info!(server = %args.server, device = %args.device, "正在连接");
    let c = match connect::connect(&args.server, &args.device, &args.password, &args.name, caps).await
    {
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

    println!(
        "已连上「{}」（{} {}，屏幕 {}x{}，编码器 {}）",
        c.host.name, c.host.os, c.host.version, c.host.screen.w, c.host.screen.h, c.host.caps.len()
    );
    info!(session = %c.session, os = %c.host.os, "鉴权通过");

    let started = Instant::now();
    let mut watcher = VideoWatcher::start(c.video, args.ffmpeg.clone())
        .await
        .context("启动视频解码失败")?;

    let info = watcher.info().clone();

    // ── 窗口模式 ──────────────────────────────────────────────────────────
    // winit 的事件循环**必须占住主线程**，而画面接收/解码跑在 tokio 里。
    // 所以这里把当前这个 async 主函数（它已在 tokio 的多线程运行时上）整体搬到
    // 后台线程去跑，主线程腾出来给窗口。`rx` 是 watch 通道，天然跨线程。
    if !args.headless {
        let rx = watcher.subscribe();
        let stats = watcher.stats();
        let overlay = uulan_client::window::Overlay {
            host_name: format!("{}({})", c.host.name, c.host.os),
            screen: (c.host.screen.w, c.host.screen.h),
            ..Default::default()
        };
        println!(
            "已连上「{}」（{} {}，屏幕 {}x{}）—— 正在打开窗口…",
            c.host.name, c.host.os, c.host.version, c.host.screen.w, c.host.screen.h
        );

        // winit 的事件循环**必须在主线程上**（winit 0.30 会硬性拒绝在其他线程创建：
        // “Initializing the event loop outside of the main thread is a significant
        //  cross-platform compatibility hazard”）。而 `#[tokio::main]` 的 async 主体
        // 本来就跑在主线程，所以这里**直接同步调用**，不要再 spawn 线程。
        //
        // 主线程被事件循环占住不影响画面：收流、喂解码器、取帧都是 tokio 任务，
        // 跑在多线程运行时的 worker 上。
        let r = uulan_client::window::run(rx, stats, overlay, args.seconds);
        watcher.shutdown();
        let s = watcher.snapshot();
        println!(
            "窗口已关闭。收到 AU {} / 关键帧 {} / 解出帧 {} / 丢帧 {} / 首帧 {}ms",
            s.aus, s.keyframes, s.decoded, s.dropped, s.first_frame_ms
        );
        return r.context("窗口退出异常");
    }

    if !args.json {
        println!(
            "画面：{}x{} {} {} fps（编码器 {}）",
            info.w, info.h, info.codec, info.fps, info.encoder
        );
    }

    let deadline = args.seconds.map(|s| started + Duration::from_secs_f64(s));
    let mut count = 0u64;
    // 只留最后一帧在内存里，退出时落盘 —— 每帧都写盘的话 1080p 一帧 6 MB，
    // 30fps 下就是 180 MB/s 的无用 IO。
    let mut last_frame: Option<uulan_client::watch::Frame> = None;
    loop {
        if let Some(n) = args.frames {
            if count >= n {
                break;
            }
        }
        if let Some(d) = deadline {
            if Instant::now() >= d {
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
            f = watcher.next_frame() => match f {
                Some(frame) => {
                    count += 1;
                    if args.save_frame.is_some() {
                        last_frame = Some(frame.clone());
                    }
                    if !args.json && (count == 1 || count % 30 == 0) {
                        let s = watcher.snapshot();
                        println!(
                            "第 {count} 帧：{}x{} 首帧 {}ms 丢包 {} 已解 {} 码率 {:.0} kbps",
                            frame.width,
                            frame.height,
                            s.first_frame_ms,
                            s.dropped,
                            s.decoded,
                            watcher.stats().bitrate_kbps(watcher.elapsed()),
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
                if deadline.is_some() {
                    break;
                }
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

    let s = watcher.snapshot();
    let elapsed = watcher.elapsed();
    let kbps = watcher.stats().bitrate_kbps(elapsed);

    if args.json {
        println!(
            "{}",
            serde_json::json!({
                "host": { "name": c.host.name, "os": c.host.os, "version": c.host.version,
                          "screen": { "w": c.host.screen.w, "h": c.host.screen.h } },
                "video": { "w": info.w, "h": info.h, "codec": info.codec, "fps": info.fps,
                           "encoder": info.encoder },
                "stats": { "aus": s.aus, "keyframes": s.keyframes, "bytes": s.bytes,
                           "dropped": s.dropped, "decoded": s.decoded,
                           "first_frame_ms": s.first_frame_ms, "ended": s.ended },
                "rendered": count,
                "elapsed_s": elapsed.as_secs_f64(),
                "bitrate_kbps": kbps,
            })
        );
    } else {
        println!("────────────── 统计 ──────────────");
        println!("  收到 AU    : {}", s.aus);
        println!("  关键帧     : {}", s.keyframes);
        println!("  解出帧     : {}（本进程渲染 {}）", s.decoded, count);
        println!("  解码丢帧   : {}", s.dropped);
        println!("  首帧延迟   : {} ms", s.first_frame_ms);
        println!("  压缩字节   : {}", s.bytes);
        println!("  平均码率   : {:.0} kbps", kbps);
        println!("  持续时长   : {:.1} s", elapsed.as_secs_f64());
        if elapsed.as_secs_f64() > 0.0 {
            println!("  实测帧率   : {:.1} fps", s.decoded as f64 / elapsed.as_secs_f64());
        }
    }

    watcher.shutdown();
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
