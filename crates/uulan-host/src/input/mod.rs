//! 键鼠注入：把协议消息翻译成「注入指令」，再交给具体后端执行。
//!
//! 分两层是有意的：
//!
//! - [`Mapper`] 是**纯状态机**（协议消息 → [`Inject`]），完全不碰 X11。
//!   所以它在任何平台上都能跑单测 —— 坐标换算、滚轮格数、按键按下/松开配对、
//!   自动连发的去重、退出时防止卡键，这些最容易写错的地方都在这儿被钉死。
//! - [`InputSink`] 负责真的打到系统里。Linux + X11 下是 XTest（见 [`x11`]）；
//!   其他平台是 [`NullSink`]，它会**如实报错**（控制端会收到 `InputError`），
//!   而不是静默把消息吃掉 —— 静默吞掉的表现是「点了没反应」，最难查。
//!
//! ## 为什么 VK 要在被控端翻译
//!
//! 控制端只管发 Windows VK（或 IME 上屏后的字符），VK→KeySym 的解析只在被控端做一次，
//! 跟着被控端的键盘布局走。未知 VK 一律**不注入**：把 VK 数值当 keycode 塞给 X 会乱敲。

use uulan_protocol::msg::MouseButton;
use uulan_protocol::vk::{char_to_keysym, vk_to_keysym, KeyTarget};

use crate::capture::CaptureInput;

/// KeySym：Shift_L / Control_L / Alt_L。
const SHIFT_L: u32 = 0xffe1;
const CONTROL_L: u32 = 0xffe3;
const ALT_L: u32 = 0xffe9;

/// 一条与后端无关的注入指令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inject {
    /// 绝对移动，单位是**被控端屏幕像素**（协议里是 0..1 的归一化坐标）。
    MoveAbs { x: i32, y: i32 },
    /// 相对移动，单位像素。
    MoveRel { dx: i32, dy: i32 },
    /// X11 按钮号：1 左 / 2 中 / 3 右 / 8 侧键1 / 9 侧键2。
    Button { button: u8, down: bool },
    /// 滚轮：X11 里就是「按一下按钮」——4 上 / 5 下 / 6 左 / 7 右，`count` 是格数。
    Wheel { button: u8, count: u32 },
    Key { keysym: u32, down: bool },
}

/// 注入后端。
pub trait InputSink: Send {
    fn name(&self) -> &'static str;
    /// 执行一批指令。整批执行完再 flush（每格滚轮/每次移动都 flush 会把 X 打成瓶颈）。
    fn apply(&mut self, batch: &[Inject]) -> Result<(), String>;
    /// 会话结束：恢复被改动过的全局状态（X11 的 autorepeat、动态 keycode）。
    fn finish(&mut self) {}
}

/// 没有可用的注入后端。**报错而不是装作成功。**
pub struct NullSink {
    reason: String,
}

impl NullSink {
    pub fn new(reason: impl Into<String>) -> Self {
        Self { reason: reason.into() }
    }
}

impl InputSink for NullSink {
    fn name(&self) -> &'static str {
        "none"
    }

    fn apply(&mut self, _batch: &[Inject]) -> Result<(), String> {
        Err(self.reason.clone())
    }
}

/// 协议消息 → 注入指令的状态机。
///
/// 状态只有一个用途：**别让键卡住**。控制端丢了一个松开事件（网络断了、客户端崩了），
/// 被控端的键就会一直按住 —— 在远控里这是很严重的故障（相当于远程按住了 Ctrl）。
/// 所以这里记下「当前按着哪些键/按钮」，会话结束一律 [`Mapper::release_all`]。
pub struct Mapper {
    w: u32,
    h: u32,
    /// 当前按下的 KeySym。**有序保存**（不是集合）：收尾时要按「后按先松」的顺序释放，
    /// 修饰键才会最后起来 —— 集合的字典序会把 Ctrl 先松开，跟真人操作相反。
    keys: Vec<u32>,
    /// 当前按下的 X11 按钮号，同样按按下顺序保存。
    buttons: Vec<u8>,
    /// 为打一个字符而临时按下的修饰键（松开时一并释放）。
    temp_mods: Vec<u32>,
}

