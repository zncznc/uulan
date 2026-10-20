//! 控制端的输入：把窗口事件翻译成协议消息。
//!
//! 这个文件里**没有任何 socket 和窗口代码**，全是纯函数与两个小状态机 —— 因为
//! 「点击位置换算」「滚轮一格是多少」「输入法上屏的字符要不要再发一遍物理键」
//! 这类事最容易写错，又最容易在没有显示器的环境里测。
//!
//! 三个关键约定：
//!
//! 1. **坐标按画面在窗口里的实际位置换算**（[`fit_rect`]），不是按整个窗口。
//!    窗口是 16:9、被控端是 4:3 时会留黑边，按窗口算的话点哪儿都偏移 —— 这条
//!    和 `window::blit` 用同一个几何函数，两边不会跑偏。
//! 2. **滚轮一格 = 120**（Windows WHEEL_DELTA）。触摸板给的是像素增量，要累积够一行再发，
//!    否则一次轻扫就发出几十条滚动消息。
//! 3. **物理键与字符二选一**。输入法已上屏的字符只发 [`C2H::KeyChar`]，不再发物理键，
//!    否则一次输入会打出「拼音字母 + 汉字」两份。

use uulan_protocol::msg::C2H;
use uulan_protocol::vk::KeyTarget;
use winit::keyboard::KeyCode;

/// 一次事件里带上的修饰键状态。用来判断「这是快捷键还是文本输入」。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub sup: bool,
}

impl Mods {
    pub fn from_state(s: winit::keyboard::ModifiersState) -> Self {
        Self {
            ctrl: s.control_key(),
            alt: s.alt_key(),
            shift: s.shift_key(),
            sup: s.super_key(),
        }
    }

    /// 按着 Ctrl/Alt/Win 时一律当快捷键处理（不发字符）——否则 Ctrl+C 会变成「打出 c」。
    pub fn shortcut(&self) -> bool {
        self.ctrl || self.alt || self.sup
    }
}

/// 画面在窗口里实际占据的矩形（像素）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub ox: u32,
    pub oy: u32,
    pub w: u32,
    pub h: u32,
}

/// 画面 → 窗口的摆放：**整数倍放大、从不缩小、居中留边**。
///
/// 与 `window::blit` 共用同一份算法（那个函数负责画，这个函数负责算点击位置）。
/// 两边一旦各算一遍，黑边区域就会点错位置 —— 这是远控里典型的「点不准」。
pub fn fit_rect(frame_w: u32, frame_h: u32, win_w: u32, win_h: u32) -> Rect {
    if frame_w == 0 || frame_h == 0 || win_w == 0 || win_h == 0 {
        return Rect { ox: 0, oy: 0, w: win_w, h: win_h };
    }
    // 只取整数倍：远程桌面上要看清的是文字，缩过的字比重采样的锯齿更难受。
    let scale = (win_w / frame_w).min(win_h / frame_h).max(1);
    let w = (frame_w * scale).min(win_w);
    let h = (frame_h * scale).min(win_h);
    Rect { ox: (win_w - w) / 2, oy: (win_h - h) / 2, w, h }
}

/// 窗口里的光标位置 → 画面里的归一化坐标（0..1）。
///
/// 落在黑边里就夹到最近的边：把鼠标甩出画面外是很常见的操作，夹边比丢弃更符合直觉。
pub fn norm_point(px: f64, py: f64, r: Rect) -> Option<(f32, f32)> {
    if r.w == 0 || r.h == 0 || !px.is_finite() || !py.is_finite() {
        return None;
    }
    let x = ((px - r.ox as f64) / r.w as f64).clamp(0.0, 1.0) as f32;
    let y = ((py - r.oy as f64) / r.h as f64).clamp(0.0, 1.0) as f32;
    Some((x, y))
}

/// 协议里滚轮的单位：一格 = 120（Windows `WHEEL_DELTA`）。
pub const WHEEL_UNIT: f64 = 120.0;
/// 触摸板/精确滚动给的是像素，多少像素算一行。Windows 上大约是 40～50，
/// 取 40：偏向「稍微灵敏一点」，用户觉得快了可以再调。
pub const PIXELS_PER_LINE: f64 = 40.0;

