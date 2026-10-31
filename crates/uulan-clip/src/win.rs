//! Windows 剪贴板后端（控制端在 Windows 上用）。
//!
//! 直接声明 `user32` / `kernel32` 的符号，**不引 `windows-sys`**：
//! 那个 crate 在这台开发机（GNU 工具链）上会额外要求 `dlltool.exe`，
//! 而我们只需要几个函数。用到的全是 Win95 时代就有的 ABI，不会变。
//!
//! 四个要点：
//!
//! - **打开剪贴板可能失败**。别的程序（含资源管理器）会短暂持有它，
//!   错误是 `ERROR_ACCESS_DENIED`。这不是异常，重试几十毫秒就好；
//!   重试到头也只当「这次没读到」，绝不把它当成「剪贴板空了」。
//! - **失败原因要留痕**（[`WinClipboard::last_error`]）。`get_text` 的 `None`
//!   既可能是「空的」也可能是「打不开」，两者混为一谈时现场只能看到「发送 0」，
//!   查不出是哪种。
//! - **读写都用 `CF_UNICODETEXT`（13）**。ANSI 版本会在中文上按当前代码页丢字。
//! - **`SetClipboardData` 成功之后那块内存归系统所有**，不能再 `GlobalFree`；
//!   失败才要自己释放，否则每粘一次就漏一块。

use std::os::raw::c_void;
use std::time::Duration;

use super::Clipboard;

const CF_UNICODETEXT: u32 = 13;
const GMEM_MOVEABLE: u32 = 0x0002;

#[link(name = "user32")]
extern "system" {
    fn OpenClipboard(hwnd: *mut c_void) -> i32;
    fn CloseClipboard() -> i32;
    fn EmptyClipboard() -> i32;
    fn SetClipboardData(format: u32, mem: *mut c_void) -> *mut c_void;
    fn GetClipboardData(format: u32) -> *mut c_void;
    fn IsClipboardFormatAvailable(format: u32) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
    fn GlobalLock(h: *mut c_void) -> *mut c_void;
    fn GlobalUnlock(h: *mut c_void) -> i32;
    fn GlobalSize(h: *mut c_void) -> usize;
    fn GlobalFree(h: *mut c_void) -> *mut c_void;
    fn GetLastError() -> u32;
}

/// 打开剪贴板的重试次数与间隔。总共约 100ms —— 再久就属于「对方卡死」，
/// 不该拖住会话（这段跑在 tokio 任务里，`sleep` 是真的占着一个 worker 线程）。
const OPEN_RETRIES: u32 = 10;
const OPEN_RETRY_GAP: Duration = Duration::from_millis(10);

/// Win32 错误码 → 人话。只覆盖这条路上真会遇到的几个。
///
/// 补这一条的理由：现场那句「打不开剪贴板（被其他程序占用）」把
/// **拒绝访问（5）**、**没有错误码** 全糊在一起，看日志分不清
/// 「短暂争用」和「整条会话都被拦」。
fn err_hint(code: u32) -> &'static str {
    match code {
        5 => "拒绝访问：剪贴板被别的程序长期占着，或被会话策略拦下",
        6 | 1400 => "句柄无效",
        0 => "没有错误码（调用没设 LastError）",
        _ => "未收录的错误码",
    }
}

pub struct WinClipboard {
    /// 最近一次失败的完整描述；成功一次就清空。
    last_error: Option<String>,
}

impl WinClipboard {
    pub fn new() -> Self {
        Self { last_error: None }
    }

    /// 记下当前 `GetLastError()` 并翻译成一句话。
    fn note_open_failure(&mut self) {
        let code = unsafe { GetLastError() };
        self.last_error =
            Some(format!("OpenClipboard 失败（Win32 错误 {code}：{}）", err_hint(code)));
    }

    /// 反复试 `OpenClipboard`，成功就执行 `f` 并关掉。拿不到返回 `None`。
    fn open_and<T>(f: impl FnOnce() -> T) -> Option<T> {
        let mut opened = false;
        for _ in 0..OPEN_RETRIES {
            if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
                opened = true;
                break;
            }
            std::thread::sleep(OPEN_RETRY_GAP);
        }
        if !opened {
            return None;
        }
        let out = f();
        unsafe { CloseClipboard() };
        Some(out)
    }

    /// 拿剪贴板锁后执行 `f`；失败时留痕。`f` 不捕获 `self`（否则借用冲突）。
    fn with_open<T>(&mut self, f: impl FnOnce() -> T) -> Option<T> {
        match Self::open_and(f) {
            Some(v) => {
                // 成功即清空：`last_error` 表达「现在这一次怎么样」，不是历史。
                self.last_error = None;
                Some(v)
            }
            None => {
                self.note_open_failure();
                None
            }
        }
    }
}

