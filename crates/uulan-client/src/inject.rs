//! 脚本化键鼠注入（`--inject`）—— 把「键鼠真的打进了被控端」变成可自动验证的事。
//!
//! 窗口模式全靠人手点，验证 GUI 的键鼠只能「我按了一下，好像动了」。这个模式把一串
//! 动作写成脚本，沿**真实链路**发出去：控制通道 → 中继 → 被控端 `Mapper` → XTest。
//! 于是验收的一方（比如真麒麟上的 `xev` / `xdotool`）可以用机器可查的方式看结果。
//!
//! 用法：
//!
//! ```text
//! --inject "move:960,540; click:left; type:hello; wait:300"
//! ```
//!
//! 步骤用 `;` 或换行分隔，参数用 `,` 或 `:`：
//!
//! | 写法 | 含义 |
//! |---|---|
//! | `move:X,Y` | 绝对移动，单位是**被控端屏幕像素** |
//! | `movn:X,Y` | 绝对移动，归一化 0..1（排查坐标换算时用） |
//! | `click:left\|right\|middle` | 按下再松开 |
//! | `down:left` / `up:left` | 按住/松开（测「不卡键」用） |
//! | `wheel:up\|down\|left\|right[:N]` | 滚轮 N 格（默认 1 格 = 120） |
//! | `tap:65` / `tap:0x41` | 物理键（Windows VK），按下再松开 |
//! | `keydown:65` / `keyup:65` | 物理键单边（测修饰键配对） |
//! | `type:你好abc` | 按字符打（走 `KeyChar`，中文用 Unicode KeySym） |
//! | `wait:300` | 停 300 ms |
//!
//! 解析和「步骤 → 协议帧」的换算是纯函数（[`parse`] / [`plan`]），本机就能单测；
//! [`run`] 才碰网络。
//!
//! ## 为什么坐标要两种
//!
//! 协议里是归一化坐标（跨分辨率安全），但验的时候人想的是「挪到 960,540」。
//! 像素写法在 [`plan`] 里用 `hello_ok` 报来的真实屏幕尺寸换算 —— 顺序、边界、取整
//! 全在纯函数里，和窗口模式的 [`crate::input`] 用的是同一套取整规则（`max = 边长-1`），
//! 所以两边不会各算一套。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use uulan_protocol::crypto::SecControl;
use uulan_protocol::msg::{C2H, KeyInput, MouseButton};
use uulan_protocol::vk::KeyTarget;

use crate::control::{ControlLink, HostStatus, InputSender, InputStats};

/// 一个脚本步骤。
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// 绝对移动，被控端屏幕像素。
    MovePx { x: i32, y: i32 },
    /// 绝对移动，归一化 0..1。
    MoveNorm { x: f32, y: f32 },
    /// 单边：按下或松开。
    Button { button: MouseButton, down: bool },
    /// 按下再松开（一次完整点击）。
    Click { button: MouseButton },
    /// 滚轮，协议单位（一格 = 120）。
    Wheel { dx: i32, dy: i32 },
    /// 物理键（Windows VK）单边。
    Vk { vk: u16, down: bool, repeat: bool },
    /// 按下再松开一个物理键。
    Tap { vk: u16 },
    /// 按字符打（每个字符一对 down/up）。
    Type(String),
    /// 停顿。
    Wait(u64),
}

/// 规划出来的一件事：发一帧，或者等一下。
///
/// 不 derive `PartialEq`：`C2H` 本身没有实现它（协议消息只保证能序列化），
/// 测试里改比 JSON 文本，比的是线上真实形状。
#[derive(Debug, Clone)]
pub enum Action {
    Send(C2H),
    Sleep(u64),
}

/// 解析脚本。空步骤（多的分号、末尾分号）忽略。
pub fn parse(script: &str) -> Result<Vec<Step>> {
    let mut out = Vec::new();
    for raw in script.split([';', '\n']) {
        let s = raw.trim();
        if s.is_empty() {
            continue;
        }
        out.push(parse_step(s).with_context(|| format!("脚本步骤 {s:?} 不对"))?);
    }
    if out.is_empty() {
        bail!("--inject 的脚本是空的");
    }
    Ok(out)
}

