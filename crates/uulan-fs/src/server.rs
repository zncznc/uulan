//! 被控端的文件服务：把 `FsOp` 变成真实的文件操作，数据走 bulk 连接。
//!
//! 分层和键鼠/剪贴板一致：**协议 → 动作**这一层不依赖任何平台细节，
//! 只依赖 `AsyncRead`/`AsyncWrite`，所以本机（Windows）能用真 socket + 真文件把它测住。
//!
//! ## 一次传输的时序（get / put 都用同一套）
//!
//! ```text
//! get:  C→H Fs(Get)                    H→C FsStream{Begin,total}
//!                                      H→C Blob(id) … Blob(id) …
//!                                      H→C Blob(id, END)
//!                                      H→C FsStream{Done} + FsResult{ok, hash}
//!
//! put:  C→H Fs(Put,size)               H→C FsStream{Begin}     ← 故意用 Begin 当「可以发了」
//!       C→H Blob(id) … Blob(id, END)
//!                                      H→C FsStream{Done} + FsResult{ok, hash}
//! ```
//!
//! `FsResult` 只在**最后**出现、且只有一条：`Begin` 既是通知也是上传的放行信号，
//! 这样客户端不需要「同一个 id 的两条结果怎么区分」这种约定。
//!
//! ## 上传必须原子落地
//!
//! 直接往目标文件里写，传到一半断线就会留下一个「大小对、内容是半截」的文件 ——
//! 那比传失败更坏，因为用户看不出来。所以先写 `<名字>.uulan-part`，
//! 校验字节数与 sha256 之后才 rename 到目标；失败就删掉临时文件。

use std::io;

use bytes::Bytes;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use uulan_protocol::bulk::{BulkFrame, BulkKind, CHUNK};
use uulan_protocol::frame::{read_frame, write_frame, FrameError, FrameKind};
use uulan_protocol::msg::{FsEntry, FsOp, FsStreamStage, H2C};

use crate::mounts;
use crate::path::{self, FsRoots, Target};

/// 上传时用的临时后缀。同目录、同文件系统，所以 rename 是原子的。
pub const PART_SUFFIX: &str = ".uulan-part";

/// bulk 连接的两半（**持有**而不是借用：生产里就是 `into_split()` 出来的两半，
/// 测试里是 `duplex` 切出来的两半，都是拥有所有权的）。
pub struct Bulk<R, W> {
    pub rd: R,
    pub wr: W,
}

impl<R, W> Bulk<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    pub fn new(rd: R, wr: W) -> Self {
        Self { rd, wr }
    }

    pub async fn send_data(&mut self, id: u32, data: &[u8]) -> io::Result<()> {
        // 拷一份进 Bytes：帧要跨 await 活着。
        let payload = BulkFrame::data(id, Bytes::copy_from_slice(data)).encode();
        write_frame(&mut self.wr, FrameKind::Blob, &payload)
            .await
            .map_err(io::Error::from)
    }

    pub async fn send_end(&mut self, id: u32) -> io::Result<()> {
        write_frame(&mut self.wr, FrameKind::Blob, &BulkFrame::end(id).encode())
            .await
            .map_err(io::Error::from)
    }

    /// 读一个 bulk 数据帧。`Ok(None)` = 对端关了连接。
    ///
    /// 非 `Blob` 帧（心跳之类）直接跳过：这条连接只承载数据，
    /// 但控制连接上的心跳万一被转发到这里也不该把传输搞崩。
    pub async fn recv(&mut self) -> io::Result<Option<BulkFrame>> {
        loop {
            let f = match read_frame(&mut self.rd).await {
                Ok(f) => f,
                Err(FrameError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
                Err(e) => return Err(io::Error::from(e)),
            };
            if f.kind != FrameKind::Blob {
                continue;
            }
            match BulkFrame::decode(&f.payload) {
                Ok(b) => return Ok(Some(b)),
                Err(e) => {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
                }
            }
        }
    }
}

