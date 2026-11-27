//! Windows 键鼠注入后端：直接通过 Win32 `SendInput` 注入鼠标、滚轮、按键与 Unicode 文本。
//!
//! 零额外 crate 依赖，直接 FFI 调 `user32.dll`。

use super::{Inject, InputSink};

const INPUT_MOUSE: u32 = 0;
const INPUT_KEYBOARD: u32 = 1;

const MOUSEEVENTF_MOVE: u32 = 0x0001;
const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
const MOUSEEVENTF_RIGHTDOWN: u32 = 0x0008;
const MOUSEEVENTF_RIGHTUP: u32 = 0x0010;
const MOUSEEVENTF_MIDDLEDOWN: u32 = 0x0020;
const MOUSEEVENTF_MIDDLEUP: u32 = 0x0040;
const MOUSEEVENTF_XDOWN: u32 = 0x0080;
const MOUSEEVENTF_XUP: u32 = 0x0100;
const MOUSEEVENTF_WHEEL: u32 = 0x0800;
const MOUSEEVENTF_HWHEEL: u32 = 0x1000;
const MOUSEEVENTF_VIRTUALDESK: u32 = 0x4000;
const MOUSEEVENTF_ABSOLUTE: u32 = 0x8000;

const XBUTTON1: u32 = 0x0001;
const XBUTTON2: u32 = 0x0002;
const WHEEL_DELTA: i32 = 120;

const KEYEVENTF_EXTENDEDKEY: u32 = 0x0001;
const KEYEVENTF_KEYUP: u32 = 0x0002;
const KEYEVENTF_UNICODE: u32 = 0x0004;

const SM_XVIRTUALSCREEN: i32 = 76;
const SM_YVIRTUALSCREEN: i32 = 77;
const SM_CXVIRTUALSCREEN: i32 = 78;
const SM_CYVIRTUALSCREEN: i32 = 79;

#[repr(C)]
#[derive(Copy, Clone)]
struct MouseInput {
    dx: i32,
    dy: i32,
    mouse_data: u32,
    dw_flags: u32,
    time: u32,
    dw_extra_info: usize,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct KeybdInput {
    w_vk: u16,
    w_scan: u16,
    dw_flags: u32,
    time: u32,
    dw_extra_info: usize,
}

#[repr(C)]
#[derive(Copy, Clone)]
union InputUnion {
    mi: MouseInput,
    ki: KeybdInput,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct Input {
    r#type: u32,
    u: InputUnion,
}

#[link(name = "user32")]
extern "system" {
    fn SendInput(c_inputs: u32, p_inputs: *const Input, cb_size: i32) -> u32;
    fn GetSystemMetrics(index: i32) -> i32;
}

pub struct Win32Sink;

impl Win32Sink {
    pub fn open() -> Self {
        uulan_media::win_gdi::set_dpi_aware();
        Self
    }
}

fn mouse_input(dx: i32, dy: i32, mouse_data: u32, dw_flags: u32) -> Input {
    Input {
        r#type: INPUT_MOUSE,
        u: InputUnion {
            mi: MouseInput { dx, dy, mouse_data, dw_flags, time: 0, dw_extra_info: 0 },
        },
    }
}

fn key_input(w_vk: u16, w_scan: u16, dw_flags: u32) -> Input {
    Input {
        r#type: INPUT_KEYBOARD,
        u: InputUnion {
            ki: KeybdInput { w_vk, w_scan, dw_flags, time: 0, dw_extra_info: 0 },
        },
    }
}

/// 将 X11 KeySym 映射回 Windows 虚拟键码 (VK) 以及是否需要 `KEYEVENTF_EXTENDEDKEY`。
fn keysym_to_vk(ks: u32) -> Option<(u16, bool)> {
    // ASCII 字母与数字
    if (b'a' as u32..=b'z' as u32).contains(&ks) {
        return Some(((ks - b'a' as u32 + b'A' as u32) as u16, false));
    }
    if (b'A' as u32..=b'Z' as u32).contains(&ks) || (b'0' as u32..=b'9' as u32).contains(&ks) {
        return Some((ks as u16, false));
    }
    let r = match ks {
        0x0020 => (0x20, false), // Space
        0xff08 => (0x08, false), // BackSpace
        0xff09 => (0x09, false), // Tab
        0xff0d => (0x0D, false), // Return
        0xff1b => (0x1B, false), // Escape
        0xff63 => (0x2D, true),  // Insert
        0xffff => (0x2E, true),  // Delete
        0xff50 => (0x24, true),  // Home
        0xff57 => (0x23, true),  // End
        0xff55 => (0x21, true),  // PageUp
        0xff56 => (0x22, true),  // PageDown
        0xff51 => (0x25, true),  // Left
        0xff52 => (0x26, true),  // Up
        0xff53 => (0x27, true),  // Right
        0xff54 => (0x28, true),  // Down
        0xffe1 => (0xA0, false), // Shift_L
        0xffe2 => (0xA1, false), // Shift_R
        0xffe3 => (0xA2, false), // Control_L
        0xffe4 => (0xA3, true),  // Control_R
        0xffe9 => (0xA4, false), // Alt_L
        0xffea => (0xA5, true),  // Alt_R
        0xffeb => (0x5B, true),  // Super_L (LWin)
        0xffec => (0x5C, true),  // Super_R (RWin)
        0xffe5 => (0x14, false), // Caps_Lock
        0xff14 => (0x91, false), // Scroll_Lock
        0xff7f => (0x90, true),  // Num_Lock
        0xff61 => (0x2C, true),  // PrintScreen
        0xff13 => (0x13, false), // Pause
        0xffbe..=0xffc9 => ((0x70 + (ks - 0xffbe)) as u16, false), // F1..F12
        // 常见标点（美式键盘布局主位）
        0x003b | 0x003a => (0xBA, false), // ; :
        0x003d | 0x002b => (0xBB, false), // = +
        0x002c | 0x003c => (0xBC, false), // , <
        0x002d | 0x005f => (0xBD, false), // - _
        0x002e | 0x003e => (0xBE, false), // . >
        0x002f | 0x003f => (0xBF, false), // / ?
        0x0060 | 0x007e => (0xC0, false), // ` ~
        0x005b | 0x007b => (0xDB, false), // [ {
        0x005c | 0x007c => (0xDC, false), // \ |
        0x005d | 0x007d => (0xDD, false), // ] }
        0x0027 | 0x0022 => (0xDE, false), // ' "
        _ => return None,
    };
    Some(r)
}

impl InputSink for Win32Sink {
    fn name(&self) -> &'static str {
        "win32-sendinput"
    }