fn parse_step(s: &str) -> Result<Step> {
    let (cmd, arg) = match s.split_once(':') {
        Some((c, a)) => (c.trim().to_ascii_lowercase(), a.trim()),
        None => (s.trim().to_ascii_lowercase(), ""),
    };
    match cmd.as_str() {
        "move" => {
            let (x, y) = two_ints(arg)?;
            Ok(Step::MovePx { x, y })
        }
        "movn" => {
            let (x, y) = two_floats(arg)?;
            Ok(Step::MoveNorm { x, y })
        }
        "click" => Ok(Step::Click { button: button(arg)? }),
        "down" => Ok(Step::Button { button: button(arg)?, down: true }),
        "up" => Ok(Step::Button { button: button(arg)?, down: false }),
        "wheel" => {
            let (dir, n) = match arg.split_once(':') {
                Some((d, n)) => (d.trim(), n.trim().parse::<i32>().context("格数要整数")?),
                None => (arg, 1),
            };
            if n <= 0 {
                bail!("滚轮格数要大于 0");
            }
            // 协议单位是 120/格（Windows WHEEL_DELTA）。
            let (dx, dy) = match dir.to_ascii_lowercase().as_str() {
                "up" => (0, 120 * n),
                "down" => (0, -120 * n),
                "right" => (120 * n, 0),
                "left" => (-120 * n, 0),
                _ => bail!("滚轮方向只认 up/down/left/right"),
            };
            Ok(Step::Wheel { dx, dy })
        }
        "tap" => Ok(Step::Tap { vk: vk(arg)? }),
        "keydown" => Ok(Step::Vk { vk: vk(arg)?, down: true, repeat: false }),
        "keyup" => Ok(Step::Vk { vk: vk(arg)?, down: false, repeat: false }),
        "type" => {
            if arg.is_empty() {
                bail!("type 后面没有内容");
            }
            Ok(Step::Type(arg.to_string()))
        }
        "wait" => Ok(Step::Wait(arg.parse::<u64>().context("毫秒要整数")?)),
        other => bail!("不认识的步骤 {other:?}（见 --help 里的 --inject 一节）"),
    }
}

fn two_ints(s: &str) -> Result<(i32, i32)> {
    let mut it = s.split(',').map(str::trim);
    let x = it.next().unwrap_or("").parse::<i32>().context("x 要整数")?;
    let y = it.next().unwrap_or("").parse::<i32>().context("y 要整数")?;
    Ok((x, y))
}

fn two_floats(s: &str) -> Result<(f32, f32)> {
    let mut it = s.split(',').map(str::trim);
    let x = it.next().unwrap_or("").parse::<f32>().context("x 要小数")?;
    let y = it.next().unwrap_or("").parse::<f32>().context("y 要小数")?;
    Ok((x, y))
}

fn button(s: &str) -> Result<MouseButton> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "left" => MouseButton::Left,
        "right" => MouseButton::Right,
        "middle" => MouseButton::Middle,
        "x1" => MouseButton::X1,
        "x2" => MouseButton::X2,
        _ => bail!("按钮只认 left/right/middle/x1/x2"),
    })
}

fn vk(s: &str) -> Result<u16> {
    let s = s.trim();
    let v = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => u16::from_str_radix(h, 16).context("十六进制 VK 不对")?,
        None => s.parse::<u16>().context("VK 要十进制或 0x 开头")?,
    };
    if v == 0 {
        bail!("VK 不能是 0");
    }
    Ok(v)
}