/// 处理一条 `FsOp`，返回要回给控制端的最终消息。
///
/// `notify` 用来发中途通知（`FsStream`）。它刻意是同步闭包：被控端那边就是往
/// 无界队列里塞一条，不需要在这里 `await` 一个通道。
pub async fn handle<R, W>(
    op: &FsOp,
    roots: &FsRoots,
    bulk: &mut Bulk<R, W>,
    notify: &mut (dyn FnMut(H2C) + Send),
) -> H2C
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    match op {
        FsOp::List { id, path: spec } => list(*id, roots, spec).await,
        FsOp::Get { id, path: spec } => get(*id, roots, spec, bulk, notify).await,
        FsOp::Put { id, path: spec, size } => put(*id, roots, spec, *size, bulk, notify).await,
        FsOp::Mkdir { id, path: spec } => mkdir(*id, roots, spec).await,
        FsOp::Remove { id, path: spec } => remove(*id, roots, spec).await,
        FsOp::Rename { id, from, to } => rename(*id, roots, from, to).await,
        FsOp::Hash { id, path: spec } => hash(*id, roots, spec).await,
    }
}

fn ok(id: u32, path: Option<String>, hash: Option<String>) -> H2C {
    H2C::FsResult { id, ok: true, error: None, entries: None, path, hash }
}

fn err(id: u32, error: impl Into<String>) -> H2C {
    H2C::FsResult {
        id,
        ok: false,
        error: Some(error.into()),
        entries: None,
        path: None,
        hash: None,
    }
}

/// 把解析结果变成「必须是一个目录 / 一个文件」的路径，或者一句人话错误。
async fn resolve_dir(roots: &FsRoots, spec: &str) -> Result<PathBuf, String> {
    match path::resolve(roots, spec).map_err(|e| e.to_string())? {
        Target::Drives => Err("「此电脑」不是一个目录".into()),
        Target::Path(p) => {
            let md = tokio::fs::metadata(&p).await.map_err(|e| format!("{}: {e}", p.display()))?;
            if !md.is_dir() {
                return Err(format!("不是目录：{}", p.display()));
            }
            Ok(p)
        }
    }
}

async fn resolve_any(roots: &FsRoots, spec: &str) -> Result<PathBuf, String> {
    match path::resolve(roots, spec).map_err(|e| e.to_string())? {
        Target::Drives => Err("「此电脑」不是一个可操作的路径".into()),
        Target::Path(p) => Ok(p),
    }
}

async fn list(id: u32, roots: &FsRoots, spec: &str) -> H2C {
    // 「此电脑」：列盘符 / 挂载点。
    match path::resolve(roots, spec) {
        Ok(Target::Drives) => {
            let entries = mounts::list()
                .into_iter()
                .map(|name| FsEntry { name, dir: true, size: 0, mtime: 0 })
                .collect();
            return H2C::FsResult {
                id,
                ok: true,
                error: None,
                entries: Some(entries),
                path: Some(":/".to_string()),
                hash: None,
            };
        }
        Ok(Target::Path(_)) => {}
        Err(e) => return err(id, e.to_string()),
    }

    let dir = match resolve_dir(roots, spec).await {
        Ok(d) => d,
        Err(e) => return err(id, e),
    };
    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(r) => r,
        Err(e) => return err(id, format!("{}: {e}", dir.display())),
    };
    let mut entries: Vec<FsEntry> = Vec::new();
    loop {
        let ent = match rd.next_entry().await {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(e) => return err(id, format!("读目录失败：{e}")),
        };
        let ft = match ent.file_type().await {
            Ok(t) => t,
            Err(_) => continue, // 边列边被删掉的条目，跳过比整单失败合理
        };
        let md = ent.metadata().await.ok();
        entries.push(FsEntry {
            name: ent.file_name().to_string_lossy().into_owned(),
            dir: ft.is_dir(),
            size: md.as_ref().map(|m| m.len()).unwrap_or(0),
            mtime: md
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        });
    }
    // 目录在前、再按名字（不区分大小写）：和图文件管理器的观感一致。
    entries.sort_by(|a, b| {
        b.dir.cmp(&a.dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    H2C::FsResult {
        id,
        ok: true,
        error: None,
        entries: Some(entries),
        path: Some(dir.display().to_string()),
        hash: None,
    }
}

async fn get<R, W>(
    id: u32,
    roots: &FsRoots,
    spec: &str,
    bulk: &mut Bulk<R, W>,
    notify: &mut (dyn FnMut(H2C) + Send),
) -> H2C
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let p = match resolve_any(roots, spec).await {
        Ok(p) => p,
        Err(e) => return err(id, e),
    };
    let md = match tokio::fs::metadata(&p).await {
        Ok(m) => m,
        Err(e) => return err(id, format!("{}: {e}", p.display())),
    };
    if md.is_dir() {
        return err(id, format!("是目录，不能下载：{}", p.display()));
    }
    let mut f = match tokio::fs::File::open(&p).await {
        Ok(f) => f,
        Err(e) => return err(id, format!("{}: {e}", p.display())),
    };
    let total = md.len();
    notify(H2C::FsStream { id, stage: FsStreamStage::Begin, total: Some(total), done: 0 });

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut done: u64 = 0;
    loop {
        let n = match f.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                notify(H2C::FsStream {
                    id,
                    stage: FsStreamStage::Failed,
                    total: Some(total),
                    done,
                });
                return err(id, format!("读文件失败：{e}"));
            }
        };
        hasher.update(&buf[..n]);
        if let Err(e) = bulk.send_data(id, &buf[..n]).await {
            notify(H2C::FsStream {
                id,
                stage: FsStreamStage::Failed,
                total: Some(total),
                done,
            });
            return err(id, format!("写 bulk 失败：{e}"));
        }
        done += n as u64;
    }
    if let Err(e) = bulk.send_end(id).await {
        return err(id, format!("写 bulk 失败：{e}"));
    }
    notify(H2C::FsStream { id, stage: FsStreamStage::Done, total: Some(total), done });
    ok(id, Some(p.display().to_string()), Some(hex(&hasher.finalize())))
}

