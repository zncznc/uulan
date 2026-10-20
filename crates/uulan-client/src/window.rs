//! 控制端窗口：把 [`watch`] 推来的最新一帧画到屏幕上。
//!
//! ## 线程模型（关键，别改错）
//!
//! winit 的事件循环**必须占住主线程**，而画面的接收与解码跑在 tokio 运行时里。
//! 所以这里把两者拆开：tokio 运行时扔到一个后台线程，主线程专心跑 winit。
//! 窗口的绘制挂在 `AboutToWait` 上——那是 winit 0.30 里「该画一帧了」的位置，
//! 配合 `ControlFlow::Poll` 形成连续的渲染循环。
//!
//! 两个线程之间只共享两样东西，都是无锁的：
//! - `watch::Receiver<Option<Frame>>`：tokio 侧不停写「最新一帧」，窗口侧每次绘制
//!   取当前值。**用 watch 而不是队列是有意的**：远控要的是「最新画面」，
//!   追不上时应该丢旧帧，而不是排队把延迟越堆越高。
//! - `Arc<WatchStats>`：状态栏读的统计。
//!
//! ## 为什么用 softbuffer 而不是 GTK / OpenGL
//!
//! 参考项目在这一点上栽过：**GTK 窗口在锁屏/息屏时停止重绘**，控制端界面会假死。
//! softbuffer 直接把一块内存位图交给窗口系统，没有自己的事件循环、没有 GPU 上下文，
//! 在麒麟 V10 的自研桌面（UKUI/openKylin）上也最不容易出意外。

use std::num::NonZeroU32;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::control::InputSender;
use crate::input::{fit_rect, key_action, key_messages, line_units, norm_point, vk_from_key, Mods,
                  Rect, WheelAccum};
use crate::watch::{Frame, WatchStats, WatchStatsSnapshot};
use uulan_protocol::msg::{C2H, MouseButton as ProtoButton};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::window::{Window, WindowId};

/// 窗口标题里显示的统计，约每秒刷新一次。
#[derive(Debug, Clone, Default)]
pub struct Overlay {
    pub host_name: String,
    pub screen: (u32, u32),
    pub fps: f64,
    pub kbps: f64,
    pub dropped: u64,
    /// 首帧延迟（ms），未收到首帧时为 None。
    pub first_frame_ms: Option<u64>,
}

impl Overlay {
    fn title(&self, w: u32, h: u32) -> String {
        let mut s = format!(
            "uulan · {} · {}x{} · 画面 {}x{} · {:.0} fps · {:.0} kbps",
            self.host_name, self.screen.0, self.screen.1, w, h, self.fps, self.kbps
        );
        if self.dropped > 0 {
            s.push_str(&format!(" · 丢帧 {}", self.dropped));
        }
        if let Some(ms) = self.first_frame_ms {
            s.push_str(&format!(" · 首帧 {}ms", ms));
        }
        s
    }
}

struct App {
    rx: watch::Receiver<Option<Frame>>,
    stats: Arc<WatchStats>,
    overlay: Overlay,
    /// 缩放后的位图（0RGB），大小 = 窗口物理尺寸。
    buf: Vec<u32>,
    window: Option<Arc<Window>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    context: Option<softbuffer::Context<Arc<Window>>>,
    painted: u64,
    last_title: std::time::Instant,
    /// 已经画过的帧序号（用 at 判断，避免同帧重画）。
    last_frame_at: Option<std::time::Instant>,
    /// 上一秒的统计，用来看增量。
    prev: WatchStatsSnapshot,
    overlay_since: std::time::Instant,
    /// 到点自动关窗（自测/脚本化验证用；None = 一直开着）。
    deadline: Option<std::time::Instant>,
    /// 键鼠上报通道。`None` = 只读模式（`--no-input`，或没有图形会话的被控端）。
    input: Option<InputSender>,
    /// 当前修饰键状态。判断「这是快捷键还是文本输入」用。
    mods: Mods,
    /// 最近一帧的画面尺寸，用来把光标位置换算成画面坐标。
    frame_sz: Option<(u32, u32)>,
    /// 触摸板的像素滚动要攒够一行才发。
    wheel: WheelAccum,
    /// 上报了多少条键鼠消息（收工时的证据数字）。
    input_sent: u64,
}