/// 像素坐标 → 归一化。取整规则与窗口模式一致：`max = 边长 - 1`，
/// 所以「右下角」正好是 1.0。
pub fn px_to_norm(x: i32, y: i32, w: u32, h: u32) -> (f32, f32) {
    let mw = w.saturating_sub(1).max(1) as f32;
    let mh = h.saturating_sub(1).max(1) as f32;
    ((x as f32 / mw).clamp(0.0, 1.0), (y as f32 / mh).clamp(0.0, 1.0))
}

/// 步骤 → 线上动作序列。纯函数：屏幕尺寸由调用方给（生产里来自 `hello_ok`）。
pub fn plan(steps: &[Step], screen_w: u32, screen_h: u32) -> Vec<Action> {
    let mut out = Vec::new();
    for st in steps {
        out.extend(plan_one(st, screen_w, screen_h));
    }
    out
}

/// **一个**步骤 → 动作序列。
///
/// 为什么要把 `plan` 拆成逐条的：现场注入（`--inject-live`）每一步都得按「当时」的
/// 画幅换算 —— 被控端在会话中途换了分辨率，还按开会话那一刻的尺寸乘坐标，落点就偏
/// （偏多少取决于新旧尺寸比，1080p→720p 是 17%）。一遍循环算完整脚本的 `plan`
/// 只能用一个尺寸，所以留出这个逐条入口，`plan` 变成它的一遍循环。
pub fn plan_one(st: &Step, screen_w: u32, screen_h: u32) -> Vec<Action> {
    let mut out = Vec::new();
    {
        match st {
            Step::MovePx { x, y } => {
                let (nx, ny) = px_to_norm(*x, *y, screen_w, screen_h);
                out.push(Action::Send(C2H::MouseMoveAbs { x: nx, y: ny }));
            }
            Step::MoveNorm { x, y } => out.push(Action::Send(C2H::MouseMoveAbs {
                x: x.clamp(0.0, 1.0),
                y: y.clamp(0.0, 1.0),
            })),
            Step::Button { button, down } => {
                out.push(Action::Send(C2H::MouseButton { button: *button, down: *down }))
            }
            Step::Click { button } => {
                out.push(Action::Send(C2H::MouseButton { button: *button, down: true }));
                out.push(Action::Send(C2H::MouseButton { button: *button, down: false }));
            }
            Step::Wheel { dx, dy } => out.push(Action::Send(C2H::MouseWheel { dx: *dx, dy: *dy })),
            Step::Vk { vk, down, repeat } => {
                out.push(Action::Send(C2H::Key { vk: *vk, down: *down, repeat: *repeat }))
            }
            Step::Tap { vk } => {
                out.push(Action::Send(C2H::Key { vk: *vk, down: true, repeat: false }));
                out.push(Action::Send(C2H::Key { vk: *vk, down: false, repeat: false }));
            }
            Step::Type(text) => {
                for ch in text.chars() {
                    let target = KeyTarget::Char { ch };
                    out.push(Action::Send(C2H::KeyChar(KeyInput {
                        target: target.clone(),
                        down: true,
                    })));
                    out.push(Action::Send(C2H::KeyChar(KeyInput { target, down: false })));
                }
            }
            Step::Wait(ms) => out.push(Action::Sleep(*ms)),
        }
        out
    }
}

/// 给日志用的一行步骤描述（只描述位置类步骤，别的返回空串就不打）。
fn step_tag(st: &Step) -> String {
    match st {
        Step::MovePx { x, y } => format!("move:{},{}", x, y),
        Step::MoveNorm { x, y } => format!("movn:{:.4},{:.4}", x, y),
        _ => String::new(),
    }
}

/// 端口（输入通道）取不到时最多等多久 —— 重连中的会话会在几秒内回来。
pub const PORT_WAIT: Duration = Duration::from_secs(10);