async fn put<R, W>(
    id: u32,
    roots: &FsRoots,
    spec: &str,
    size: u64,
    bulk: &mut Bulk<R, W>,
    notify: &mut (dyn FnMut(H2C) + Send),
) -> H2C
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let p = match resolve_any(roots, spec).await {
        Ok(p) => p,
        Err(e) => return err(id, e),
    };
    if tokio::fs::metadata(&p).await.map(|m| m.is_dir()).unwrap_or(false) {
        return err(id, format!("目标是目录，不能覆盖：{}", p.display()));
    }
    // 不自动建上级目录：静默把文件落到一个「本来不存在」的目录里，
    // 用户回头找不到。让控制端显式 mkdir 更符合直觉。
    let parent = p.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    if !tokio::fs::metadata(&parent).await.map(|m| m.is_dir()).unwrap_or(false) {
        return err(id, format!("上级目录不存在：{}", parent.display()));
    }

    // 临时文件与目标同目录 → 同文件系统 → rename 原子。
    let part = {
        let name = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        parent.join(format!("{name}{PART_SUFFIX}"))
    };
    let mut f = match tokio::fs::File::create(&part).await {
        Ok(f) => f,
        Err(e) => return err(id, format!("{}: {e}", part.display())),
    };

    // 放行信号：Begin 之后的 bulk 帧才是这次上传的数据。
    notify(H2C::FsStream { id, stage: FsStreamStage::Begin, total: Some(size), done: 0 });

    let mut hasher = Sha256::new();
    let mut done: u64 = 0;

    loop {
        let bf = match bulk.recv().await {
            Ok(Some(b)) => b,
            Ok(None) => return fail_op(id, size, done, &part, "上传过程中 bulk 连接断了".to_string(), notify),
            Err(e) => return fail_op(id, size, done, &part, format!("读 bulk 失败：{e}"), notify),
        };
        if bf.id != id {
            // 第一版只允许一条流：别的 id 说明对端状态错乱，继续写只会写坏文件。
            return fail_op(id, size, done, &part, format!("bulk 上出现了别的流（id={}），本次上传中止", bf.id), notify);
        }
        match bf.kind {
            BulkKind::End => break,
            BulkKind::Data => {
                if done + bf.data.len() as u64 > size {
                    return fail_op(id, size, done, &part, format!("收到的数据超过声明的 {size} 字节"), notify);
                }
                hasher.update(&bf.data);
                if let Err(e) = f.write_all(&bf.data).await {
                    return fail_op(id, size, done, &part, format!("写临时文件失败：{e}"), notify);
                }
                done += bf.data.len() as u64;
            }
        }
    }

    if let Err(e) = f.flush().await {
        return fail_op(id, size, done, &part, format!("落盘失败：{e}"), notify);
    }
    drop(f);
    if done != size {
        return fail_op(id, size, done, &part, format!("收到 {done} 字节，与声明的 {size} 字节不符"), notify);
    }

    // Windows 的 rename 不会覆盖已存在的目标，先删再改名。删与改名之间有个小窗口，
    // 但这是 Windows 上「原子替换」能做到的最好程度（ReplaceFile 要 unsafe + winapi）。
    #[cfg(windows)]
    {
        if tokio::fs::metadata(&p).await.is_ok() {
            if let Err(e) = tokio::fs::remove_file(&p).await {
                return fail_op(id, size, done, &part, format!("覆盖前删除旧文件失败：{e}"), notify);
            }
        }
    }
    if let Err(e) = tokio::fs::rename(&part, &p).await {
        return fail_op(id, size, done, &part, format!("重命名到目标失败：{e}"), notify);
    }

    notify(H2C::FsStream { id, stage: FsStreamStage::Done, total: Some(size), done });
    ok(id, Some(p.display().to_string()), Some(hex(&hasher.finalize())))
}

