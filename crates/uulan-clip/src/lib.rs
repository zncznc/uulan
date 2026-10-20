//! 剪贴板同步：**纯逻辑（[`ClipSync`]）+ 平台后端（[`Clipboard`]）**。
//!
//! 分层和键鼠注入那一套一模一样，理由也一样：能在本机用 `cargo test` 钉住的逻辑
//! （去重、防回声、大小上限）全放在与平台无关的状态机里；只有「真的读写系统剪贴板」
//! 才落到后端。这样 Windows 开发机上也能验证同步行为，不必等真麒麟。
//!
//! ## 三个必须做对的地方
//!
//! 1. **防回声**。控制端把远端文本写进本地剪贴板、又被自己的轮询读到，如果原样回发，
//!    两端就会无限互发。做法是记住「最近一次已知的内容」（不论来源是本端还是对端），
//!    内容哈希相同就不再发。
//! 2. **绝不传播「剪贴板被清空」**。X11 上读剪贴板是一次跨进程请求（`XConvertSelection`），
//!    所有者可能不响应、可能超时 —— 那读出来是 `None`。若把 `None` 当成「内容变成了空」，
//!    一次超时就会把对面剪贴板擦掉。所以只同步**非空文本**，`None` 一律当成「没读到」。
//! 3. **大小上限**。X11 的属性写入受单个请求大小限制（约 256 KiB），超了必须走 INCR 分段；
//!    第一版不做 INCR 写入，改为限额 + **如实计数**（[`ClipSync::skipped_oversize`]），
//!    而不是悄悄截断（截断的剪贴板比不同步更坑人）。
//!
//! ## 已知限制（照实写）
//!
//! - 只同步文本。图片、文件列表（`text/uri-list`）一律不同步。
//! - 「清空剪贴板」不传播（见上）。
//! - X11 上超过 [`MAX_TEXT_BYTES`] 的内容不发送；Windows 上同样限额，保持两端行为一致。

use std::time::Duration;

/// 单条剪贴板文本的上限（UTF-8 字节）。超过就不同步、只计数。
pub const MAX_TEXT_BYTES: usize = 192 * 1024;

/// 本地剪贴板的轮询间隔。
///
/// 500ms 是刻意的折中：更密会让 X11 每轮都发一次 `XConvertSelection`（跨进程往返），
/// 更疏则「Ctrl+C 之后立刻去对面按 Ctrl+V」会粘到旧内容。
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// 剪贴板后端。
///
/// 所有方法都允许失败 —— 剪贴板是「尽力而为」的系统资源：X11 上没有桌面、
/// Windows 上被别的程序占着，都可能读写不了。失败**不算错误**，但后端要如实报出来。
pub trait Clipboard: Send {
    /// 后端名，用于日志（`x11` / `win32` / `none`）。
    fn name(&self) -> &'static str;

    /// 是否真的能读写剪贴板。`false` 时两端都不该声明 `Cap::Clipboard`。
    fn available(&self) -> bool;

    /// 读当前文本。`None` = 读不到（不是「剪贴板是空的」）。
    fn get_text(&mut self) -> Option<String>;

    /// 写入文本。返回 `Err` 表示本端写不进去（对面内容不会丢，只是这台机器粘不了）。
    fn set_text(&mut self, text: &str) -> Result<(), String>;
}

/// 没有剪贴板后端（无桌面 / 非 X11 会话 / 关闭了该功能）。
///
/// **报错而不是装作成功**：和 `NullSink` 的道理一样，静默吞掉的表现是
/// 「对面复制了，这边粘不出来」，最难查。
pub struct NoClipboard {
    reason: String,
}

