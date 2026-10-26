//! 目录树传输（M4 文件传输的第三档：单文件 → 整棵树）。
//!
//! `--put` / `--get` 一次只搬一个文件，现场要往被控端送一整个目录（工具包、驱动、
//! 部署包）时只能自己写循环 —— 于是「谁去建远端那些子目录」「某个文件失败了要不要停」
//! 「空目录怎么过去」全落到用户身上。这个模块把那套循环固化下来：
//!
//! ```text
//! --put-tree <本地目录> <远程目录>    整棵本地树 → 被控端
//! --get-tree <远程目录> <本地目录>    整棵远端树 → 本机
//! ```
//!
//! ## 三条设计约束
//!
//! 1. **协议一个字都没变**：树传输就是「一串串行的 Mkdir / Put / Get / List」。
//!    被控端不知道也不关心对面在搬一棵树，所以不用升版本、不用两端同批换。
//! 2. **先规划、再动手**：上传前先把本地树走一遍，把「目录 N / 文件 M / 字节 X /
//!    跳过 K」打出来，然后才开始建目录、发数据。用户看到的第一行就是总量，
//!    传一半断了也能立刻判断缺多少。
//! 3. **一个文件不完整就不算传完**：每个文件仍然走 `.uulan-part` + sha256 校验
//!    （与单文件传输同一套语义），所以「目标文件存在」永远等于「内容完整」。
//!
//! ## 符号链接：跳过，不跟随
//!
//! 跟随链接会把树变成图：指回上级目录的链接能让上传无限循环，指到 `/etc` 的链接
//! 会把被控端的系统文件搬过来。所以本地符号链接**一律跳过并计数**，
//! 远端条目名里带分隔符（`/`、`\`）或就是 `..` 的也一律跳过 —— 那是被控端返回的、
//! 不能全信的名字，直接拿它拼本地路径等于把写入点交给对面。

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use uulan_protocol::bulk::{BulkKind, CHUNK};
use uulan_protocol::msg::{C2H, FsEntry, FsOp};

use crate::fsx::{part_path, Step};
use crate::fsx::{Conn, Ctl};

/// 层数上限。符号链接被跳过之后正常目录树到不了这个深度；
/// 留着是为了「远端的目录其实是个指向祖先的链接」这种情况下也能停下来。
const MAX_DEPTH: usize = 64;

/// 规划中的一项。顺序就是执行顺序（目录先于它的内容）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Dir(String),
    File { rel: String, size: u64 },
}

/// 一棵本地树的传输计划。`rel` 一律用 `/` 分隔（与协议里的远程路径同形）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub items: Vec<Item>,
    pub dirs: usize,
    pub files: usize,
    pub bytes: u64,
    /// 跳过的符号链接数。
    pub links: usize,
    /// 跳过的其它特殊文件（fifo / socket / 设备）。
    pub special: usize,
}

impl Plan {
    /// 人看的一句话总量。日志里那一行就是它。
    pub fn summary(&self) -> String {
        format!(
            "目录 {} / 文件 {} / 共 {} 字节 / 跳过符号链接 {}",
            self.dirs, self.files, self.bytes, self.links
        )
    }
}

/// 走一遍本地目录树，产出传输计划。**不写任何东西**，所以在动手前就能打印总量。
pub fn plan_local(root: &Path) -> Result<Plan> {
    let md = std::fs::metadata(root)
        .with_context(|| format!("本地目录读不到：{}", root.display()))?;
    if !md.is_dir() {
        bail!("{} 不是目录（整棵树只支持目录）", root.display());
    }
    let mut plan = Plan::default();
    walk(root, "", 0, &mut plan)?;
    Ok(plan)
}

