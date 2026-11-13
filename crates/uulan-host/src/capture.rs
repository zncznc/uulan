//! 采集源抽象。
//!
//! 生产环境是 `x11grab`（抓 X11 屏幕）。但这里把「输入段」抽象出来，是为了让
//! **整条被控端链路在没有 X11 的机器上也能跑通和自测** ——
//! 换成合成源（`lavfi`）后，除了「像素从哪来」之外的一切（编码参数、AU 切分、
//! 时间戳、网络传输、解码）都是同一条真实路径。
//!
//! 顺带的好处：演示和排障时不需要真的有一块屏幕。

use serde::{Deserialize, Serialize};

use uulan_media::ffmpeg::{self, CaptureOptions};
use uulan_protocol::msg::Screen;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CaptureInput {
    /// 抓 X11 屏幕。`display` 为 `None` 时用 `$DISPLAY`，再退回 `:0.0`。
    ///
    /// `monitor` = 抓取第几块显示器（按 `xrandr` 列出的**已连接**输出顺序，0 基），
    /// 也就是多屏扩展桌面里只出其中一块屏的画面。`None` = 抓整块帧缓冲（老行为）。
    /// 表里没有那个号就退回第一块已连接屏，表是空的就抓全屏 —— 降级是确定的，
    /// 不报错打断会话（副屏被拔掉时画面应该退回主屏，而不是把会话打死）。
    X11 {
        #[serde(default)]
        display: Option<String>,
        #[serde(default)]
        monitor: Option<u32>,
    },
    /// 合成测试图（不需要 X11）。`frames` 为 `None` 表示无限产出。
    Lavfi {
        #[serde(default = "default_lavfi_spec")]
        spec: String,
        #[serde(default)]
        frames: Option<u32>,
    },
}

// `#[derive(Default)]` 不能用在带字段的变体上（`#[default]` 只允许标在单元变体）。
// 这里手写：默认就是抓 X11，也就是生产环境的默认行为。
impl Default for CaptureInput {
    fn default() -> Self {
        CaptureInput::X11 { display: None, monitor: None }
    }
}

fn default_lavfi_spec() -> String {
    "testsrc2=size=1280x720:rate=30".to_string()
}