impl Mapper {
    pub fn new(screen_w: u32, screen_h: u32) -> Self {
        Self {
            w: screen_w.max(1),
            h: screen_h.max(1),
            keys: Vec::new(),
            buttons: Vec::new(),
            temp_mods: Vec::new(),
        }
    }

    /// 屏幕尺寸变了（分辨率热切换）时更新，旧坐标不需要重算。
    pub fn set_screen(&mut self, w: u32, h: u32) {
        self.w = w.max(1);
        self.h = h.max(1);
    }

    pub fn held_keys(&self) -> usize {
        self.keys.len()
    }

    pub fn held_buttons(&self) -> usize {
        self.buttons.len()
    }

    /// 归一化坐标（0..1）→ 屏幕像素。
    ///
    /// 越界一律夹到边缘：远控里把鼠标甩到窗口外面很常见，夹边比丢弃更符合直觉。
    /// NaN / 无穷直接丢弃 —— 换算出来会是个巨大的负数，X 会拒绝或者把指针扔到屏幕角落。
    pub fn mouse_abs(&self, x: f32, y: f32) -> Option<Inject> {
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        let max_x = self.w.saturating_sub(1) as f32;
        let max_y = self.h.saturating_sub(1) as f32;
        let px = (x.clamp(0.0, 1.0) * max_x).round() as i32;
        let py = (y.clamp(0.0, 1.0) * max_y).round() as i32;
        Some(Inject::MoveAbs {
            x: px.clamp(0, max_x as i32),
            y: py.clamp(0, max_y as i32),
        })
    }

    /// 相对移动。0,0 不值得发；过大的值夹一下（客户端算错时不至于把指针甩飞）。
    pub fn mouse_rel(&self, dx: i32, dy: i32) -> Option<Inject> {
        if dx == 0 && dy == 0 {
            return None;
        }
        const LIM: i32 = 10_000;
        Some(Inject::MoveRel {
            dx: dx.clamp(-LIM, LIM),
            dy: dy.clamp(-LIM, LIM),
        })
    }

    /// 按钮按下/松开。同一按钮重复按下（客户端重发）只算一次。
    pub fn button(&mut self, b: MouseButton, down: bool) -> Vec<Inject> {
        let n = x11_button(b);
        if down {
            if !self.buttons.push_new(n) {
                return Vec::new();
            }
            vec![Inject::Button { button: n, down: true }]
        } else {
            if !self.buttons.remove_val(&n) {
                return Vec::new();
            }
            vec![Inject::Button { button: n, down: false }]
        }
    }

    /// 滚轮。协议单位是「一格 = 120」（Windows WHEEL_DELTA）。
    pub fn wheel(&self, dx: i32, dy: i32) -> Vec<Inject> {
        let mut out = Vec::new();
        // 一次最多 50 格：防御性的上限，防止控制端算错时把 X 刷爆。
        let steps = |v: i32| (v.unsigned_abs() / 120).clamp(0, 50);
        let up = steps(dy);
        if up > 0 {
            out.push(Inject::Wheel { button: if dy > 0 { 4 } else { 5 }, count: up });
        }
        let side = steps(dx);
        if side > 0 {
            out.push(Inject::Wheel { button: if dx > 0 { 7 } else { 6 }, count: side });
        }
        out
    }

