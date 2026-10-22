//! 真机剪贴板探针：用 `uulan-clip` 的 X11 后端跟**外部** X 客户端换一次剪贴板。
//!
//! 为什么需要它：`verify-clipboard-e2e.sh` 在本机验的是 Win32 那一侧；X11 侧
//! （SelectionRequest / 常驻 owner 线程 / UTF8_STRING / INCR）只有在真机上、
//! 且对面是**别的程序**时才算验过。自己做两端互相读写的「自证」不算 ——
//! 两边都错也能对上。
//!
//! 用法（麒麟上，DISPLAY 指向真桌面；另起一个外部客户端当判据）：
//!
//! ```text
//! # 1) 外部客户端持有 CLIPBOARD
//! timeout 30 ./xclip-or set "ORACLE-OWNED-7f3a" &
//! # 2) 探针：先读它（考验我们的 requestor 侧），再写（考验我们的 owner 侧）
//! #    写完会保持持有 N 秒（第二个参数，默认 8），好让外部客户端这期间读回
//! DISPLAY=:0 ./x11_clip_probe PROBE-WRITE-42 8
//! # 3) 探针还 hold 着的时候，外部客户端读回来
//! ./xclip-or get
//! ```
//!
//! `RUST_LOG=debug` 能看到「读剪贴板超时」「所有者拒绝 UTF8_STRING」这类内部状态 ——
//! 没有它，`None` 到底是「对面拒绝」还是「我们没等到」就分不清（这个区别决定了
//! 该改请求侧还是应答侧）。

fn main() {
    #[cfg(target_os = "linux")]
    linux_main();
    #[cfg(not(target_os = "linux"))]
    println!("x11_clip_probe 只在 Linux/X11 上有意义（当前平台跳过）");
}

#[cfg(target_os = "linux")]
fn linux_main() {
    use std::time::Duration;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let text = args.next().unwrap_or_else(|| "PROBE-WRITE".to_string());
    let hold: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(8);

    let mut c = uulan_clip::default_clipboard(None);
    println!("backend={} available={}", c.name(), c.available());
    match c.get_text() {
        Some(t) => println!("GET-BEFORE len={} <<<{}>>>", t.chars().count(), t),
        None => println!("GET-BEFORE none"),
    }
    match c.set_text(&text) {
        Ok(()) => println!("SET-OK <<<{text}>>>"),
        Err(e) => println!("SET-ERR {e}"),
    }
    // 保持持有：owner 线程活着才能应答别人的 SelectionRequest。
    println!("HOLDING {hold}s（这期间外部客户端可以读回）");
    for i in 0..hold {
        std::thread::sleep(Duration::from_secs(1));
        match c.get_text() {
            Some(t) => println!("  tick {i}: get_text len={}", t.chars().count()),
            None => println!("  tick {i}: get_text none"),
        }
    }
    println!("DONE");
}