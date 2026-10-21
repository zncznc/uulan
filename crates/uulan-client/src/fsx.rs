//! 控制端的文件操作（M4 文件传输的客户端半边）。
//!
//! 命令行用法（`uulan-client`）：
//!
//! ```text
//! --ls <远程路径>              列目录（`:/` = 此电脑）
//! --get <远程路径> <本地路径>   下载
//! --put <本地路径> <远程路径>   上传
//! --rm <远程路径>
//! --mkdir <远程路径>
//! ```
//!
//! 这些操作**不开窗口、不收画面**：连上、鉴权、干完、退出。视频通道连上就丢掉了
//! （被控端会看到它 EOF，自己停编码器）。
//!
//! ## 为什么下载/上传都先写 `.part`
//!
//! 断线时「目标文件存在且大小对、内容半截」是最坏的结果 —— 用户看不出问题。
//! 所以两边都写临时文件，**sha256 对上之后**才 rename 到目标。
//! 这样「文件存在」就等于「内容完整」，是远控里最省心的一个保证。

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use uulan_protocol::bulk::{BulkFrame, BulkKind, CHUNK};
use uulan_protocol::frame::{read_frame, write_frame, FrameKind};
use uulan_protocol::msg::{C2H, FsEntry, FsOp, FsStreamStage, H2C};

/// 要做的文件操作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Ls(String),
    Get { remote: String, local: PathBuf },
    Put { local: PathBuf, remote: String },
    Rm(String),
    Mkdir(String),
}

impl Op {
    /// 从命令行参数解析。返回 `None` 表示这次不是文件操作（正常开窗/看画面）。
    ///
    /// 解析是「第一个命中的子命令生效」，其余参数仍由 `main` 自己解析 ——
    /// 这样 `--server/--device/--password` 这些公共参数不用在这里重复一遍。
    pub fn from_args(args: &[String]) -> Result<Option<Op>> {
        let at = |i: usize, what: &str| -> Result<String> {
            args.get(i)
                .cloned()
                .with_context(|| format!("{what} 缺少参数（用法见 --help）"))
        };
        for (i, a) in args.iter().enumerate() {
            match a.as_str() {
                "--ls" => return Ok(Some(Op::Ls(at(i + 1, "--ls")?))),
                "--rm" => return Ok(Some(Op::Rm(at(i + 1, "--rm")?))),
                "--mkdir" => return Ok(Some(Op::Mkdir(at(i + 1, "--mkdir")?))),
                "--get" => {
                    return Ok(Some(Op::Get {
                        remote: at(i + 1, "--get")?,
                        local: PathBuf::from(at(i + 2, "--get")?),
                    }))
                }
                "--put" => {
                    return Ok(Some(Op::Put {
                        local: PathBuf::from(at(i + 1, "--put")?),
                        remote: at(i + 2, "--put")?,
                    }))
                }
                _ => {}
            }
        }
        Ok(None)
    }
}

/// 控制通道上与本操作有关的一条消息。
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

/// 生产环境里 bulk 连接的两个半端。
type Conn = Peer<tokio::net::tcp::OwnedReadHalf, tokio::net::tcp::OwnedWriteHalf>;

struct Ctl {
    rd: tokio::net::tcp::OwnedReadHalf,
    wr: tokio::net::tcp::OwnedWriteHalf,
}

impl Ctl {
    async fn send(&mut self, m: &C2H) -> Result<()> {
        let f = m.to_frame()?;
        write_frame(&mut self.wr, f.kind, &f.payload).await?;
        Ok(())
    }