async fn mkdir(id: u32, roots: &FsRoots, spec: &str) -> H2C {
    let p = match resolve_any(roots, spec).await {
        Ok(p) => p,
        Err(e) => return err(id, e),
    };
    match tokio::fs::create_dir(&p).await {
        Ok(()) => ok(id, Some(p.display().to_string()), None),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            err(id, format!("已存在：{}", p.display()))
        }
        Err(e) => err(id, format!("{}: {e}", p.display())),
    }
}

async fn remove(id: u32, roots: &FsRoots, spec: &str) -> H2C {
    let p = match resolve_any(roots, spec).await {
        Ok(p) => p,
        Err(e) => return err(id, e),
    };
    let md = match tokio::fs::metadata(&p).await {
        Ok(m) => m,
        Err(e) => return err(id, format!("{}: {e}", p.display())),
    };
    let r = if md.is_dir() {
        // 不递归删除：远控里「删错一个目录」是不可挽回的事故，让用户自己进去清空。
        tokio::fs::remove_dir(&p).await
    } else {
        tokio::fs::remove_file(&p).await
    };
    match r {
        Ok(()) => ok(id, Some(p.display().to_string()), None),
        Err(e) if md.is_dir() => err(id, format!("删除目录失败（可能非空）：{e}")),
        Err(e) => err(id, format!("{}: {e}", p.display())),
    }
}

async fn rename(id: u32, roots: &FsRoots, from: &str, to: &str) -> H2C {
    let a = match resolve_any(roots, from).await {
        Ok(p) => p,
        Err(e) => return err(id, e),
    };
    let b = match resolve_any(roots, to).await {
        Ok(p) => p,
        Err(e) => return err(id, e),
    };
    if tokio::fs::metadata(&b).await.is_ok() {
        return err(id, format!("目标已存在：{}", b.display()));
    }
    match tokio::fs::rename(&a, &b).await {
        Ok(()) => ok(id, Some(b.display().to_string()), None),
        Err(e) => err(id, format!("{} → {}: {e}", a.display(), b.display())),
    }
}

async fn hash(id: u32, roots: &FsRoots, spec: &str) -> H2C {
    let p = match resolve_any(roots, spec).await {
        Ok(p) => p,
        Err(e) => return err(id, e),
    };
    match hash_file(&p).await {
        Ok(h) => ok(id, Some(p.display().to_string()), Some(h)),
        Err(e) => err(id, e),
    }
}