    /// 物理键（Windows VK）。`repeat=true` 是控制端操作系统产生的连发。
    ///
    /// 被控端的 X 全局 autorepeat 在会话开始时就被关掉了（见 [`x11::X11Sink`]），
    /// 所以连发的唯一来源就是这里：收到一次 repeat 就再发一次「按下」，
    /// **不配对的松开**。这样两端的连发速率、首次延迟都跟控制端一致，不会叠成两倍。
    pub fn key(&mut self, vk: u16, down: bool, repeat: bool) -> Vec<Inject> {
        let Some(ks) = vk_to_keysym(vk) else {
            // 不认识的键绝不注入。把 VK 数值当 keycode 用会乱敲一通。
            return Vec::new();
        };
        if down {
            let held = self.keys.holds(&ks);
            if !held {
                self.keys.push_new(ks);
            }
            if !held || repeat {
                return vec![Inject::Key { keysym: ks, down: true }];
            }
            // 重复的「按下」且不是连发：客户端重发了，忽略。
            Vec::new()
        } else {
            if !self.keys.remove_val(&ks) {
                // 没按过就松开：忽略。否则会往 X 里塞一个无配对的 KeyRelease。
                return Vec::new();
            }
            vec![Inject::Key { keysym: ks, down: false }]
        }
    }

    /// 字符注入（输入法上屏、软键盘）。
    ///
    /// 控制端是成对发的（先 `down` 再 `down=false`）：`down` 时按下（必要时临时按住 Shift），
    /// 松开时释放按键与那些临时修饰。这样一次字符正好打出一个字，不多不少。
    pub fn key_char(&mut self, target: &KeyTarget, down: bool) -> Vec<Inject> {
        let Some((keysym, shift, ctrl, alt)) = resolve_target(target) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if down {
            if shift && self.hold_mod(SHIFT_L, &mut out) {
                self.temp_mods.push(SHIFT_L);
            }
            if ctrl && self.hold_mod(CONTROL_L, &mut out) {
                self.temp_mods.push(CONTROL_L);
            }
            if alt && self.hold_mod(ALT_L, &mut out) {
                self.temp_mods.push(ALT_L);
            }
            if self.keys.push_new(keysym) {
                out.push(Inject::Key { keysym, down: true });
            }
        } else {
            if self.keys.remove_val(&keysym) {
                out.push(Inject::Key { keysym, down: false });
            }
            for m in std::mem::take(&mut self.temp_mods).into_iter().rev() {
                if self.keys.remove_val(&m) {
                    out.push(Inject::Key { keysym: m, down: false });
                }
            }
        }
        out
    }

    /// 会话结束/异常时的兜底：把还按着的键和按钮全松开。
    /// 倒序释放，跟按下顺序相反 —— 修饰键最后才起来，和真人操作一致。
    pub fn release_all(&mut self) -> Vec<Inject> {
        let mut out = Vec::new();
        for ks in std::mem::take(&mut self.keys).into_iter().rev() {
            out.push(Inject::Key { keysym: ks, down: false });
        }
        for b in std::mem::take(&mut self.buttons).into_iter().rev() {
            out.push(Inject::Button { button: b, down: false });
        }
        self.temp_mods.clear();
        out
    }

    /// 临时按住一个修饰键；已经在按着（用户自己按着 Shift）就不重复按。返回是否由我们按下。
    fn hold_mod(&mut self, keysym: u32, out: &mut Vec<Inject>) -> bool {
        if self.keys.holds(&keysym) {
            return false;
        }
        self.keys.push(keysym);
        out.push(Inject::Key { keysym, down: true });
        true
    }
}

/// 按下的键/按钮要保序，用 `Vec` 而不是 `BTreeSet`；这两个小工具让语义读起来还是集合。
trait PressedSet<T> {
    fn holds(&self, v: &T) -> bool;
    fn push_new(&mut self, v: T) -> bool;
    fn remove_val(&mut self, v: &T) -> bool;
}

impl<T: PartialEq> PressedSet<T> for Vec<T> {
    fn holds(&self, v: &T) -> bool {
        self.iter().any(|x| x == v)
    }

    /// 追加并返回「是不是新按下」。
    fn push_new(&mut self, v: T) -> bool {
        if self.holds(&v) {
            return false;
        }
        self.push(v);
        true
    }

    fn remove_val(&mut self, v: &T) -> bool {
        match self.iter().position(|x| x == v) {
            Some(i) => {
                self.remove(i);
                true
            }
            None => false,
        }
    }
}

/// 协议按钮 → X11 按钮号。
pub fn x11_button(b: MouseButton) -> u8 {
    match b {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 3,
        MouseButton::X1 => 8,
        MouseButton::X2 => 9,
    }
}