impl CaptureInput {
    /// 解析命令行写法：`x11` / `x11::0.0` / `lavfi` / `lavfi:<spec>[@<frames>]`。
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if let Some(rest) = s.strip_prefix("lavfi") {
            let rest = rest.strip_prefix(':').unwrap_or("");
            if rest.is_empty() {
                return Ok(CaptureInput::Lavfi { spec: default_lavfi_spec(), frames: None });
            }
            // 末尾 @N 表示帧数上限
            let (spec, frames) = match rest.rsplit_once('@') {
                Some((sp, f)) => {
                    let n: u32 = f.parse().map_err(|_| format!("帧数 {f:?} 不是数字"))?;
                    (sp.to_string(), Some(n))
                }
                None => (rest.to_string(), None),
            };
            if spec.trim().is_empty() {
                return Err("lavfi 需要给出合成源，例如 lavfi:testsrc2=size=1280x720:rate=30".into());
            }
            return Ok(CaptureInput::Lavfi { spec, frames });
        }
        if let Some(rest) = s.strip_prefix("x11") {
            // 尾巴的合法形态四种：空（裸 x11）、`::显示[@N]`（规范写法）、
            // `:显示[@N]`（少打一个冒号，很常见，宽容）、`@N`（只挑显示器）。
            // 其余（比如 `x11x`）明确拒绝 —— 老写法会把任何垃圾尾巴静默当成裸 x11。
            let tail = if let Some(a) = rest.strip_prefix("::") {
                a
            } else if rest.is_empty() {
                ""
            } else if let Some(a) = rest.strip_prefix(':') {
                a
            } else if rest.starts_with('@') {
                rest
            } else {
                return Err(format!("无法识别的采集源 {s:?}（可用 x11[:<显示>[@显示器号]] / lavfi[:spec[@frames]]）"));
            };
            let (dspec, monitor) = split_display_monitor(tail)?;
            // X 显示名的形式是 `:0.0` 或 `host:0.0`。用户写 `x11::0.0` 时
            // 顺手省掉冒号很常见，这里补上；已经带冒号的（含主机名那种）原样保留。
            if dspec.is_empty() {
                return Ok(CaptureInput::X11 { display: None, monitor });
            }
            let display = if dspec.contains(':') { dspec } else { format!(":{dspec}") };
            return Ok(CaptureInput::X11 { display: Some(display), monitor });
        }
        // 也接受直接写一个显示号，比如 ":0.0"（可带 @显示器号）
        if s.starts_with(':') {
            let (dspec, monitor) = split_display_monitor(s)?;
            return Ok(CaptureInput::X11 { display: Some(dspec), monitor });
        }
        Err(format!("无法识别的采集源 {s:?}（可用 x11[:<显示>[@显示器号]] / lavfi[:spec[@frames]]）"))
    }

    /// 实际使用的 X11 显示号。
    pub fn display(&self) -> Option<String> {
        match self {
            CaptureInput::X11 { display, .. } => Some(
                display
                    .clone()
                    .or_else(|| std::env::var("DISPLAY").ok())
                    .filter(|d| !d.trim().is_empty())
                    .unwrap_or_else(|| ":0.0".to_string()),
            ),
            CaptureInput::Lavfi { .. } => None,
        }
    }

    /// 多屏时选的第几块显示器（`xrandr` 已连接顺序，0 基）。`None` = 抓整块帧缓冲。
    pub fn monitor_idx(&self) -> Option<u32> {
        match self {
            CaptureInput::X11 { monitor, .. } => *monitor,
            CaptureInput::Lavfi { .. } => None,
        }
    }

    /// 转成 FFmpeg 的输入段参数。
    pub fn to_ffmpeg_input(&self, o: &CaptureOptions) -> Vec<String> {
        match self {
            CaptureInput::X11 { .. } => ffmpeg::x11grab_input(o),
            CaptureInput::Lavfi { spec, frames } => {
                // 合成源也跟随目标尺寸：`testsrc2=size=WxH` 里的尺寸改成 `-video_size` 那份。
                // 这样 lavfi 才真的能模拟「抓屏设备分辨率变了」——否则 spec 里写死的尺寸
                // 会让「重启编码源」只改了 VideoCfg、画面还是旧尺寸，测试就验不到东西。
                let spec = with_size(spec, o.width, o.height);
                let mut a = vec!["-f".to_string(), "lavfi".to_string(), "-i".to_string(), spec];
                if let Some(n) = frames {
                    a.push("-frames:v".to_string());
                    a.push(n.to_string());
                }
                a
            }
        }
    }

    /// 是不是无限产出（X11 抓屏永不结束；lavfi 不设帧数上限也一样）。
    pub fn is_unbounded(&self) -> bool {
        match self {
            CaptureInput::X11 { .. } => true,
            CaptureInput::Lavfi { frames, .. } => frames.is_none(),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            CaptureInput::X11 { .. } => {
                let base = match self.display() {
                    Some(d) => format!("X11 屏幕 {d}"),
                    None => "X11 屏幕".into(),
                };
                match self.monitor_idx() {
                    Some(m) => format!("{base} 显示器{m}"),
                    None => base,
                }
            },
            CaptureInput::Lavfi { spec, frames } => match frames {
                Some(n) => format!("合成源 {spec}（{n} 帧）"),
                None => format!("合成源 {spec}（不限）"),
            },
        }
    }
}

/// 把 `:0.0@1` 拆成（显示名，显示器号）。X 显示名里不会出现 `@`，从后切是安全的。
fn split_display_monitor(rest: &str) -> Result<(String, Option<u32>), String> {
    match rest.rsplit_once('@') {
        Some((d, n)) => {
            let i: u32 = n.parse().map_err(|_| format!("显示器号 {n:?} 不是数字"))?;
            Ok((d.to_string(), Some(i)))
        }
        None => Ok((rest.to_string(), None)),
    }
}

