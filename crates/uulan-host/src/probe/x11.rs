//! 锁屏 / 桌面探测的 X11 实现（仅 Linux）。
//!
//! 判定依据只有一条：**输入焦点落在谁身上**。锁屏程序锁屏时会把焦点抢到自己的窗口，
//! 而它的窗口名是固定的（UKUI = `ukui-screensaver-dialog`）。这个名字最短路径是
//! `xdotool getwindowfocus getwindowname`，但这里不调外部命令：
//!
//! - 现场不装 `xdotool` 的机器多得是（麒麟最小安装就没带），一条 `Command` 失败
//!   就意味着「永远探不到锁屏」；
//! - 每秒一次 fork/exec 不如开一个 Display 便宜。
//!
//! 名字按两条属性读：先 `WM_NAME`（`XFetchName`），空再读 `_NET_WM_NAME`
//! （UTF8_STRING）。两条都是 libX11 里的**真函数**（不是宏），能链接到。
//! 这就是 [`super::looks_locked`] 的输入。
//!
//! 任何一步失败都返回现成的最保守结论：**打不开 Display** ⇒ `has_desktop=false`；
//! **读不到焦点名** ⇒ 当作没锁（`locked=false`）。掐掉键鼠必须有正面证据。

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_long, c_uchar, c_ulong};
use std::ptr;

use super::{looks_locked, DesktopState};

// **复用**输入后端那个不透明 `Display*`：同一个 `XOpenDisplay` 在同一个 crate 里
// 声明两次、各自带一个 `Display` 类型，rustc 会报「signature doesn't match the
// previous declaration」（它们是同一个符号）。两个模块各写一份就得是同一个类型。
use crate::input::x11::Display;

type Window = c_ulong;
type Atom = c_ulong;

/// `XGetInputFocus` 的 revert_to 取值（我们不用，但要传个有效指针）。
const REVERT_TO_PARENT: c_int = 2;
const FALSE: c_int = 0;


#[link(name = "X11")]
extern "C" {
    fn XOpenDisplay(name: *const c_char) -> *mut Display;
    fn XCloseDisplay(dpy: *mut Display) -> c_int;
    fn XFree(data: *mut std::os::raw::c_void) -> c_int;
    fn XGetInputFocus(dpy: *mut Display, focus: *mut Window, revert: *mut c_int) -> c_int;
    fn XFetchName(dpy: *mut Display, w: Window, name: *mut *mut c_char) -> c_int;
    fn XInternAtom(dpy: *mut Display, name: *const c_char, only_if_exists: c_int) -> Atom;
    fn XGetWindowProperty(
        dpy: *mut Display,
        w: Window,
        property: Atom,
        long_offset: c_long,
        long_length: c_long,
        delete: c_int,
        req_type: Atom,
        actual_type: *mut Atom,
        actual_format: *mut c_int,
        nitems: *mut c_ulong,
        bytes_after: *mut c_ulong,
        prop: *mut *mut c_uchar,
    ) -> c_int;
}

/// 探一次：`(locked, has_desktop)`。`display` 形如 `:0` / `:0.0`。
pub fn system_state(display: &str) -> Option<DesktopState> {
    let name = CString::new(display).ok()?;
    // 探针每秒开一次显示、用完就关；它和别的会话的 X 调用在同一个进程里，
    // 必须先把 Xlib 的线程锁装上（见 uulan_clip::x11::init_xlib 的说明）。
    uulan_clip::x11::init_xlib();
    unsafe {
        let dpy = XOpenDisplay(name.as_ptr());
        if dpy.is_null() {
            // 打不开显示：这台机器现在没有可用桌面。**不是**「探不到」——
            // 这一点对控制端有用（它会切成只读模式），所以要如实报出去。
            return Some(DesktopState { locked: false, has_desktop: false });
        }
        let state = DesktopState {
            locked: focus_is_a_locker(dpy),
            has_desktop: true,
        };
        XCloseDisplay(dpy);
        Some(state)
    }
}

/// 只读一次「输入焦点在哪个窗口」的名字（`--diagnose` 的输出里要给人看）。
///
/// 与 [`system_state`] 一样是「开一次显示、读一次、关掉」，但这边只返回名字，
/// 好让现场看到 `焦点 ukui-screensaver-dialog` 就明白为什么键鼠被门禁挡下。
pub fn focus_name(display: &str) -> Option<String> {
    let name = CString::new(display).ok()?;
    uulan_clip::x11::init_xlib();
    unsafe {
        let dpy = XOpenDisplay(name.as_ptr());
        if dpy.is_null() {
            return None;
        }
        let mut focus: Window = 0;
        let mut revert: c_int = REVERT_TO_PARENT;
        XGetInputFocus(dpy, &mut focus, &mut revert);
        let out = if focus == 0 || focus == 1 { None } else { window_name(dpy, focus) };
        XCloseDisplay(dpy);
        out
    }
}

unsafe fn focus_is_a_locker(dpy: *mut Display) -> bool {
    let mut focus: Window = 0;
    let mut revert: c_int = REVERT_TO_PARENT;
    XGetInputFocus(dpy, &mut focus, &mut revert);
    // 焦点是 None(0) / PointerRoot(1) 时没有可读的名字 —— 当作没锁。
    if focus == 0 || focus == 1 {
        return false;
    }
    // `XGetInputFocus` 本身就是带回包的请求（已经同步过了），不必再 XSync。
    let name = window_name(dpy, focus).unwrap_or_default();
    looks_locked(&name)
}

/// 先 `WM_NAME`，空再 `_NET_WM_NAME`。
unsafe fn window_name(dpy: *mut Display, w: Window) -> Option<String> {
    let mut raw: *mut c_char = ptr::null_mut();
    if XFetchName(dpy, w, &mut raw) != 0 && !raw.is_null() {
        let s = CStr::from_ptr(raw).to_string_lossy().into_owned();
        XFree(raw as *mut std::os::raw::c_void);
        if !s.trim().is_empty() {
            return Some(s);
        }
    }

    // _NET_WM_NAME（UTF-8）：现代工具包只设这一条，WM_NAME 可能是空的或 latin1 乱码。
    let net_wm_name = CString::new("_NET_WM_NAME").ok()?;
    let utf8 = CString::new("UTF8_STRING").ok()?;
    let prop_atom = XInternAtom(dpy, net_wm_name.as_ptr(), FALSE);
    if prop_atom == 0 {
        return None;
    }
    let utf8_atom = XInternAtom(dpy, utf8.as_ptr(), FALSE);
    let mut actual_type: Atom = 0;
    let mut actual_format: c_int = 0;
    let mut nitems: c_ulong = 0;
    let mut bytes_after: c_ulong = 0;
    let mut prop: *mut c_uchar = ptr::null_mut();
    // 256 个 32 位单位 = 1 KiB，够放窗口标题；再长就不是锁屏窗口的名字了。
    let rc = XGetWindowProperty(
        dpy,
        w,
        prop_atom,
        0,
        256,
        FALSE,
        utf8_atom,
        &mut actual_type,
        &mut actual_format,
        &mut nitems,
        &mut bytes_after,
        &mut prop,
    );
    if rc != 0 || prop.is_null() {
        return None;
    }
    let out = if actual_format == 8 && nitems > 0 {
        let bytes = std::slice::from_raw_parts(prop, nitems as usize);
        Some(String::from_utf8_lossy(bytes).into_owned())
    } else {
        None
    };
    XFree(prop as *mut std::os::raw::c_void);
    out
}