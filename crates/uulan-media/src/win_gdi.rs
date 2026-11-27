//! Windows 原生桌面采集（GDI BitBlt）+ 内置 OpenH264 编码。
//!
//! 仅在 `windows` 且启用 `builtin-h264` 特性时编译：
//! - 零外部 `ffmpeg.exe` 依赖，无需任何额外 Win32 crate，直接 FFI 调 `user32` / `gdi32`。
//! - `CreateDIBSection` 分配 top-down 32-bit BGRA 缓冲，`BitBlt` / `StretchBlt` 抓取桌面。
//! - 复用单一 `YUVBuffer` 将 BGRA 转 I420，再由 Cisco OpenH264 编成 Baseline Annex-B H.264 AU。

use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, Level, Profile, RateControlMode,
    SpsPpsStrategy, UsageType,
};
use openh264::formats::{BGRA8Source, RGB8Source, RGBSource, YUVBuffer};
use openh264::OpenH264API;
use tokio::sync::mpsc;

use crate::annexb;
use crate::ffmpeg::{self, CaptureOptions};
use crate::source::Au;
use crate::MediaError;

const SM_CXSCREEN: i32 = 0;
const SM_CYSCREEN: i32 = 1;
const BI_RGB: u32 = 0;
const DIB_RGB_COLORS: u32 = 0;
const SRCCOPY: u32 = 0x00CC_0020;
const CAPTUREBLT: u32 = 0x4000_0000;
const HALFTONE: i32 = 4;

#[repr(C)]
struct BitmapInfoHeader {
    bi_size: u32,
    bi_width: i32,
    bi_height: i32,
    bi_planes: u16,
    bi_bit_count: u16,
    bi_compression: u32,
    bi_size_image: u32,
    bi_x_pels_per_meter: i32,
    bi_y_pels_per_meter: i32,
    bi_clr_used: u32,
    bi_clr_important: u32,
}

#[repr(C)]
struct BitmapInfo {
    bmi_header: BitmapInfoHeader,
    bmi_colors: [u32; 1],
}

#[link(name = "user32")]
extern "system" {
    fn SetProcessDPIAware() -> i32;
    fn GetSystemMetrics(index: i32) -> i32;
    fn GetDC(hwnd: *mut c_void) -> *mut c_void;
    fn ReleaseDC(hwnd: *mut c_void, hdc: *mut c_void) -> i32;
}

#[link(name = "gdi32")]
extern "system" {
    fn CreateCompatibleDC(hdc: *mut c_void) -> *mut c_void;
    fn DeleteDC(hdc: *mut c_void) -> i32;
    fn SelectObject(hdc: *mut c_void, h: *mut c_void) -> *mut c_void;
    fn DeleteObject(ho: *mut c_void) -> i32;
    fn CreateDIBSection(
        hdc: *mut c_void,
        lpbmi: *const BitmapInfo,
        usage: u32,
        ppv_bits: *mut *mut c_void,
        h_section: *mut c_void,
        offset: u32,
    ) -> *mut c_void;
    fn BitBlt(
        hdc: *mut c_void,
        x: i32,
        y: i32,
        cx: i32,
        cy: i32,
        hdc_src: *mut c_void,
        x1: i32,
        y1: i32,
        rop: u32,
    ) -> i32;
    fn SetStretchBltMode(hdc: *mut c_void, mode: i32) -> i32;
    fn StretchBlt(
        hdc_dest: *mut c_void,
        x_dest: i32,
        y_dest: i32,
        w_dest: i32,
        h_dest: i32,
        hdc_src: *mut c_void,
        x_src: i32,
        y_src: i32,
        w_src: i32,
        h_src: i32,
        rop: u32,
    ) -> i32;
}

/// 声明进程为 DPI 感知，确保 `GetSystemMetrics`、`BitBlt` 与 `SendInput` 均使用真实物理像素。
pub fn set_dpi_aware() {
    unsafe {
        let _ = SetProcessDPIAware();
    }
}

/// 探测 Windows 主显示器物理尺寸（宽、高）。
pub fn primary_screen_size() -> Option<(u32, u32)> {
    set_dpi_aware();
    let w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    if w >= 2 && h >= 2 {
        Some((w as u32, h as u32))
    } else {
        None
    }
}

struct BgraView<'a> {
    data: &'a [u8],
    dims: (usize, usize),
}

impl RGBSource for BgraView<'_> {
    fn dimensions(&self) -> (usize, usize) {
        self.dims
    }

    fn pixel_f32(&self, x: usize, y: usize) -> (f32, f32, f32) {
        let i = (y * self.dims.0 + x) * 4;
        (self.data[i + 2] as f32, self.data[i + 1] as f32, self.data[i] as f32)
    }
}

impl RGB8Source for BgraView<'_> {
    fn rgb8_data(&self) -> &[u8] {
        self.data
    }

    fn dimensions_padded(&self) -> (usize, usize) {
        self.dims
    }
}

