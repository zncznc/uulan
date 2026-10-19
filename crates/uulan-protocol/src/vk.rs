//! 键位映射：Windows 虚拟键码（VK）→ X11 KeySym。
//!
//! 为什么线上传 VK 而不是 keysym 或 keycode：控制端（Windows / 麒麟）只需要一张表，
//! VK→KeySym→KeyCode 的解析只在被控端做一次，跟着被控端的键盘布局走。
//! 这跟参考项目（uu-remote-linux）的做法一致，它的表已被官方客户端实机验证过；
//! 本文件的映射表从它 `windows_vk_to_keysym` 移植并扩展（原表按 Mac 客户端的行为定，
//! 这里补齐中文 Windows 控制端会发的键：输入法、多媒体、浏览器键、小键盘回车）。
//!
//! 规则：
//! - 字母一律映射到**未加 Shift 的小写** KeySym，Shift 由单独的修饰键事件携带；
//! - 未知 VK 返回 `None` 并被忽略，**绝不**把原始数值当 keycode/keySym 注入（那会乱敲）；
//! - 控制端若开着中文输入法，字符由 IME 合成后会作为独立事件发过来（见 [`KeyTarget::Char`]）。

use serde::{Deserialize, Serialize};

/// 归一化后的按键目标：要么"按某个物理键"，要么"打出某个字符"。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KeyTarget {
    /// 物理键：一个 KeySym，外加"按下期间要临时保持的修饰"。
    Sym {
        #[serde(rename = "sym")]
        keysym: u32,
        #[serde(default)]
        shift: bool,
        #[serde(default)]
        ctrl: bool,
        #[serde(default)]
        alt: bool,
    },
    /// 字符：由被控端解析成 KeySym（必要时动态注册到空闲 keycode，见 uulan-x11）。
    Char { ch: char },
}

impl KeyTarget {
    /// 一个不带修饰的普通 KeySym。
    pub fn plain(keysym: u32) -> Self {
        KeyTarget::Sym { keysym, shift: false, ctrl: false, alt: false }
    }
}