    /// 等下一条属于 `id` 的通知/结果。别的消息（剪贴板、指标、输入错误）直接跳过。
    async fn wait(&mut self, id: u32) -> Result<Step> {
        loop {
            let f = read_frame(&mut self.rd).await.context("控制通道读失败（被控端断线了？）")?;
            if f.kind == FrameKind::Ping {
                let _ = write_frame(&mut self.wr, FrameKind::Pong, &f.payload).await;
                continue;
            }
            if f.kind != FrameKind::Json {
                continue;
            }
            let m: H2C = match f.decode_json() {
                Ok(m) => m,
                Err(_) => continue,
            };
            match m {
                H2C::FsResult { id: rid, ok, error, entries, path, hash } if rid == id => {
                    return Ok(Step::Result { ok, error, entries, path, hash })
                }
                H2C::FsStream { id: rid, stage, total, done } if rid == id => {
                    return Ok(match stage {
                        FsStreamStage::Begin => Step::Begin { total },
                        FsStreamStage::Done => Step::Done { done },
                        FsStreamStage::Failed => Step::Failed { done },
                    })
                }
                H2C::Bye { reason } => bail!("被控端道别：{reason}"),
                _ => continue,
            }
        }
    }

    /// 一直等到 `FsResult`（跳过中途的 Begin/Done，但把 Done 的字节数记进日志里，
    /// 大文件传一半断了时这行是唯一线索）。
    async fn wait_result(&mut self, id: u32) -> Result<Step> {
        loop {
            match self.wait(id).await? {
                r @ Step::Result { .. } => return Ok(r),
                Step::Done { done } => tracing::debug!(id, done, "对端报告传输完成"),
                _ => {}
            }
        }
    }
}

fn hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

fn short_hash(h: &str) -> &str {
    &h[..h.len().min(12)]
}

/// 执行一次文件操作。`control` / `bulk` 来自 `connect::Connected`。
pub async fn run(control: TcpStream, bulk: TcpStream, op: Op) -> Result<()> {
    let (cr, cw) = control.into_split();
    let (br, bw) = bulk.into_split();
    let mut ctl = Ctl { rd: cr, wr: cw };
    let mut peer = Peer { rd: br, wr: bw };

    match op {
        Op::Ls(p) => ls(&mut ctl, p).await,
        Op::Get { remote, local } => get(&mut ctl, &mut peer, remote, local).await,
        Op::Put { local, remote } => put(&mut ctl, &mut peer, local, remote).await,
        Op::Rm(p) => simple(&mut ctl, FsOp::Remove { id: 1, path: p }, "已删除").await,
        Op::Mkdir(p) => simple(&mut ctl, FsOp::Mkdir { id: 1, path: p }, "已创建").await,
    }
}

async fn ls(ctl: &mut Ctl, path: String) -> Result<()> {
    ctl.send(&C2H::Fs(FsOp::List { id: 1, path: path.clone() })).await?;
    let Step::Result { ok, error, entries, path: shown, .. } = ctl.wait_result(1).await? else {
        unreachable!()
    };
    if !ok {
        bail!("列目录失败：{}", error.unwrap_or_default());
    }
    let entries = entries.unwrap_or_default();
    // 时间一律按 UTC 显示并明说 —— 本机没有时区换算（不引时区库），
    // 与其让人对着差 8 小时的目录瞎猜，不如标清楚。
    println!("目录：{}（时间均为 UTC）", shown.unwrap_or(path));
    if entries.is_empty() {
        println!("  （空）");
    }
    for e in &entries {
        let kind = if e.dir { "d" } else { "-" };
        let when = fmt_time(e.mtime);
        if e.dir {
            println!("  {kind}        -  {when}  {}/", e.name);
        } else {
            println!("  {kind} {:>8}  {when}  {}", e.size, e.name);
        }
    }
    let files = entries.iter().filter(|e| !e.dir).count();
    let dirs = entries.len() - files;
    println!("共 {} 项（目录 {dirs} / 文件 {files}）", entries.len());
    Ok(())
}

async fn simple(ctl: &mut Ctl, op: FsOp, verb: &str) -> Result<()> {
    let id = match &op {
        FsOp::Remove { id, .. } | FsOp::Mkdir { id, .. } => *id,
        _ => 1,
    };
    ctl.send(&C2H::Fs(op)).await?;
    let Step::Result { ok, error, path, .. } = ctl.wait_result(id).await? else {
        unreachable!()
    };
    if !ok {
        bail!("{verb}失败：{}", error.unwrap_or_default());
    }
    println!("{verb}：{}", path.unwrap_or_default());
    Ok(())
}