impl NoClipboard {
    pub fn new(reason: impl Into<String>) -> Self {
        Self { reason: reason.into() }
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl Clipboard for NoClipboard {
    fn name(&self) -> &'static str {
        "none"
    }

    fn available(&self) -> bool {
        false
    }

    fn get_text(&mut self) -> Option<String> {
        None
    }

    fn set_text(&mut self, _text: &str) -> Result<(), String> {
        Err(self.reason.clone())
    }
}

/// 按当前平台挑一个默认后端。
///
/// `display` 只对 X11 有意义（`None` = 用 `$DISPLAY`）；被控端要把采集用的那个
/// 显示号传进来 —— 抓的是哪个桌面，剪贴板就必须是哪个桌面的。
pub fn default_clipboard(display: Option<&str>) -> Box<dyn Clipboard> {
    #[cfg(target_os = "linux")]
    {
        match x11::X11Clipboard::open(display) {
            Ok(c) => return Box::new(c),
            Err(e) => {
                tracing::info!(error = %e, "剪贴板后端不可用（X11），本次会话不同步剪贴板");
                return Box::new(NoClipboard::new(e));
            }
        }
    }
    #[cfg(windows)]
    {
        let _ = display;
        return Box::new(win::WinClipboard::new());
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = display;
        Box::new(NoClipboard::new("当前平台没有剪贴板后端".to_string()))
    }
}

// ───────────────────────────── 去重状态机 ─────────────────────────────

/// 剪贴板同步的纯逻辑：什么时候该发、什么时候该写、什么时候该闭嘴。
///
/// 用法固定为两条路径：
/// - 轮询到本地内容 → [`ClipSync::outbound`]
/// - 收到对端内容 → [`ClipSync::inbound`]
///
/// 两者都会更新「已知内容」，所以**回声在第二次经过时就消失了**。
#[derive(Debug)]
pub struct ClipSync {
    max_bytes: usize,
    /// 最近一次已知内容的指纹（已做换行归一化）。
    known: Option<u64>,
    /// 因为超限没同步的次数（排障时看这个数，而不是猜）。
    pub skipped_oversize: u64,
    /// 因为内容为空（含只有 `\0`）被跳过的次数。
    pub skipped_empty: u64,
    /// 因为与已知内容相同而跳过的次数（正常运行时这个数会随轮询平稳增长）。
    pub skipped_same: u64,
}

impl Default for ClipSync {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipSync {
    pub fn new() -> Self {
        Self::with_max_bytes(MAX_TEXT_BYTES)
    }

    pub fn with_max_bytes(max_bytes: usize) -> Self {
        Self { max_bytes, known: None, skipped_oversize: 0, skipped_empty: 0, skipped_same: 0 }
    }

    /// 先记住一段内容（例如会话开始时本地已有的剪贴板），避免刚连上就把历史内容推过去。
    pub fn note(&mut self, text: &str) {
        self.known = Some(fingerprint(text));
    }

    /// 本地剪贴板当前内容 → 要发给对端的文本（`None` = 不用发）。
    pub fn outbound(&mut self, text: Option<&str>) -> Option<String> {
        let Some(t) = text else { return None }; // 读不到 ≠ 被清空
        self.decide(t).map(|s| s.to_string())
    }

    /// 收到的对端文本 → 要写进本端剪贴板的文本（`None` = 不用写）。
    ///
    /// 与 [`ClipSync::outbound`] 共用同一份「已知内容」，所以「写进去之后又被轮询读到」
    /// 这一圈不会回发。
    pub fn inbound(&mut self, text: &str) -> Option<String> {
        self.decide(text).map(|s| s.to_string())
    }

    /// 核心判定。返回 `Some(规范化后的文本)` 表示「这是一次真正的新内容」。
    fn decide<'a>(&mut self, text: &'a str) -> Option<&'a str> {
        // 去掉平台附加的结尾 NUL（Windows 的 CF_UNICODETEXT 一定带）。
        let text = text.trim_end_matches('\0');
        if text.is_empty() {
            self.skipped_empty += 1;
            return None;
        }
        if text.len() > self.max_bytes {
            self.skipped_oversize += 1;
            tracing::debug!(
                bytes = text.len(),
                cap = self.max_bytes,
                "剪贴板内容超过上限，不同步"
            );
            return None;
        }
        let fp = fingerprint(text);
        if self.known == Some(fp) {
            self.skipped_same += 1;
            return None;
        }
        self.known = Some(fp);
        Some(text)
    }

    /// 会话结束时的统计（两端都会打印这个，作为「真的同步过」的证据）。
    pub fn skipped(&self) -> (u64, u64, u64) {
        (self.skipped_oversize, self.skipped_empty, self.skipped_same)
    }
}

/// 轮询驱动的一拍：**第一拍只记录，不发送**。
///
/// 为什么需要它：会话一连上就读到本机剪贴板的内容，如果直接推过去，
/// 就会把对面正在用的剪贴板覆盖成「我这边的历史内容」—— 用户会以为对方乱粘东西。
/// 所以第一拍只把当前内容记为「已知」，之后只有**真的变化**才同步。
///
/// `armed` 由调用方持有（每端一个），第一次调用后置 `true`。
pub fn poll_outbound(sync: &mut ClipSync, cur: Option<&str>, armed: &mut bool) -> Option<String> {
    if !*armed {
        *armed = true;
        if let Some(t) = cur {
            sync.note(t);
        }
        return None;
    }
    sync.outbound(cur)
}

