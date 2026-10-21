//! 路径解析：把协议里的 `path` 字符串变成被控端上的真实路径。
//!
//! 协议只允许三种写法（见 `docs/DESIGN.md` 5.2）：
//!
//! | 写法 | 含义 | 例 |
//! |---|---|---|
//! | `:/` | 「此电脑」——列出根 / 盘符 / 挂载点 | `:/` |
//! | `~/…` | 家目录下的相对路径 | `~/文档/a.txt` |
//! | 绝对路径 | 直接落到该路径（可被配置禁止） | `D:/data/x.bin`、`/etc/hosts` |
//!
//! 这里是**纯函数**，不碰文件系统（除了列盘符那一处）。理由和键鼠的 `Mapper` 一样：
//! 路径穿越这类 bug 必须在 `cargo test` 里就能钉住，而不是等真机上被人试出来。
//!
//! ## 「沙箱」的边界在哪（别夸张）
//!
//! - `~/` 开头的路径**不可能**通过 `..` 跑到家目录外面（这条有测试）。
//! - 绝对路径默认**是允许的** —— 远控产品（UU/向日葵）的用户预期就是能翻全盘，
//!   这不是漏洞而是产品语义；不想要就在配置里关掉 `fs_allow_absolute`。
//! - 不做符号链接解析：`canonicalize()` 会跟随链接、也会因为「目标还不存在」而失败，
//!   而我们要解析的恰恰包括「还不存在的上传目标」。所以只用词法归一化。

use std::path::{Component, Path, PathBuf};

/// 被控端允许浏览的基准。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsRoots {
    /// 家目录。`~/` 与相对路径都落在这里。
    pub home: PathBuf,
    /// 是否允许客户端直接用绝对路径。
    pub allow_absolute: bool,
}

impl FsRoots {
    pub fn new(home: impl Into<PathBuf>, allow_absolute: bool) -> Self {
        Self { home: home.into(), allow_absolute }
    }

    /// 生产环境默认：当前用户的家目录 + 允许绝对路径。
    pub fn for_current_user() -> Self {
        Self { home: home_dir(), allow_absolute: true }
    }
}

/// 解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// `:/` —— 要列的是盘符 / 挂载点，不是某个目录。
    Drives,
    /// 具体路径。
    Path(PathBuf),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PathError {
    #[error("路径为空")]
    Empty,
    #[error("路径里含 NUL 字节")]
    Nul,
    #[error("不允许访问绝对路径：{0}")]
    AbsoluteDenied(String),
    #[error("路径逃出了家目录：{0}")]
    Escape(String),
}

/// 把协议里的路径字符串解析成实际路径。
pub fn resolve(roots: &FsRoots, spec: &str) -> Result<Target, PathError> {
    if spec.is_empty() {
        return Err(PathError::Empty);
    }
    if spec.contains('\0') {
        return Err(PathError::Nul);
    }
    // 「此电脑」：`:`、`:/`、`:\` 都认（不同控制端拼出来的不一样）。
    if spec == ":" || spec == ":/" || spec == ":\\" {
        return Ok(Target::Drives);
    }
    if spec == "~" || spec == "~/" || spec == "~\\" {
        return Ok(Target::Path(roots.home.clone()));
    }
    if let Some(rest) = spec.strip_prefix("~/").or_else(|| spec.strip_prefix("~\\")) {
        // Windows 控制端可能发 `~\a\b`：折成 `/`。Linux 文件名里虽然允许反斜杠，
        // 但那种名字不会从一个 Windows 客户端发来，折了才符合意图。
        let rest = rest.replace('\\', "/");
        return Ok(Target::Path(join_within(&roots.home, Path::new(&rest)).map_err(|_| {
            PathError::Escape(spec.to_string())
        })?));
    }

    let p = Path::new(spec);
    if p.is_absolute() {
        if !roots.allow_absolute {
            return Err(PathError::AbsoluteDenied(spec.to_string()));
        }
        return Ok(Target::Path(normalize_absolute(p)));
    }
    // 相对路径相对**家目录**，不相对被控端进程的 CWD —— 后者取决于 systemd 怎么起的，
    // 拿它当基准会让「同一个路径」在两台机器上指向不同地方。
    Ok(Target::Path(
        join_within(&roots.home, p).map_err(|_| PathError::Escape(spec.to_string()))?,
    ))
}

/// 把 `rel` 接到 `base` 下面，任何 `..` 都不许越过 `base`。
pub fn join_within(base: &Path, rel: &Path) -> Result<PathBuf, PathError> {
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(s) => parts.push(s.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.pop().is_none() {
                    return Err(PathError::Escape(rel.display().to_string()));
                }
            }
            // `~/` 之后还出现根或盘符，说明这不是一个「家目录下的相对路径」。
            Component::RootDir | Component::Prefix(_) => {
                return Err(PathError::Escape(rel.display().to_string()))
            }
        }
    }
    let mut out = base.to_path_buf();
    for s in parts {
        out.push(s);
    }
    Ok(out)
}