async fn get(ctl: &mut Ctl, peer: &mut Conn, remote: String, local: PathBuf) -> Result<()> {
    ctl.send(&C2H::Fs(FsOp::Get { id: 1, path: remote.clone() })).await?;
    let total = match ctl.wait(1).await? {
        Step::Begin { total } => total,
        Step::Result { ok: false, error, .. } => {
            bail!("下载失败：{}", error.unwrap_or_default())
        }
        Step::Failed { done } => bail!("被控端中止了传输（已收 {done} 字节）"),
        other => bail!("被控端没有开始发数据：{other:?}"),
    };
    if let Some(t) = total {
        println!("开始下载 {}（{} 字节）→ {}", remote, t, local.display());
    }

    let part = part_path(&local);
    let mut f = tokio::fs::File::create(&part)
        .await
        .with_context(|| format!("建本地文件失败：{}", part.display()))?;
    let mut hasher = Sha256::new();
    let mut got: u64 = 0;
    while let Some(bf) = peer.recv().await? {
        if bf.id != 1 {
            continue;
        }
        match bf.kind {
            BulkKind::End => break,
            BulkKind::Data => {
                hasher.update(&bf.data);
                f.write_all(&bf.data).await?;
                got += bf.data.len() as u64;
            }
        }
    }
    f.flush().await?;
    drop(f);

    let Step::Result { ok, error, hash, .. } = ctl.wait_result(1).await? else {
        unreachable!()
    };
    if !ok {
        let _ = tokio::fs::remove_file(&part).await;
        bail!("下载失败：{}", error.unwrap_or_default());
    }
    let mine = hex(&hasher.finalize());
    if let Some(want) = &hash {
        if *want != mine {
            let _ = tokio::fs::remove_file(&part).await;
            bail!("校验失败：被控端 sha256={} 本地={}，已丢弃", short_hash(want), short_hash(&mine));
        }
    }
    if let Some(t) = total {
        if got != t {
            let _ = tokio::fs::remove_file(&part).await;
            bail!("收到 {got} 字节，被控端声明 {t} 字节，已丢弃");
        }
    }
    tokio::fs::rename(&part, &local)
        .await
        .with_context(|| format!("改名到 {} 失败", local.display()))?;
    println!(
        "已下载 {} 字节 → {}（sha256 {}）",
        got,
        local.display(),
        hash.as_deref().map(short_hash).unwrap_or("未提供")
    );
    Ok(())
}

async fn put(ctl: &mut Ctl, peer: &mut Conn, local: PathBuf, remote: String) -> Result<()> {
    let md = tokio::fs::metadata(&local)
        .await
        .with_context(|| format!("本地文件读不到：{}", local.display()))?;
    if md.is_dir() {
        bail!("{} 是目录，不能上传", local.display());
    }
    let size = md.len();
    ctl.send(&C2H::Fs(FsOp::Put { id: 1, path: remote.clone(), size })).await?;

    // Begin 是被控端的「放行信号」：它已经校验过路径、建好临时文件了。
    match ctl.wait(1).await? {
        Step::Begin { .. } => {}
        Step::Result { ok: false, error, .. } => bail!("上传失败：{}", error.unwrap_or_default()),
        Step::Failed { done } => bail!("被控端中止了接收（已发 {done} 字节）"),
        other => bail!("被控端没有准备好接收：{other:?}"),
    }

    let mut f = tokio::fs::File::open(&local).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut sent: u64 = 0;
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        peer.send_data(1, &buf[..n]).await?;
        sent += n as u64;
    }
    peer.send_end(1).await?;

    let Step::Result { ok, error, hash, .. } = ctl.wait_result(1).await? else {
        unreachable!()
    };
    if !ok {
        bail!("上传失败：{}", error.unwrap_or_default());
    }
    let mine = hex(&hasher.finalize());
    if let Some(want) = &hash {
        if *want != mine {
            bail!(
                "上传后校验不一致：被控端 sha256={} 本地={}（文件可能已被截断）",
                short_hash(want),
                short_hash(&mine)
            );
        }
    }
    println!(
        "已上传 {} 字节 → {}（sha256 {}）",
        sent,
        remote,
        hash.as_deref().map(short_hash).unwrap_or("未提供")
    );
    Ok(())
}

