//! Windows 剪贴板后端（控制端在 Windows 上用）。
//!
//! 直接声明 `user32` / `kernel32` 的符号，**不引 `windows-sys`**：
//! 那个 crate 在这台开发机（GNU 工具链）上会额外要求 `dlltool.exe`，
//! 而我们只需要六个函数。用到的全是 Win95 时代就有的 ABI，不会变。
//!
//! 三个要点：
//!
//! - **打开剪贴板可能失败**。别的程序（含资源管理器）会短暂持有它，
//!   错误是 `ERROR_ACCESS_DENIED`。这不是异常，重试几十毫秒就好；
//!   重试到头也只当「这次没读到」，绝不把它当成「剪贴板空了」。
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
}

/// 打开剪贴板的重试次数与间隔。总共约 60ms —— 再久就属于「对方卡死」，不该拖住会话。
const OPEN_RETRIES: u32 = 6;
const OPEN_RETRY_GAP: Duration = Duration::from_millis(10);

pub struct WinClipboard;

impl WinClipboard {
    pub fn new() -> Self {
        Self
    }

    /// 拿剪贴板锁后执行 `f`。拿不到返回 `None`（而不是 panic / 阻塞）。
    fn with_open<T>(f: impl FnOnce() -> T) -> Option<T> {
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

    fn available(&self) -> bool {
        true
    }

    fn get_text(&mut self) -> Option<String> {
        Self::with_open(|| {
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

        let ok = Self::with_open(|| {
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
            None => Err("打不开剪贴板（被其他程序占用）".to_string()),
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
    #[test]
    fn win32_roundtrip_and_restore() {
        let mut c = WinClipboard::new();
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
}