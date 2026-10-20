//! XTest 注入后端（仅 Linux / X11）。
//!
//! 直接用 `extern "C"` 声明 libX11 / libXtst 的符号，**不引第三方 crate**：
//! 这个 crate 的其余部分要能在 Windows 上编译和跑测试，所以整个文件被
//! `cfg(target_os = "linux")` 关掉；用到的 Xlib 函数都是 1990 年代就有的核心符号，
//! 麒麟 V10 上的 libX11/libXtst 一定有。
//!
//! 三件必须做对的事：
//!
//! 1. **关掉全局 autorepeat**。否则 X 自己的连发会和控制端送来的连发叠成双倍速率，
//!    而且首次延迟用的是被控端的设置而不是用户的。会话结束时恢复。
//! 2. **动态注册 Unicode KeySym**。中文是 `0x01000000 + 码点`，键盘布局里没有，
//!    得临时占一个空闲 keycode；退出时把那几行键盘映射还原（改的是全局 X 状态）。
//! 3. **一批指令只在最后 flush 一次**。鼠标移动 100+ 次/秒，每次 flush 都是一个往返。

use std::collections::HashMap;
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_uchar, c_uint, c_ulong};

use tracing::{debug, info};

use super::{Inject, InputSink};

/// 不透明的 `Display*`。用空 enum 表示「只能透过指针访问」。
pub enum Display {}

#[repr(C)]
#[allow(dead_code)] // 字段由 C 填，我们只读 global_auto_repeat
struct XKeyboardState {
    key_click_percent: c_int,
    bell_percent: c_int,
    bell_pitch: c_uint,
    bell_duration: c_uint,
    led_mask: c_ulong,
    global_auto_repeat: c_int,
    auto_repeats: [c_uchar; 32],
}

type KeySym = c_ulong;
type KeyCode = c_uchar;

#[link(name = "X11")]
extern "C" {
    fn XOpenDisplay(name: *const c_char) -> *mut Display;
    fn XCloseDisplay(dpy: *mut Display) -> c_int;
    fn XFlush(dpy: *mut Display) -> c_int;
    fn XSync(dpy: *mut Display, discard: c_int) -> c_int;
    fn XFree(data: *mut std::os::raw::c_void) -> c_int;
    fn XKeysymToKeycode(dpy: *mut Display, keysym: KeySym) -> KeyCode;
    fn XDisplayKeycodes(dpy: *mut Display, min: *mut c_int, max: *mut c_int) -> c_int;
    fn XGetKeyboardMapping(
        dpy: *mut Display,
        first_keycode: KeyCode,
        count: c_int,
        syms_per_keycode: *mut c_int,
    ) -> *mut KeySym;
    fn XChangeKeyboardMapping(
        dpy: *mut Display,
        first_keycode: c_int,
        syms_per_keycode: c_int,
        keysyms: *const KeySym,
        num_codes: c_int,
    ) -> c_int;
    fn XGetKeyboardControl(dpy: *mut Display, state: *mut XKeyboardState) -> c_int;
    fn XAutoRepeatOff(dpy: *mut Display) -> c_int;
    fn XAutoRepeatOn(dpy: *mut Display) -> c_int;
}

#[link(name = "Xtst")]
extern "C" {
    fn XTestQueryExtension(
        dpy: *mut Display,
        ev: *mut c_int,
        err: *mut c_int,
        major: *mut c_int,
        minor: *mut c_int,
    ) -> c_int;
    fn XTestFakeKeyEvent(dpy: *mut Display, keycode: c_uint, is_press: c_int, delay: c_ulong) -> c_int;
    fn XTestFakeButtonEvent(dpy: *mut Display, button: c_uint, is_press: c_int, delay: c_ulong) -> c_int;
    fn XTestFakeMotionEvent(
        dpy: *mut Display,
        screen: c_int,
        x: c_int,
        y: c_int,
        delay: c_ulong,
    ) -> c_int;
    fn XTestFakeRelativeMotionEvent(dpy: *mut Display, x: c_int, y: c_int, delay: c_ulong) -> c_int;
}