/// 下载用的临时文件名（与目标同目录，rename 才是原子的）。
fn part_path(local: &Path) -> PathBuf {
    let name = local.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let dir = local.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    dir.join(format!("{name}.uulan-part"))
}

/// 秒级时间戳 → `YYYY-MM-DD HH:MM`（UTC 换算成系统本地时区的偏移不做：
/// 只用来给用户一个大致印象，不值得为此引一个时区库）。
fn fmt_time(secs: i64) -> String {
    if secs <= 0 {
        return "         -      ".to_string();
    }
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, m) = (rem / 3600, (rem % 3600) / 60);
    // 民用历法换算（Howard Hinnant 的 civil_from_days）
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}")
}

/// bulk 连接的客户端半边。
struct Peer<R, W> {
    rd: R,
    wr: W,
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> Peer<R, W> {
    async fn send_data(&mut self, id: u32, data: &[u8]) -> Result<()> {
        // 拷一份进 Bytes：帧要跨 await 活着，不能借着调用方的切片。
        let payload = BulkFrame::data(id, bytes::Bytes::copy_from_slice(data)).encode();
        write_frame(&mut self.wr, FrameKind::Blob, &payload)
            .await
            .context("写 bulk 失败（被控端可能已断开）")?;
        Ok(())
    }

    async fn send_end(&mut self, id: u32) -> Result<()> {
        write_frame(&mut self.wr, FrameKind::Blob, &BulkFrame::end(id).encode())
            .await
            .context("写 bulk 失败（被控端可能已断开）")?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<BulkFrame>> {
        let f = read_frame(&mut self.rd).await.context("读 bulk 失败")?;
        if f.kind != FrameKind::Blob {
            return Ok(None);
        }
        Ok(Some(BulkFrame::decode(&f.payload)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_each_file_operation() {
        assert_eq!(Op::from_args(&args(&["--ls", ":/"])).unwrap(), Some(Op::Ls(":/".into())));
        assert_eq!(
            Op::from_args(&args(&["--get", "~/a.bin", "D:/tmp/a.bin"])).unwrap(),
            Some(Op::Get { remote: "~/a.bin".into(), local: PathBuf::from("D:/tmp/a.bin") })
        );
        assert_eq!(
            Op::from_args(&args(&["--put", "D:/tmp/a.bin", "~/a.bin"])).unwrap(),
            Some(Op::Put { local: PathBuf::from("D:/tmp/a.bin"), remote: "~/a.bin".into() })
        );
        assert_eq!(Op::from_args(&args(&["--rm", "~/x"])).unwrap(), Some(Op::Rm("~/x".into())));
    }

    #[test]
    fn plain_viewer_args_are_not_file_operations() {
        assert_eq!(Op::from_args(&args(&["--server", "1.2.3.4:1", "--device", "123456789"])).unwrap(), None);
    }

    #[test]
    fn missing_operands_are_errors_not_panics() {
        assert!(Op::from_args(&args(&["--get", "only-one"])).is_err());
        assert!(Op::from_args(&args(&["--ls"])).is_err());
        assert!(Op::from_args(&args(&["--put", "a"])).is_err());
    }

    #[test]
    fn part_file_sits_next_to_the_target() {
        let p = part_path(Path::new("D:/tmp/x.bin"));
        assert_eq!(p.file_name().unwrap().to_string_lossy(), "x.bin.uulan-part");
        assert_eq!(p.parent().unwrap(), Path::new("D:/tmp"));
    }

    #[test]
    fn time_formatting_is_civil() {
        // 1700000000 = 2023-11-14 22:13:20 UTC
        let s = fmt_time(1700000000);
        assert!(s.starts_with("2023-11-14"), "got {s}");
        assert_eq!(fmt_time(0), "         -      ");
    }
}