/// 抓取区域：X 显示根窗口坐标里的一个矩形。
///
/// 单屏就是 `x=0, y=0, w=全屏宽, h=全屏高` —— 与「整帧缓冲」的老行为一字不差。
/// 多屏（扩展桌面，几块显示器拼成一个大根窗口）时「只抓第 N 块」就是这块矩形：
/// x11grab 用 `-i :0.0+X,Y -video_size WxH` 抓它，键鼠注入也要把区域原点加回去
/// （归一化坐标 × 区域尺寸得到的是**区域内**像素，XTest 认的是**整根窗口**像素）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Region {
    /// 全屏区域（单屏 / 没指定显示器时的默认）。
    pub fn full(w: u32, h: u32) -> Self {
        Region { x: 0, y: 0, w, h }
    }

    /// 报给控制端的「屏幕尺寸」。控制端的归一化坐标按**送出去的那幅画面**走，
    /// 它不需要知道原点在整屏哪里 —— 偏移全在被控端内部消化，协议一字没改。
    pub fn screen(&self) -> Screen {
        Screen { w: self.w, h: self.h }
    }
}

/// 解析一个带符号整数（`+1920` / `-1080`），返回（值，消耗字符数）。
fn parse_signed(s: &str) -> Option<(i32, usize)> {
    let b = s.as_bytes();
    if b.is_empty() || (b[0] != b'+' && b[0] != b'-') {
        return None;
    }
    let mut i = 1;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == 1 {
        return None;
    }
    s[..i].parse::<i32>().ok().map(|v| (v, i))
}

/// 解析 `1920x1080+0+0` 这种几何 token。偏移可为负（xrandr 用 `%+d` 打印，
/// 负值就写成 `1024x768-512+0` 或 `+0-1080`），两个符号都走 `[+-]数字` 规则。
fn parse_geom_token(t: &str) -> Option<Region> {
    let xi = t.find('x')?;
    let w: u32 = t[..xi].parse().ok()?;
    let rest = &t[xi + 1..];
    let cut = rest.find(['+', '-'])?;
    let h: u32 = rest[..cut].parse().ok()?;
    let (x, used) = parse_signed(&rest[cut..])?;
    let (y, _) = parse_signed(&rest[cut + used..])?;
    if w == 0 || h == 0 {
        return None;
    }
    Some(Region { x, y, w, h })
}

/// 从 `xrandr --query` 输出解析所有**已连接且有几何**的显示器区域，保持 xrandr 顺序。
///
/// 真机输出形如：
/// ```text
/// Screen 0: minimum 320 x 200, current 3840 x 1080, maximum 16384 x 16384
/// VGA-1 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 508mm x 286mm
///    1920x1080     60.00*+   50.00
/// HDMI-1 connected 1920x1080+1920+0 (normal left inverted right x axis y axis) 527mm x 296mm
/// DP-1 disconnected (normal left inverted right x axis y axis)
/// ```
/// 「connected 但没有几何 token」的行是**没点亮**的口（不在当前布局里），跳过。
/// 麒麟现网的裸帧缓冲机器通常一个 connected 都没有，返回空表由调用方降级。
pub fn parse_xrandr_connected(text: &str) -> Vec<Region> {
    let mut out = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 3 || fields[1] != "connected" {
            continue;
        }
        // 几何 token 可以在第 3 段（HDMI-1 connected 1920...），也可能排在 primary 之后。
        for f in &fields[2..] {
            if let Some(r) = parse_geom_token(f) {
                out.push(r);
                break;
            }
        }
    }
    out
}

/// 枚举某 X 显示上「已连接」的显示器区域（`xrandr` 顺序，0 基）。
///
/// 任何失败（没有 xrandr / 显示打不开）返回空表 —— 调用方按确定规则降级，绝不 panic。
/// 探针周期 1 秒，多一次 xrandr 是几毫秒量级；没指定 `@N` 的会话**根本不会走到这**。
pub fn probe_monitors_blocking(display: &str) -> Vec<Region> {
    let out = std::process::Command::new("xrandr")
        .env("DISPLAY", display)
        .arg("--query")
        .output()
        .ok()
        .filter(|o| o.status.success());
    match out {
        Some(o) => parse_xrandr_connected(&String::from_utf8_lossy(&o.stdout)),
        None => Vec::new(),
    }
}

