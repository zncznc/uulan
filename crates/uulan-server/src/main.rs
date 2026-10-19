//! uulan-server 命令行入口。
//!
//! 用法：
//!   uulan-server                          # 监听 0.0.0.0:21116，台账落在当前目录
//!   uulan-server --bind 10.0.0.1:21116
//!   uulan-server --state /var/lib/uulan/devices.json
//!   uulan-server --list                   # 打印台账里登记过的设备
//!   uulan-server --forget 123456789       # 从台账里摘掉一台设备

use std::path::PathBuf;

use anyhow::{bail, Result};
use uulan_protocol::DEFAULT_PORT;
use uulan_server::{DeviceBook, Server};

struct Args {
    bind: String,
    state: PathBuf,
    list: bool,
    forget: Option<String>,
}

fn parse_args() -> Result<Args> {
    let mut bind = format!("0.0.0.0:{DEFAULT_PORT}");
    let mut state = PathBuf::from("uulan-devices.json");
    let mut list = false;
    let mut forget = None;

    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--bind" => bind = it.next().unwrap_or_default(),
            "--state" => state = PathBuf::from(it.next().unwrap_or_default()),
            "--list" => list = true,
            "--forget" => forget = it.next(),
            "-h" | "--help" => {
                println!("{}", HELP);
                std::process::exit(0);
            }
            other => bail!("未知参数 {other}（用 --help 查看用法）"),
        }
    }
    if bind.is_empty() {
        bail!("--bind 需要一个地址，例如 --bind 0.0.0.0:{DEFAULT_PORT}");
    }
    Ok(Args { bind, state, list, forget })
}

const HELP: &str = "\
uulan-server —— uulan 内网远控的中继/会合服务器

  --bind <addr>     监听地址（默认 0.0.0.0:21116）
  --state <path>    设备台账 JSON 路径（默认 ./uulan-devices.json）
  --list            打印台账里登记过的设备后退出
  --forget <id>     从台账里摘掉一台设备后退出
  -h, --help        显示本帮助

服务器是盲转发的：它不认识屏幕内容、键盘、剪贴板、文件，
也不知道任何设备密码。访问控制由被控端自己做。
";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = parse_args()?;

    if args.list || args.forget.is_some() {
        let mut book = DeviceBook::load(&args.state).unwrap_or_default();
        if let Some(id) = &args.forget {
            if book.forget(id) {
                book.save(&args.state)?;
                println!("已摘除设备 {id}");
            } else {
                println!("台账里没有设备 {id}");
            }
            return Ok(());
        }
        let rows = book.list();
        if rows.is_empty() {
            println!("台账为空：{}", args.state.display());
        } else {
            println!("{} 台设备（{}）:", rows.len(), args.state.display());
            for (id, name) in rows {
                println!("  {id}  {name}");
            }
        }
        return Ok(());
    }

    let server = Server::new(Some(args.state))?;
    let listener = server.bind(&args.bind).await?;
    server.serve(listener).await
}