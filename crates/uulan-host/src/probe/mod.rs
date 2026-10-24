//! 系统指标与「锁屏 / 有没有桌面」的探测。
//!
//! 这两样东西在协议里早就定义好了（`H2C::Metrics` / `H2C::SystemState`），但被控端
//! **一条都没发过** —— 于是控制端既看不到对面的负载，也不知道对面已经锁屏，
//! 还会继续把键鼠往锁屏里送（往锁屏里打字可能把口令打进别的窗口，也可能被锁屏吞掉）。
//!
//! 分层沿用键鼠那套：能在本机验的逻辑（`/proc` 解析、差分、锁屏窗口名判定）
//! 全部是纯函数，Windows 上也能 `cargo test` 钉住；只有真正碰 X 的那一步在
//! [`x11`] 里，按 `cfg(target_os = "linux")` 编译。
//!
//! 两个刻意的「宁可不知道」：
//!
//! - 探针失败（读不到 `/proc`、X 打不开）一律 **`None`**，调用方当作「没变 / 不知道」。
//!   一次探测失败就把好好的输入掐掉，比偶尔漏一次锁屏更坑人。
//! - 第一拍没有差分基线，CPU 与网速 **报 0 而不是编一个数**。`0` 在这里的语义
//!   「还没测出来」（同 `VideoCfg.actual_fps`），不是「测得 0」。

use std::sync::Arc;
use std::time::Instant;

pub use uulan_protocol::msg::Metrics;

#[cfg(target_os = "linux")]
pub mod x11;

/// 一次 `/proc/stat` 的 CPU 累计值（jiffies）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuTimes {
    /// 前 8 个字段之和（user…steal）：**总**时间。
    pub total: u64,
    /// `idle + iowait`：**空闲**时间。
    pub idle: u64,
}

/// 系统信息的来源。做成 trait 只为一件事：本机（Windows）没有 `/proc`，
/// 要把「差分算什么」这件事用假数据测到。生产实现是 [`ProcSource`]。
pub trait SysSource: Send + Sync {
    fn cpu_times(&self) -> Option<CpuTimes>;
    /// `(已用, 总量)`，**字节**。
    fn mem(&self) -> Option<(u64, u64)>;
    /// 开机以来的 `(rx, tx)` 累计字节（不含 `lo`）。
    fn net(&self) -> Option<(u64, u64)>;
}

/// `/proc` 的生产实现。非 Linux 上一律 `None`（于是什么都不发）。
#[derive(Debug, Default)]
pub struct ProcSource;

impl SysSource for ProcSource {
    fn cpu_times(&self) -> Option<CpuTimes> {
        parse_cpu_times(&std::fs::read_to_string("/proc/stat").ok()?)
    }

    fn mem(&self) -> Option<(u64, u64)> {
        parse_meminfo(&std::fs::read_to_string("/proc/meminfo").ok()?)
    }

    fn net(&self) -> Option<(u64, u64)> {
        parse_net_dev(&std::fs::read_to_string("/proc/net/dev").ok()?)
    }
}

/// 解析 `/proc/stat` 的第一行：`cpu  user nice system idle iowait irq softirq steal …`。
///
/// `total` 只累加前 8 项（到 `steal`），后两项 `guest`/`guest_nice` 已经包含在
/// `user`/`nice` 里，重复累加会让总时间虚高、`cpu%` 偏低。
pub fn parse_cpu_times(proc_stat: &str) -> Option<CpuTimes> {
    let line = proc_stat.lines().find(|l| l.starts_with("cpu "))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(|f| f.parse::<u64>().ok())
        .collect::<Option<Vec<u64>>>()?;
    // 至少要 user/nice/system/idle 四列才有 `idle` 可说；不足就当作这一拍不知道，
    // 而不是把「缺列」当成「idle=0」（那会报出一个 100% 的假 CPU 占用）。
    if fields.len() < 4 {
        return None;
    }
    let total = fields.iter().fold(0u64, |a, v| a.saturating_add(*v));
    let idle = fields[3] + fields.get(4).copied().unwrap_or(0);
    (total > 0).then_some(CpuTimes { total, idle })
}