pub struct X11Sink {
    dpy: *mut Display,
    display_name: String,
    /// 打开时 X 的全局 autorepeat 本来是开的吗（是的话退出时恢复）。
    autorepeat_was_on: bool,
    /// KeySym → KeyCode 缓存。XKeysymToKeycode 是个线性查找，别每帧都调。
    cache: HashMap<u32, KeyCode>,
    /// 动态改过的 keycode 行，退出时按原样写回。
    remapped: Vec<(c_int, Vec<KeySym>)>,
}

// Xlib 不是线程安全的。这里把 `Send` 标上，前提是**同一时刻只有一个线程用它** ——
// 实践上就是被控端会话任务的那一个循环，所有调用都串行。
unsafe impl Send for X11Sink {}

impl X11Sink {
    /// 连上 X 显示（`None` = 用 `$DISPLAY`）并检查 XTest 扩展。
    pub fn open(display: Option<&str>) -> Result<Self, String> {
        let cname = match display {
            Some(s) if !s.is_empty() => Some(
                CString::new(s).map_err(|e| format!("显示名 {s:?} 含非法字符：{e}"))?,
            ),
            _ => None,
        };
        let dpy = unsafe {
            XOpenDisplay(match &cname {
                Some(c) => c.as_ptr(),
                None => std::ptr::null(),
            })
        };
        if dpy.is_null() {
            return Err(format!(
                "打不开 X 显示 {}（DISPLAY 不对，或当前用户没有该 X 会话的访问权）",
                display.unwrap_or("$DISPLAY")
            ));
        }

        let (mut ev, mut err, mut major, mut minor) = (0, 0, 0, 0);
        if unsafe { XTestQueryExtension(dpy, &mut ev, &mut err, &mut major, &mut minor) } == 0 {
            unsafe { XCloseDisplay(dpy) };
            return Err("X 服务器没有 XTest 扩展，无法注入键鼠".to_string());
        }

        // 关全局 autorepeat，连发交给控制端送（协议 Key.repeat）。
        let mut ks: XKeyboardState = unsafe { std::mem::zeroed() };
        let mut autorepeat_was_on = false;
        if unsafe { XGetKeyboardControl(dpy, &mut ks) } != 0 {
            autorepeat_was_on = ks.global_auto_repeat != 0;
        }
        if autorepeat_was_on {
            unsafe {
                XAutoRepeatOff(dpy);
                XSync(dpy, 0);
            }
        }

        let display_name = display.unwrap_or("$DISPLAY").to_string();
        info!(
            display = %display_name,
            xtest_major = major,
            xtest_minor = minor,
            autorepeat_turned_off = autorepeat_was_on,
            "XTest 注入后端已就绪"
        );
        Ok(Self {
            dpy,
            display_name,
            autorepeat_was_on,
            cache: HashMap::new(),
            remapped: Vec::new(),
        })
    }

    /// KeySym → keycode。布局里没有的（中文等）动态注册到一个空闲 keycode。
    fn keycode_for(&mut self, keysym: u32) -> Option<KeyCode> {
        if let Some(&kc) = self.cache.get(&keysym) {
            return Some(kc);
        }
        let kc = unsafe { XKeysymToKeycode(self.dpy, keysym as KeySym) };
        if kc != 0 {
            self.cache.insert(keysym, kc);
            return Some(kc);
        }
        let kc = self.register_keysym(keysym)?;
        self.cache.insert(keysym, kc);
        Some(kc)
    }