    fn apply(&mut self, batch: &[Inject]) -> Result<(), String> {
        if batch.is_empty() {
            return Ok(());
        }
        let mut events: Vec<Input> = Vec::with_capacity(batch.len());

        let (vx, vy, vw, vh) = unsafe {
            let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let w = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1);
            let h = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1);
            (x, y, w, h)
        };

        for item in batch {
            match *item {
                Inject::MoveAbs { x, y } => {
                    let nx = (((x - vx) as i64 * 65535) / (vw as i64 - 1).max(1)).clamp(0, 65535) as i32;
                    let ny = (((y - vy) as i64 * 65535) / (vh as i64 - 1).max(1)).clamp(0, 65535) as i32;
                    events.push(mouse_input(
                        nx,
                        ny,
                        0,
                        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    ));
                }
                Inject::MoveRel { dx, dy } => {
                    events.push(mouse_input(dx, dy, 0, MOUSEEVENTF_MOVE));
                }
                Inject::Button { button, down } => {
                    let (flags, data) = match (button, down) {
                        (1, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                        (1, false) => (MOUSEEVENTF_LEFTUP, 0),
                        (2, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                        (2, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                        (3, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                        (3, false) => (MOUSEEVENTF_RIGHTUP, 0),
                        (8, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
                        (8, false) => (MOUSEEVENTF_XUP, XBUTTON1),
                        (9, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
                        (9, false) => (MOUSEEVENTF_XUP, XBUTTON2),
                        _ => continue,
                    };
                    events.push(mouse_input(0, 0, data, flags));
                }
                Inject::Wheel { button, count } => {
                    let steps = (count.min(64) as i32).max(1);
                    let (flags, delta) = match button {
                        4 => (MOUSEEVENTF_WHEEL, WHEEL_DELTA * steps),
                        5 => (MOUSEEVENTF_WHEEL, -WHEEL_DELTA * steps),
                        6 => (MOUSEEVENTF_HWHEEL, -WHEEL_DELTA * steps),
                        7 => (MOUSEEVENTF_HWHEEL, WHEEL_DELTA * steps),
                        _ => continue,
                    };
                    events.push(mouse_input(0, 0, delta as u32, flags));
                }
                Inject::Key { keysym, down } => {
                    if (0x0100_0000..=0x0110_FFFF).contains(&keysym) {
                        // Unicode 字符（中文输入法上屏等）：走 KEYEVENTF_UNICODE
                        if let Some(ch) = char::from_u32(keysym - 0x0100_0000) {
                            let mut buf = [0u16; 2];
                            let flags = KEYEVENTF_UNICODE | if down { 0 } else { KEYEVENTF_KEYUP };
                            for u in ch.encode_utf16(&mut buf).iter().copied() {
                                events.push(key_input(0, u, flags));
                            }
                        }
                    } else if let Some((vk, ext)) = keysym_to_vk(keysym) {
                        let mut flags = if down { 0 } else { KEYEVENTF_KEYUP };
                        if ext {
                            flags |= KEYEVENTF_EXTENDEDKEY;
                        }
                        events.push(key_input(vk, 0, flags));
                    }
                }
            }
        }

        if !events.is_empty() {
            let sent = unsafe {
                SendInput(
                    events.len() as u32,
                    events.as_ptr(),
                    std::mem::size_of::<Input>() as i32,
                )
            };
            if sent == 0 {
                return Err("Win32 SendInput 返回 0（可能被 UIPI 提权窗口拦截）".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win32_input_struct_matches_abi_size() {
        #[cfg(target_pointer_width = "64")]
        assert_eq!(std::mem::size_of::<Input>(), 40);
    }

    #[test]
    fn keysym_mapping_covers_common_keys() {
        assert_eq!(keysym_to_vk(b'a' as u32), Some((b'A' as u16, false)));
        assert_eq!(keysym_to_vk(0xff0d), Some((0x0D, false)));
        assert_eq!(keysym_to_vk(0xff52), Some((0x26, true)));
    }
}