impl App {
    fn new(
        rx: watch::Receiver<Option<Frame>>,
        stats: Arc<WatchStats>,
        overlay: Overlay,
        deadline: Option<std::time::Instant>,
        input: Option<InputSender>,
    ) -> Self {
        Self {
            rx,
            stats,
            overlay,
            buf: Vec::new(),
            window: None,
            surface: None,
            context: None,
            painted: 0,
            last_title: std::time::Instant::now(),
            last_frame_at: None,
            prev: WatchStatsSnapshot::default(),
            overlay_since: std::time::Instant::now(),
            deadline,
            input,
            mods: Mods::default(),
            frame_sz: None,
            wheel: WheelAccum::default(),
            input_sent: 0,
        }
    }

    /// 画面在窗口里的实际位置。光标换算必须用它，不能用整个窗口 —— 否则留黑边时点哪儿都偏。
    fn image_rect(&self) -> Option<Rect> {
        let (fw, fh) = self.frame_sz?;
        let size = self.window.as_ref()?.inner_size();
        Some(fit_rect(fw, fh, size.width, size.height))
    }

    fn send_input(&mut self, m: C2H) {
        if let Some(i) = &self.input {
            if i.send(m) {
                self.input_sent += 1;
            }
        }
    }

    /// 窗口尺寸变了就重建绘制面。
    fn resize_surface(&mut self, size: winit::dpi::PhysicalSize<u32>) -> Result<()> {
        let (Some(window), Some(context)) = (self.window.as_ref(), self.context.as_ref()) else {
            return Ok(());
        };
        let (w, h) = (size.width.max(1), size.height.max(1));
        let mut surface = self
            .surface
            .take()
            .unwrap_or_else(|| softbuffer::Surface::new(context, window.clone()).unwrap());
        surface
            .resize(
                NonZeroU32::new(w).ok_or_else(|| anyhow!("窗口宽度为 0"))?,
                NonZeroU32::new(h).ok_or_else(|| anyhow!("窗口高度为 0"))?,
            )
            .map_err(|e| anyhow!("重建绘制面失败：{e}"))?;
        self.surface = Some(surface);
        self.buf = vec![0u32; (w as usize) * (h as usize)];
        Ok(())
    }

    /// 把最新一帧按「保持宽高比 + 居中 + 最近邻缩放」铺进窗口位图。
    ///
    /// 最近邻而不是双线性：远程桌面上要看清的是**文字**，糊了的字比重采样的锯齿更难受。
    fn blit(&mut self, frame: &Frame, ww: u32, wh: u32) {
        let need = (ww as usize) * (wh as usize);
        if self.buf.len() != need {
            self.buf = vec![0u32; need];
        }

        // 清成近黑（不是纯黑：纯黑在有些面板上分不清「没画面」和「画面就是黑的」）
        self.buf.iter_mut().for_each(|p| *p = 0x0010_1010);

        if frame.width == 0 || frame.height == 0 {
            return;
        }
        let expect = (frame.width as usize) * (frame.height as usize) * 4;
        if frame.pixels.len() != expect {
            warn!(
                got = frame.pixels.len(),
                expect, "帧数据长度与宽高不符，跳过这一帧"
            );
            return;
        }

        // 整倍缩放（1x / 2x…）—— 整数倍时最近邻刚好像素对齐，文字最清楚。
        // 几何和「点击位置换算」共用 `input::fit_rect`：两边各算一遍的话，黑边区域就会点偏。
        let r = fit_rect(frame.width, frame.height, ww, wh);
        let (dw, dh, ox, oy) = (r.w, r.h, r.ox, r.oy);

        // 最近邻的倍率由实际画面尺寸反推（裁剪时 dw 会被窗口宽度夹住，那就是 1 倍）。
        let scale = (dw / frame.width).max(1);
        let (fw, fh) = (frame.width as usize, frame.height as usize);
        for y in 0..dh {
            let sy = ((y / scale) as usize).min(fh - 1);
            let srow = sy * fw * 4;
            let drow = ((oy + y) as usize) * (ww as usize);
            for x in 0..dw {
                let sx = ((x / scale) as usize).min(fw - 1);
                let o = srow + sx * 4;
                // FFmpeg 解出来是 BGRA（小端序），softbuffer 要 0RGB
                let b = frame.pixels[o] as u32;
                let g = frame.pixels[o + 1] as u32;
                let r = frame.pixels[o + 2] as u32;
                self.buf[drow + (ox + x) as usize] = (r << 16) | (g << 8) | b;
            }
        }
    }