/// 解析 `/proc/meminfo`：`MemTotal - MemAvailable` = 已用。
///
/// 没有 `MemAvailable`（3.14 之前的内核）就退回 `MemFree` —— 会偏大，
/// 但好过不发。单位是 kB，这里换成字节。
pub fn parse_meminfo(meminfo: &str) -> Option<(u64, u64)> {
    let mut total_kb = None;
    let mut avail_kb = None;
    for line in meminfo.lines() {
        let mut it = line.split_whitespace();
        let Some(key) = it.next() else { continue };
        let Some(val) = it.next().and_then(|v| v.parse::<u64>().ok()) else {
            continue;
        };
        match key {
            "MemTotal:" => total_kb = Some(val),
            "MemAvailable:" => avail_kb = Some(val),
            "MemFree:" if avail_kb.is_none() => avail_kb = Some(val),
            _ => {}
        }
    }
    let total = total_kb?;
    let used = total.saturating_sub(avail_kb?);
    Some((used.saturating_mul(1024), total.saturating_mul(1024)))
}

/// 解析 `/proc/net/dev`：所有接口的 rx/tx 字节数之和，**跳过 `lo`**。
///
/// 跳过回环是有意的：中继和被控端装在同一台机器上时，回环流量会把「网络负载」
/// 算成一个假的大数，而用户想看的是「这台机器的网卡在忙不忙」。
pub fn parse_net_dev(net_dev: &str) -> Option<(u64, u64)> {
    let mut rx = 0u64;
    let mut tx = 0u64;
    let mut seen = false;
    for line in net_dev.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() || name == "lo" {
            continue;
        }
        let cols: Vec<&str> = rest.split_whitespace().collect();
        // 接收列 0 = bytes；发送列 8 = bytes（中间是 packets/errs/drop/...）。
        let (Some(rx_b), Some(tx_b)) = (cols.first(), cols.get(8)) else {
            continue;
        };
        if let (Ok(r), Ok(t)) = (rx_b.parse::<u64>(), tx_b.parse::<u64>()) {
            rx = rx.saturating_add(r);
            tx = tx.saturating_add(t);
            seen = true;
        }
    }
    seen.then_some((rx, tx))
}

/// CPU 使用率（0..100）。任一拍缺数据、总时间没走（同一毫秒内连拍两次）
/// 或计数器回绕（`cur < prev`，例如容器被重置）都返回 `None` = 不知道。
pub fn cpu_usage(prev: CpuTimes, cur: CpuTimes) -> Option<f32> {
    if cur.total <= prev.total || cur.idle < prev.idle {
        return None;
    }
    let dt = (cur.total - prev.total) as f64;
    let di = cur.idle - prev.idle;
    let busy = (dt - di as f64).max(0.0);
    Some(((busy / dt) * 100.0).clamp(0.0, 100.0) as f32)
}

/// 两拍之间的字节速率（字节/秒）。回绕或时间没走一律 0。
pub fn byte_rate(prev: u64, cur: u64, secs: f64) -> u64 {
    if secs <= 0.0 || cur < prev {
        return 0;
    }
    (((cur - prev) as f64) / secs) as u64
}

/// 采样器：每拍把 `/proc` 的三个累计值取一次，与上一拍做差分。
///
/// 只有**第一拍**没有基线（CPU/网速报 0）。内存是瞬时量，第一拍就能报。
pub struct MetricsSampler {
    src: Arc<dyn SysSource>,
    prev: Option<(Instant, Option<CpuTimes>, Option<(u64, u64)>)>,
}

impl MetricsSampler {
    pub fn new(src: Arc<dyn SysSource>) -> Self {
        Self { src, prev: None }
    }