impl BGRA8Source for BgraView<'_> {}

struct GdiCapturer {
    hdc_screen: *mut c_void,
    hdc_mem: *mut c_void,
    hbmp: *mut c_void,
    old_obj: *mut c_void,
    bits: *const u8,
    src_x: i32,
    src_y: i32,
    src_w: i32,
    src_h: i32,
    dst_w: usize,
    dst_h: usize,
}

impl GdiCapturer {
    fn new(
        src_x: i32,
        src_y: i32,
        src_w: u32,
        src_h: u32,
        dst_w: u32,
        dst_h: u32,
    ) -> Result<Self, MediaError> {
        set_dpi_aware();
        unsafe {
            let hdc_screen = GetDC(ptr::null_mut());
            if hdc_screen.is_null() {
                return Err(MediaError::Decoder("GetDC(NULL) 失败".into()));
            }
            let hdc_mem = CreateCompatibleDC(hdc_screen);
            if hdc_mem.is_null() {
                ReleaseDC(ptr::null_mut(), hdc_screen);
                return Err(MediaError::Decoder("CreateCompatibleDC 失败".into()));
            }
            let bmi = BitmapInfo {
                bmi_header: BitmapInfoHeader {
                    bi_size: std::mem::size_of::<BitmapInfoHeader>() as u32,
                    bi_width: dst_w as i32,
                    // 负高度 = top-down DIB，首行即屏幕顶部
                    bi_height: -(dst_h as i32),
                    bi_planes: 1,
                    bi_bit_count: 32,
                    bi_compression: BI_RGB,
                    bi_size_image: 0,
                    bi_x_pels_per_meter: 0,
                    bi_y_pels_per_meter: 0,
                    bi_clr_used: 0,
                    bi_clr_important: 0,
                },
                bmi_colors: [0],
            };
            let mut bits: *mut c_void = ptr::null_mut();
            let hbmp = CreateDIBSection(
                hdc_mem,
                &bmi,
                DIB_RGB_COLORS,
                &mut bits,
                ptr::null_mut(),
                0,
            );
            if hbmp.is_null() || bits.is_null() {
                DeleteDC(hdc_mem);
                ReleaseDC(ptr::null_mut(), hdc_screen);
                return Err(MediaError::Decoder("CreateDIBSection 失败".into()));
            }
            let old_obj = SelectObject(hdc_mem, hbmp);
            if src_w != dst_w || src_h != dst_h {
                SetStretchBltMode(hdc_mem, HALFTONE);
            }
            Ok(Self {
                hdc_screen,
                hdc_mem,
                hbmp,
                old_obj,
                bits: bits as *const u8,
                src_x,
                src_y,
                src_w: src_w as i32,
                src_h: src_h as i32,
                dst_w: dst_w as usize,
                dst_h: dst_h as usize,
            })
        }
    }

    fn capture(&self) -> Result<&[u8], MediaError> {
        let ok = unsafe {
            if self.src_w == self.dst_w as i32 && self.src_h == self.dst_h as i32 {
                BitBlt(
                    self.hdc_mem,
                    0,
                    0,
                    self.dst_w as i32,
                    self.dst_h as i32,
                    self.hdc_screen,
                    self.src_x,
                    self.src_y,
                    SRCCOPY | CAPTUREBLT,
                )
            } else {
                StretchBlt(
                    self.hdc_mem,
                    0,
                    0,
                    self.dst_w as i32,
                    self.dst_h as i32,
                    self.hdc_screen,
                    self.src_x,
                    self.src_y,
                    self.src_w,
                    self.src_h,
                    SRCCOPY | CAPTUREBLT,
                )
            }
        };
        if ok == 0 {
            return Err(MediaError::Decoder("GDI 抓屏 BitBlt 失败（桌面不可访问或处于 UAC/锁屏桌面）".into()));
        }
        let len = self.dst_w * self.dst_h * 4;
        Ok(unsafe { std::slice::from_raw_parts(self.bits, len) })
    }
}

impl Drop for GdiCapturer {
    fn drop(&mut self) {
        unsafe {
            if !self.old_obj.is_null() {
                SelectObject(self.hdc_mem, self.old_obj);
            }
            if !self.hbmp.is_null() {
                DeleteObject(self.hbmp);
            }
            if !self.hdc_mem.is_null() {
                DeleteDC(self.hdc_mem);
            }
            if !self.hdc_screen.is_null() {
                ReleaseDC(ptr::null_mut(), self.hdc_screen);
            }
        }
    }
}