/// 词法归一化一个绝对路径：去掉 `.`、就地消化 `..`，**永不越过根**。
pub fn normalize_absolute(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                // pop 在根上返回 false，于是 `/../x` 等于 `/x`，符合「不能越过根」。
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// 当前用户的家目录。
///
/// 不用 `dirs` crate：少一个依赖，而且这里的语义足够清楚 ——
/// Linux/麒麟看 `HOME`，Windows 看 `USERPROFILE`。
pub fn home_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(h) = std::env::var_os("USERPROFILE") {
            if !h.is_empty() {
                return PathBuf::from(h);
            }
        }
        if let (Some(d), Some(u)) = (std::env::var_os("HOMEDRIVE"), std::env::var_os("HOMEPATH")) {
            let mut p = PathBuf::from(d);
            p.push(u);
            return p;
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(h) = std::env::var_os("HOME") {
            if !h.is_empty() {
                return PathBuf::from(h);
            }
        }
    }
    PathBuf::from(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots(home: &str, allow_absolute: bool) -> FsRoots {
        FsRoots::new(home, allow_absolute)
    }

    #[test]
    fn colon_means_this_pc() {
        let r = roots("/home/u", true);
        assert_eq!(resolve(&r, ":/").unwrap(), Target::Drives);
        assert_eq!(resolve(&r, ":").unwrap(), Target::Drives);
    }

    #[test]
    fn tilde_is_home_and_tilde_slash_joins() {
        let r = roots("/home/u", true);
        assert_eq!(resolve(&r, "~").unwrap(), Target::Path(PathBuf::from("/home/u")));
        assert_eq!(
            resolve(&r, "~/文档/a.txt").unwrap(),
            Target::Path(PathBuf::from("/home/u/文档/a.txt"))
        );
    }

    #[test]
    fn tilde_cannot_escape_home() {
        let r = roots("/home/u", true);
        assert_eq!(
            resolve(&r, "~/../../etc/passwd").unwrap_err(),
            PathError::Escape("~/../../etc/passwd".into())
        );
        // 但「进去再出来」是合法的：等价于 ~/b
        assert_eq!(
            resolve(&r, "~/a/../b").unwrap(),
            Target::Path(PathBuf::from("/home/u/b"))
        );
    }

    #[test]
    fn relative_paths_are_relative_to_home_not_cwd() {
        let r = roots("/home/u", true);
        assert_eq!(
            resolve(&r, "docs/x.md").unwrap(),
            Target::Path(PathBuf::from("/home/u/docs/x.md"))
        );
        assert_eq!(
            resolve(&r, "../etc").unwrap_err(),
            PathError::Escape("../etc".into())
        );
    }

    #[test]
    fn absolute_is_allowed_or_denied_by_config() {
        let abs = if cfg!(windows) { "C:/Windows/x" } else { "/etc/hosts" };
        let ok = resolve(&roots("/home/u", true), abs).unwrap();
        assert_eq!(ok, Target::Path(normalize_absolute(Path::new(abs))));
        assert_eq!(
            resolve(&roots("/home/u", false), abs).unwrap_err(),
            PathError::AbsoluteDenied(abs.into())
        );
    }

    #[test]
    fn absolute_is_normalized_lexically() {
        let p = if cfg!(windows) { "C:/a/./b/../c" } else { "/a/./b/../c" };
        let want = if cfg!(windows) { "C:\\a\\c" } else { "/a/c" };
        assert_eq!(
            normalize_absolute(Path::new(p)),
            PathBuf::from(want),
            "词法归一化后应当是 {want}"
        );
    }

    #[test]
    fn parent_beyond_root_is_clamped_not_escaped() {
        let p = if cfg!(windows) { "C:/../x" } else { "/../x" };
        let want = if cfg!(windows) { "C:\\x" } else { "/x" };
        assert_eq!(normalize_absolute(Path::new(p)), PathBuf::from(want));
    }

    #[test]
    fn empty_and_nul_are_rejected() {
        let r = roots("/home/u", true);
        assert_eq!(resolve(&r, "").unwrap_err(), PathError::Empty);
        assert_eq!(resolve(&r, "~/a\0b").unwrap_err(), PathError::Nul);
    }

    #[test]
    fn names_with_spaces_and_dots_are_kept() {
        let r = roots("/home/u", true);
        assert_eq!(
            resolve(&r, "~/a b/..hidden").unwrap(),
            Target::Path(PathBuf::from("/home/u/a b/..hidden"))
        );
    }

    #[test]
    fn windows_style_tilde_separator_works() {
        let r = roots("/home/u", true);
        assert_eq!(
            resolve(&r, "~\\a\\b").unwrap(),
            Target::Path(PathBuf::from("/home/u/a/b")),
            "控制端可能是 Windows，`~\\` 也要认"
        );
    }

    #[test]
    fn home_dir_is_not_empty() {
        assert!(!home_dir().as_os_str().is_empty());
    }
}