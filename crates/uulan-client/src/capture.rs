//! 鼠标捕获模式（capture）的纯状态机 —— 不碰 winit，逻辑全在这台机器上测。
//!
//! 背景：`C2H::MouseMoveRel` 的发送半边 2026-11-17 才接上（`--inject moverel` 动词），
//! 归档里写着「将来做鼠标捕获模式，传输层就是它」，本轮就是那一笔的兑现。
//!
//! 捕获模式 = 把本地光标锁进窗口并隐藏，移动改发**相对增量**。这时
//! `WindowEvent::CursorMoved` 的绝对坐标失去意义（指针根本不动了），
//! 唯一的移动来源是 `DeviceEvent::MouseMotion` 的原始增量。
//!
//! 两条纪律（都有单测钉住，别改回去）：
//! - **不掺画幅换算**：增量是设备的像素数，原样上线，被控端直接喂
//!   `XTestFakeRelativeMotionEvent`。一旦按缩放比例去乘，「对面指针动了多少」
//!   就不再等于「本地手动了多少」，捕获模式变成漂移器。
//! - **单条不超过 ±10000 就拆开，而不是夹掉**：被控端 mapper 对**单条**增量夹 ±10000
//!   （那是防呆，不是承诺收大数）。一条 25000 的原始事件如果整条发过去，
//!   对面只挪 10000 —— 丢掉的 15000 是用户的真实手移动，必须拆成多条补齐。

/// 被控端 mapper 对单条 `MouseMoveRel` 增量的钳位上限
/// （见 `crates/uulan-host/src/input/mod.rs` 的 `mouse_rel`）。控制端按同一上限**拆分**，
/// 于是「夹掉」永远不会发生。
pub const MAX_REL_STEP: i32 = 10_000;

/// 捕获模式开关 + 亚像素残余累积器。
///
/// 触摸板与慢速移动经常每次只给 0.3 px 的增量；逐个 `trunc` 丢掉小数，
/// 攒一整晚就是一次肉眼可见的漂移。所以分数部分跨事件累积，整数部分才上线。
#[derive(Debug, Default)]
pub struct CaptureCtl {
    enabled: bool,
    frac_x: f64,
    frac_y: f64,
}

impl CaptureCtl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// 设置开关。关掉时把残余清零 —— 半像素的旧账不该在下次开启时凭空多挪一格。
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
        if !on {
            self.frac_x = 0.0;
            self.frac_y = 0.0;
        }
    }

    /// 翻转并返回**新**状态（调用方拿它决定去抓/放指针）。
    pub fn toggle(&mut self) -> bool {
        let on = !self.enabled;
        self.set_enabled(on);
        on
    }

    /// 一条设备级 `MouseMotion` → 若干条要上线的整数增量。
    /// 关闭状态一律返回空（窗口模式下这个回调始终在响，不能靠调用方把关）。
    pub fn motion(&mut self, dx: f64, dy: f64) -> Vec<(i32, i32)> {
        if !self.enabled {
            return Vec::new();
        }
        let ix = absorb(&mut self.frac_x, dx);
        let iy = absorb(&mut self.frac_y, dy);
        let mut out = Vec::new();
        let (mut rx, mut ry) = (ix, iy);
        while rx != 0 || ry != 0 {
            let cx = rx.clamp(-MAX_REL_STEP, MAX_REL_STEP);
            let cy = ry.clamp(-MAX_REL_STEP, MAX_REL_STEP);
            out.push((cx, cy));
            rx -= cx;
            ry -= cy;
        }
        out
    }
}

/// 吃进一个浮点增量，吐出累计出来的整数部分，小数留在 `acc` 里。
fn absorb(acc: &mut f64, delta: f64) -> i32 {
    if !delta.is_finite() {
        // raw input 理论上给得出异常值；一个坏样本不该把指针从此不再上报。
        return 0;
    }
    *acc += delta;
    let whole = acc.trunc();
    *acc -= whole;
    if !acc.is_finite() {
        *acc = 0.0;
    }
    whole.clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_swallows_everything() {
        let mut c = CaptureCtl::new();
        assert!(c.motion(500.0, -500.0).is_empty(), "没开启就一条都不能发");
        c.set_enabled(true);
        assert_eq!(c.motion(1.0, -2.0), vec![(1, -2)]);
        c.set_enabled(false);
        assert!(c.motion(1.0, 1.0).is_empty());
    }

    #[test]
    fn sub_pixel_residual_survives_events() {
        // 主断言用 0.75（二进制下精确）：4 个样本 = 3.0 px，一个都不能少、不能多。
        let mut c = CaptureCtl::new();
        c.set_enabled(true);
        let all: Vec<(i32, i32)> = (0..4).flat_map(|_| c.motion(0.75, 0.75)).collect();
        let sum: (i32, i32) = all.iter().fold((0, 0), |a, p| (a.0 + p.0, a.1 + p.1));
        assert_eq!(sum, (3, 3), "跨事件的亚像素残余必须攒出来：{all:?}");

        // 浮点结合律不是漂移：5 个 0.6 加出来是 0.9999999999999997 而不是 1.0，
        // 第 3 格**记在账上**、下个样本立刻兑现 —— 残余机制的立身之本就是「欠账必还」。
        let mut d = CaptureCtl::new();
        d.set_enabled(true);
        let mut total = 0;
        for _ in 0..5 {
            total += d.motion(0.6, 0.0).iter().map(|p| p.0).sum::<i32>();
        }
        assert_eq!(total, 2, "0.6×5 在 IEEE754 下差一口气到第 3 格：已上线 2，欠 1");
        assert_eq!(d.motion(0.6, 0.0), vec![(1, 0)], "第 6 个样本必须把欠的那格吐出来");
    }

    #[test]
    fn huge_delta_is_split_not_clamped_away() {
        // 25000,-13000：被控端单条只认 ±10000。拆分的语义是「总和一字不少」；
        // 夹掉的语义是「到 10000 为止」—— 后者会悄悄吞掉用户的真实移动。
        let mut c = CaptureCtl::new();
        c.set_enabled(true);
        let chunks = c.motion(25000.0, -13000.0);
        assert_eq!(chunks, vec![(10000, -10000), (10000, -3000), (5000, 0)]);
    }

    #[test]
    fn toggle_reports_new_state_and_clears_residual() {
        let mut c = CaptureCtl::new();
        assert!(c.toggle(), "toggle 返回的是新状态");
        assert!(c.enabled());
        assert!(c.motion(0.7, 0.7).is_empty()); // 只攒下 0.7 的残余
        assert!(!c.toggle());
        assert!(c.motion(0.4, 0.4).is_empty()); // 关闭期间一概不吃
        assert!(c.toggle()); // 重新开启：残余若没清掉，0.7+0.5 会凭空多挪一格
        assert!(c.motion(0.5, 0.5).is_empty());
        assert_eq!(c.motion(0.6, 0.6), vec![(1, 1)]); // 0.5+0.6 = 1.1 → 上线 1
    }

    #[test]
    fn non_finite_sample_does_not_poison_the_accumulator() {
        let mut c = CaptureCtl::new();
        c.set_enabled(true);
        assert!(c.motion(f64::NAN, f64::INFINITY).is_empty());
        assert_eq!(c.motion(2.0, 2.0), vec![(2, 2)], "坏样本之后照常工作");
    }
}