/// 像素增量累加器：攒够一行才发，避免一次轻扫发出几十条消息。
#[derive(Debug, Default)]
pub struct WheelAccum {
    x: f64,
    y: f64,
}

impl WheelAccum {
    /// 吃进一段像素增量，吐出协议单位的 `(dx, dy)`（可能都是 0）。
    pub fn feed_pixels(&mut self, dx: f64, dy: f64) -> (i32, i32) {
        if dx.is_finite() {
            self.x += dx;
        }
        if dy.is_finite() {
            self.y += dy;
        }
        // 只取「整行」的部分，余下的留到下一次 —— 触摸板的手感就看这里。
        let lx = (self.x / PIXELS_PER_LINE).trunc();
        let ly = (self.y / PIXELS_PER_LINE).trunc();
        self.x -= lx * PIXELS_PER_LINE;
        self.y -= ly * PIXELS_PER_LINE;
        ((lx * WHEEL_UNIT) as i32, (ly * WHEEL_UNIT) as i32)
    }
}

/// 一行（LineDelta）→ 协议单位。1 行 = 120，正数 = 向上/向右。
pub fn line_units(dx: f32, dy: f32) -> (i32, i32) {
    if !dx.is_finite() || !dy.is_finite() {
        return (0, 0);
    }
    (
        (dx * WHEEL_UNIT as f32).round() as i32,
        (dy * WHEEL_UNIT as f32).round() as i32,
    )
}