/// 一次注入的产出。
#[derive(Debug, Clone)]
pub struct Outcome {
    /// 实际发出去的输入帧数（`wait:` 步骤不算）。
    pub sent: u64,
    /// 被控端报告的注入错误数（0 = 全部真的打进去了）。
    pub host_errors: u64,
    /// 按出现顺序去重的**用到过的画幅**。
    ///
    /// 这是「换代之后按新尺寸换算」的证据：里面出现两个画幅，就说明确实有两段
    /// 各自的换算；只出现一个，说明这一轮压根没换过分辨率（不能拿来当已验）。
    pub sizes: Vec<(u32, u32)>,
}

/// 「现场注入」要的三个入口，全部由调用方给 —— 同 `Mapper` / `InputSink` 的分层思路：
/// 逻辑在这里，接线在外面，于是本机能用内存假通道把「逐步换画幅」这条路钉住。
pub struct Hooks {
    /// 当前的输入发送端。`None` = 这一刻没有可注入的会话（正在重连 / 已放弃）。
    pub port: Box<dyn Fn() -> Option<InputSender> + Send + Sync>,
    /// **当前**画幅（被控端屏幕尺寸，不是编码画幅）。会话中途换过分辨率必须报新值。
    pub size: Box<dyn Fn() -> (u32, u32) + Send + Sync>,
    /// 被控端状态（锁屏门禁在这里读）。
    pub status: Arc<HostStatus>,
    /// 输入计数：与这条会话共用同一份，收工时的「已发送 N 条」才和窗口/统计一致。
    pub stats: Arc<InputStats>,
    /// 端口取不到时的等待上限（单测把它调短）。
    pub port_wait: Duration,
}