/// Windows VK → 基础 KeySym（未加 Shift 的那个）。返回 None 表示不认识的键，应当忽略。
pub fn vk_to_keysym(vk: u16) -> Option<u32> {
    let ks = match vk {
        // 0..9 数字行：KeySym 等于 ASCII 码
        0x30..=0x39 => vk as u32,
        // A..Z → a..z（Shift 由修饰键事件携带）
        0x41..=0x5a => (vk as u32) + 0x20,
        // 小键盘 0..9 → KP_0..KP_9
        0x60..=0x69 => 0xffb0 + (vk as u32 - 0x60),
        // F1..F24
        0x70..=0x87 => 0xffbe + (vk as u32 - 0x70),
        // 控制键
        0x08 => 0xff08, // BackSpace
        0x09 => 0xff09, // Tab
        0x0c => 0xff0b, // Clear
        0x0d => 0xff0d, // Return
        0x13 => 0xff13, // Pause
        0x14 => 0xffe5, // Caps_Lock
        0x1b => 0xff1b, // Escape
        0x20 => 0x0020, // space
        // 修饰键：区分左右
        0x10 | 0xa0 => 0xffe1, // Shift_L
        0xa1 => 0xffe2,        // Shift_R
        0x11 | 0xa2 => 0xffe3, // Control_L
        0xa3 => 0xffe4,        // Control_R
        0x12 | 0xa4 => 0xffe9, // Alt_L
        0xa5 => 0xffea,        // Alt_R
        0x5b => 0xffeb,        // Super_L（Win 键）
        0x5c => 0xffec,        // Super_R
        0x5d => 0xff67,        // Menu（上下文菜单键）
        // 导航
        0x21 => 0xff55, // Page_Up
        0x22 => 0xff56, // Page_Down
        0x23 => 0xff57, // End
        0x24 => 0xff50, // Home
        0x25 => 0xff51, // Left
        0x26 => 0xff52, // Up
        0x27 => 0xff53, // Right
        0x28 => 0xff54, // Down
        0x2c => 0xff61, // Print
        0x2d => 0xff63, // Insert
        0x2e => 0xffff, // Delete
        // 锁定
        0x90 => 0xff7f, // Num_Lock
        0x91 => 0xff14, // Scroll_Lock
        // 小键盘运算符
        0x6a => 0xffaa, // KP_Multiply
        0x6b => 0xffab, // KP_Add
        0x6c => 0xffac, // KP_Separator
        0x6d => 0xffad, // KP_Subtract
        0x6e => 0xffae, // KP_Decimal
        0x6f => 0xffaf, // KP_Divide
        0x92 => 0xff8d, // KP_Enter
        // OEM 标点：按美式布局的物理位置（中文 Windows 的物理位置与美式一致）
        0xba => 0x003b, // ; :
        0xbb => 0x003d, // = +
        0xbc => 0x002c, // , <
        0xbd => 0x002d, // - _
        0xbe => 0x002e, // . >
        0xbf => 0x002f, // / ?
        0xc0 => 0x0060, // ` ~
        0xdb => 0x005b, // [ {
        0xdc => 0x005c, // \ |
        0xdd => 0x005d, // ] }
        0xde => 0x0027, // ' "
        // 输入法相关：中文 Windows 上输入法状态切换常被映射成这些
        0x15 => 0xff13, // VK_HANGUL → Pause
        0x19 => 0xff14, // VK_HANJA → Scroll_Lock
        0x1c => 0xff0d, // VK_CONVERT → Return
        0x1d => 0xff09, // VK_NONCONVERT → Tab
        0x1e => 0xffe5, // VK_ACCEPT → Caps_Lock
        0x1f => 0xffe5, // VK_MODECHANGE → Caps_Lock
        0x29 => 0xff3c, // VK_SELECT → backslash
        0x03 => 0xff69, // VK_CANCEL → Cancel
        // 多媒体键
        0xad => 0x1008ff12, // XF86AudioMute
        0xae => 0x1008ff11, // XF86AudioLowerVolume
        0xaf => 0x1008ff13, // XF86AudioRaiseVolume
        0xb0 => 0x1008ff17, // XF86AudioNext
        0xb1 => 0x1008ff14, // XF86AudioPlay
        0xb2 => 0x1008ff16, // XF86AudioPrev
        0xb3 => 0x1008ff15, // XF86AudioStop
        // 浏览器键
        0xa6 => 0x1008ff26, // XF86Back
        0xa7 => 0x1008ff27, // XF86Forward
        0xa8 => 0x1008ff29, // XF86Refresh
        0xa9 => 0x1008ff2a, // XF86Stop
        0xaa => 0x1008ff2b, // XF86Search
        0xab => 0x1008ff2c, // XF86Favorites
        0xac => 0x1008ff2d, // XF86HomePage
        _ => return None,
    };
    Some(ks)
}

/// 这个 VK 控制端是否应当作为"修饰键状态"跟踪（而不是发一次 press/release）。
pub fn vk_is_modifier(vk: u16) -> bool {
    matches!(
        vk,
        0x10 | 0xa0 | 0xa1 | 0x11 | 0xa2 | 0xa3 | 0x12 | 0xa4 | 0xa5 | 0x5b | 0x5c
    )
}

/// 把"字符"解析成"哪个 KeySym + 要不要 Shift"。
///
/// 只负责 ASCII 的理论映射。真正能不能打出来由被控端查询当前 X 键盘映射决定，
/// 布局里没有的字符走动态注册。中文字符这里返回 None —— 它们由输入法或动态 keysym 处理。
pub fn char_to_keysym(ch: char) -> Option<(u32, bool)> {
    let c = ch as u32;
    if (0x20..0x7f).contains(&c) {
        let (sym, shift) = match ch {
            'A'..='Z' => (c + 0x20, true),
            'a'..='z' | '0'..='9' | ' ' => (c, false),
            '!' => (b'1' as u32, true),
            '@' => (b'2' as u32, true),
            '#' => (b'3' as u32, true),
            '$' => (b'4' as u32, true),
            '%' => (b'5' as u32, true),
            '^' => (b'6' as u32, true),
            '&' => (b'7' as u32, true),
            '*' => (b'8' as u32, true),
            '(' => (b'9' as u32, true),
            ')' => (b'0' as u32, true),
            '_' => (b'-' as u32, true),
            '+' => (b'=' as u32, true),
            '{' => (b'[' as u32, true),
            '}' => (b']' as u32, true),
            '|' => (b'\\' as u32, true),
            ':' => (b';' as u32, true),
            '"' => (b'\'' as u32, true),
            '<' => (b',' as u32, true),
            '>' => (b'.' as u32, true),
            '?' => (b'/' as u32, true),
            '~' => (b'`' as u32, true),
            other => (other as u32, false),
        };
        return Some((sym, shift));
    }
    if c <= 0xff {
        // Latin-1 里的字母（é、ü 等）：KeySym 等于码点，且映射要求的是"未加 Shift"的形态，
        // 布局里天然就有，直接给。需要 Shift 的那些（如 À）同样能匹配到。
        return Some((c, false));
    }
    None
}