/// 物理键 → Windows VK。
///
/// 表驱动而不是一大坨 `match`：测试要遍历全表，确认**每个能发出去的 VK 被控端都认识**
/// （见本文件末尾的一致性测试）。表里出现重复的 KeyCode 也是 bug。
pub const VK_TABLE: &[(KeyCode, u16)] = &[
    // 字母
    (KeyCode::KeyA, 0x41),
    (KeyCode::KeyB, 0x42),
    (KeyCode::KeyC, 0x43),
    (KeyCode::KeyD, 0x44),
    (KeyCode::KeyE, 0x45),
    (KeyCode::KeyF, 0x46),
    (KeyCode::KeyG, 0x47),
    (KeyCode::KeyH, 0x48),
    (KeyCode::KeyI, 0x49),
    (KeyCode::KeyJ, 0x4a),
    (KeyCode::KeyK, 0x4b),
    (KeyCode::KeyL, 0x4c),
    (KeyCode::KeyM, 0x4d),
    (KeyCode::KeyN, 0x4e),
    (KeyCode::KeyO, 0x4f),
    (KeyCode::KeyP, 0x50),
    (KeyCode::KeyQ, 0x51),
    (KeyCode::KeyR, 0x52),
    (KeyCode::KeyS, 0x53),
    (KeyCode::KeyT, 0x54),
    (KeyCode::KeyU, 0x55),
    (KeyCode::KeyV, 0x56),
    (KeyCode::KeyW, 0x57),
    (KeyCode::KeyX, 0x58),
    (KeyCode::KeyY, 0x59),
    (KeyCode::KeyZ, 0x5a),
    // 数字行
    (KeyCode::Digit0, 0x30),
    (KeyCode::Digit1, 0x31),
    (KeyCode::Digit2, 0x32),
    (KeyCode::Digit3, 0x33),
    (KeyCode::Digit4, 0x34),
    (KeyCode::Digit5, 0x35),
    (KeyCode::Digit6, 0x36),
    (KeyCode::Digit7, 0x37),
    (KeyCode::Digit8, 0x38),
    (KeyCode::Digit9, 0x39),
    // OEM 标点（按美式布局的物理位置，中文 Windows 的物理位置与美式一致）
    (KeyCode::Backquote, 0xc0),
    (KeyCode::Backslash, 0xdc),
    (KeyCode::BracketLeft, 0xdb),
    (KeyCode::BracketRight, 0xdd),
    (KeyCode::Comma, 0xbc),
    (KeyCode::Equal, 0xbb),
    (KeyCode::IntlBackslash, 0xe2),
    (KeyCode::Minus, 0xbd),
    (KeyCode::Period, 0xbe),
    (KeyCode::Quote, 0xde),
    (KeyCode::Semicolon, 0xba),
    (KeyCode::Slash, 0xbf),
    // 修饰键：区分左右，远控里「右 Alt / 右 Ctrl」有时是输入法切换键
    (KeyCode::ShiftLeft, 0xa0),
    (KeyCode::ShiftRight, 0xa1),
    (KeyCode::ControlLeft, 0xa2),
    (KeyCode::ControlRight, 0xa3),
    (KeyCode::AltLeft, 0xa4),
    (KeyCode::AltRight, 0xa5),
    (KeyCode::SuperLeft, 0x5b),
    (KeyCode::SuperRight, 0x5c),
    // 编辑与导航
    (KeyCode::Backspace, 0x08),
    (KeyCode::Tab, 0x09),
    (KeyCode::Enter, 0x0d),
    (KeyCode::Escape, 0x1b),
    (KeyCode::Space, 0x20),
    (KeyCode::CapsLock, 0x14),
    (KeyCode::ContextMenu, 0x5d),
    (KeyCode::Delete, 0x2e),
    (KeyCode::Insert, 0x2d),
    (KeyCode::Home, 0x24),
    (KeyCode::End, 0x23),
    (KeyCode::PageUp, 0x21),
    (KeyCode::PageDown, 0x22),
    (KeyCode::ArrowLeft, 0x25),
    (KeyCode::ArrowUp, 0x26),
    (KeyCode::ArrowRight, 0x27),
    (KeyCode::ArrowDown, 0x28),
    (KeyCode::PrintScreen, 0x2c),
    (KeyCode::ScrollLock, 0x91),
    (KeyCode::Pause, 0x13),
    (KeyCode::NumLock, 0x90),
    // 小键盘
    (KeyCode::Numpad0, 0x60),
    (KeyCode::Numpad1, 0x61),
    (KeyCode::Numpad2, 0x62),
    (KeyCode::Numpad3, 0x63),
    (KeyCode::Numpad4, 0x64),
    (KeyCode::Numpad5, 0x65),
    (KeyCode::Numpad6, 0x66),
    (KeyCode::Numpad7, 0x67),
    (KeyCode::Numpad8, 0x68),
    (KeyCode::Numpad9, 0x69),
    (KeyCode::NumpadAdd, 0x6b),
    (KeyCode::NumpadSubtract, 0x6d),
    (KeyCode::NumpadMultiply, 0x6a),
    (KeyCode::NumpadDivide, 0x6f),
    (KeyCode::NumpadDecimal, 0x6e),
    // 功能键
    (KeyCode::F1, 0x70),
    (KeyCode::F2, 0x71),
    (KeyCode::F3, 0x72),
    (KeyCode::F4, 0x73),
    (KeyCode::F5, 0x74),
    (KeyCode::F6, 0x75),
    (KeyCode::F7, 0x76),
    (KeyCode::F8, 0x77),
    (KeyCode::F9, 0x78),
    (KeyCode::F10, 0x79),
    (KeyCode::F11, 0x7a),
    (KeyCode::F12, 0x7b),
    (KeyCode::F13, 0x7c),
    (KeyCode::F14, 0x7d),
    (KeyCode::F15, 0x7e),
    (KeyCode::F16, 0x7f),
    (KeyCode::F17, 0x80),
    (KeyCode::F18, 0x81),
    (KeyCode::F19, 0x82),
    (KeyCode::F20, 0x83),
    (KeyCode::F21, 0x84),
    (KeyCode::F22, 0x85),
    (KeyCode::F23, 0x86),
    (KeyCode::F24, 0x87),
    // 媒体键
    (KeyCode::AudioVolumeMute, 0xad),
    (KeyCode::AudioVolumeDown, 0xae),
    (KeyCode::AudioVolumeUp, 0xaf),
    (KeyCode::MediaTrackNext, 0xb0),
    (KeyCode::MediaStop, 0xb1),
    (KeyCode::MediaTrackPrevious, 0xb2),
    (KeyCode::MediaPlayPause, 0xb3),
    // 浏览器键
    (KeyCode::BrowserBack, 0xa6),
    (KeyCode::BrowserForward, 0xa7),
    (KeyCode::BrowserRefresh, 0xa8),
    (KeyCode::BrowserStop, 0xa9),
    (KeyCode::BrowserSearch, 0xaa),
    (KeyCode::BrowserFavorites, 0xab),
    (KeyCode::BrowserHome, 0xac),
];

