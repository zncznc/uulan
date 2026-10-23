//! 链接期自备 libX11 / libXtst 的 `.so` 名字 —— 不需要 root、不改系统目录。
//!
//! 为什么需要：银河麒麟 V10 SP1 的源里 `libx11-dev` **装不上**
//! （dev 包依赖 `libx11-6 (= 2:1.6.9-2kylin1.2)`，系统里是 `2:1.6.9-2kylin1.6`，
//! dev 比运行时落后一个 revision，apt 一律「无法修正错误」）。缺了 dev 包就没有
//! `/usr/lib/x86_64-linux-gnu/libX11.so`（只有 `libX11.so.6`）、没有 `x11.pc`、
//! 没有 `X11/*.h`。而 `src/x11.rs` 里是 `#[link(name = "X11")]`，链接器要的是
//! 一个叫 `libX11.so` 的文件 —— 于是编不出被控端。
//!
//! 以前的做法是 `sudo ln -s libX11.so.6 libX11.so`（现场要 root 口令）。这里改成
//! 构建期在 `OUT_DIR` 里造一个私有目录，把 `libX11.so.6` 软链成 `libX11.so`，
//! 再用 `cargo:rustc-link-search` 把它排到系统目录前面 —— 普通用户就能编。
//! 系统里本来就有 `.so`（正常发行版）时什么都不做。
//!
//! 与 `crates/uulan-clip/build.rs` 是同一份逻辑，区别只在本 crate 还要 XTest
//! （`input/x11.rs` 用 `#[link(name = "Xtst")]` 做键鼠注入）。（两个 crate 各自需要，
//! 没有共享 build-dependency 的地方，只能各放一份，改的时候两边一起改）。
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    shim_x11_libs(&["X11", "Xtst"]);
}

/// 非 Linux 构建机：Win32 走 user32/kernel32，系统库齐全，什么都不做。
#[cfg(not(target_os = "linux"))]
fn shim_x11_libs(_want: &[&str]) {}

/// 在私有链接目录里补 `lib<name>.so`，并把它加进 link search。
#[cfg(target_os = "linux")]
fn shim_x11_libs(want: &[&str]) {
    use std::path::{Path, PathBuf};

    // 构建机是 Linux 不代表目标也是 Linux（交叉编 Windows 时仍然不该补软链）。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }

    let mut dirs: Vec<PathBuf> = Vec::new();
    // 现场可以覆盖：多架构目录名各发行版不同（linux-gnu / linux-gnuabi64）。
    for var in ["UULAN_X11_LIBDIR", "LIBRARY_PATH"] {
        if let Ok(v) = std::env::var(var) {
            for d in v.split(':').filter(|s| !s.is_empty()) {
                dirs.push(PathBuf::from(d));
            }
        }
    }
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    for d in [
        format!("/usr/lib/{arch}-linux-gnu"),
        format!("/lib/{arch}-linux-gnu"),
        "/usr/lib64".to_string(),
        "/lib64".to_string(),
        "/usr/lib".to_string(),
        "/lib".to_string(),
    ] {
        dirs.push(PathBuf::from(d));
    }

    // 找一个带版本号的运行库（libX11.so.6 这种）。
    fn versioned(dir: &Path, name: &str) -> Option<PathBuf> {
        let prefix = format!("lib{name}.so.");
        let rd = std::fs::read_dir(dir).ok()?;
        let mut best: Option<(u32, PathBuf)> = None;
        for e in rd.flatten() {
            let f = e.file_name().to_string_lossy().to_string();
            if let Some(rest) = f.strip_prefix(&prefix) {
                if let Some(major) = rest.split('.').next().and_then(|s| s.parse::<u32>().ok()) {
                    if best.as_ref().map(|(m, _)| major > *m).unwrap_or(true) {
                        best = Some((major, dir.join(&f)));
                    }
                }
            }
        }
        best.map(|(_, p)| p)
    }

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let shim = out.join("x11-link");
    let mut needed = false;

    for name in want {
        let plain = format!("lib{name}.so");
        if dirs.iter().any(|d| d.join(&plain).exists()) {
            continue; // 系统里的 dev 软链在，正常链接即可
        }
        let Some(target) = dirs.iter().find_map(|d| versioned(d, name)) else {
            println!("cargo:warning=找不到 lib{name}.so.N，链接可能会失败（装 lib{name}-dev，或用 UULAN_X11_LIBDIR 指定目录）");
            continue;
        };
        if std::fs::create_dir_all(&shim).is_err() {
            continue;
        }
        let link = shim.join(&plain);
        let _ = std::fs::remove_file(&link);
        if std::os::unix::fs::symlink(&target, &link).is_ok() {
            println!("cargo:warning=lib{name}-dev 缺失，已把 {} 软链成私有链接目录里的 {plain}", target.display());
            needed = true;
        }
    }

    if needed {
        println!("cargo:rustc-link-search=native={}", shim.display());
    }
}