    fn redraw(&mut self) {
        let Some(window) = self.window.clone() else { return };
        let size = window.inner_size();
        if size.width == 0 || size.height == 0 {
            return;
        }

        // 取最新一帧；同一帧不重复重绘
        let frame = self.rx.borrow_and_update().clone();
        if let Some(f) = &frame {
            if Some(f.at) == self.last_frame_at {
                return;
            }
            self.blit(f, size.width, size.height);
            self.frame_sz = Some((f.width, f.height)); // 光标换算要用
            self.last_frame_at = Some(f.at);
        } else {
            // 还没收到首帧：画个深灰底，窗口不是白的就不至于像卡死
            let need = (size.width as usize) * (size.height as usize);
            if self.buf.len() != need {
                self.buf = vec![0x0010_1010u32; need];
            }
        }

        // surface 的借用放在最后：blit 也要用 &mut self
        let Some(surface) = self.surface.as_mut() else { return };
        let mut sb = match surface.buffer_mut() {
            Ok(b) => b,
            Err(e) => {
                debug!("取绘制缓冲失败：{e}");
                return;
            }
        };
        let n = sb.len().min(self.buf.len());
        sb[..n].copy_from_slice(&self.buf[..n]);
        if let Err(e) = sb.present() {
            debug!("呈现失败：{e}");
            return;
        }
        self.painted += 1;

        // 标题每秒刷一次，避免每帧 set_title 触发窗口系统重排
        if self.last_title.elapsed() >= std::time::Duration::from_millis(1000) {
            self.last_title = std::time::Instant::now();
            self.update_overlay(&window, size.width, size.height);
        }
    }