/// 从显示器表里挑抓取区域（规则写死在这**一个**纯函数里，单测钉住）：
/// - `monitor = None` → 整块帧缓冲（与老版本行为一字不差）；
/// - 号存在 → 那块屏；
/// - 号超界但表非空 → **第一块**（副屏被拔时退回主屏，会话不断线）；
/// - 表为空（麒麟现网的裸帧缓冲）→ 整块帧缓冲。
pub fn pick_region(monitors: &[Region], fb: (u32, u32), monitor: Option<u32>) -> Region {
    match monitor {
        None => Region::full(fb.0, fb.1),
        Some(n) => monitors
            .get(n as usize)
            .or_else(|| if monitors.is_empty() { None } else { Some(&monitors[0]) })
            .copied()
            .unwrap_or_else(|| Region::full(fb.0, fb.1)),
    }
}

/// 阻塞版「探测抓取区域」（给 RegionProbe 用）。`monitor = None` 时
/// 只跑 xdpyinfo，与 [`detect_x11_screen_blocking`] 同一条命令、零额外开销。
pub fn detect_x11_region_blocking(display: &str, monitor: Option<u32>) -> Option<Region> {
    let fb = detect_x11_screen_blocking(display)?;
    match monitor {
        None => Some(Region::full(fb.0, fb.1)),
        Some(n) => Some(pick_region(&probe_monitors_blocking(display), fb, Some(n))),
    }
}

/// 异步版 [`detect_x11_region_blocking`]。任何失败返回 `None`，调用方回退缓存/配置。
pub async fn detect_x11_region(display: &str, monitor: Option<u32>) -> Option<Region> {
    let fb = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::process::Command::new("xdpyinfo")
            .arg("-display")
            .arg(display)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !fb.status.success() {
        return None;
    }
    let dims = parse_xdpyinfo_dimensions(&String::from_utf8_lossy(&fb.stdout))?;
    match monitor {
        None => Some(Region::full(dims.0, dims.1)),
        Some(n) => {
            let mon = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                tokio::process::Command::new("xrandr")
                    .env("DISPLAY", display)
                    .arg("--query")
                    .output(),
            )
            .await
            .ok()?
            .ok()?;
            let list = if mon.status.success() {
                parse_xrandr_connected(&String::from_utf8_lossy(&mon.stdout))
            } else {
                Vec::new()
            };
            Some(pick_region(&list, dims, Some(n)))
        }
    }
}

/// 把 lavfi spec 里的 `size=WxH` 换成给定尺寸；spec 没写尺寸就原样返回。
///
/// 只替换真的形如 `数字x数字` 的值 —— `size=hd720` 这种别名不能动，
/// 改坏了 ffmpeg 会直接报「无法解析的滤镜参数」。
fn with_size(spec: &str, w: u32, h: u32) -> String {
    if w == 0 || h == 0 {
        return spec.to_string();
    }
    let Some(idx) = spec.find("size=") else {
        return spec.to_string();
    };
    let start = idx + "size=".len();
    let rest = &spec[start..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == 'x'))
        .unwrap_or(rest.len());
    if !rest[..end].contains('x') {
        return spec.to_string();
    }
    format!("{}{}x{}{}", &spec[..start], w, h, &rest[end..])
}

/// 从 `xdpyinfo` 输出里解析屏幕尺寸。
///
/// 输出形如：
/// ```text
/// screen #0:
///   dimensions:    1920x1080 pixels (508x285 millimeters)
/// ```
/// 我们只认 `dimensions:` 那一行 —— `xrandr` 的输出格式因驱动而异，不如这个稳。
pub fn parse_xdpyinfo_dimensions(text: &str) -> Option<(u32, u32)> {
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("dimensions:") else { continue };
        let rest = rest.trim();
        // 取 "1920x1080" 这一段
        let token = rest.split_whitespace().next()?;
        let (w, h) = token.split_once('x')?;
        let w: u32 = w.parse().ok()?;
        let h: u32 = h.parse().ok()?;
        if w > 0 && h > 0 {
            return Some((w, h));
        }
    }
    None
}