    /// 找一个整行都是 NoSymbol 的空闲 keycode，把 KeySym 写进第 0 列。
    /// 这是 xdotool 处理「布局里没有的字符」的同款做法。
    fn register_keysym(&mut self, keysym: u32) -> Option<KeyCode> {
        let (mut min, mut max) = (0, 0);
        if unsafe { XDisplayKeycodes(self.dpy, &mut min, &mut max) } == 0 {
            return None;
        }
        let count = max - min + 1;
        if count <= 0 {
            return None;
        }
        let mut per: c_int = 0;
        let map = unsafe { XGetKeyboardMapping(self.dpy, min as KeyCode, count, &mut per) };
        if map.is_null() || per <= 0 {
            return None;
        }
        let mut found: Option<(c_int, Vec<KeySym>)> = None;
        // 从高位往低位找：低 keycode 是主键盘，别去动。
        for i in (0..count).rev() {
            let base = (i as usize) * (per as usize);
            let row = unsafe { std::slice::from_raw_parts(map.add(base), per as usize) };
            if row.iter().all(|k| *k == 0) && (min + i) >= 8 {
                found = Some((min + i, row.to_vec()));
                break;
            }
        }
        unsafe { XFree(map as *mut std::os::raw::c_void) };

        let (kc, original) = found?;
        let mut row = original.clone();
        row[0] = keysym as KeySym;
        unsafe {
            XChangeKeyboardMapping(self.dpy, kc, per, row.as_ptr(), 1);
            XSync(self.dpy, 0);
        }
        self.remapped.push((kc, original));
        debug!(keycode = kc, keysym = format!("{keysym:#x}"), "已动态注册 KeySym 到空闲 keycode");
        Some(kc as KeyCode)
    }

    fn apply_one(&mut self, inj: &Inject) -> Result<(), String> {
        match *inj {
            Inject::MoveAbs { x, y } => {
                // screen = -1：跟随「指针当前所在的那块屏」。
                unsafe { XTestFakeMotionEvent(self.dpy, -1, x as c_int, y as c_int, 0) };
            }
            Inject::MoveRel { dx, dy } => {
                unsafe { XTestFakeRelativeMotionEvent(self.dpy, dx as c_int, dy as c_int, 0) };
            }
            Inject::Button { button, down } => {
                unsafe { XTestFakeButtonEvent(self.dpy, button as c_uint, down as c_int, 0) };
            }
            Inject::Wheel { button, count } => {
                for _ in 0..count {
                    unsafe {
                        XTestFakeButtonEvent(self.dpy, button as c_uint, 1, 0);
                        XTestFakeButtonEvent(self.dpy, button as c_uint, 0, 0);
                    }
                }
            }
            Inject::Key { keysym, down } => {
                let Some(kc) = self.keycode_for(keysym) else {
                    return Err(format!("X 键盘映射里找不到 keysym {keysym:#x} 对应的键位"));
                };
                unsafe { XTestFakeKeyEvent(self.dpy, kc as c_uint, down as c_int, 0) };
            }
        }
        Ok(())
    }
}

impl InputSink for X11Sink {
    fn name(&self) -> &'static str {
        "xtest"
    }

    fn apply(&mut self, batch: &[Inject]) -> Result<(), String> {
        for inj in batch {
            self.apply_one(inj)?;
        }
        // 整批一次 flush：鼠标 100+ 次/秒，每条都 flush 会把往返打成瓶颈。
        unsafe { XFlush(self.dpy) };
        Ok(())
    }

    fn finish(&mut self) {
        if self.remapped.is_empty() && !self.autorepeat_was_on {
            return;
        }
        for (kc, row) in std::mem::take(&mut self.remapped) {
            unsafe {
                XChangeKeyboardMapping(self.dpy, kc, row.len() as c_int, row.as_ptr(), 1);
            }
        }
        if self.autorepeat_was_on {
            unsafe { XAutoRepeatOn(self.dpy) };
        }
        unsafe { XSync(self.dpy, 0) };
        info!(display = %self.display_name, "已还原 X 的键盘映射与 autorepeat");
    }
}

impl Drop for X11Sink {
    fn drop(&mut self) {
        // 会话异常结束时也要把全局状态还原 —— 否则本机用户的连发就没了。
        self.finish();
        if !self.dpy.is_null() {
            unsafe { XCloseDisplay(self.dpy) };
            self.dpy = std::ptr::null_mut();
        }
    }
}