    /// 每秒刷一次标题。`WatchStatsSnapshot` 里没有 fps/kbps 字段，
    /// 用两次采样之间的**增量**算瞬时值 —— 累计平均在长时间会话里会越算越钝，
    /// 看不出「刚刚掉到 5 fps」这种事。
    fn update_overlay(&mut self, window: &Window, ww: u32, wh: u32) {
        let s = self.stats.snapshot();
        let el = self.overlay_since.elapsed().as_secs_f64().max(1e-6);
        let d_decoded = s.decoded.saturating_sub(self.prev.decoded) as f64;
        let d_bytes = s.bytes.saturating_sub(self.prev.bytes) as f64;
        self.overlay.fps = d_decoded / el;
        self.overlay.kbps = d_bytes * 8.0 / el / 1000.0;
        self.overlay.dropped = s.dropped;
        self.overlay.first_frame_ms = (s.first_frame_ms > 0).then_some(s.first_frame_ms);
        self.prev = s;
        self.overlay_since = std::time::Instant::now();
        window.set_title(&self.overlay.title(ww, wh));
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(self.overlay.title(0, 0))
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                warn!("创建窗口失败：{e}");
                event_loop.exit();
                return;
            }
        };
        let context = match softbuffer::Context::new(window.clone()) {
            Ok(c) => c,
            Err(e) => {
                warn!("初始化 softbuffer 失败：{e}");
                event_loop.exit();
                return;
            }
        };
        self.context = Some(context);
        self.window = Some(window.clone());
        if let Err(e) = self.resize_surface(window.inner_size()) {
            warn!("{e}");
            event_loop.exit();
        }
        info!("窗口已就绪");
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Err(e) = self.resize_surface(size) {
                    warn!("{e}");
                }
                self.last_frame_at = None; // 尺寸变了，强制重画
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => self.redraw(),
            // ── 键鼠上报（M2.5）──
            WindowEvent::ModifiersChanged(m) => {
                self.mods = Mods::from_state(m.state());
            }
            WindowEvent::CursorMoved { position, .. } => {
                let Some(r) = self.image_rect() else { return };
                if let Some((x, y)) = norm_point(position.x, position.y, r) {
                    self.send_input(C2H::MouseMoveAbs { x, y });
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(b) = proto_button(button) {
                    let down = state == ElementState::Pressed;
                    self.send_input(C2H::MouseButton { button: b, down });
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // winit 的正方向是「内容向右/向下滚动」，也就是向上滚 —— 和协议一致（正 = 上/右）。
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => line_units(x, y),
                    MouseScrollDelta::PixelDelta(p) => self.wheel.feed_pixels(p.x, p.y),
                };
                if dx != 0 || dy != 0 {
                    self.send_input(C2H::MouseWheel { dx, dy });
                }
            }
            WindowEvent::KeyboardInput { event, is_synthetic, .. } => {
                // 焦点切换时 winit 会补发「合成」的按下/松开事件。转发过去就成了
                // 在对面莫名其妙敲一下键 —— 必须丢掉。
                if is_synthetic {
                    return;
                }
                let vk = match event.physical_key {
                    PhysicalKey::Code(c) => vk_from_key(c),
                    PhysicalKey::Unidentified(_) => None,
                };
                let pressed = event.state == ElementState::Pressed;
                let action = key_action(
                    vk,
                    event.text.as_deref(),
                    pressed,
                    event.repeat,
                    &self.mods,
                );
                for m in key_messages(&action) {
                    self.send_input(m);
                }
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                if let Some(w) = self.window.as_ref() {
                    let _ = self.resize_surface(w.inner_size());
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // 到点自动收工 —— 没有这个，脚本化验证就只能靠人去点关闭按钮。
        if let Some(d) = self.deadline {
            if std::time::Instant::now() >= d {
                info!("已到指定时长，自动关窗");
                event_loop.exit();
                return;
            }
        }
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }
}

/// 跑窗口，直到用户关掉它。
///
/// **必须在主线程调用**（winit 的硬性要求）。tokio 运行时应该已经跑在别的线程上，
/// 这里的 `rx` 就是从那边 `VideoWatcher::subscribe()` 拿到的；`input` 为 `None` 就是只读模式。
pub fn run(
    rx: watch::Receiver<Option<Frame>>,
    stats: Arc<WatchStats>,
    overlay: Overlay,
    seconds: Option<f64>,
    input: Option<InputSender>,
) -> Result<()> {
    let event_loop = EventLoop::new().context("创建事件循环失败（没有可用的显示环境？）")?;
    // Poll：不睡等事件，保证有帧时能立刻画出来
    event_loop.set_control_flow(ControlFlow::Poll);
    let deadline = seconds.map(|s| std::time::Instant::now() + std::time::Duration::from_secs_f64(s));
    let mut app = App::new(rx, stats, overlay, deadline, input);
    event_loop.run_app(&mut app).context("窗口事件循环异常退出")?;
    info!(painted = app.painted, input_sent = app.input_sent, "窗口已关闭");
    Ok(())
}

/// winit 的按钮 → 协议按钮。`Other(_)` 是八键鼠标上的额外键，协议里没有对应项，直接丢掉。
fn proto_button(b: MouseButton) -> Option<ProtoButton> {
    match b {
        MouseButton::Left => Some(ProtoButton::Left),
        MouseButton::Middle => Some(ProtoButton::Middle),
        MouseButton::Right => Some(ProtoButton::Right),
        MouseButton::Back => Some(ProtoButton::X1),
        MouseButton::Forward => Some(ProtoButton::X2),
        MouseButton::Other(_) => None,
    }
}

/// 供状态栏使用：把 `Arc<WatchStats>` 的当前值快照出来。
pub fn snapshot(stats: &Arc<WatchStats>) -> WatchStatsSnapshot {
    stats.snapshot()
}

/// 读一眼「有没有画面」——无窗口环境（CI / 无 X11）下自测用。
pub fn has_frame(rx: &watch::Receiver<Option<Frame>>) -> bool {
    rx.borrow().is_some()
}

/// 统计里的 `dropped` 是原子量，窗口线程需要能看见它。
pub fn dropped(stats: &Arc<WatchStats>) -> u64 {
    stats.dropped.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn frame(w: u32, h: u32, rgb: (u8, u8, u8)) -> Frame {
        // watch::Frame 的像素是 BGRA
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..(w * h) {
            px.push(rgb.2);
            px.push(rgb.1);
            px.push(rgb.0);
            px.push(255);
        }
        Frame {
            pixels: px,
            width: w,
            height: h,
            ts_us: 0,
            at: Instant::now(),
        }
    }

    fn app_with(frame: Option<Frame>) -> App {
        let (_tx, rx) = watch::channel(frame);
        App::new(rx, Arc::new(WatchStats::default()), Overlay::default(), None, None)
    }

    #[test]
    fn blit_keeps_aspect_ratio_and_centers() {
        // 4x2 的画面放进 8x8 的窗口：整数倍取 2，画成 8x4，上下各留 2 行
        let mut app = app_with(None);
        let f = frame(4, 2, (255, 0, 0));
        app.blit(&f, 8, 8);

        assert_eq!(app.buf.len(), 64);
        // 左边框：上下留白
        assert_eq!(app.buf[0], 0x0010_1010, "第一行应是背景");
        assert_eq!(app.buf[8 * 2], 0x00FF_0000, "第 3 行应是红画面");
        assert_eq!(app.buf[8 * 6 - 1], 0x00FF_0000, "画面底边还是要红");
        assert_eq!(app.buf[8 * 7], 0x0010_1010, "最后一行应是背景");
    }

    #[test]
    fn blit_maps_bgra_to_0rgb_in_the_right_order() {
        // 纯蓝：BGRA 里 B=255，0RGB 应得 0x0000FF（不是 0xFF0000）
        let mut app = app_with(None);
        app.blit(&frame(2, 2, (0, 0, 255)), 2, 2);
        assert_eq!(app.buf[0], 0x0000_00FF, "蓝/红通道搞反了");
    }

    #[test]
    fn blit_rejects_mismatched_pixel_buffer() {
        // 宽高说是 4x4，像素却只有 2x2 → 拒绝这一帧，但背景要画出来
        let mut app = app_with(None);
        let mut f = frame(4, 4, (255, 255, 255));
        f.pixels.truncate(2 * 2 * 4);
        app.blit(&f, 4, 4);
        assert!(app.buf.iter().all(|p| *p == 0x0010_1010), "坏帧不该画上去");
    }

    #[test]
    fn blit_never_scales_below_one_to_one() {
        // 画面比窗口大时按 1x 裁切，不做缩小（缩小会把字糊掉）
        let mut app = app_with(None);
        app.blit(&frame(100, 100, (0, 255, 0)), 10, 10);
        assert_eq!(app.buf.len(), 100);
        assert!(app.buf.iter().all(|p| *p == 0x0000_FF00), "应是满屏绿");
    }
}
