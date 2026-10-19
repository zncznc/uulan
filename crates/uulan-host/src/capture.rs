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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CaptureInput {
    /// 抓 X11 屏幕。`display` 为 `None` 时用 `$DISPLAY`，再退回 `:0.0`。
    X11 {
        #[serde(default)]
        display: Option<String>,
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
        CaptureInput::X11 { display: None }
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
            let rest = rest.strip_prefix("::").unwrap_or("");
            if rest.is_empty() {
                return Ok(CaptureInput::X11 { display: None });
            }
            // X 显示名的形式是 `:0.0` 或 `host:0.0`。用户写 `x11::0.0` 时
            // 顺手省掉冒号很常见，这里补上；已经带冒号的（含主机名那种）原样保留。
            let display = if rest.contains(':') { rest.to_string() } else { format!(":{rest}") };
            return Ok(CaptureInput::X11 { display: Some(display) });
        }
        // 也接受直接写一个显示号，比如 ":0.0"
        if s.starts_with(':') {
            return Ok(CaptureInput::X11 { display: Some(s.to_string()) });
        }
        Err(format!("无法识别的采集源 {s:?}（可用 x11 / lavfi[:spec[@frames]]）"))
    }

    /// 实际使用的 X11 显示号。
    pub fn display(&self) -> Option<String> {
        match self {
            CaptureInput::X11 { display } => Some(
                display
                    .clone()
                    .or_else(|| std::env::var("DISPLAY").ok())
                    .filter(|d| !d.trim().is_empty())
                    .unwrap_or_else(|| ":0.0".to_string()),
            ),
            CaptureInput::Lavfi { .. } => None,
        }
    }

    /// 转成 FFmpeg 的输入段参数。
    pub fn to_ffmpeg_input(&self, o: &CaptureOptions) -> Vec<String> {
        match self {
            CaptureInput::X11 { .. } => ffmpeg::x11grab_input(o),
            CaptureInput::Lavfi { spec, frames } => {
                let mut a = vec!["-f".to_string(), "lavfi".to_string(), "-i".to_string(), spec.clone()];
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
            CaptureInput::X11 { .. } => match self.display() {
                Some(d) => format!("X11 屏幕 {d}"),
                None => "X11 屏幕".into(),
            },
            CaptureInput::Lavfi { spec, frames } => match frames {
                Some(n) => format!("合成源 {spec}（{n} 帧）"),
                None => format!("合成源 {spec}（不限）"),
            },
        }
    }
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
    fn parse_x11_forms() {
        assert_eq!(CaptureInput::parse("x11").unwrap(), CaptureInput::X11 { display: None });
        assert_eq!(
            CaptureInput::parse("x11::1.0").unwrap(),
            CaptureInput::X11 { display: Some(":1.0".into()) }
        );
        assert_eq!(
            CaptureInput::parse(":0.0").unwrap(),
            CaptureInput::X11 { display: Some(":0.0".into()) }
        );
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
}