/// 在**正在跑的会话里**按脚本发键鼠：每一步发出前重新读一次画幅。
///
/// `annotate = true` 时每发一条移动都打一行「画幅 / 请求 / 归一化」的换算，
/// 验收脚本据此把「客户端按新尺寸换算」和「被控端按新尺寸落点」两端对上号
/// （只看到指针动了，分不出是哪一端跟着换了尺寸）。
pub async fn run_with(hooks: Hooks, steps: &[Step], annotate: bool) -> Result<Outcome> {
    // 先等被控端的桌面状态（它开局就发，正常是几毫秒）。
    //
    // 不等就会有一个真踩到过的窗口：注入跑在状态到达之前，于是键鼠进了锁屏。
    // 被控端那边现在也会挡（那是权威的一侧），但脚本化验收要的是「一开始就不发」。
    // 等不到（老版本被控端不发这条）就照旧往下走，不因为等不到而失败。
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    while !hooks.status.snapshot().known && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let mut sent = 0u64;
    let mut sizes: Vec<(u32, u32)> = Vec::new();
    for st in steps {
        // 每一步取一次画幅。**不能**在循环外取：那是「开会话时的尺寸」，
        // 换过分辨率之后就变成按旧尺寸乘坐标。
        let (mut w, mut h) = (hooks.size)();
        if w == 0 || h == 0 {
            // 还没有任何一条 VideoCfg 报过画幅（或对面报了 0）—— 宁可沿用上一次换算
            // 用的尺寸，也不拿 0 去除（`px_to_norm` 会把它夹成 1.0，落点悄悄跑到右下角）。
            match sizes.last() {
                Some(&(pw, ph)) => {
                    w = pw;
                    h = ph;
                }
                None => bail!("还没收到被控端的画幅（0x0），换不出坐标：等画面通了再注入"),
            }
        }
        if !sizes.iter().any(|s| *s == (w, h)) {
            sizes.push((w, h));
        }
        let tag = step_tag(st);
        for a in plan_one(st, w, h) {
            match a {
                Action::Send(m) => {
                    let port = wait_port(&hooks).await?;
                    if !port.send(m.clone()) {
                        bail!("控制通道已断开（已经发了 {sent} 条）");
                    }
                    sent += 1;
                    if annotate && !tag.is_empty() {
                        if let C2H::MouseMoveAbs { x, y } = &m {
                            println!(
                                "注入换算 : 画幅={}x{} 步骤={} → 归一化={:.4},{:.4}",
                                w, h, tag, x, y
                            );
                        }
                    }
                    // 别一股脑塞进去：留一点时间给被控端和它后面的 X 客户端。
                    tokio::time::sleep(Duration::from_millis(12)).await;
                }
                Action::Sleep(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
            }
        }
    }
    // 留一点时间让被控端的 InputError 回来 —— 不然「注入失败」会被当成成功。
    tokio::time::sleep(Duration::from_millis(600)).await;

    let si = hooks.stats.snapshot();
    let hs = hooks.status.snapshot();
    if hs.known && hs.locked {
        // 说清楚「为什么一条都没注入」：不然看图的人会以为是注入坏了。
        println!("被控端处于锁屏：键鼠按策略不发（XTest 会绕过锁屏抓取，往锁屏里打字有风险）");
    }
    println!(
        "已发送 {} 条输入帧（移动 {} / 按键 {} / 文本 {} / 点击 {} / 滚轮 {}；未送出 {}；被控端拒绝 {}）",
        sent, si.moves, si.keys, si.chars, si.buttons, si.wheels, si.dropped, si.host_errors
    );
    Ok(Outcome { sent, host_errors: si.host_errors, sizes })
}

/// 取输入通道；拿不到就等（会话正在重连时这是常态，不该算失败）。
async fn wait_port(hooks: &Hooks) -> Result<InputSender> {
    let until = tokio::time::Instant::now() + hooks.port_wait;
    loop {
        if let Some(p) = (hooks.port)() {
            return Ok(p);
        }
        if tokio::time::Instant::now() >= until {
            bail!(
                "等了 {} 秒还是没有可注入的输入通道（画面已断且没接回来，或被控端没有图形会话）",
                hooks.port_wait.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// 跑一遍脚本（独立会话，`--inject`）。返回「被控端报告的注入错误数」。
///
/// 这条路的画幅是**开会话时**握手报来的，整个脚本用同一个尺寸 —— 这正是它验不了
/// 「换分辨率之后的落点」的原因，那条走 [`run_with`]（`--inject-live`）。
pub async fn run(control: SecControl, steps: &[Step], screen_w: u32, screen_h: u32) -> Result<u64> {
    let link = ControlLink::start(control);
    let sender = link.input();
    let out = run_with(
        Hooks {
            port: Box::new(move || Some(sender.clone())),
            size: Box::new(move || (screen_w, screen_h)),
            status: link.status(),
            stats: link.stats(),
            port_wait: PORT_WAIT,
        },
        steps,
        false,
    )
    .await?;
    link.close("注入脚本执行完毕").await;
    Ok(out.host_errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msgs(v: &[Action]) -> Vec<C2H> {
        v.iter()
            .filter_map(|a| match a {
                Action::Send(m) => Some(m.clone()),
                Action::Sleep(_) => None,
            })
            .collect()
    }

    /// `C2H` 没有 `PartialEq`，也比整串 JSON —— 逐字段看类型和值更抗字段顺序/可选字段变化。
    fn val(m: &C2H) -> serde_json::Value {
        serde_json::to_value(m).unwrap()
    }

    #[test]
    fn parse_full_script() {
        let s = "move:960,540; click:left; wheel:down:3; type:hello; wait:250; tap:0x41";
        let steps = parse(s).unwrap();
        assert_eq!(steps.len(), 6);
        assert_eq!(steps[0], Step::MovePx { x: 960, y: 540 });
        assert_eq!(steps[1], Step::Click { button: MouseButton::Left });
        assert_eq!(steps[2], Step::Wheel { dx: 0, dy: -360 });
        assert_eq!(steps[3], Step::Type("hello".into()));
        assert_eq!(steps[4], Step::Wait(250));
        assert_eq!(steps[5], Step::Tap { vk: 0x41 });
    }

    #[test]
    fn parse_tolerates_newlines_and_trailing_semicolon() {
        let steps = parse("move:1,2;\n\nclick:right;\n").unwrap();
        assert_eq!(steps.len(), 2);
    }

    #[test]
    fn parse_rejects_junk() {
        assert!(parse("").is_err());
        assert!(parse("frobnicate:1").is_err());
        assert!(parse("move:1").is_err());
        assert!(parse("click:elbow").is_err());
        assert!(parse("wheel:sideways").is_err());
        assert!(parse("type:").is_err());
        assert!(parse("wait:soon").is_err());
        assert!(parse("tap:0").is_err());
    }

    #[test]
    fn px_conversion_hits_the_corners() {
        assert_eq!(px_to_norm(0, 0, 1920, 1080), (0.0, 0.0));
        assert_eq!(px_to_norm(1919, 1079, 1920, 1080), (1.0, 1.0));
        // 越界夹到边上（不是丢弃、不是回绕）。
        assert_eq!(px_to_norm(-50, 5000, 1920, 1080), (0.0, 1.0));
        let (x, y) = px_to_norm(960, 540, 1920, 1080);
        assert!((x - 0.5003).abs() < 0.001, "x = {x}");
        assert!((y - 0.5005).abs() < 0.001, "y = {y}");
    }

    #[test]
    fn click_expands_to_press_and_release() {
        let m = msgs(&plan(&parse("click:right").unwrap(), 800, 600));
        assert_eq!(m.len(), 2);
        let a = val(&m[0]);
        assert_eq!(a["type"], "mouse_button");
        assert_eq!(a["button"], "right");
        assert_eq!(a["down"], true);
        let b = val(&m[1]);
        assert_eq!(b["down"], false);
    }

    #[test]
    fn type_expands_to_pairs_and_keeps_unicode() {
        let steps = parse("type:a中").unwrap();
        let m = msgs(&plan(&steps, 1920, 1080));
        assert_eq!(m.len(), 4);
        for (i, ch) in ['a', '中'].iter().enumerate() {
            for (j, down) in [(0usize, true), (1usize, false)] {
                match &m[i * 2 + j] {
                    C2H::KeyChar(k) => {
                        assert_eq!(k.target, KeyTarget::Char { ch: *ch });
                        assert_eq!(k.down, down);
                    }
                    other => panic!("第 {} 条不是 KeyChar：{other:?}", i * 2 + j),
                }
            }
        }
        // 字符一律走 KeyChar（被控端负责解析/动态注册），不发物理键。
        assert!(!m.iter().any(|x| matches!(x, C2H::Key { .. })));
    }

    #[test]
    fn wheel_uses_120_steps() {
        let m = msgs(&plan(&parse("wheel:up; wheel:left:2").unwrap(), 800, 600));
        assert_eq!(m.len(), 2);
        let a = val(&m[0]);
        assert_eq!(a["type"], "mouse_wheel");
        assert_eq!(a["dx"], 0);
        assert_eq!(a["dy"], 120);
        let b = val(&m[1]);
        assert_eq!(b["dx"], -240);
        assert_eq!(b["dy"], 0);
    }

    #[test]
    fn button_and_key_edges_are_not_duplicated() {
        let m = msgs(&plan(&parse("down:left; keydown:17; keyup:17").unwrap(), 800, 600));
        assert_eq!(m.len(), 3);
        let a = val(&m[0]);
        assert_eq!(a["type"], "mouse_button");
        assert_eq!(a["button"], "left");
        assert_eq!(a["down"], true);
        let b = val(&m[1]);
        assert_eq!(b["type"], "key");
        assert_eq!(b["vk"], 17);
        assert_eq!(b["down"], true);
        assert_eq!(b["repeat"], false);
        let c = val(&m[2]);
        assert_eq!(c["type"], "key");
        assert_eq!(c["down"], false);
    }

    #[test]
    fn movn_passes_through_normalized() {
        let m = msgs(&plan(&parse("movn:0.25,0.75").unwrap(), 1920, 1080));
        assert_eq!(m.len(), 1);
        let a = val(&m[0]);
        assert_eq!(a["type"], "mouse_move_abs");
        assert!((a["x"].as_f64().unwrap() - 0.25).abs() < 1e-6);
        assert!((a["y"].as_f64().unwrap() - 0.75).abs() < 1e-6);
    }

    #[test]
    fn wait_becomes_sleep_not_a_frame() {
        let acts = plan(&parse("wait:120").unwrap(), 800, 600);
        assert_eq!(acts.len(), 1);
        match &acts[0] {
            Action::Sleep(ms) => assert_eq!(*ms, 120),
            other => panic!("wait 变成了 {other:?}"),
        }
    }

    // ── 现场注入（--inject-live）：每一步都用「当时」的画幅 ──────────────────
    //
    // 这台机器上没有第二个 X 服务器，所以这里用内存假通道：`InputSender::for_test`
    // 把帧收到一个 mpsc 里，画幅由闭包按调用次数给 —— 于是「换代之后落点跟不跟得上」
    // 这一端（控制端换算）能在本机钉死，被控端那一端的真机数字见 docs/milestones/。

    use std::sync::atomic::{AtomicU64, Ordering as Ord2};
    use tokio::sync::mpsc;

    fn hooks_with(
        sizes: Vec<(u32, u32)>,
        port_available_after: u64,
        port_wait: Duration,
    ) -> (Hooks, mpsc::UnboundedReceiver<C2H>, Arc<AtomicU64>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let status = Arc::new(HostStatus::default());
        let sender = InputSender::for_test(tx, status.clone());
        let stats = Arc::new(InputStats::default());
        let calls = Arc::new(AtomicU64::new(0));
        let table: Vec<(u32, u32)> = sizes;
        let c1 = calls.clone();
        let c2 = calls.clone();
        let hooks = Hooks {
            port: Box::new(move || {
                if c1.fetch_add(1, Ord2::SeqCst) < port_available_after {
                    None
                } else {
                    Some(sender.clone())
                }
            }),
            size: Box::new(move || {
                let n = (c2.fetch_add(1, Ord2::SeqCst) as usize).min(table.len() - 1);
                table[n]
            }),
            status,
            stats,
            port_wait,
        };
        (hooks, rx, calls)
    }

    fn moves(rx: &mut mpsc::UnboundedReceiver<C2H>) -> Vec<(f32, f32)> {
        let mut out = Vec::new();
        while let Ok(m) = rx.try_recv() {
            if let C2H::MouseMoveAbs { x, y } = m {
                out.push((x, y));
            }
        }
        out
    }

    /// 会话中途换了分辨率：第二条 `move:` 必须按**新**画幅换算。
    ///
    /// 这条就是「独立会话 + 握手尺寸」那种写法永远测不到的东西：按旧尺寸算，
    /// 1280x720 的 (640,360) 会变成 0.3333（该 0.5004），落点偏 17%。
    #[tokio::test]
    async fn live_injection_replans_every_step_with_the_current_size() {
        let (hooks, mut rx, _) = hooks_with(
            vec![(1920, 1080), (1280, 720), (1280, 720)],
            0,
            Duration::from_millis(50),
        );
        let steps = parse("move:960,540; move:640,360").unwrap();
        let out = run_with(hooks, &steps, false).await.unwrap();

        let got = moves(&mut rx);
        assert_eq!(got.len(), 2, "两条移动都要发出去");
        assert!((got[0].0 - 960.0 / 1919.0).abs() < 1e-5, "第一条按 1920x1080：{:?}", got[0]);
        assert!((got[0].1 - 540.0 / 1079.0).abs() < 1e-5);
        // 第二步换成 1280x720：按旧尺寸的话这里是 0.3333，肉眼看得见的偏。
        assert!((got[1].0 - 640.0 / 1279.0).abs() < 1e-5, "第二条按 1280x720：{:?}", got[1]);
        assert!((got[1].1 - 360.0 / 719.0).abs() < 1e-5);
        assert_eq!(out.sent, 2);
        assert_eq!(out.sizes, vec![(1920, 1080), (1280, 720)], "用到过的画幅按顺序去重");
    }

    /// 断线重连期间没有输入通道：等它回来，而不是当失败（也不许悄悄丢计数）。
    #[tokio::test]
    async fn live_injection_waits_for_the_port_instead_of_failing() {
        // 前 2 次取不到（poll 间隔 100ms，两次 200ms），第三次起有。
        let (hooks, mut rx, _) = hooks_with(vec![(800, 600)], 2, Duration::from_millis(2000));
        let steps = parse("click:left").unwrap();
        let out = run_with(hooks, &steps, false).await.unwrap();
        assert_eq!(out.sent, 2, "一次按下 + 一次松开");
        let mut n = 0;
        while let Ok(m) = rx.try_recv() {
            if matches!(m, C2H::MouseButton { .. }) {
                n += 1;
            }
        }
        assert_eq!(n, 2);
    }

    /// 等不到通道就把话说清楚，别伪装成成功。
    #[tokio::test]
    async fn live_injection_reports_when_the_port_never_comes_back() {
        let (hooks, _rx, _) = hooks_with(vec![(800, 600)], 1_000_000, Duration::from_millis(120));
        let steps = parse("move:10,10").unwrap();
        let e = run_with(hooks, &steps, false).await.unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains("没有可注入的输入通道"), "实际：{msg}");
    }

    /// 画幅报了 0x0（还没收到任何 VideoCfg）：第一步就明确失败，
    /// 不能拿 0 去除 —— `px_to_norm` 会把它夹成 1.0，落点悄悄跑到右下角。
    #[tokio::test]
    async fn live_injection_refuses_a_zero_screen_before_any_conversion() {
        let (hooks, _rx, _) = hooks_with(vec![(0, 0)], 0, Duration::from_millis(50));
        let steps = parse("move:100,100").unwrap();
        let e = run_with(hooks, &steps, false).await.unwrap_err();
        assert!(format!("{e:#}").contains("换不出坐标"), "实际：{e:#}");
    }

    /// 中途报了一次 0x0（对面短暂没报画幅）：沿用上一次换算用的尺寸，而不是报错，
    /// 也不是拿 0 去除。
    #[tokio::test]
    async fn live_injection_reuses_the_last_known_size_for_a_zero_reading() {
        let (hooks, mut rx, _) = hooks_with(
            vec![(1920, 1080), (0, 0), (1920, 1080)],
            0,
            Duration::from_millis(50),
        );
        let steps = parse("move:960,540; move:960,540").unwrap();
        let out = run_with(hooks, &steps, false).await.unwrap();
        let got = moves(&mut rx);
        assert_eq!(got.len(), 2);
        for g in &got {
            assert!((g.0 - 960.0 / 1919.0).abs() < 1e-5, "第二步本该沿用 1920x1080：{g:?}");
        }
        assert_eq!(out.sizes, vec![(1920, 1080)], "0x0 不该被当成一个新画幅记进去");
    }

    /// `plan_one` 必须与 `plan` 逐条一致：`--inject`（老路）不能被这次重构改了行为。
    #[test]
    fn plan_one_is_plan_applied_one_step_at_a_time() {
        let steps = parse("move:1,2; click:right; wheel:down:3; type:a; wait:5; tap:0x41").unwrap();
        let whole = plan(&steps, 800, 600);
        let mut piece = Vec::new();
        for st in &steps {
            piece.extend(plan_one(st, 800, 600));
        }
        let j = |v: &[Action]| serde_json::to_string(&v.iter().filter_map(|a| match a {
            Action::Send(m) => Some(m),
            Action::Sleep(_) => None,
        }).collect::<Vec<_>>()).unwrap();
        assert_eq!(j(&whole), j(&piece));
        assert_eq!(whole.len(), piece.len(), "wait 的 Sleep 也要逐条对齐");
    }

}