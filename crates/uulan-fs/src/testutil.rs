//! 测试辅助：建一个**真的能写**的临时目录。
//!
//! 为什么不直接用 `std::env::temp_dir()`：这台开发机（和某些被安全软件接管的工作区）
//! 会拒绝「位于 `D:\default-workspace\` 之下的 exe」写任何文件，连 `%TEMP%` 也写不了
//! —— 见 skill `uulan-remote-dev` 的坑 2。硬断言会让环境一变就是一片假红，
//! 所以统一走这里：逐个候选目录**真的写一个文件再删掉**，第一个能写的就用。
//!
//! 全都不可写时返回 `None`，调用方打印「跳过」并让测试通过 ——
//! 环境问题不该伪装成功能坏了。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// 进程结束时删掉的临时目录。
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 在 `name` 子目录下建一个唯一目录。`None` = 所有候选位置都不可写。
    pub fn new(tag: &str) -> Option<TempDir> {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let uniq = format!(
            "uulan-{tag}-{}-{}-{}",
            std::process::id(),
            n,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        for base in candidates() {
            if !writable(&base) {
                continue;
            }
            let path = base.join(&uniq);
            if std::fs::create_dir_all(&path).is_err() {
                continue;
            }
            if !writable(&path) {
                let _ = std::fs::remove_dir_all(&path);
                continue;
            }
            return Some(TempDir { path });
        }
        None
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    for k in ["UULAN_TEST_TMP", "TMPDIR", "TEMP", "TMP"] {
        if let Some(p) = std::env::var_os(k) {
            if !p.is_empty() {
                v.push(PathBuf::from(p));
            }
        }
    }
    for k in ["USERPROFILE", "HOME"] {
        if let Some(p) = std::env::var_os(k) {
            if !p.is_empty() {
                v.push(PathBuf::from(p).join("uulan-test-tmp"));
            }
        }
    }
    v.push(PathBuf::from(if cfg!(windows) { "C:/uulan-test-tmp" } else { "/tmp/uulan-test-tmp" }));
    v
}

/// 真的写一个文件再删掉 —— 只 `create_dir_all` 不足以判断（目录可能已存在而写入仍被拒）。
pub fn writable(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(format!(".probe-{}", SEQ.fetch_add(1, Ordering::Relaxed)));
    match std::fs::write(&probe, b"ok") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}