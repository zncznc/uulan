//! 文件系统探针 —— 诊断「这台机器上可执行文件能不能写文件」。
//!
//! 起因：本机（Windows 开发机）上，**位于 `D:/default-workspace/` 下的 exe 无法写任何文件**，
//! 一律 `Os error 5 拒绝访问`；把同一个 exe 拷到 `D:/` 下跑就完全正常。
//! 这是目录级的执行策略，不是 cargo、也不是本项目代码的问题 —— 但它会让
//! 「cargo test / cargo run」看到的写盘行为完全失真，白白浪费半天。
//!
//! 用法：
//! ```bash
//! cargo run -q -p uulan-host --example fs_probe
//! cp target/debug/examples/fs_probe.exe /d/fs_probe.exe && /d/fs_probe.exe   # 对照
//! ```
//! 两条输出一对比，立刻能分清「代码写错了」还是「环境不让写」。

fn main() {
    for p in [r"D:\uulan-probe\cargo-a", r"D:\uulan-probe\cargo-a\b"] {
        println!("create_dir_all({p}) -> {:?}", std::fs::create_dir_all(p));
    }
    let f = r"D:\uulan-probe\cargo.txt";
    println!("write({f}) -> {:?}", std::fs::write(f, "hi"));
    let t = r"D:\uulan-probe\cargo-renamed.txt";
    let _ = std::fs::remove_file(t);
    println!("rename -> {t} => {:?}", std::fs::rename(f, t));
    let td = std::env::temp_dir().join("uulan-cargo-probe");
    println!("create_dir_all({}) -> {:?}", td.display(), std::fs::create_dir_all(&td));
    let _ = std::fs::remove_dir_all(&td);
}