    /// 采一拍。`now` 由调用方给，测试才好造「两拍间隔 2 秒」。
    pub fn sample(&mut self, now: Instant) -> Metrics {
        let cpu = self.src.cpu_times();
        let net = self.src.net();
        let (used, total) = self.src.mem().unwrap_or((0, 0));

        let mut cpu_pct = 0.0f32;
        let mut rx = 0u64;
        let mut tx = 0u64;
        if let Some((at, prev_cpu, prev_net)) = self.prev {
            let secs = now.saturating_duration_since(at).as_secs_f64();
            if let (Some(a), Some(b)) = (prev_cpu, cpu) {
                cpu_pct = cpu_usage(a, b).unwrap_or(0.0);
            }
            if let (Some((prx, ptx)), Some((crx, ctx))) = (prev_net, net) {
                rx = byte_rate(prx, crx, secs);
                tx = byte_rate(ptx, ctx, secs);
            }
        }
        self.prev = Some((now, cpu, net));

        Metrics {
            cpu: cpu_pct,
            mem_used: used,
            mem_total: total,
            net_rx: rx,
            net_tx: tx,
            // RTT 是**控制端侧**的观测量（它自己算 Ping→Pong 的往返）。
            // 被控端没有可比的时钟，这里如实填 0 而不是编一个数。
            rtt_ms: 0,
        }
    }
}

/// 锁屏 / 有没有桌面（`H2C::SystemState` 的载荷）。
///
/// 协议里这个变体是内联字段（没有独立结构体），这里给一个同形的值类型，
/// 好让探针、测试、会话三处不用各写一对 bool。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DesktopState {
    pub locked: bool,
    pub has_desktop: bool,
}

/// 锁屏状态探测钩子。
///
/// 与 `ScreenProbe` 同一套思路：可注入，测试塞闭包就能验「锁屏后输入被丢掉」。
/// 返回 `None` = 这一拍不知道（打不开 X），**不是**「没锁」。
pub type LockProbe = Arc<dyn Fn() -> Option<DesktopState> + Send + Sync>;

/// 已知的锁屏窗口名（小写比较）。
///
/// 判定依据是「输入焦点落在谁身上」：锁屏程序锁住屏幕时会把焦点抢到自己的窗口上，
/// 而它的窗口名是固定的（UKUI 是 `ukui-screensaver-dialog`，`xdotool getwindowfocus
/// getwindowname` 读到的就是它）。**不做「标题里含 screensaver 就算锁屏」**：
/// `Screensaver Preferences` 这种设置窗口会被误判，用户在设置里一改就发不出键鼠了。
const LOCK_WINDOW_NAMES: &[&str] = &[
    "ukui-screensaver-dialog",
    "ukui-screensaver",
    "gnome-screensaver",
    "mate-screensaver",
    "xfce4-screensaver",
    "xscreensaver",
    "light-locker",
    "kscreenlocker",
    "i3lock",
    "swaylock",
    "waylock",
    "hyprlock",
    "slock",
    "lock screen",
    "lockscreen",
];

/// 焦点窗口名是否属于「锁屏」。
///
/// 规则：小写、去空白后**整串相等**，或以 `-screensaver-dialog` 结尾
/// （覆盖 `ukui-screensaver-dialog`、`<品牌>-screensaver-dialog` 这类命名）。
/// 宁可漏判也不误判 —— 误判的后果是用户的键鼠被无声地掐掉。
pub fn looks_locked(window_name: &str) -> bool {
    let n = window_name.trim().to_ascii_lowercase();
    if n.is_empty() {
        return false;
    }
    LOCK_WINDOW_NAMES.contains(&n.as_str()) || n.ends_with("-screensaver-dialog")
}