/// 中文标点 → 美式键盘上对应的物理键（字符, 是否需要 Shift）。
///
/// 中文标点最终由被控端输入法产生；这里只告诉调用方"物理上按哪个键"，
/// 让"控制端 IP 直连、被控端装着中文输入法"的场景有机会还原。
pub fn cjk_punct_source(ch: char) -> Option<(char, bool)> {
    let v = match ch {
        '，' => (',', false),
        '。' => ('.', false),
        '、' => ('\\', false),
        '；' => (';', false),
        '：' => (';', true),
        '？' => ('/', true),
        '！' => ('1', true),
        '“' | '”' => ('\'', true),
        '‘' | '’' => ('\'', false),
        '（' => ('9', true),
        '）' => ('0', true),
        '【' => ('[', false),
        '】' => (']', false),
        '《' => (',', true),
        '》' => ('.', true),
        '…' => ('6', true),
        '—' => ('-', true),
        '·' => ('`', true),
        '￥' => ('4', true),
        _ => return None,
    };
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_are_unshifted_and_digits_are_ascii() {
        assert_eq!(vk_to_keysym(0x41), Some(b'a' as u32));
        assert_eq!(vk_to_keysym(0x5a), Some(b'z' as u32));
        for d in b'0'..=b'9' {
            assert_eq!(vk_to_keysym(d as u16), Some(d as u32));
        }
    }

    #[test]
    fn modifiers_keep_left_right_identity() {
        assert_eq!(vk_to_keysym(0x10), Some(0xffe1));
        assert_eq!(vk_to_keysym(0xa0), Some(0xffe1));
        assert_eq!(vk_to_keysym(0xa1), Some(0xffe2));
        assert_eq!(vk_to_keysym(0x11), Some(0xffe3));
        assert_eq!(vk_to_keysym(0xa3), Some(0xffe4));
        assert_eq!(vk_to_keysym(0x12), Some(0xffe9));
        assert_eq!(vk_to_keysym(0xa5), Some(0xffea));
        assert_eq!(vk_to_keysym(0x5b), Some(0xffeb));
        assert_eq!(vk_to_keysym(0x5c), Some(0xffec));
        for vk in [0x10u16, 0xa0, 0xa1, 0x11, 0xa2, 0xa3, 0x12, 0xa4, 0xa5, 0x5b, 0x5c] {
            assert!(vk_is_modifier(vk), "vk {vk:#x}");
        }
        assert!(!vk_is_modifier(0x41));
    }

    #[test]
    fn function_and_keypad_ranges() {
        assert_eq!(vk_to_keysym(0x70), Some(0xffbe)); // F1
        assert_eq!(vk_to_keysym(0x87), Some(0xffd5)); // F24
        assert_eq!(vk_to_keysym(0x60), Some(0xffb0)); // KP_0
        assert_eq!(vk_to_keysym(0x69), Some(0xffb9)); // KP_9
        assert_eq!(vk_to_keysym(0x6b), Some(0xffab)); // KP_Add
        assert_eq!(vk_to_keysym(0x6f), Some(0xffaf)); // KP_Divide
        assert_eq!(vk_to_keysym(0x92), Some(0xff8d)); // KP_Enter
    }

    #[test]
    fn navigation_keys_are_named_not_raw() {
        assert_eq!(vk_to_keysym(0x2e), Some(0xffff)); // Delete，不能是 0x2e
        assert_eq!(vk_to_keysym(0x24), Some(0xff50)); // Home
        assert_eq!(vk_to_keysym(0x25), Some(0xff51)); // Left
        assert_eq!(vk_to_keysym(0x28), Some(0xff54)); // Down
        assert_eq!(vk_to_keysym(0x1b), Some(0xff1b)); // Esc
    }

    #[test]
    fn oem_keys_follow_us_physical_positions() {
        assert_eq!(vk_to_keysym(0xba), Some(b';' as u32));
        assert_eq!(vk_to_keysym(0xbb), Some(b'=' as u32));
        assert_eq!(vk_to_keysym(0xdc), Some(b'\\' as u32));
        assert_eq!(vk_to_keysym(0xde), Some(b'\'' as u32));
        assert_eq!(vk_to_keysym(0xc0), Some(b'`' as u32));
    }

    #[test]
    fn unknown_vks_are_none_not_raw_injection() {
        for vk in [0u16, 1, 2, 7, 0x100, 0xe8, u16::MAX] {
            if let Some(ks) = vk_to_keysym(vk) {
                assert_ne!(ks, vk as u32, "vk {vk:#x} 被当成了裸 keycode");
            }
        }
        assert_eq!(vk_to_keysym(0x100), None);
        // 0xE8 在 Windows 虚拟键表里没有分配
        assert_eq!(vk_to_keysym(0xe8), None);
        assert_eq!(vk_to_keysym(0), None, "0 是「没有按键」，不能映射成任何键");
    }

    #[test]
    fn f13_to_f24_are_real_windows_vks_not_unknown() {
        // 回归：早先我把 0x7f 当成「未知 VK」写进了测试，其实它是 VK_F16。
        // F13–F24（0x7C–0x87）是真实存在的虚拟键，对应 XK_F13–XK_F24（0xFFCA–0xFFD5）。
        assert_eq!(vk_to_keysym(0x7c), Some(0xffca)); // F13
        assert_eq!(vk_to_keysym(0x7f), Some(0xffcd)); // F16
        assert_eq!(vk_to_keysym(0x87), Some(0xffd5)); // F24
        // 再往后就越界了
        assert_eq!(vk_to_keysym(0x88), None);
    }

    #[test]
    fn shift_and_super_are_never_raw_values() {
        // 回归：参考项目曾把 vk 本身当 keysym 返回（160 → 160），结果打不出 Shift。
        assert_ne!(vk_to_keysym(160), Some(160));
        assert_ne!(vk_to_keysym(0x5b), Some(0x5b));
    }

    #[test]
    fn media_keys_map_to_xf86() {
        assert_eq!(vk_to_keysym(0xad), Some(0x1008ff12));
        assert_eq!(vk_to_keysym(0xaf), Some(0x1008ff13));
        assert_eq!(vk_to_keysym(0xb1), Some(0x1008ff14));
    }

    #[test]
    fn ascii_chars_pick_the_unshifted_key_plus_shift() {
        assert_eq!(char_to_keysym('a'), Some((b'a' as u32, false)));
        assert_eq!(char_to_keysym('A'), Some((b'a' as u32, true)));
        assert_eq!(char_to_keysym('1'), Some((b'1' as u32, false)));
        assert_eq!(char_to_keysym('!'), Some((b'1' as u32, true)));
        assert_eq!(char_to_keysym('{'), Some((b'[' as u32, true)));
        assert_eq!(char_to_keysym(':'), Some((b';' as u32, true)));
        assert_eq!(char_to_keysym('~'), Some((b'`' as u32, true)));
        assert_eq!(char_to_keysym('/'), Some((b'/' as u32, false)));
        assert_eq!(char_to_keysym(' '), Some((0x20, false)));
        // 非 ASCII 不在这里硬解
        assert_eq!(char_to_keysym('中'), None);
    }

    #[test]
    fn cjk_punctuation_points_at_physical_keys() {
        assert_eq!(cjk_punct_source('，'), Some((',', false)));
        assert_eq!(cjk_punct_source('：'), Some((';', true)));
        assert_eq!(cjk_punct_source('“'), Some(('\'', true)));
        assert_eq!(cjk_punct_source('a'), None);
    }

    #[test]
    fn key_target_wire_shape() {
        let t = KeyTarget::Sym { keysym: 0xffe1, shift: false, ctrl: false, alt: false };
        let s = serde_json::to_string(&t).unwrap();
        assert_eq!(s, r#"{"kind":"sym","sym":65505,"shift":false,"ctrl":false,"alt":false}"#);
        let c = KeyTarget::Char { ch: '中' };
        let s = serde_json::to_string(&c).unwrap();
        assert_eq!(s, r#"{"kind":"char","ch":"中"}"#);
        assert_eq!(serde_json::from_str::<KeyTarget>(&s).unwrap(), c);
    }

    /// KeyTarget 的默认字段可以省略：老客户端只发 sym 也能解析。
    #[test]
    fn key_target_modifiers_are_optional() {
        let t: KeyTarget = serde_json::from_str(r#"{"kind":"sym","sym":97}"#).unwrap();
        assert_eq!(t, KeyTarget::plain(97));
    }
}