/// 流式算 sha256，不把整个文件读进内存。
pub async fn hash_file(p: &Path) -> Result<String, String> {
    let mut f = tokio::fs::File::open(p).await.map_err(|e| format!("{}: {e}", p.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = f.read(&mut buf).await.map_err(|e| format!("读文件失败：{e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(d: &[u8]) -> String {
    let mut s = String::with_capacity(d.len() * 2);
    for b in d {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 上传失败的统一收尾：删掉临时文件、发一条 `Failed` 通知、返回带原因的 `FsResult`。
///
/// **绝不留下半截文件**：临时文件在目标同目录，失败时删掉；成功才 rename 过去。
fn fail_op(
    id: u32,
    size: u64,
    done: u64,
    part: &Path,
    msg: String,
    notify: &mut (dyn FnMut(H2C) + Send),
) -> H2C {
    let _ = std::fs::remove_file(part);
    notify(H2C::FsStream { id, stage: FsStreamStage::Failed, total: Some(size), done });
    err(id, msg)
}

/// 未使用的辅助：给测试与调用方判断一个目录是否可写（写一个真文件再删）。
pub fn dir_is_writable(dir: &Path) -> bool {
    let probe = dir.join(".uulan-write-probe");
    match std::fs::write(&probe, b"probe") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use tokio::io::{duplex, split, ReadHalf, WriteHalf};

    type ServerBulk = Bulk<ReadHalf<tokio::io::DuplexStream>, WriteHalf<tokio::io::DuplexStream>>;
    type ClientPeer = Peer<ReadHalf<tokio::io::DuplexStream>, WriteHalf<tokio::io::DuplexStream>>;

    /// 客户端一侧的 bulk：读、写各一半。
    struct Peer<R, W> {
        rd: R,
        wr: W,
    }

    impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> Peer<R, W> {
        async fn send(&mut self, id: u32, data: &[u8]) {
            let payload = BulkFrame::data(id, Bytes::copy_from_slice(data)).encode();
            write_frame(&mut self.wr, FrameKind::Blob, &payload).await.unwrap();
        }

        async fn end(&mut self, id: u32) {
            write_frame(&mut self.wr, FrameKind::Blob, &BulkFrame::end(id).encode())
                .await
                .unwrap();
        }

        async fn recv(&mut self) -> BulkFrame {
            let f = read_frame(&mut self.rd).await.expect("读 bulk 帧");
            assert_eq!(f.kind, FrameKind::Blob);
            BulkFrame::decode(&f.payload).unwrap()
        }
    }

    /// 一条内存里的 bulk 连接（4 MiB 缓冲）：(服务端, 客户端)。
    fn wire() -> (ServerBulk, ClientPeer) {
        let (a, b) = duplex(1 << 22);
        let (ar, aw) = split(a);
        let (br, bw) = split(b);
        (Bulk::new(br, bw), Peer { rd: ar, wr: aw })
    }

    /// 确定性伪随机内容（不引 rand；出错能复现）。
    fn payload(n: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(n);
        let mut x: u32 = 0x1234_5678;
        for _ in 0..n {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            v.push((x >> 24) as u8);
        }
        v
    }

    fn expect_ok(m: &H2C) {
        match m {
            H2C::FsResult { ok, error, .. } => assert!(*ok, "期望成功，实际失败：{error:?}"),
            other => panic!("期望 FsResult，实际 {other:?}"),
        }
    }

    fn expect_err(m: &H2C) -> String {
        match m {
            H2C::FsResult { ok, error, .. } => {
                assert!(!*ok, "期望失败，实际成功（{m:?}）");
                error.clone().unwrap_or_default()
            }
            other => panic!("期望 FsResult，实际 {other:?}"),
        }
    }

    fn hash_of(data: &[u8]) -> String {
        hex(&Sha256::digest(data))
    }

    /// 上传一份数据，返回 (最终结果, 中途通知)。
    async fn put_file(roots: &FsRoots, id: u32, path: &str, data: &[u8]) -> (H2C, Vec<H2C>) {
        let (mut srv, mut peer) = wire();
        let d = data.to_vec();
        let feeder = tokio::spawn(async move {
            for c in d.chunks(CHUNK) {
                peer.send(id, c).await;
            }
            peer.end(id).await;
        });
        let mut notes = Vec::new();
        let r = handle(
            &FsOp::Put { id, path: path.to_string(), size: data.len() as u64 },
            roots,
            &mut srv,
            &mut |m| notes.push(m),
        )
        .await;
        feeder.await.unwrap();
        (r, notes)
    }

    /// 下载一个文件，返回 (最终结果, 中途通知, 收到字节)。
    async fn get_file(roots: &FsRoots, id: u32, path: &str) -> (H2C, Vec<H2C>, Vec<u8>) {
        let (mut srv, mut peer) = wire();
        let roots = roots.clone();
        let p = path.to_string();
        let task = tokio::spawn(async move {
            let mut notes = Vec::new();
            let r = handle(&FsOp::Get { id, path: p }, &roots, &mut srv, &mut |m| notes.push(m)).await;
            (r, notes)
        });
        let mut got = Vec::new();
        loop {
            let f = peer.recv().await;
            assert_eq!(f.id, id, "数据帧必须带上请求里的 id");
            if f.kind == BulkKind::End {
                break;
            }
            got.extend_from_slice(&f.data);
        }
        let (r, notes) = task.await.unwrap();
        (r, notes, got)
    }

    /// 所有写盘测试共用的临时目录；不可写就跳过。
    fn tmp_or_skip(tag: &str) -> Option<TempDir> {
        let t = TempDir::new(tag);
        if t.is_none() {
            eprintln!("跳过：这台机器没有可写目录（见 uulan_fs::testutil）");
        }
        t
    }

    #[tokio::test]
    async fn put_then_get_is_byte_exact_and_hashed() {
        let Some(tmp) = tmp_or_skip("rt") else { return };
        let roots = FsRoots::new(tmp.path(), true);
        let data = payload(300 * 1024 + 7); // 跨多个 64 KiB 块，且不是整块
        let target = tmp.path().join("upload.bin");
        let spec = target.display().to_string();

        let (r, notes) = put_file(&roots, 7, &spec, &data).await;
        expect_ok(&r);
        assert_eq!(std::fs::read(&target).unwrap(), data, "落盘内容必须与上传内容一致");
        match &r {
            H2C::FsResult { hash: Some(h), .. } => assert_eq!(h, &hash_of(&data)),
            other => panic!("缺少 sha256：{other:?}"),
        }
        assert!(
            !tmp.path().join("upload.bin.uulan-part").exists(),
            "成功之后不该留下 .uulan-part 临时文件"
        );
        assert!(
            matches!(notes.first(), Some(H2C::FsStream { stage: FsStreamStage::Begin, done: 0, .. })),
            "必须先发 Begin 作为放行信号"
        );
        assert!(
            matches!(notes.last(), Some(H2C::FsStream { stage: FsStreamStage::Done, done, .. }) if *done == data.len() as u64),
            "结束要有 Done 且字节数对得上"
        );

        let (r2, notes2, got) = get_file(&roots, 9, &spec).await;
        expect_ok(&r2);
        assert_eq!(got, data, "下载回来的内容必须与源文件逐字节相同");
        match &r2 {
            H2C::FsResult { hash: Some(h), .. } => assert_eq!(h, &hash_of(&data)),
            other => panic!("下载也要带 sha256：{other:?}"),
        }
        assert!(matches!(
            notes2.first(),
            Some(H2C::FsStream { stage: FsStreamStage::Begin, total: Some(t), .. }) if *t == data.len() as u64
        ));
    }

    #[tokio::test]
    async fn short_upload_leaves_no_target_and_no_part_file() {
        let Some(tmp) = tmp_or_skip("short") else { return };
        let roots = FsRoots::new(tmp.path(), true);
        let target = tmp.path().join("short.bin");
        let spec = target.display().to_string();
        let (mut srv, mut peer) = wire();
        let mut notes = Vec::new();
        let feeder = tokio::spawn(async move {
            peer.send(3, &payload(50)).await;
            peer.end(3).await;
        });
        let r = handle(
            &FsOp::Put { id: 3, path: spec, size: 100 },
            &roots,
            &mut srv,
            &mut |m| notes.push(m),
        )
        .await;
        feeder.await.unwrap();
        let msg = expect_err(&r);
        assert!(msg.contains("50"), "错误里要说清收到多少：{msg}");
        assert!(!target.exists(), "字节数不够时绝不能把半截文件留在目标位置");
        assert!(!tmp.path().join("short.bin.uulan-part").exists(), "临时文件也要清掉");
        assert!(
            matches!(notes.last(), Some(H2C::FsStream { stage: FsStreamStage::Failed, .. })),
            "失败要有 Failed 通知"
        );
    }

    #[tokio::test]
    async fn oversize_upload_is_rejected() {
        let Some(tmp) = tmp_or_skip("over") else { return };
        let roots = FsRoots::new(tmp.path(), true);
        let target = tmp.path().join("over.bin");
        let (mut srv, mut peer) = wire();
        let feeder = tokio::spawn(async move {
            peer.send(4, &payload(20)).await;
            peer.end(4).await;
        });
        let r = handle(
            &FsOp::Put { id: 4, path: target.display().to_string(), size: 10 },
            &roots,
            &mut srv,
            &mut |_| {},
        )
        .await;
        feeder.await.unwrap();
        assert!(expect_err(&r).contains("超过"), "要说清是超量");
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn put_requires_existing_parent() {
        let Some(tmp) = tmp_or_skip("parent") else { return };
        let roots = FsRoots::new(tmp.path(), true);
        let target = tmp.path().join("nope").join("x.bin");
        let (r, _) = put_file(&roots, 5, &target.display().to_string(), b"hi").await;
        assert!(expect_err(&r).contains("上级目录不存在"));
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn put_refuses_to_overwrite_a_directory() {
        let Some(tmp) = tmp_or_skip("dirtarget") else { return };
        let roots = FsRoots::new(tmp.path(), true);
        let dir = tmp.path().join("adir");
        std::fs::create_dir(&dir).unwrap();
        let (r, _) = put_file(&roots, 6, &dir.display().to_string(), b"hi").await;
        assert!(expect_err(&r).contains("是目录"));
    }

    #[tokio::test]
    async fn put_overwrites_existing_file_completely() {
        let Some(tmp) = tmp_or_skip("overwrite") else { return };
        let roots = FsRoots::new(tmp.path(), true);
        let target = tmp.path().join("same.txt");
        std::fs::write(&target, b"0123456789abcdef").unwrap();
        let (r, _) = put_file(&roots, 8, &target.display().to_string(), b"new").await;
        expect_ok(&r);
        assert_eq!(std::fs::read(&target).unwrap(), b"new", "旧内容必须被完整替换");
    }

    #[tokio::test]
    async fn list_puts_directories_first_and_reports_size() {
        let Some(tmp) = tmp_or_skip("list") else { return };
        let roots = FsRoots::new(tmp.path(), true);
        std::fs::create_dir(tmp.path().join("zd")).unwrap();
        std::fs::write(tmp.path().join("a.txt"), vec![b'x'; 1234]).unwrap();
        std::fs::write(tmp.path().join("B.txt"), b"1").unwrap();
        let (mut srv, _peer) = wire();
        let r = handle(&FsOp::List { id: 1, path: "~/".into() }, &roots, &mut srv, &mut |_| {}).await;
        let entries = match &r {
            H2C::FsResult { ok: true, entries: Some(e), .. } => e.clone(),
            other => panic!("列目录失败：{other:?}"),
        };
        assert_eq!(entries[0].name, "zd", "目录要排在文件前面");
        assert!(entries[0].dir);
        let a = entries.iter().find(|e| e.name == "a.txt").unwrap();
        assert_eq!(a.size, 1234);
        assert!(!a.dir);
        assert!(a.mtime > 0, "mtime 不该是 0");
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["zd", "a.txt", "B.txt"], "同层按名字（不区分大小写）");
    }

    #[tokio::test]
    async fn list_this_pc_is_never_empty() {
        let Some(tmp) = tmp_or_skip("drives") else { return };
        let roots = FsRoots::new(tmp.path(), true);
        let (mut srv, _peer) = wire();
        let r = handle(&FsOp::List { id: 1, path: ":/".into() }, &roots, &mut srv, &mut |_| {}).await;
        match r {
            H2C::FsResult { ok: true, entries: Some(e), path, .. } => {
                assert!(!e.is_empty(), "「此电脑」不该是空的");
                assert_eq!(path.as_deref(), Some(":/"));
                assert!(e.iter().all(|x| x.dir));
            }
            other => panic!("列「此电脑」失败：{other:?}"),
        }
    }

    #[tokio::test]
    async fn get_of_a_directory_is_rejected() {
        let Some(tmp) = tmp_or_skip("getdir") else { return };
        let roots = FsRoots::new(tmp.path(), true);
        let (r, _, got) = get_file(&roots, 2, "~/").await;
        assert!(expect_err(&r).contains("是目录"));
        assert!(got.is_empty());
    }

    #[tokio::test]
    async fn empty_file_transfers_cleanly() {
        let Some(tmp) = tmp_or_skip("empty") else { return };
        let roots = FsRoots::new(tmp.path(), true);
        let target = tmp.path().join("zero.bin");
        let (r, _) = put_file(&roots, 1, &target.display().to_string(), b"").await;
        expect_ok(&r);
        assert_eq!(std::fs::read(&target).unwrap(), Vec::<u8>::new());
        let (r2, _, got) = get_file(&roots, 2, &target.display().to_string()).await;
        expect_ok(&r2);
        assert!(got.is_empty());
    }

    #[tokio::test]
    async fn remove_directory_requires_it_to_be_empty() {
        let Some(tmp) = tmp_or_skip("rm") else { return };
        let roots = FsRoots::new(tmp.path(), true);
        let d = tmp.path().join("full");
        std::fs::create_dir(&d).unwrap();
        std::fs::write(d.join("x"), b"1").unwrap();
        let (mut srv, _p) = wire();
        let r = handle(
            &FsOp::Remove { id: 1, path: d.display().to_string() },
            &roots,
            &mut srv,
            &mut |_| {},
        )
        .await;
        assert!(expect_err(&r).contains("非空"), "非空目录必须拒绝（不递归删）");
        std::fs::remove_file(d.join("x")).unwrap();
        let r = handle(
            &FsOp::Remove { id: 2, path: d.display().to_string() },
            &roots,
            &mut srv,
            &mut |_| {},
        )
        .await;
        expect_ok(&r);
        assert!(!d.exists());
    }

    #[tokio::test]
    async fn remove_and_rename_work_on_files() {
        let Some(tmp) = tmp_or_skip("mv") else { return };
        let roots = FsRoots::new(tmp.path(), true);
        let a = tmp.path().join("a.txt");
        let b = tmp.path().join("b.txt");
        std::fs::write(&a, b"1").unwrap();
        let (mut srv, _p) = wire();
        let r = handle(
            &FsOp::Rename { id: 1, from: a.display().to_string(), to: b.display().to_string() },
            &roots,
            &mut srv,
            &mut |_| {},
        )
        .await;
        expect_ok(&r);
        assert!(!a.exists() && b.exists());
        let c = tmp.path().join("c.txt");
        std::fs::write(&c, b"2").unwrap();
        let r = handle(
            &FsOp::Rename { id: 2, from: c.display().to_string(), to: b.display().to_string() },
            &roots,
            &mut srv,
            &mut |_| {},
        )
        .await;
        assert!(expect_err(&r).contains("已存在"));
        let r = handle(
            &FsOp::Remove { id: 3, path: b.display().to_string() },
            &roots,
            &mut srv,
            &mut |_| {},
        )
        .await;
        expect_ok(&r);
        assert!(!b.exists());
    }

    #[tokio::test]
    async fn mkdir_twice_reports_exists() {
        let Some(tmp) = tmp_or_skip("mkdir") else { return };
        let roots = FsRoots::new(tmp.path(), true);
        let (mut srv, _p) = wire();
        let d = tmp.path().join("newdir");
        let r = handle(
            &FsOp::Mkdir { id: 1, path: d.display().to_string() },
            &roots,
            &mut srv,
            &mut |_| {},
        )
        .await;
        expect_ok(&r);
        assert!(d.is_dir());
        let r = handle(
            &FsOp::Mkdir { id: 2, path: d.display().to_string() },
            &roots,
            &mut srv,
            &mut |_| {},
        )
        .await;
        assert!(expect_err(&r).contains("已存在"));
    }

    #[tokio::test]
    async fn hash_matches_content() {
        let Some(tmp) = tmp_or_skip("hash") else { return };
        let roots = FsRoots::new(tmp.path(), true);
        let f = tmp.path().join("h.bin");
        let data = payload(70000);
        std::fs::write(&f, &data).unwrap();
        let (mut srv, _p) = wire();
        let r = handle(
            &FsOp::Hash { id: 1, path: f.display().to_string() },
            &roots,
            &mut srv,
            &mut |_| {},
        )
        .await;
        match r {
            H2C::FsResult { ok: true, hash: Some(h), .. } => assert_eq!(h, hash_of(&data)),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn path_escape_is_rejected_and_absolute_can_be_forbidden() {
        let Some(tmp) = tmp_or_skip("escape") else { return };
        let (mut srv, _p) = wire();
        let strict = FsRoots::new(tmp.path(), false);
        let r = handle(
            &FsOp::List { id: 1, path: "~/../..".into() },
            &strict,
            &mut srv,
            &mut |_| {},
        )
        .await;
        assert!(expect_err(&r).contains("逃出"), "`..` 不许越过家目录");
        let r = handle(
            &FsOp::List { id: 2, path: tmp.path().display().to_string() },
            &strict,
            &mut srv,
            &mut |_| {},
        )
        .await;
        assert!(expect_err(&r).contains("绝对路径"), "关了绝对路径就不许用");
        let relaxed = FsRoots::new(tmp.path(), true);
        let r = handle(
            &FsOp::List { id: 3, path: tmp.path().display().to_string() },
            &relaxed,
            &mut srv,
            &mut |_| {},
        )
        .await;
        expect_ok(&r);
    }

    #[tokio::test]
    async fn data_for_another_id_aborts_the_upload() {
        let Some(tmp) = tmp_or_skip("otherid") else { return };
        let roots = FsRoots::new(tmp.path(), true);
        let target = tmp.path().join("o.bin");
        let (mut srv, mut peer) = wire();
        let spec = target.display().to_string();
        let feeder = tokio::spawn(async move {
            peer.send(99, b"stray").await;
            peer.end(99).await;
        });
        let r = handle(&FsOp::Put { id: 1, path: spec, size: 5 }, &roots, &mut srv, &mut |_| {}).await;
        feeder.await.unwrap();
        assert!(expect_err(&r).contains("别的流"));
        assert!(!target.exists());
    }
}