/// 启动 Windows GDI 抓屏 + 内置 OpenH264 编码线程，持续产出 Annex-B AU。
pub fn spawn_windows_source(
    opts: CaptureOptions,
) -> Result<(mpsc::Receiver<Au>, Arc<AtomicBool>, JoinHandle<()>), MediaError> {
    let (raw_ew, raw_eh) = ffmpeg::scaled_size(opts.width, opts.height, opts.scale);
    // OpenH264 I420 要求宽高为偶数
    let ew = (raw_ew & !1).max(2);
    let eh = (raw_eh & !1).max(2);
    let fps = opts.fps.clamp(1, 60);
    let bps = if opts.max_bitrate_kbps > 0 {
        opts.max_bitrate_kbps.saturating_mul(1000)
    } else {
        4_000_000
    };

    let cfg = EncoderConfig::new()
        .bitrate(BitRate::from_bps(bps))
        .max_frame_rate(FrameRate::from_hz(fps as f32))
        .usage_type(UsageType::ScreenContentRealTime)
        .rate_control_mode(RateControlMode::Bitrate)
        .skip_frames(false)
        .sps_pps_strategy(SpsPpsStrategy::ConstantId)
        .intra_frame_period(IntraFramePeriod::from_num_frames(ffmpeg::gop_for(fps)))
        .profile(Profile::Baseline)
        .level(Level::Level_5_2);

    // 先在调用线程验证编码器可初始化，有错当场返回
    let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), cfg)
        .map_err(|e| MediaError::Decoder(format!("初始化 OpenH264 编码器失败: {e}")))?;

    let (tx, rx) = mpsc::channel::<Au>(4);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_bg = stop.clone();

    let worker = std::thread::Builder::new()
        .name("lanlink-win-enc".to_string())
        .spawn(move || {
            let cap = match GdiCapturer::new(opts.grab_x, opts.grab_y, opts.width, opts.height, ew, eh) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "Windows GDI 抓屏初始化失败");
                    return;
                }
            };
            let mut yuv = YUVBuffer::new(ew as usize, eh as usize);
            let started = Instant::now();
            let frame_interval = Duration::from_secs_f64(1.0 / fps as f64);
            let mut next_tick = Instant::now();
            let mut first = true;
            let mut fail_logged = false;

            while !stop_bg.load(Ordering::Relaxed) {
                let now = Instant::now();
                if now < next_tick {
                    std::thread::sleep((next_tick - now).min(Duration::from_millis(50)));
                    continue;
                }
                next_tick = now + frame_interval;

                let bgra = match cap.capture() {
                    Ok(b) => {
                        fail_logged = false;
                        b
                    }
                    Err(e) => {
                        if !fail_logged {
                            tracing::warn!(error = %e, "Windows GDI 抓屏失败，等待桌面恢复");
                            fail_logged = true;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                };
                yuv.read_bgra8(BgraView { data: bgra, dims: (ew as usize, eh as usize) });
                if first {
                    encoder.force_intra_frame();
                    first = false;
                }
                let bs = match encoder.encode(&yuv) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(error = %e, "OpenH264 编码单帧失败");
                        continue;
                    }
                };
                // 拼出标准 Annex-B AU（带 AUD 头）
                let mut data = Vec::with_capacity(6 + 16 * 1024);
                data.extend_from_slice(&[0, 0, 0, 1, 0x09, 0x10]);
                bs.write_vec(&mut data);
                if data.len() <= 6 {
                    tracing::debug!("OpenH264 跳过空帧");
                    continue;
                }
                let keyframe = annexb::is_keyframe_au(&data);
                let ts_us = started.elapsed().as_micros() as u64;
                if tx.blocking_send(Au { ts_us, data, keyframe }).is_err() {
                    return;
                }
            }
        })
        .map_err(MediaError::Io)?;

    Ok((rx, stop, worker))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openh264_encodes_2560x1440_bgra_frame() {
        let w = 2560usize;
        let h = 1440usize;
        let bgra = vec![128u8; w * h * 4];
        let mut yuv = YUVBuffer::new(w, h);
        yuv.read_bgra8(BgraView { data: &bgra, dims: (w, h) });

        let cfg = EncoderConfig::new()
            .bitrate(BitRate::from_bps(4_000_000))
            .max_frame_rate(FrameRate::from_hz(30.0))
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .skip_frames(false)
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            .intra_frame_period(IntraFramePeriod::from_num_frames(15))
            .profile(Profile::Baseline)
            .level(Level::Level_5_2);
        let mut enc = Encoder::with_api_config(OpenH264API::from_source(), cfg).unwrap();
        let bs = enc.encode(&yuv).unwrap();
        let raw = bs.to_vec();
        assert!(!raw.is_empty(), "2560x1440 首帧编码不能为空");

        // 编码 → 解码闭环验证（纯内存，不依赖桌面会话）
        let mut dec = openh264::decoder::Decoder::new().unwrap();
        let mut decoded_dims = None;
        for nal in openh264::nal_units(&raw) {
            if let Ok(Some(frame)) = dec.decode(nal) {
                decoded_dims = Some(openh264::formats::YUVSource::dimensions(&frame));
            }
        }
        assert_eq!(decoded_dims, Some((w, h)), "OpenH264 必须能解出刚编码的 2560x1440 帧");
    }
}