/// 协议里的 `KeyTarget` → `(KeySym, 需要 Shift, 需要 Ctrl, 需要 Alt)`。
fn resolve_target(t: &KeyTarget) -> Option<(u32, bool, bool, bool)> {
    match t {
        KeyTarget::Sym { keysym, shift, ctrl, alt } => Some((*keysym, *shift, *ctrl, *alt)),
        KeyTarget::Char { ch } => {
            if let Some((sym, shift)) = char_to_keysym(*ch) {
                return Some((sym, shift, false, false));
            }
            // 非 ASCII：X11 的 Unicode KeySym 约定是 0x01000000 + 码点。
            // 布局里通常没有这个 KeySym，由后端动态注册到一个空闲 keycode（xdotool 同款做法）。
            let c = *ch as u32;
            Some((0x0100_0000 + c, false, false, false))
        }
    }
}

/// 按采集源挑一个注入后端。
pub fn default_sink(capture: &CaptureInput) -> Box<dyn InputSink> {
    if capture.display().is_none() {
        // 合成画面：没有可注入的桌面会话，如实说清楚。
        return Box::new(NullSink::new(
            "被控端采集源是合成画面（lavfi），没有可注入的桌面会话",
        ));
    }
    #[cfg(target_os = "linux")]
    {
        let disp = capture.display();
        match x11::X11Sink::open(disp.as_deref()) {
            Ok(s) => Box::new(s),
            Err(e) => {
                // 变量名别叫 display：tracing 的 `?display` 会解析成它自己的 display() 辅助函数。
                tracing::warn!(error = %e, disp = ?disp, "XTest 注入后端不可用");
                Box::new(NullSink::new(format!("XTest 注入不可用：{e}")))
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        Box::new(NullSink::new(format!(
            "本平台（{}）不支持 X11 输入注入：被控端需要跑在麒麟/Linux 的 X11 会话里",
            std::env::consts::OS
        )))
    }
}

#[cfg(target_os = "linux")]
pub mod x11;

#[cfg(test)]
mod tests {
    use super::*;

    fn m(w: u32, h: u32) -> Mapper {
        Mapper::new(w, h)
    }

    #[test]
    fn mouse_abs_maps_normalized_to_pixels() {
        let mp = m(1920, 1080);
        assert_eq!(mp.mouse_abs(0.0, 0.0), Some(Inject::MoveAbs { x: 0, y: 0 }));
        assert_eq!(mp.mouse_abs(1.0, 1.0), Some(Inject::MoveAbs { x: 1919, y: 1079 }));
        // 正中：1920/2 = 960 附近（用 w-1 换算，允许 1 像素误差）
        match mp.mouse_abs(0.5, 0.5).unwrap() {
            Inject::MoveAbs { x, y } => {
                assert!((x - 960).abs() <= 1, "x={x}");
                assert!((y - 540).abs() <= 1, "y={y}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn mouse_abs_clamps_and_rejects_non_finite() {
        let mp = m(800, 600);
        // 越界夹边，而不是丢弃
        assert_eq!(mp.mouse_abs(-3.0, 9.0), Some(Inject::MoveAbs { x: 0, y: 599 }));
        assert_eq!(mp.mouse_abs(f32::NAN, 0.5), None);
        assert_eq!(mp.mouse_abs(0.5, f32::INFINITY), None);
    }

    #[test]
    fn mouse_rel_skips_zero_and_clamps() {
        let mp = m(100, 100);
        assert_eq!(mp.mouse_rel(0, 0), None);
        assert_eq!(mp.mouse_rel(5, -7), Some(Inject::MoveRel { dx: 5, dy: -7 }));
        assert_eq!(mp.mouse_rel(1 << 20, 0), Some(Inject::MoveRel { dx: 10_000, dy: 0 }));
    }

    #[test]
    fn buttons_map_to_x11_numbers() {
        assert_eq!(x11_button(MouseButton::Left), 1);
        assert_eq!(x11_button(MouseButton::Middle), 2);
        assert_eq!(x11_button(MouseButton::Right), 3);
        assert_eq!(x11_button(MouseButton::X1), 8);
        assert_eq!(x11_button(MouseButton::X2), 9);

        let mut mp = m(100, 100);
        assert_eq!(mp.button(MouseButton::Right, true), vec![Inject::Button { button: 3, down: true }]);
        // 重复按下只算一次（客户端重发不该变成双击）
        assert!(mp.button(MouseButton::Right, true).is_empty());
        assert_eq!(mp.button(MouseButton::Right, false), vec![Inject::Button { button: 3, down: false }]);
        // 没按过就松开：忽略
        assert!(mp.button(MouseButton::Right, false).is_empty());
    }

    #[test]
    fn wheel_uses_120_units_per_notch() {
        let mp = m(100, 100);
        assert_eq!(mp.wheel(0, 120), vec![Inject::Wheel { button: 4, count: 1 }]);
        assert_eq!(mp.wheel(0, -240), vec![Inject::Wheel { button: 5, count: 2 }]);
        assert_eq!(mp.wheel(120, 0), vec![Inject::Wheel { button: 7, count: 1 }]);
        assert_eq!(mp.wheel(-120, 0), vec![Inject::Wheel { button: 6, count: 1 }]);
        // 不足一格不动作（噪声不能被放大成滚动）
        assert!(mp.wheel(0, 60).is_empty());
        // 同时有横竖
        assert_eq!(
            mp.wheel(120, 120),
            vec![
                Inject::Wheel { button: 4, count: 1 },
                Inject::Wheel { button: 7, count: 1 },
            ]
        );
        // 防御性上限
        assert_eq!(mp.wheel(0, 120 * 1000), vec![Inject::Wheel { button: 4, count: 50 }]);
    }

    #[test]
    fn unknown_vk_is_never_injected() {
        let mut mp = m(100, 100);
        for vk in [0u16, 0x100, 0xe8, u16::MAX] {
            assert!(mp.key(vk, true, false).is_empty(), "vk {vk:#x} 不该产生注入");
        }
        assert_eq!(mp.held_keys(), 0);
    }

    #[test]
    fn key_press_release_is_paired() {
        let mut mp = m(100, 100);
        assert_eq!(
            mp.key(0x41, true, false),
            vec![Inject::Key { keysym: 0x61, down: true }]
        );
        assert_eq!(mp.held_keys(), 1);
        assert_eq!(
            mp.key(0x41, false, false),
            vec![Inject::Key { keysym: 0x61, down: false }]
        );
        assert_eq!(mp.held_keys(), 0);
        // 多出来的松开忽略：不能往 X 里塞无配对的 KeyRelease
        assert!(mp.key(0x41, false, false).is_empty());
    }

    #[test]
    fn autorepeat_repeats_the_press_without_a_release() {
        let mut mp = m(100, 100);
        assert_eq!(mp.key(0x41, true, false).len(), 1);
        // 连发：再发一次按下，不配松开（X 的全局 autorepeat 已被关掉，不会叠成双倍）
        assert_eq!(
            mp.key(0x41, true, true),
            vec![Inject::Key { keysym: 0x61, down: true }]
        );
        assert_eq!(mp.held_keys(), 1, "连发不该改变按下集合");
        // 重复的「按下」但不是连发：忽略
        assert!(mp.key(0x41, true, false).is_empty());
        assert_eq!(mp.key(0x41, false, false).len(), 1);
        assert_eq!(mp.held_keys(), 0);
    }

    #[test]
    fn release_all_clears_stuck_keys_and_buttons() {
        let mut mp = m(100, 100);
        mp.key(0x11, true, false); // Ctrl
        mp.key(0x41, true, false); // A
        mp.button(MouseButton::Left, true);
        let batch = mp.release_all();
        assert_eq!(
            batch,
            vec![
                // 倒序：A 先松，Ctrl 后松
                Inject::Key { keysym: 0x61, down: false },
                Inject::Key { keysym: 0xffe3, down: false },
                Inject::Button { button: 1, down: false },
            ]
        );
        assert_eq!(mp.held_keys(), 0);
        assert_eq!(mp.held_buttons(), 0);
        // 再放一次是空的
        assert!(mp.release_all().is_empty());
    }

    #[test]
    fn char_injection_wraps_shift_around_uppercase() {
        let mut mp = m(100, 100);
        let t = KeyTarget::Char { ch: 'A' };
        assert_eq!(
            mp.key_char(&t, true),
            vec![
                Inject::Key { keysym: SHIFT_L, down: true },
                Inject::Key { keysym: 0x61, down: true },
            ]
        );
        assert_eq!(
            mp.key_char(&t, false),
            vec![
                Inject::Key { keysym: 0x61, down: false },
                Inject::Key { keysym: SHIFT_L, down: false },
            ]
        );
        assert_eq!(mp.held_keys(), 0);
    }

    #[test]
    fn char_injection_of_lowercase_needs_no_modifier() {
        let mut mp = m(100, 100);
        let t = KeyTarget::Char { ch: 'a' };
        assert_eq!(mp.key_char(&t, true), vec![Inject::Key { keysym: 0x61, down: true }]);
        assert_eq!(mp.key_char(&t, false), vec![Inject::Key { keysym: 0x61, down: false }]);
    }

    #[test]
    fn cjk_char_uses_the_unicode_keysym_convention() {
        let mut mp = m(100, 100);
        let t = KeyTarget::Char { ch: '中' };
        let out = mp.key_char(&t, true);
        assert_eq!(out, vec![Inject::Key { keysym: 0x0100_4E2D, down: true }]);
        let up = mp.key_char(&t, false);
        assert_eq!(up, vec![Inject::Key { keysym: 0x0100_4E2D, down: false }]);
    }

    #[test]
    fn shift_already_held_is_not_pressed_twice() {
        let mut mp = m(100, 100);
        // 用户自己按着 Shift
        mp.key(0x10, true, false);
        let out = mp.key_char(&KeyTarget::Char { ch: 'A' }, true);
        assert_eq!(out, vec![Inject::Key { keysym: 0x61, down: true }], "不该再按一次 Shift");
        let up = mp.key_char(&KeyTarget::Char { ch: 'A' }, false);
        assert_eq!(
            up,
            vec![Inject::Key { keysym: 0x61, down: false }],
            "松开字符时不该松开用户的 Shift"
        );
        // Shift 仍在按下集合里，等着控制端自己发松开
        assert_eq!(mp.held_keys(), 1);
    }

    #[test]
    fn sym_target_carries_its_modifiers() {
        let mut mp = m(100, 100);
        let t = KeyTarget::Sym { keysym: 0x63, shift: false, ctrl: true, alt: false };
        assert_eq!(
            mp.key_char(&t, true),
            vec![
                Inject::Key { keysym: CONTROL_L, down: true },
                Inject::Key { keysym: 0x63, down: true },
            ]
        );
        let up = mp.key_char(&t, false);
        assert_eq!(up.len(), 2);
        assert_eq!(mp.held_keys(), 0);
    }

    #[test]
    fn null_sink_explains_why_it_cannot_inject() {
        let mut s = NullSink::new("测试环境没有 X11");
        assert_eq!(s.name(), "none");
        let e = s.apply(&[Inject::Key { keysym: 0x61, down: true }]).unwrap_err();
        assert!(e.contains("X11"), "{e}");
    }

    #[test]
    fn set_screen_updates_coordinate_range() {
        let mut mp = m(1000, 1000);
        mp.set_screen(100, 50);
        assert_eq!(mp.mouse_abs(1.0, 1.0), Some(Inject::MoveAbs { x: 99, y: 49 }));
        mp.set_screen(0, 0); // 不能把坐标系搞成 0 宽度（会除以 0）
        assert_eq!(mp.mouse_abs(1.0, 1.0), Some(Inject::MoveAbs { x: 0, y: 0 }));
    }
}