/// 内容指纹：FNV-1a 64，**换行归一化后再算**。
///
/// 为什么要归一化：Windows 的剪贴板里换行是 `\r\n`，X11 那边是 `\n`。
/// 若按原始字节比对，远端来的 `a\nb` 写进 Windows 再读回来变成 `a\r\nb`，
/// 指纹不同 → 又发回对端 → 对端写回去又读回同样的内容 → 两端互相刷一晚上。
/// 所以统一丢掉 `\r`。这个变换是幂等的，因此回路一定会收敛。
fn fingerprint(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        if *b == b'\r' {
            continue;
        }
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(windows)]
pub mod win;
#[cfg(target_os = "linux")]
pub mod x11;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_content_is_not_sent_twice() {
        let mut s = ClipSync::new();
        assert_eq!(s.outbound(Some("hello")).as_deref(), Some("hello"));
        assert_eq!(s.outbound(Some("hello")), None, "同一份内容不该重复发送");
        assert_eq!(s.outbound(Some("hello!")).as_deref(), Some("hello!"));
    }

    /// 最关键的一条：远端写进来的内容，被本地轮询再读到，**绝不能回发**。
    #[test]
    fn inbound_text_does_not_echo_back() {
        let mut s = ClipSync::new();
        assert_eq!(s.inbound("对面复制的").as_deref(), Some("对面复制的"));
        // 本地剪贴板已经被写进这段文字，轮询读到的就是它 —— 不能再发一次
        assert_eq!(s.outbound(Some("对面复制的")), None);
    }

    /// Windows 的 CF_UNICODETEXT 会把 `\n` 变 `\r\n`，不能因此变成回声。
    #[test]
    fn crlf_normalisation_breaks_the_echo_loop() {
        let mut s = ClipSync::new();
        assert!(s.inbound("a\nb").is_some());
        assert_eq!(s.outbound(Some("a\r\nb")), None, "换行被平台改写后仍应视为同一份内容");
        // 反向也一样
        let mut s = ClipSync::new();
        assert!(s.outbound(Some("x\r\ny")).is_some());
        assert_eq!(s.inbound("x\ny"), None);
    }

    #[test]
    fn unreadable_clipboard_is_not_treated_as_cleared() {
        let mut s = ClipSync::new();
        assert_eq!(s.outbound(Some("keep me")).as_deref(), Some("keep me"));
        // null = 「没读到」（X11 超时 / 被占用），不是「被清空」
        assert_eq!(s.outbound(None), None);
        // 之后读到同样的内容，也不该再发
        assert_eq!(s.outbound(Some("keep me")), None);
    }

    #[test]
    fn empty_and_nul_only_content_is_skipped() {
        let mut s = ClipSync::new();
        assert_eq!(s.outbound(Some("")), None);
        assert_eq!(s.outbound(Some("\0")), None);
        assert_eq!(s.inbound(""), None);
        assert_eq!(s.skipped_empty, 3);
    }

    #[test]
    fn trailing_nul_is_stripped_before_comparing() {
        let mut s = ClipSync::new();
        assert_eq!(s.outbound(Some("abc\0")).as_deref(), Some("abc"));
        assert_eq!(s.inbound("abc").as_deref().is_some(), false, "带/不带 NUL 是同一份内容");
    }

    #[test]
    fn oversize_is_counted_not_truncated() {
        let mut s = ClipSync::with_max_bytes(8);
        assert!(s.outbound(Some("12345678")).is_some(), "正好等于上限应当可以通过");
        let mut s = ClipSync::with_max_bytes(4);
        assert_eq!(s.outbound(Some("12345")), None, "超限必须整条丢弃，不能截断");
        assert_eq!(s.skipped_oversize, 1);
        // 超限的内容也不能污染「已知内容」：随后一条小的仍要能发出去
        assert!(s.outbound(Some("ok")).is_some());
    }

    #[test]
    fn note_suppresses_initial_push() {
        let mut s = ClipSync::new();
        s.note("连上之前就在剪贴板里的");
        assert_eq!(s.outbound(Some("连上之前就在剪贴板里的")), None);
        assert!(s.outbound(Some("连上之后新复制的")).is_some());
    }

    #[test]
    fn first_poll_arms_without_sending() {
        let mut s = ClipSync::new();
        let mut armed = false;
        // 连上那一刻剪贴板里已有的内容：只记录，绝不推给对面
        assert_eq!(poll_outbound(&mut s, Some("连上之前就在的"), &mut armed), None);
        assert!(armed);
        assert_eq!(poll_outbound(&mut s, Some("连上之前就在的"), &mut armed), None);
        assert_eq!(
            poll_outbound(&mut s, Some("用户真的复制了新东西"), &mut armed).as_deref(),
            Some("用户真的复制了新东西")
        );
        // 第一拍读不到内容时也不能「欠一次发送」：下一次读到的仍视为初始内容
        let mut s = ClipSync::new();
        let mut armed = false;
        assert_eq!(poll_outbound(&mut s, None, &mut armed), None);
        assert_eq!(poll_outbound(&mut s, Some("后来的"), &mut armed).as_deref(), Some("后来的"));
    }

    #[test]
    fn no_clipboard_reports_unavailable_and_refuses_silently_not() {
        let mut c = NoClipboard::new("没有桌面");
        assert!(!c.available());
        assert_eq!(c.name(), "none");
        assert_eq!(c.get_text(), None);
        assert_eq!(c.set_text("x"), Err("没有桌面".to_string()));
    }
}