/// 查表：物理键 → VK。不在表里就返回 `None`（不猜）。
pub fn vk_from_key(code: KeyCode) -> Option<u16> {
    VK_TABLE.iter().find(|(c, _)| *c == code).map(|(_, vk)| *vk)
}

/// 一个键事件该发什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAction {
    /// 物理键（按下/松开）。`repeat` 由控制端操作系统产生。
    Vk { vk: u16, down: bool, repeat: bool },
    /// 字符（输入法已上屏 / 没有对应物理键但产生了文本）。
    Chars(Vec<char>),
    /// 什么都不发。
    Ignore,
}

/// 键事件的判定。**纯函数**，所以「输入法场景」可以在没有窗口的测试里跑。
pub fn key_action(
    physical_vk: Option<u16>,
    text: Option<&str>,
    pressed: bool,
    repeat: bool,
    mods: &Mods,
) -> KeyAction {
    if !pressed {
        // 松开事件永远不发字符：字符是「按下+松开」成对发的，再发一遍就会重复一个字。
        return match physical_vk {
            Some(vk) => KeyAction::Vk { vk, down: false, repeat: false },
            None => KeyAction::Ignore,
        };
    }

    // 输入法上屏的非 ASCII 文本：只发字符，不发物理键 —— 否则会打出「拼音字母 + 汉字」。
    if !mods.shortcut() {
        if let Some(t) = text {
            if t.chars().any(|c| (c as u32) > 0x7f) {
                return KeyAction::Chars(t.chars().collect());
            }
        }
    }

    if let Some(vk) = physical_vk {
        return KeyAction::Vk { vk, down: true, repeat };
    }

    // 没有对应物理键（软键盘、某些 IME 的提交事件）：有文本就发字符。
    if let Some(t) = text {
        let chars: Vec<char> = t.chars().filter(|c| !c.is_control()).collect();
        if !chars.is_empty() {
            return KeyAction::Chars(chars);
        }
    }
    KeyAction::Ignore
}