/// 阻塞版屏幕尺寸探测：给 `ScreenProbe` 用（探针本身是同步 trait object，
/// 由调用方丢进 `spawn_blocking`）。异步版 [`detect_x11_screen`] 用它实现。
pub fn detect_x11_screen_blocking(display: &str) -> Option<(u32, u32)> {
    let out = std::process::Command::new("xdpyinfo")
        .arg("-display")
        .arg(display)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_xdpyinfo_dimensions(&String::from_utf8_lossy(&out.stdout))
}

/// 探测 X11 屏幕尺寸。任何失败都返回 `None`，由调用方回退到配置值。
///
/// **绝不 panic、绝不猜**：探测不到时调用方会明确用配置里的尺寸并打日志，
/// 而不是静静地按 1920x1080 去抓一块可能不存在的区域（那样控制端会看到黑边或花屏）。
pub async fn detect_x11_screen(display: &str) -> Option<(u32, u32)> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::process::Command::new("xdpyinfo")
            .arg("-display")
            .arg(display)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_xdpyinfo_dimensions(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lavfi_spec_follows_the_requested_frame_size() {
        // 分辨率变了之后编码源要用新尺寸重开；合成源必须跟着，
        // 否则「重启」只改了 VideoCfg，画面还是旧尺寸（测试就测了个寂寞）。
        let cap = CaptureInput::parse("lavfi:testsrc2=size=320x240:rate=10").unwrap();
        let o = CaptureOptions::new(":0.0", 480, 360);
        let args = cap.to_ffmpeg_input(&o);
        assert_eq!(args[0], "-f");
        assert_eq!(args[3], "testsrc2=size=480x360:rate=10");
    }

    #[test]
    fn lavfi_size_alias_and_sizeless_spec_are_left_alone() {
        // `size=hd720` 是别名，不认识数字形式就不能动；没有 size 参数的 spec 也不能硬塞
        assert_eq!(with_size("testsrc2=size=hd720:rate=30", 640, 480), "testsrc2=size=hd720:rate=30");
        assert_eq!(with_size("testsrc2=rate=30", 640, 480), "testsrc2=rate=30");
        assert_eq!(with_size("testsrc2=size=320x240", 0, 0), "testsrc2=size=320x240");
    }

    #[test]
    fn x11_input_carries_the_frame_size() {
        // x11grab 的抓取区域就是 -video_size，分辨率变化必须体现在这里
        let cap = CaptureInput::parse("x11::0.0").unwrap();
        let o = CaptureOptions::new(":0.0", 1280, 720);
        let args = cap.to_ffmpeg_input(&o).join(" ");
        assert!(args.contains("1280x720"), "抓取尺寸没传给 x11grab: {args}");
    }

    #[test]
    fn parse_x11_forms() {
        assert_eq!(
            CaptureInput::parse("x11").unwrap(),
            CaptureInput::X11 { display: None, monitor: None }
        );
        assert_eq!(
            CaptureInput::parse("x11::1.0").unwrap(),
            CaptureInput::X11 { display: Some(":1.0".into()), monitor: None }
        );
        assert_eq!(
            CaptureInput::parse(":0.0").unwrap(),
            CaptureInput::X11 { display: Some(":0.0".into()), monitor: None }
        );
    }

    #[test]
    fn parse_x11_monitor_forms() {
        assert_eq!(
            CaptureInput::parse("x11::0.0@1").unwrap(),
            CaptureInput::X11 { display: Some(":0.0".into()), monitor: Some(1) }
        );
        assert_eq!(
            CaptureInput::parse("x11@2").unwrap(),
            CaptureInput::X11 { display: None, monitor: Some(2) }
        );
        // 直接写显示号也接受 @N
        assert_eq!(
            CaptureInput::parse(":0.0@3").unwrap(),
            CaptureInput::X11 { display: Some(":0.0".into()), monitor: Some(3) }
        );
        assert!(CaptureInput::parse("x11::0.0@x").is_err(), "显示器号不是数字应报错");
        let c = CaptureInput::parse("x11::0.0@1").unwrap();
        assert_eq!(c.monitor_idx(), Some(1));
        assert!(c.describe().contains("显示器1"), "{}", c.describe());
    }

    #[test]
    fn old_x11_json_loads_without_monitor() {
        // 老 host.json（没有 monitor 字段）必须照常解析 —— 部署过的机器不用重配
        let old: CaptureInput = serde_json::from_str(r#"{"kind":"x11","display":":0.0"}"#).unwrap();
        assert_eq!(old, CaptureInput::X11 { display: Some(":0.0".into()), monitor: None });
        let bare: CaptureInput = serde_json::from_str(r#"{"kind":"x11"}"#).unwrap();
        assert_eq!(bare.monitor_idx(), None);
    }

    #[test]
    fn parse_lavfi_forms() {
        assert_eq!(
            CaptureInput::parse("lavfi").unwrap(),
            CaptureInput::Lavfi { spec: default_lavfi_spec(), frames: None }
        );
        assert_eq!(
            CaptureInput::parse("lavfi:testsrc=size=320x240:rate=10@50").unwrap(),
            CaptureInput::Lavfi { spec: "testsrc=size=320x240:rate=10".into(), frames: Some(50) }
        );
        // 合成源里常带 ':'，只有末尾的 @N 才是帧数
        let c = CaptureInput::parse("lavfi:testsrc2=size=640x480:rate=25@30").unwrap();
        assert_eq!(
            c,
            CaptureInput::Lavfi { spec: "testsrc2=size=640x480:rate=25".into(), frames: Some(30) }
        );
    }

    #[test]
    fn parse_rejects_junk() {
        assert!(CaptureInput::parse("vnc").is_err());
        assert!(CaptureInput::parse("x11x").is_err(), "x11 后面接不明尾巴应报错，不许静默当裸 x11");
        assert!(CaptureInput::parse("").is_err());
        assert!(CaptureInput::parse("lavfi:testsrc@abc").is_err(), "帧数不是数字应报错");
    }

    #[test]
    fn x11_input_args_match_the_production_path() {
        let o = CaptureOptions::new(":0.0", 1920, 1080);
        let a = CaptureInput::parse("x11::0.0").unwrap().to_ffmpeg_input(&o);
        assert!(a.contains(&"x11grab".to_string()));
        assert!(a.contains(&":0.0+0,0".to_string()));
        assert!(a.contains(&"1920x1080".to_string()));
    }

    #[test]
    fn lavfi_input_args_carry_the_frames_limit() {
        let o = CaptureOptions::new(":0.0", 320, 240);
        let a = CaptureInput::parse("lavfi:testsrc=size=320x240:rate=10@7").unwrap().to_ffmpeg_input(&o);
        assert_eq!(a, vec!["-f", "lavfi", "-i", "testsrc=size=320x240:rate=10", "-frames:v", "7"]);
    }

    #[test]
    fn unbounded_detection() {
        assert!(CaptureInput::parse("x11").unwrap().is_unbounded());
        assert!(!CaptureInput::parse("lavfi:testsrc@10").unwrap().is_unbounded());
        assert!(CaptureInput::parse("lavfi").unwrap().is_unbounded());
    }

    #[test]
    fn describe_is_human_readable() {
        assert!(CaptureInput::parse("x11").unwrap().describe().contains("X11"));
        assert!(CaptureInput::parse("lavfi:testsrc@10").unwrap().describe().contains("10 帧"));
    }

    #[test]
    fn xdpyinfo_dimensions_are_parsed_from_real_output() {
        // 真实 xdpyinfo 输出的缩略
        let text = "\
name of display:    :0
version number:    11.0
default screen number:    0
number of screens:    1

screen #0:
  dimensions:    1920x1080 pixels (508x285 millimeters)
  resolution:    96x96 dots per inch
";
        assert_eq!(parse_xdpyinfo_dimensions(text), Some((1920, 1080)));
    }

    #[test]
    fn xdpyinfo_parser_tolerates_variants() {
        assert_eq!(parse_xdpyinfo_dimensions("  dimensions:  2560x1440 pixels  "), Some((2560, 1440)));
        // 没有 dimensions 行
        assert_eq!(parse_xdpyinfo_dimensions("screen #0:\n  depth of root window: 24 planes\n"), None);
        // 尺寸非法
        assert_eq!(parse_xdpyinfo_dimensions("dimensions: 0x0 pixels"), None);
        assert_eq!(parse_xdpyinfo_dimensions("dimensions: 1920 pixels"), None);
        assert_eq!(parse_xdpyinfo_dimensions(""), None);
    }

    #[test]
    fn xrandr_connected_layout_parses_from_real_output() {
        let text = r#"Screen 0: minimum 320 x 200, current 3840 x 1080, maximum 16384 x 16384
VGA-1 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 508mm x 286mm
   1920x1080     60.00*+   50.00
HDMI-1 connected 1920x1080+1920+0 (normal left inverted right x axis y axis) 527mm x 296mm
   1920x1080     60.00*
DP-1 disconnected (normal left inverted right x axis y axis)
"#;
        let mons = parse_xrandr_connected(text);
        assert_eq!(mons.len(), 2, "只该有已连接的两块：{mons:?}");
        assert_eq!(mons[0], Region { x: 0, y: 0, w: 1920, h: 1080 });
        assert_eq!(mons[1], Region { x: 1920, y: 0, w: 1920, h: 1080 });
    }

    #[test]
    fn xrandr_negative_offsets_and_unlit_ports() {
        // 负偏移：副屏摆在主屏左侧/上方（xrandr 用 %+d，负号紧贴数字）
        let text = r#"Screen 0: minimum 8 x 8, current 2560 x 1440, maximum 32767 x 32767
HDMI-0 connected 2560x1440+0+0 (normal) 600mm x 340mm
eDP-1 connected 1280x800-1280-400 (normal) 300mm x 190mm
VGA-1 connected (normal left inverted right) 0mm x 0mm
"#;
        let mons = parse_xrandr_connected(text);
        assert_eq!(mons.len(), 2, "connected 但没点亮的口不该进布局：{mons:?}");
        assert_eq!(mons[1], Region { x: -1280, y: -400, w: 1280, h: 800 });
    }

    #[test]
    fn pick_region_falls_back_deterministically() {
        let m0 = Region { x: 0, y: 0, w: 1024, h: 768 };
        let m1 = Region { x: 1024, y: 0, w: 800, h: 600 };
        let mons = vec![m0, m1];
        let fb = (1824u32, 768u32);
        // 不指定显示器：整块帧缓冲（老行为）
        assert_eq!(pick_region(&mons, fb, None), Region::full(1824, 768));
        // 号存在：那块屏
        assert_eq!(pick_region(&mons, fb, Some(1)), m1);
        // 号超界但有输出（副屏被拔）：退回第一块，会话不断
        assert_eq!(pick_region(&mons, fb, Some(7)), m0);
        // xrandr 没列出任何输出（麒麟现网）：退回整块帧缓冲
        assert_eq!(pick_region(&[], fb, Some(2)), Region::full(1824, 768));
    }

    #[test]
    fn x11_region_offset_reaches_the_grab_url() {
        // 抓取区域的原点必须进 x11grab 的地址，否则永远只抓得到主屏
        let mut o = CaptureOptions::new(":0.0", 800, 600);
        o.grab_x = 1024;
        let a = CaptureInput::parse("x11::0.0@1").unwrap().to_ffmpeg_input(&o).join(" ");
        assert!(a.contains(":0.0+1024,0"), "抓取偏移没传给 x11grab: {a}");
        assert!(a.contains("800x600"), "抓取尺寸没传给 x11grab: {a}");
    }
}