impl Default for WinClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl Clipboard for WinClipboard {
    fn name(&self) -> &'static str {
        "win32"
    }

    /// 平台支持（Windows 一定有剪贴板 API）。
    /// **「现在能不能用」必须看 [`Clipboard::probe`]** —— 这里恒为真，
    /// 因为 `available` 只回答「这个平台有没有这条路」。
    fn available(&self) -> bool {
        true
    }

    fn probe(&mut self) -> Result<(), String> {
        // 只开一次就关：探的是「现在能不能进剪贴板」，不碰里面的内容。
        match self.with_open(|| ()) {
            Some(()) => Ok(()),
            None => Err(self
                .last_error
                .clone()
                .unwrap_or_else(|| "打不开剪贴板（原因未知）".to_string())),
        }
    }

    fn last_error(&self) -> Option<String> {
        self.last_error.clone()
    }

    fn get_text(&mut self) -> Option<String> {
        self.with_open(|| {
            if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) } == 0 {
                return None;
            }
            let h = unsafe { GetClipboardData(CF_UNICODETEXT) };
            if h.is_null() {
                return None;
            }
            let p = unsafe { GlobalLock(h) } as *const u16;
            if p.is_null() {
                return None;
            }
            // 用 GlobalSize 兜住边界：绝不指望那块内存一定以 NUL 结尾。
            let bytes = unsafe { GlobalSize(h) };
            let cap = bytes / std::mem::size_of::<u16>();
            let mut out = String::new();
            if cap > 0 {
                let mut len = 0usize;
                while len < cap {
                    let c = unsafe { *p.add(len) };
                    if c == 0 {
                        break;
                    }
                    len += 1;
                }
                let units = unsafe { std::slice::from_raw_parts(p, len) };
                out = String::from_utf16_lossy(units);
            }
            unsafe { GlobalUnlock(h) };
            if out.is_empty() {
                None
            } else {
                Some(out)
            }
        })
        .flatten()
    }

    fn set_text(&mut self, text: &str) -> Result<(), String> {
        // 结尾那个 NUL 是 CF_UNICODETEXT 的硬性要求。
        let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes = units.len() * std::mem::size_of::<u16>();

        let ok = self.with_open(|| {
            if unsafe { EmptyClipboard() } == 0 {
                return Err("EmptyClipboard 失败".to_string());
            }
            let h = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) };
            if h.is_null() {
                return Err("GlobalAlloc 失败".to_string());
            }
            let p = unsafe { GlobalLock(h) } as *mut u16;
            if p.is_null() {
                unsafe { GlobalFree(h) };
                return Err("GlobalLock 失败".to_string());
            }
            unsafe {
                std::ptr::copy_nonoverlapping(units.as_ptr(), p, units.len());
                GlobalUnlock(h);
            }
            if unsafe { SetClipboardData(CF_UNICODETEXT, h) }.is_null() {
                // 失败时内存仍归我们，必须自己释放。
                unsafe { GlobalFree(h) };
                return Err("SetClipboardData 失败（可能被其他程序占用）".to_string());
            }
            Ok(())
        });

        match ok {
            Some(r) => r,
            None => Err(self
                .last_error
                .clone()
                .unwrap_or_else(|| "打不开剪贴板（原因未知）".to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真跑一遍 Win32 剪贴板：写 → 读回 → 还原。
    ///
    /// 这会动到本机剪贴板，所以有两道保护：
    /// - **只在剪贴板当前是纯文本时才跑**（否则可能把用户复制的图片弄丢）；
    /// - 跑完把原来的文字写回去。
    ///
    /// 这是本机唯一能真正执行的剪贴板后端 —— X11 那个只能在麒麟上验。
    /// 剪贴板打不开时（无人值守会话里可能是整条会话被拦）**跳过并说明原因**，
    /// 不硬断言：环境一变就是一片假红，把真问题埋掉。
    #[test]
    fn win32_roundtrip_and_restore() {
        let mut c = WinClipboard::new();
        if let Err(why) = c.probe() {
            eprintln!("跳过：本机剪贴板现在打不开 —— {why}");
            return;
        }
        let Some(before) = c.get_text() else {
            eprintln!("剪贴板当前不是文本（或为空），跳过：本测试不碰非文本剪贴板");
            return;
        };

        let marker = "uulan-clip 自测 🚀 中文换行\n第二行";
        c.set_text(marker).expect("写入剪贴板失败");
        assert_eq!(
            c.get_text().as_deref(),
            Some(marker),
            "读回来的内容与写进去的不一致（UTF-16 转换或长度算错了？）"
        );

        c.set_text(&before).expect("还原剪贴板失败");
        assert_eq!(c.get_text().as_deref(), Some(before.as_str()), "还原后内容不一致");
    }

    /// `probe` 的结论必须和真实可读写性一致 —— 这是「能力声明是否可信」的地基。
    #[test]
    fn probe_agrees_with_reality() {
        let mut c = WinClipboard::new();
        match c.probe() {
            Ok(()) => {
                assert!(c.last_error().is_none(), "probe 成功却留着错误：{:?}", c.last_error());
                // 拿到剪贴板之后读一次文本，不该报错（空剪贴板允许返回 None）
                let _ = c.get_text();
                assert!(
                    c.last_error().is_none(),
                    "拿到剪贴板之后读文本仍报错：{:?}",
                    c.last_error()
                );
            }
            Err(why) => {
                assert!(!why.is_empty(), "失败必须给出原因，不能是空串");
                assert!(
                    c.last_error().is_some(),
                    "probe 失败时 last_error 必须留痕，否则现场还是查不出原因"
                );
                eprintln!("本机剪贴板不可用，原因：{why}");
            }
        }
    }

    #[test]
    fn err_hint_covers_access_denied() {
        assert!(err_hint(5).contains("拒绝访问"));
        assert!(err_hint(0).contains("没有错误码"));
        assert_eq!(err_hint(1234), "未收录的错误码");
    }
}