/// 生产环境的锁屏探针：抓 X11 屏幕时才启用（合成源没有桌面可锁）。
pub fn production_lock_probe(capture: &crate::capture::CaptureInput) -> Option<LockProbe> {
    #[cfg(target_os = "linux")]
    {
        let disp = capture.display()?;
        return Some(Arc::new(move || x11::system_state(&disp)));
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = capture;
        // 非 Linux 没有可探测的桌面（本机只是开发机）。如实不声明、不发送。
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const STAT: &str = "cpu  2483666 1181 8931656 1349172643 842669 0 563509 0 0 0\n\
                        cpu0 1 2 3 4\n\
                        intr 1 2 3\n";

    const MEMINFO: &str = "MemTotal:       32794476 kB\n\
                           MemFree:        21075984 kB\n\
                           MemAvailable:   28897408 kB\n\
                           Buffers:          123456 kB\n";

    const NETDEV: &str = "Inter-|   Receive                                                |  Transmit\n\
                           face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n\
                             lo:  111111 1 0 0 0 0 0 0  222222 1 0 0 0 0 0 0\n\
                           eno1: 617286905 2130942 0 0 0 0 0 242680 1872434201 1929566 0 0 0 0 0 0\n";

    #[test]
    fn cpu_line_sums_first_eight_fields_only() {
        let t = parse_cpu_times(STAT).unwrap();
        // 累加 user..steal（前 8 项），跳过 guest / guest_nice 两列 0。
        assert_eq!(t.total, 2483666 + 1181 + 8931656 + 1349172643 + 842669 + 0 + 563509 + 0);
        // idle + iowait
        assert_eq!(t.idle, 1349172643 + 842669);
    }

    #[test]
    fn cpu_line_absent_is_none_not_zero() {
        assert!(parse_cpu_times("intr 1 2 3\nctxt 4\n").is_none());
        assert!(parse_cpu_times("cpu  1 2 3\n").is_none()); // 字段不够，宁可不知道
    }

    #[test]
    fn meminfo_uses_available_not_free() {
        let (used, total) = parse_meminfo(MEMINFO).unwrap();
        assert_eq!(total, 32794476 * 1024);
        assert_eq!(used, (32794476 - 28897408) * 1024);
        // 少了 MemAvailable 时退回 MemFree，而不是当成「全用满」。
        let no_avail = MEMINFO.replace("MemAvailable:   28897408 kB\n", "");
        let (used2, _) = parse_meminfo(&no_avail).unwrap();
        assert_eq!(used2, (32794476 - 21075984) * 1024);
        assert!(parse_meminfo("没有这行\n").is_none());
    }

    #[test]
    fn net_dev_skips_loopback_and_sums_interfaces() {
        let (rx, tx) = parse_net_dev(NETDEV).unwrap();
        assert_eq!(rx, 617286905, "回环的 111111 不该算进来");
        assert_eq!(tx, 1872434201, "回环的 222222 不该算进来");
        assert!(parse_net_dev("Inter-|\nface |bytes\n").is_none());
    }

    #[test]
    fn cpu_usage_is_the_delta_between_two_samples() {
        // 上一拍 1000 总 / 900 空闲；这一拍 2000 总 / 1100 空闲 → 差 1000 里忙 800。
        let prev = CpuTimes { total: 1000, idle: 900 };
        let cur = CpuTimes { total: 2000, idle: 1100 };
        assert!((cpu_usage(prev, cur).unwrap() - 80.0).abs() < 0.01);
        // 完全空闲：这 1000 jiffies 全在 idle 里
        assert!((cpu_usage(prev, CpuTimes { total: 2000, idle: 1900 }).unwrap()).abs() < 0.01);
        // 全忙：idle 一个都没走（含 iowait 被算成忙的一部分，这里取的是纯 idle 不变）
        assert!((cpu_usage(prev, CpuTimes { total: 2000, idle: 900 }).unwrap() - 100.0).abs() < 0.01);
    }

    #[test]
    fn cpu_usage_refuses_to_guess_on_reset_or_no_elapsed_time() {
        let prev = CpuTimes { total: 1000, idle: 900 };
        assert!(cpu_usage(prev, prev).is_none(), "同一拍（总时间没走）不猜");
        assert!(
            cpu_usage(prev, CpuTimes { total: 500, idle: 400 }).is_none(),
            "计数器回绕/重置不猜"
        );
        assert!(
            cpu_usage(prev, CpuTimes { total: 2000, idle: 100 }).is_none(),
            "空闲比上一拍还少 = 数据坏了，不猜"
        );
    }

    #[test]
    fn byte_rate_handles_wrap_and_zero_interval() {
        assert_eq!(byte_rate(1000, 3000, 2.0), 1000);
        assert_eq!(byte_rate(3000, 1000, 2.0), 0, "回绕报 0，不报天文数字");
        assert_eq!(byte_rate(1000, 3000, 0.0), 0, "同一拍不算速度");
    }

    /// 假来源：按剧本返回三次快照，用来断言「第一拍不编数、第二拍出真数」。
    struct Fake {
        cpu: Vec<Option<CpuTimes>>,
        net: Vec<Option<(u64, u64)>>,
        calls: AtomicUsize,
    }

    impl SysSource for Fake {
        fn cpu_times(&self) -> Option<CpuTimes> {
            let i = self.calls.load(Ordering::SeqCst);
            self.cpu.get(i).copied().flatten()
        }
        fn mem(&self) -> Option<(u64, u64)> {
            Some((3 * 1024 * 1024 * 1024, 8 * 1024 * 1024 * 1024))
        }
        fn net(&self) -> Option<(u64, u64)> {
            let i = self.calls.fetch_add(1, Ordering::SeqCst);
            self.net.get(i).copied().flatten()
        }
    }

    #[test]
    fn first_sample_reports_memory_but_not_cpu_or_net() {
        let src = Arc::new(Fake {
            cpu: vec![Some(CpuTimes { total: 1000, idle: 900 })],
            net: vec![Some((10_000, 20_000))],
            calls: AtomicUsize::new(0),
        });
        let mut s = MetricsSampler::new(src);
        let m = s.sample(Instant::now());
        assert_eq!(m.cpu, 0.0, "第一拍没有基线，不许编一个 CPU 数");
        assert_eq!((m.net_rx, m.net_tx), (0, 0), "第一拍没有基线，网速 0");
        assert_eq!(m.mem_total, 8 * 1024 * 1024 * 1024);
        assert_eq!(m.mem_used, 3 * 1024 * 1024 * 1024);
        assert_eq!(m.rtt_ms, 0, "RTT 由控制端测，被控端不编");
    }

    #[test]
    fn second_sample_computes_cpu_and_rates_over_the_real_interval() {
        let src = Arc::new(Fake {
            cpu: vec![
                Some(CpuTimes { total: 1000, idle: 900 }),
                Some(CpuTimes { total: 3000, idle: 1100 }),
            ],
            net: vec![Some((10_000, 20_000)), Some((12_048, 20_000))],
            calls: AtomicUsize::new(0),
        });
        let mut s = MetricsSampler::new(src);
        let t0 = Instant::now();
        let _ = s.sample(t0);
        // 4 秒后：总时间走了 2000，其中空闲只走了 200 → 忙 90%
        let m = s.sample(t0 + std::time::Duration::from_secs(4));
        assert!((m.cpu - 90.0).abs() < 0.01, "cpu={}", m.cpu);
        assert_eq!(m.net_rx, (12_048 - 10_000) / 4);
        assert_eq!(m.net_tx, 0);
    }

    #[test]
    fn missing_proc_data_degrades_to_zeros_not_garbage() {
        let src = Arc::new(Fake {
            cpu: vec![None, None],
            net: vec![None, None],
            calls: AtomicUsize::new(0),
        });
        let mut s = MetricsSampler::new(src);
        let t0 = Instant::now();
        let a = s.sample(t0);
        let b = s.sample(t0 + std::time::Duration::from_secs(2));
        assert_eq!((a.cpu, b.cpu), (0.0, 0.0));
        assert_eq!((b.net_rx, b.net_tx), (0, 0));
    }

    #[test]
    fn lock_window_names_are_matched_exactly_not_loosely() {
        assert!(looks_locked("ukui-screensaver-dialog"));
        assert!(looks_locked("  UKUI-ScreenSaver-Dialog \n"));
        assert!(looks_locked("i3lock"));
        assert!(looks_locked("Lock Screen"));
        assert!(looks_locked("mate-screensaver-dialog"), "厂商前缀不同、后缀相同也要认");
        // 误判会让用户的键鼠被无声掐掉，这几条必须为假：
        assert!(!looks_locked("Screensaver Preferences"));
        assert!(!looks_locked("gnome-terminal"));
        assert!(!looks_locked(""));
        assert!(!looks_locked("未命名窗口"));
    }
}