/// 把一次键事件展开成要发的协议消息（字符是按下+松开成对，物理键是一条）。
pub fn key_messages(action: &KeyAction) -> Vec<C2H> {
    match action {
        KeyAction::Vk { vk, down, repeat } => {
            vec![C2H::Key { vk: *vk, down: *down, repeat: *repeat }]
        }
        KeyAction::Chars(chars) => {
            let mut out = Vec::with_capacity(chars.len() * 2);
            for ch in chars {
                let target = KeyTarget::Char { ch: *ch };
                out.push(C2H::KeyChar(uulan_protocol::msg::KeyInput { target: target.clone(), down: true }));
                out.push(C2H::KeyChar(uulan_protocol::msg::KeyInput { target, down: false }));
            }
            out
        }
        KeyAction::Ignore => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uulan_protocol::vk::vk_to_keysym;

    #[test]
    fn fit_rect_letterboxes_at_integer_scale() {
        // 4x2 的画面放进 8x8 的窗口：整数倍 2 → 画成 8x4，上下各留 2 行
        assert_eq!(fit_rect(4, 2, 8, 8), Rect { ox: 0, oy: 2, w: 8, h: 4 });
        // 1280x720 放进 1280x800：1 倍，上下各留 40
        assert_eq!(fit_rect(1280, 720, 1280, 800), Rect { ox: 0, oy: 40, w: 1280, h: 720 });
        // 1920x1080 放进 1280x720：不缩小，1 倍，右边和下边被裁
        let r = fit_rect(1920, 1080, 1280, 720);
        assert_eq!((r.w, r.h), (1280, 720), "窗口比画面小的时候按 1 倍裁切");
    }

    #[test]
    fn fit_rect_handles_degenerate_sizes() {
        assert_eq!(fit_rect(0, 100, 50, 50), Rect { ox: 0, oy: 0, w: 50, h: 50 });
        assert_eq!(fit_rect(100, 100, 0, 0), Rect { ox: 0, oy: 0, w: 0, h: 0 });
    }

    #[test]
    fn norm_point_maps_through_the_letterbox() {
        // 4x2 的画面放在 8x8 窗口里，实际占 y=2..6
        let r = fit_rect(4, 2, 8, 8);
        assert_eq!(norm_point(0.0, 2.0, r), Some((0.0, 0.0)));
        assert_eq!(norm_point(8.0, 6.0, r), Some((1.0, 1.0)));
        let (x, y) = norm_point(4.0, 4.0, r).unwrap();
        assert!((x - 0.5).abs() < 0.01 && (y - 0.5).abs() < 0.01, "{x},{y}");
        // 黑边里：夹到边，而不是丢弃
        assert_eq!(norm_point(4.0, 0.0, r), Some((0.5, 0.0)));
        // 窗口外也夹边
        assert_eq!(norm_point(-100.0, 100.0, r), Some((0.0, 1.0)));
    }

    #[test]
    fn norm_point_rejects_bad_input() {
        let r = Rect { ox: 0, oy: 0, w: 0, h: 0 };
        assert_eq!(norm_point(1.0, 1.0, r), None);
        let r = Rect { ox: 0, oy: 0, w: 10, h: 10 };
        assert_eq!(norm_point(f64::NAN, 1.0, r), None);
    }

    #[test]
    fn every_vk_we_send_is_understood_by_the_host() {
        // 跨 crate 一致性：控制端能发的每个 VK，被控端的映射表都必须认识，
        // 否则表现是「某个键按下去没反应」—— 这种 bug 在真机上要查很久。
        for (code, vk) in VK_TABLE {
            assert!(
                vk_to_keysym(*vk).is_some(),
                "{code:?} → VK {vk:#x} 被控端不认识（协议 vk 表里没有）"
            );
        }
    }

    #[test]
    fn vk_table_has_no_duplicate_keys_or_vks() {
        for (i, (a, av)) in VK_TABLE.iter().enumerate() {
            for (b, bv) in &VK_TABLE[i + 1..] {
                assert_ne!(a, b, "KeyCode {a:?} 重复出现在表里");
                assert_ne!(av, bv, "VK {av:#x} 被两个键共用（{a:?} / {b:?}）");
            }
        }
    }

    #[test]
    fn vk_table_covers_the_keys_that_matter() {
        assert_eq!(vk_from_key(KeyCode::KeyA), Some(0x41));
        assert_eq!(vk_from_key(KeyCode::KeyZ), Some(0x5a));
        assert_eq!(vk_from_key(KeyCode::Digit1), Some(0x31));
        assert_eq!(vk_from_key(KeyCode::F12), Some(0x7b));
        assert_eq!(vk_from_key(KeyCode::ArrowLeft), Some(0x25));
        assert_eq!(vk_from_key(KeyCode::ShiftLeft), Some(0xa0));
        assert_eq!(vk_from_key(KeyCode::ControlRight), Some(0xa3));
        assert_eq!(vk_from_key(KeyCode::Numpad7), Some(0x67));
        assert_eq!(vk_from_key(KeyCode::Enter), Some(0x0d));
        // 没映射的键返回 None，不猜
        assert_eq!(vk_from_key(KeyCode::Fn), None);
    }

    #[test]
    fn ascii_typing_goes_through_the_physical_key() {
        let m = Mods::default();
        assert_eq!(
            key_action(Some(0x41), Some("a"), true, false, &m),
            KeyAction::Vk { vk: 0x41, down: true, repeat: false }
        );
        assert_eq!(
            key_action(Some(0x41), Some("a"), true, true, &m),
            KeyAction::Vk { vk: 0x41, down: true, repeat: true }
        );
    }

    #[test]
    fn ime_text_becomes_chars_not_physical_keys() {
        let m = Mods::default();
        // 输入法上屏「中」：只发字符，绝不发那一下物理键
        assert_eq!(
            key_action(Some(0x4e), Some("中"), true, false, &m),
            KeyAction::Chars(vec!['中'])
        );
        // 一次上屏多个字也照样发出去
        assert_eq!(
            key_action(Some(0x4e), Some("你好"), true, false, &m),
            KeyAction::Chars(vec!['你', '好'])
        );
    }

    #[test]
    fn shortcuts_win_over_text() {
        // 按着 Ctrl 时即使产生了非 ASCII 文本，也按快捷键发（否则 Ctrl+C 会变成「打出 c」）
        let m = Mods { ctrl: true, ..Default::default() };
        assert_eq!(
            key_action(Some(0x43), Some("ç"), true, false, &m),
            KeyAction::Vk { vk: 0x43, down: true, repeat: false }
        );
    }

    #[test]
    fn key_release_never_produces_text() {
        let m = Mods::default();
        assert_eq!(
            key_action(Some(0x41), Some("a"), false, false, &m),
            KeyAction::Vk { vk: 0x41, down: false, repeat: false }
        );
        // 输入法那次按键的松开：什么都不发（字符已经成对发过了）
        assert_eq!(key_action(None, Some("中"), false, false, &m), KeyAction::Ignore);
    }

    #[test]
    fn text_without_a_physical_key_still_types() {
        let m = Mods::default();
        assert_eq!(key_action(None, Some("7"), true, false, &m), KeyAction::Chars(vec!['7']));
        assert_eq!(key_action(None, None, true, false, &m), KeyAction::Ignore);
        // 控制字符（退格之类）不走字符路径
        assert_eq!(key_action(None, Some("\u{8}"), true, false, &m), KeyAction::Ignore);
    }

    #[test]
    fn key_messages_pair_chars_and_pass_vk_through() {
        let msgs = key_messages(&KeyAction::Chars(vec!['中']));
        assert_eq!(msgs.len(), 2, "一个字符应当是按下+松开两条");
        match (&msgs[0], &msgs[1]) {
            (C2H::KeyChar(a), C2H::KeyChar(b)) => {
                assert!(a.down && !b.down);
                assert_eq!(a.target, KeyTarget::Char { ch: '中' });
            }
            _ => panic!("字符没走 KeyChar：{msgs:?}"),
        }
        assert!(key_messages(&KeyAction::Ignore).is_empty());
    }

    #[test]
    fn wheel_lines_are_120_units_each() {
        assert_eq!(line_units(0.0, 1.0), (0, 120));
        assert_eq!(line_units(0.0, -3.0), (0, -360));
        assert_eq!(line_units(1.0, 0.0), (120, 0));
        assert_eq!(line_units(0.0, 0.0), (0, 0));
        assert_eq!(line_units(f32::NAN, 1.0), (0, 0));
    }

    #[test]
    fn pixel_wheel_accumulates_until_a_whole_line() {
        let mut w = WheelAccum::default();
        // 20px 不够一行
        assert_eq!(w.feed_pixels(0.0, 20.0), (0, 0));
        // 再来 20px：正好一行
        assert_eq!(w.feed_pixels(0.0, 20.0), (0, 120));
        // 余额清零了，再 20px 又不够
        assert_eq!(w.feed_pixels(0.0, 20.0), (0, 0));
        // 反向：负数是向下
        assert_eq!(w.feed_pixels(0.0, -60.0), (0, -120));
    }
}