fn walk(dir: &Path, rel: &str, depth: usize, plan: &mut Plan) -> Result<()> {
    if depth > MAX_DEPTH {
        bail!("目录层数超过 {MAX_DEPTH} 层（{}），疑似环状结构", dir.display());
    }
    // 排序后再走：同一个目录树无论在哪台机器上都产出同一份计划，
    // 比对两次传输的日志才有意义。
    let mut names: Vec<std::ffi::OsString> = std::fs::read_dir(dir)
        .with_context(|| format!("列本地目录失败：{}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name())
        .collect();
    names.sort();

    for name in names {
        let shown = name.to_string_lossy().to_string();
        if safe_name(&shown).is_none() {
            // 本地读出来的名字不会含分隔符，这里只是防御（Windows 的保留名等）。
            plan.special += 1;
            continue;
        }
        let path = dir.join(&name);
        let child = join_rel(rel, &shown);
        // symlink_metadata：不跟随链接，才能认出「这是个链接」而不是「它指向的东西」。
        let smd = std::fs::symlink_metadata(&path)
            .with_context(|| format!("读元数据失败：{}", path.display()))?;
        let ft = smd.file_type();
        if ft.is_symlink() {
            plan.links += 1;
        } else if ft.is_dir() {
            plan.items.push(Item::Dir(child.clone()));
            plan.dirs += 1;
            walk(&path, &child, depth + 1, plan)?;
        } else if ft.is_file() {
            plan.items.push(Item::File { rel: child, size: smd.len() });
            plan.files += 1;
            plan.bytes += smd.len();
        } else {
            plan.special += 1;
        }
    }
    Ok(())
}

/// 一个条目名能不能安全地当路径的一段。
///
/// 被控端回给我们的名字是**对面的数据**：拼本地路径之前必须挡掉
/// `..`、绝对路径、带分隔符的名字 —— 否则 `--get-tree` 就成了「对面往哪写都行」。
pub fn safe_name(name: &str) -> Option<&str> {
    if name.is_empty() || name == "." || name == ".." || name == "/" {
        return None;
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return None;
    }
    Some(name)
}

/// 相对路径拼接（`rel` 用 `/`）。
pub fn join_rel(base: &str, name: &str) -> String {
    if base.is_empty() {
        name.to_string()
    } else {
        format!("{base}/{name}")
    }
}

/// 把相对路径接到一个**协议路径**后面：`~/dst` + `a/b` → `~/dst/a/b`。
///
/// 注意 `:/`（此电脑）不能当目录树的根 —— 它是「盘符列表」这个虚拟节点，
/// 不是目录。早一点报错，比传了三个文件再断在半路好。
pub fn join_remote(base: &str, rel: &str) -> Result<String> {
    if base == ":" || base == ":/" || base == ":\\" {
        bail!("「此电脑」（`:/`）不是目录，目录树的根要写具体路径（如 `~/dst`）");
    }
    let trimmed = base.trim_end_matches(['/', '\\']);
    if rel.is_empty() {
        return Ok(if trimmed.is_empty() { base.to_string() } else { trimmed.to_string() });
    }
    if trimmed.is_empty() {
        return Ok(format!("/{rel}"));
    }
    // 盘符根 `D:/` 去掉斜杠会变成 `D:`，拼起来会变成 `D:a` —— 补回斜杠。
    if trimmed.ends_with(':') {
        return Ok(format!("{trimmed}/{rel}"));
    }
    Ok(format!("{trimmed}/{rel}"))
}

/// 请求 id 的分配器。控制通道与 bulk 流共用一套编号，
/// 所以在同一条会话里「每条请求一个 id」，不复用 —— 复用会碰上
/// 被控端那边「id 还被上一条流占着」的窗口（`mux.register` 会返回 None）。
struct Ids {
    next: u32,
}

impl Ids {
    fn new() -> Self {
        // 从 2 开始：1 留给「单文件语义」的老路径，混在一起时日志更好读。
        Self { next: 2 }
    }
    fn take(&mut self) -> u32 {
        let id = self.next;
        self.next = self.next.wrapping_add(1).max(2);
        id
    }
}

/// 列一个远端目录。失败就是失败（不是「空目录」）—— 空目录是 `Ok(vec![])`。
async fn list_remote(ctl: &mut Ctl, id: u32, spec: &str) -> Result<Vec<FsEntry>> {
    ctl.send(&C2H::Fs(FsOp::List { id, path: spec.to_string() })).await?;
    match ctl.wait_result(id).await? {
        Step::Result { ok: true, entries, .. } => Ok(entries.unwrap_or_default()),
        Step::Result { ok: false, error, .. } => {
            bail!("列目录失败（{spec}）：{}", error.unwrap_or_default())
        }
        other => bail!("列目录时收到意外消息（{spec}）：{other:?}"),
    }
}

/// 确保远端目录存在（幂等）。
///
/// 不去匹配「已存在」这句中文错误串 —— 那种判据换个措辞就失效。
/// 判据是**再列一次**：能列出来就是目录，列不出来才是真失败。
async fn ensure_remote_dir(ctl: &mut Ctl, ids: &mut Ids, spec: &str) -> Result<()> {
    if list_remote(ctl, ids.take(), spec).await.is_ok() {
        return Ok(());
    }
    let id = ids.take();
    ctl.send(&C2H::Fs(FsOp::Mkdir { id, path: spec.to_string() })).await?;
    let err = match ctl.wait_result(id).await? {
        Step::Result { ok: true, .. } => return Ok(()),
        Step::Result { ok: false, error, .. } => error.unwrap_or_default(),
        other => bail!("建目录时收到意外消息（{spec}）：{other:?}"),
    };
    // 竞态或「已经有了但是个别名」：再列一次定胜负。
    if list_remote(ctl, ids.take(), spec).await.is_ok() {
        return Ok(());
    }
    bail!("建目录失败（{spec}）：{err}")
}

/// 上传一个文件，返回写入的字节数。内容一致性由被控端回传的 sha256 保证。
async fn upload_one(
    ctl: &mut Ctl,
    peer: &mut Conn,
    ids: &mut Ids,
    local: &Path,
    remote: &str,
) -> Result<u64> {
    let size = std::fs::metadata(local)
        .with_context(|| format!("本地文件读不到：{}", local.display()))?
        .len();
    let id = ids.take();
    ctl.send(&C2H::Fs(FsOp::Put { id, path: remote.to_string(), size })).await?;
    match ctl.wait(id).await? {
        // Begin 是被控端的放行信号：它已经校验过路径、建好临时文件了。
        Step::Begin { .. } => {}
        Step::Result { ok: false, error, .. } => {
            bail!("上传失败（{remote}）：{}", error.unwrap_or_default())
        }
        Step::Failed { done } => bail!("被控端中止接收（{remote}，已发 {done} 字节）"),
        other => bail!("被控端没有准备好接收（{remote}）：{other:?}"),
    }

    let mut f = tokio::fs::File::open(local).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        peer.send_data(id, &buf[..n]).await?;
    }
    peer.send_end(id).await?;

    match ctl.wait_result(id).await? {
        Step::Result { ok: true, hash, .. } => {
            if let Some(want) = hash {
                let mine = hex(&hasher.finalize());
                if want != mine {
                    bail!(
                        "上传后校验不一致（{remote}）：被控端 {} / 本地 {}",
                        short(&want),
                        short(&mine)
                    );
                }
            }
            Ok(size)
        }
        Step::Result { ok: false, error, .. } => {
            bail!("上传失败（{remote}）：{}", error.unwrap_or_default())
        }
        other => bail!("上传收尾时收到意外消息（{remote}）：{other:?}"),
    }
}

/// 下载一个文件到 `local`（先写 `.uulan-part`，校验通过才改名）。返回字节数。
async fn download_one(
    ctl: &mut Ctl,
    peer: &mut Conn,
    ids: &mut Ids,
    remote: &str,
    local: &Path,
) -> Result<u64> {
    let id = ids.take();
    ctl.send(&C2H::Fs(FsOp::Get { id, path: remote.to_string() })).await?;
    let total = match ctl.wait(id).await? {
        Step::Begin { total } => total,
        Step::Result { ok: false, error, .. } => {
            bail!("下载失败（{remote}）：{}", error.unwrap_or_default())
        }
        Step::Failed { done } => bail!("被控端中止了发送（{remote}，已收 {done} 字节）"),
        other => bail!("被控端没有开始发数据（{remote}）：{other:?}"),
    };

    let part = part_path(local);
    if let Some(dir) = part.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let mut f = tokio::fs::File::create(&part)
        .await
        .with_context(|| format!("建本地文件失败：{}", part.display()))?;
    let mut hasher = Sha256::new();
    let mut got: u64 = 0;
    while let Some(bf) = peer.recv().await? {
        if bf.id != id {
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

    let hash = match ctl.wait_result(id).await? {
        Step::Result { ok: true, hash, .. } => hash,
        Step::Result { ok: false, error, .. } => {
            let _ = tokio::fs::remove_file(&part).await;
            bail!("下载失败（{remote}）：{}", error.unwrap_or_default())
        }
        other => {
            let _ = tokio::fs::remove_file(&part).await;
            bail!("下载收尾时收到意外消息（{remote}）：{other:?}")
        }
    };
    let mine = hex(&hasher.finalize());
    if let Some(want) = hash {
        if want != mine {
            let _ = tokio::fs::remove_file(&part).await;
            bail!(
                "校验失败（{remote}）：被控端 sha256={} 本地={}，已丢弃",
                short(&want),
                short(&mine)
            );
        }
    }
    if let Some(t) = total {
        if got != t {
            let _ = tokio::fs::remove_file(&part).await;
            bail!("收到 {got} 字节，被控端声明 {t} 字节，已丢弃（{remote}）");
        }
    }
    tokio::fs::rename(&part, local)
        .await
        .with_context(|| format!("改名到 {} 失败", local.display()))?;
    Ok(got)
}

/// `--put-tree <本地目录> <远程目录>`
pub(crate) async fn put_tree(ctl: &mut Ctl, peer: &mut Conn, root: PathBuf, remote: String) -> Result<()> {
    let started = std::time::Instant::now();
    let plan = plan_local(&root)?;
    if plan.items.is_empty() {
        println!("本地树是空的（只有空目录或全是符号链接），没有要传的文件");
    }
    println!(
        "上传目录树 {} → {}：{}",
        root.display(),
        remote,
        plan.summary()
    );
    if plan.special > 0 {
        println!("  跳过特殊文件（fifo/socket/设备）{} 个", plan.special);
    }

    let mut ids = Ids::new();
    ensure_remote_dir(ctl, &mut ids, &remote)
        .await
        .with_context(|| format!("准备远端根目录失败：{remote}"))?;

    let mut sent: u64 = 0;
    for item in &plan.items {
        match item {
            Item::Dir(rel) => {
                let spec = join_remote(&remote, rel)?;
                ensure_remote_dir(ctl, &mut ids, &spec).await?;
                println!("  + {rel}/");
            }
            Item::File { rel, .. } => {
                let local = root.join(rel.split('/').collect::<PathBuf>());
                let spec = join_remote(&remote, rel)?;
                let n = upload_one(ctl, peer, &mut ids, &local, &spec)
                    .await
                    .with_context(|| format!("上传文件失败：{rel}"))?;
                sent += n;
                println!("  ^ {rel}（{n} 字节）");
            }
        }
    }
    println!(
        "目录树上传完成：目录 {} / 文件 {} / 写入 {} 字节 / 跳过符号链接 {} / 用时 {:.1}s",
        plan.dirs,
        plan.files,
        sent,
        plan.links,
        started.elapsed().as_secs_f64()
    );
    if plan.files > 0 && sent == 0 {
        bail!("一个字节都没写进去（{} 个文件全是 0 字节？）", plan.files);
    }
    Ok(())
}

/// `--get-tree <远程目录> <本地目录>`
pub(crate) async fn get_tree(ctl: &mut Ctl, peer: &mut Conn, remote: String, local: PathBuf) -> Result<()> {
    let started = std::time::Instant::now();
    let mut ids = Ids::new();
    // 先列一次根：远端根不存在时立刻报错，而不是先在本地建出一串空目录。
    let first = list_remote(ctl, ids.take(), &remote)
        .await
        .with_context(|| format!("远端目录读不到：{remote}"))?;
    std::fs::create_dir_all(&local)
        .with_context(|| format!("建本地目录失败：{}", local.display()))?;

    let mut dirs = 0usize;
    let mut files = 0usize;
    let mut bytes = 0u64;
    let mut skipped = 0usize;

    // 广度优先：先建完一层目录再搬文件，出错时本地留下的是一棵「形状完整、内容缺」的树，
    // 比一条深深的半截路径好排查。
    let mut queue: std::collections::VecDeque<(String, PathBuf, usize, Vec<FsEntry>)> =
        std::collections::VecDeque::new();
    queue.push_back((remote.clone(), local.clone(), 0, first));

    while let Some((spec, ldir, depth, entries)) = queue.pop_front() {
        if depth > MAX_DEPTH {
            bail!("远端目录超过 {MAX_DEPTH} 层（{spec}），停止（疑似链接成环）");
        }
        for e in entries {
            let Some(name) = safe_name(&e.name) else {
                println!("  ! 跳过可疑条目名：{:?}（{spec}）", e.name);
                skipped += 1;
                continue;
            };
            let child_spec = join_remote(&spec, name)?;
            if e.dir {
                let child_local = ldir.join(name);
                std::fs::create_dir_all(&child_local)
                    .with_context(|| format!("建本地目录失败：{}", child_local.display()))?;
                let sub = list_remote(ctl, ids.take(), &child_spec).await?;
                dirs += 1;
                println!("  + {child_spec}/");
                queue.push_back((child_spec, child_local, depth + 1, sub));
            } else {
                let dst = ldir.join(name);
                let n = download_one(ctl, peer, &mut ids, &child_spec, &dst)
                    .await
                    .with_context(|| format!("下载文件失败：{child_spec}"))?;
                files += 1;
                bytes += n;
                println!("  v {child_spec}（{n} 字节）");
            }
        }
    }
    println!(
        "目录树下载完成：目录 {dirs} / 文件 {files} / 共 {bytes} 字节 / 跳过可疑条目 {skipped} / 用时 {:.1}s",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

fn hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

fn short(h: &str) -> &str {
    &h[..h.len().min(12)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_or_skip(tag: &str) -> Option<uulan_fs::testutil::TempDir> {
        let t = uulan_fs::testutil::TempDir::new(tag);
        if t.is_none() {
            eprintln!("跳过：这台机器没有可写目录");
        }
        t
    }

    #[test]
    fn plan_walks_directories_before_their_contents() {
        let Some(tmp) = tmp_or_skip("tree-plan") else { return };
        let root = tmp.path().join("src");
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("top.txt"), b"hello").unwrap();
        std::fs::write(root.join("a/one.bin"), vec![7u8; 1000]).unwrap();
        std::fs::write(root.join("a/b/two.bin"), b"").unwrap();

        let plan = plan_local(&root).unwrap();
        assert_eq!(plan.dirs, 2);
        assert_eq!(plan.files, 3);
        assert_eq!(plan.bytes, 1005);
        // 深度优先 + 目录先于内容：Dir(a) 之后立刻是 a 下面的东西，
        // 同级按名字排序（b 在 one.bin 前）。计划顺序就是执行顺序。
        assert_eq!(
            plan.items,
            vec![
                Item::Dir("a".into()),
                Item::Dir("a/b".into()),
                Item::File { rel: "a/b/two.bin".into(), size: 0 },
                Item::File { rel: "a/one.bin".into(), size: 1000 },
                Item::File { rel: "top.txt".into(), size: 5 },
            ]
        );
    }

    #[test]
    fn plan_counts_empty_dirs_so_they_still_get_created() {
        let Some(tmp) = tmp_or_skip("tree-empty") else { return };
        let root = tmp.path().join("s");
        std::fs::create_dir_all(root.join("empty/deeper")).unwrap();
        let plan = plan_local(&root).unwrap();
        assert_eq!(plan.files, 0);
        assert_eq!(plan.dirs, 2);
        assert!(matches!(plan.items[0], Item::Dir(_)));
    }

    #[test]
    fn plan_rejects_files_and_missing_paths() {
        let Some(tmp) = tmp_or_skip("tree-reject") else { return };
        let f = tmp.path().join("x.bin");
        std::fs::write(&f, b"x").unwrap();
        assert!(plan_local(&f).is_err(), "文件不该被当成树");
        assert!(plan_local(&tmp.path().join("nope")).is_err());
    }

    #[test]
    fn remote_join_keeps_the_shape_of_every_spec_style() {
        assert_eq!(join_remote("~/dst", "a/b").unwrap(), "~/dst/a/b");
        assert_eq!(join_remote("~/dst/", "a").unwrap(), "~/dst/a");
        assert_eq!(join_remote("~", "a").unwrap(), "~/a");
        assert_eq!(join_remote("/var/tmp/x", "a").unwrap(), "/var/tmp/x/a");
        assert_eq!(join_remote("/", "a").unwrap(), "/a");
        assert_eq!(join_remote("D:/data", "a").unwrap(), "D:/data/a");
        assert_eq!(join_remote("D:\\data", "a").unwrap(), "D:\\data/a");
        assert_eq!(join_remote("D:/", "a").unwrap(), "D:/a");
        assert_eq!(join_remote("~/dst", "").unwrap(), "~/dst");
        assert!(join_remote(":/", "a").is_err(), "此电脑不能当根");
    }

    #[test]
    fn unsafe_entry_names_are_refused() {
        for bad in ["", ".", "..", "a/b", "a\\b", "x\0y"] {
            assert!(safe_name(bad).is_none(), "{bad:?} 应该被拒");
        }
        for ok in ["a", "文档", "x.bin", "..."] {
            assert_eq!(safe_name(ok), Some(ok));
        }
    }

    #[test]
    fn plan_summary_is_one_greppable_line() {
        let p = Plan { dirs: 2, files: 3, bytes: 1005, links: 1, special: 0, items: vec![] };
        assert_eq!(p.summary(), "目录 2 / 文件 3 / 共 1005 字节 / 跳过符号链接 1");
    }
}