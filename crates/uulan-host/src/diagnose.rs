//! 现场自检：一条命令回答「这台机器现在能不能被控」。
//!
//! 装完、换机、现场报障时，第一件想知道的事是「哪一环没通」。以前只能逐个手敲：
//! `xdpyinfo`、`ffmpeg -encoders`、`nc` 试中继、`uulan-host --show` 看身份……
//! 而且每条都要自己判断输出对不对。`uulan-host --diagnose` 把这几步做成一组检查，
//! 每项给一行结论 + 一句修复建议，退出码直接表达「能不能跑」。
//!
//! 三条纪律：
//!
//! - **只读**。不注册中继（那是一台被控端的身份，自检不该去抢注册）、不改配置、
//!   不动 X 的全局状态（`X11Sink::open` 会关 autorepeat，所以这里用只查扩展的
//!   [`probe_xtest`]）。唯一的写操作是往配置目录写一个临时文件再删掉 —— 那是为了
//!   「可写」这个结论必须真写一次才算数。
//! - **不打印任何秘密**。口令、临时码、注册令牌在这里只出现「有 / 无」。
//! - **不知道就说不知道**。读不到 `/proc`、`xdpyinfo` 不在，都落到 `WARN` 并把
//!   回退行为写清楚，而不是假装通过。
//!
//! [`probe_xtest`]: crate::input::x11::probe_xtest

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use uulan_media::ffmpeg::{self, Encoder};

use crate::capture::CaptureInput;
use crate::config::HostConfig;

/// 单项检查的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// 通过。
    Ok,
    /// 能跑，但有一项能力用不上（例如合成源没有 X11、非 Linux 没有 XTest）。
    Warn,
    /// 跑不起来。
    Fail,
}

impl Level {
    fn tag(self) -> &'static str {
        match self {
            Level::Ok => "[ OK ]",
            Level::Warn => "[WARN]",
            Level::Fail => "[FAIL]",
        }
    }
}

/// 一项检查。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub level: Level,
    /// 检查项名字（两三个汉字，对齐用）。
    pub name: &'static str,
    /// 结论详情：现场要能照着这一行判断，所以写具体路径/数字，不写「正常」。
    pub detail: String,
    /// 不通过时给的建议（通过时一般是 `None`）。
    pub hint: Option<String>,
}

/// 一整份自检报告。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    pub fn new() -> Self {
        Self::default()
    }

    fn push(&mut self, level: Level, name: &'static str, detail: impl Into<String>) -> &mut Check {
        self.checks.push(Check { level, name, detail: detail.into(), hint: None });
        self.checks.last_mut().expect("刚 push 过")
    }

    fn ok(&mut self, name: &'static str, detail: impl Into<String>) {
        self.push(Level::Ok, name, detail);
    }

    fn warn(&mut self, name: &'static str, detail: impl Into<String>, hint: impl Into<String>) {
        self.push(Level::Warn, name, detail).hint = Some(hint.into());
    }

    fn fail(&mut self, name: &'static str, detail: impl Into<String>, hint: impl Into<String>) {
        self.push(Level::Fail, name, detail).hint = Some(hint.into());
    }

    /// `(通过, 警告, 失败)` 三项计数。
    pub fn counts(&self) -> (usize, usize, usize) {
        let n = |lv: Level| self.checks.iter().filter(|c| c.level == lv).count();
        (n(Level::Ok), n(Level::Warn), n(Level::Fail))
    }

    /// 没有失败项 = 这台机器现在能被控。警告项不影响这个结论。
    pub fn all_ok(&self) -> bool {
        self.counts().2 == 0
    }

    /// 给人看的整份报告（也是 `--diagnose` 的 stdout 格式）。
    ///
    /// 每项一行，行首固定 `[ OK ]` / `[WARN]` / `[FAIL]`，方便 `grep FAIL` 与脚本断言。
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("== uulan 被控端自检 ==\n");
        for c in &self.checks {
            out.push_str(&format!(
                "{tag} {name} {detail}\n",
                tag = c.level.tag(),
                name = pad_to(c.name, 10),
                detail = c.detail
            ));
            if let Some(h) = &c.hint {
                out.push_str(&format!("         └─ {h}\n"));
            }
        }
        let (ok, warn, fail) = self.counts();
        out.push_str(&format!("\n== {ok} 通过 / {warn} 警告 / {fail} 失败 ==\n"));
        if fail == 0 {
            out.push_str("结论：可以跑（`uulan-host` 或用 systemd 用户单元常驻）。\n");
        } else {
            out.push_str("结论：还不能跑 —— 先按上面 [FAIL] 的建议处理。\n");
        }
        out
    }
}

/// 跑一遍全部检查。
///
/// `cfg_path` 只用来「报告 + 探测可写」，绝不改写；`ffmpeg_arg` / `encoder` 来自命令行，
/// 好让 `--ffmpeg`、`--encoder` 的取值也一起被验。
pub async fn run(
    cfg: &HostConfig,
    cfg_path: &Path,
    ffmpeg_arg: Option<&Path>,
    encoder: Encoder,
) -> Report {
    let mut r = Report::new();
    check_identity(&mut r, cfg, cfg_path);
    check_writable(&mut r, cfg_path);
    let display = cfg.capture.display();
    check_display(&mut r, &cfg.capture, display.as_deref()).await;
    check_xtest(&mut r, &cfg.capture, display.as_deref());
    check_ffmpeg(&mut r, ffmpeg_arg, encoder).await;
    check_relay(&mut r, &cfg.server).await;
    check_host(&mut r);
    r
}

/// 身份与配置内容：只报「有 / 无」，不报值。
fn check_identity(r: &mut Report, cfg: &HostConfig, cfg_path: &Path) {
    let exists = cfg_path.exists();
    let id = cfg.device_id.clone().unwrap_or_else(|| "（还没分配，首次连上中继后由中继给）".into());
    let detail = format!(
        "{}；设备 ID {id}；临时码 {}；注册令牌 {}",
        if exists { cfg_path.display().to_string() } else { format!("{}（文件还不存在，会自动创建）", cfg_path.display()) },
        if cfg.temp_code.is_some() { "有" } else { "无" },
        if cfg.reg_token.is_empty() { "无（会在首次启动时补上）" } else { "有" },
    );
    if exists {
        // 权限位也是现场常见问题：配置文件里就放着访问密码。
        let mode: Option<u32> = std::fs::metadata(cfg_path).ok().and_then(|m| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                Some(m.permissions().mode() & 0o777)
            }
            #[cfg(not(unix))]
            {
                let _ = &m;
                None
            }
        });
        match mode {
            Some(m) if m & 0o077 == 0 => r.ok("身份", detail),
            Some(m) => r.warn(
                "身份",
                format!("{detail}；权限 {m:o}（同组/其他人可读）"),
                "配置文件里有访问密码：chmod 600 一下。",
            ),
            None => r.ok("身份", detail),
        }
    } else {
        r.ok("身份", detail);
    }
    if let Err(e) = cfg.check() {
        r.fail("配置", format!("配置内容不合法：{e}"), "按提示改配置文件（或删掉它重新生成）。");
    } else {
        r.ok("配置", format!("采集源 {}；中继 {}", cfg.capture.describe(), cfg.server));
    }
}

/// 配置目录能不能写。
///
/// **必须真写一次**：目录存在但只读、家目录只读、配额满，`create_dir_all` 都能过，
/// 写文件才会失败。写进去的是 `.diagnose-probe`，随即删掉。
fn check_writable(r: &mut Report, cfg_path: &Path) {
    let dir = cfg_path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    let probe = dir.join(".diagnose-probe");
    let res = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        let mut f = std::fs::File::create(&probe)?;
        f.write_all(b"uulan diagnose\n")?;
        f.sync_all()?;
        drop(f);
        std::fs::remove_file(&probe)
    })();
    match res {
        Ok(()) => r.ok("可写", format!("{} 可写（配置、设备台账、日志都落在这里）", dir.display())),
        Err(e) => r.fail(
            "可写",
            format!("{} 写不了：{e}", dir.display()),
            "被控端要在这里写配置与临时码；换 --config 指向可写目录，或修权限。",
        ),
    }
}

/// 显示（只有 X11 采集源才需要）。
async fn check_display(r: &mut Report, capture: &CaptureInput, display: Option<&str>) {
    let Some(disp) = display else {
        r.warn(
            "显示",
            format!("采集源是{}，不依赖 X11", capture.describe()),
            "合成源只用于自测/演示；正式被控要 `--capture x11`。",
        );
        return;
    };

    let dims = crate::capture::detect_x11_screen(disp).await;
    #[cfg(target_os = "linux")]
    let state = crate::probe::x11::system_state(disp);
    #[cfg(not(target_os = "linux"))]
    let state: Option<crate::probe::DesktopState> = None;

    let has_desktop = state.map(|s| s.has_desktop).unwrap_or(false);
    if !has_desktop {
        r.fail(
            "显示",
            format!("打不开 X 显示 {disp}"),
            "确认这台机器是图形登录（不是 ssh/文本控制台）、DISPLAY 与 XAUTHORITY 对，且 uulan-host 跑在该用户自己的会话里。",
        );
        return;
    }
    let locked = state.map(|s| s.locked).unwrap_or(false);
    #[cfg(target_os = "linux")]
    let focus = crate::probe::x11::focus_name(disp).unwrap_or_else(|| "（读不到焦点窗口名）".into());
    #[cfg(not(target_os = "linux"))]
    let focus = "（非 Linux，不探）".to_string();

    let size = match dims {
        Some((w, h)) => format!("{w}x{h}"),
        None => "尺寸未知（xdpyinfo 不可用，会回退 1920x1080）".into(),
    };
    let detail = format!("{disp} 可用，{size}，锁屏 {}，焦点 {focus}", if locked { "是" } else { "否" });
    if dims.is_none() {
        r.warn("显示", detail, "屏幕尺寸建议在配置里写死 screen_width/screen_height。");
    } else if locked {
        // 锁屏不是故障：控制端会收到 locked=true 并停发键鼠。这里只是提醒。
        r.warn("显示", detail, "现在锁着屏：连上能看到画面，但键鼠会被门禁挡下（这是策略，不是故障）。");
    } else {
        r.ok("显示", detail);
    }
}

/// XTest 注入能力（只查扩展，不动 autorepeat）。
fn check_xtest(r: &mut Report, capture: &CaptureInput, display: Option<&str>) {
    let Some(disp) = display else {
        r.warn("键鼠注入", format!("采集源是{}，自检不探 XTest", capture.describe()), "合成源场景不需要注入后端。");
        return;
    };
    #[cfg(target_os = "linux")]
    match crate::input::x11::probe_xtest(Some(disp)) {
        Ok((major, minor)) => r.ok("键鼠注入", format!("XTest {major}.{minor} 可用（{disp}）")),
        Err(e) => r.fail(
            "键鼠注入",
            e,
            "缺 XTest 就注入不了键鼠：确认 libXtst 在（麒麟上另见 libX11.so 软链那条配库坑）。",
        ),
    }
    #[cfg(not(target_os = "linux"))]
    {
        r.warn("键鼠注入", format!("{disp}：非 Linux 没有 XTest 后端"), "被控端正式部署在麒麟上。");
    }
}

/// ffmpeg 与编码器。
async fn check_ffmpeg(r: &mut Report, ffmpeg_arg: Option<&Path>, encoder: Encoder) {
    let Some(path) = ffmpeg::find_ffmpeg(ffmpeg_arg) else {
        r.fail(
            "ffmpeg",
            "找不到 ffmpeg",
            "被控端靠它抓屏与编码：装上 ffmpeg（麒麟自带），或用 --ffmpeg 指定路径。",
        );
        return;
    };
    let ver = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(&path).arg("-version").output(),
    )
    .await;
    let version = match ver {
        Ok(Ok(o)) if o.status.success() => parse_ffmpeg_version(&String::from_utf8_lossy(&o.stdout)),
        Ok(Ok(o)) => {
            r.fail("ffmpeg", format!("{} 跑不起来（-version 退出码 {}）", path.display(), o.status.code().unwrap_or(-1)), "换一个 ffmpeg，或用 --ffmpeg 指定。");
            return;
        }
        Ok(Err(e)) => {
            r.fail("ffmpeg", format!("{} 执行失败：{e}", path.display()), "检查路径与执行权限。");
            return;
        }
        Err(_) => {
            r.fail("ffmpeg", format!("{} -version 5 秒没返回", path.display()), "这个 ffmpeg 卡住了（或路径指向了别的程序）。");
            return;
        }
    };

    let list = tokio::time::timeout(
        Duration::from_secs(8),
        tokio::process::Command::new(&path).args(["-hide_banner", "-encoders"]).output(),
    )
    .await;
    let encoders = match list {
        Ok(Ok(o)) => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => String::new(),
    };
    let want = encoder.codec_name();
    let Some(version) = version else {
        r.fail(
            "ffmpeg",
            format!("{} -version 里没有 ffmpeg 版本行（这大概不是 ffmpeg）", path.display()),
            "`--ffmpeg` 指错了：给它真正的 ffmpeg 可执行文件，或不加这个参数走 PATH。",
        );
        return;
    };
    let detail = format!("{}（{version}）", path.display());
    if encoders.is_empty() {
        r.warn("ffmpeg", format!("{detail}；拿不到编码器列表"), format!("手工确认 `ffmpeg -encoders | grep {want}`。"));
    } else if has_encoder(&encoders, want) {
        r.ok("ffmpeg", format!("{detail}，编码器 {want} 在"));
    } else {
        r.fail(
            "ffmpeg",
            format!("{detail}，但编码器列表里没有 {want}"),
            "这个 ffmpeg 没带该编码器；换一个，或用 --encoder x264（软件编码哪台都能跑）。",
        );
    }
}

/// 中继可达性：只做 TCP 连接，**不注册**（注册会占用设备身份）。
async fn check_relay(r: &mut Report, server: &str) {
    let addrs = tokio::time::timeout(Duration::from_secs(5), tokio::net::lookup_host(server)).await;
    let addr = match addrs {
        Ok(Ok(mut it)) => it.next(),
        Ok(Err(e)) => {
            r.fail("中继", format!("解析不了 {server}：{e}"), "地址要写成 `ip:端口`（域名在这台机器上也行，但内网一般直接写 IP）。");
            return;
        }
        Err(_) => {
            r.fail("中继", format!("解析 {server} 超时"), "检查 DNS 与地址写法。");
            return;
        }
    };
    let Some(addr) = addr else {
        r.fail("中继", format!("{server} 解析不出任何地址"), "地址写成 `ip:端口`。");
        return;
    };
    match tokio::time::timeout(Duration::from_secs(3), tokio::net::TcpStream::connect(addr)).await {
        Ok(Ok(_)) => r.ok("中继", format!("{addr} TCP 可达")),
        Ok(Err(e)) => r.fail(
            "中继",
            format!("连不上 {addr}：{e}"),
            "中继没起、端口不对、或被防火墙拦了：在机器上 `systemctl --user status uulan-server`，再 `ss -lnt | grep 21116`。",
        ),
        Err(_) => r.fail(
            "中继",
            format!("连 {addr} 3 秒超时"),
            "网络不通或被防火墙丢包（不是「拒绝」，是本机发出去没回音）。",
        ),
    }
}

/// 本机资源（`/proc` 只在 Linux 上，读不到就如实说读不到）。
fn check_host(r: &mut Report) {
    #[cfg(target_os = "linux")]
    {
        let mem = std::fs::read_to_string("/proc/meminfo").ok().and_then(|s| crate::probe::parse_meminfo(&s));
        let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0);
        match mem {
            Some((total_kb, avail_kb)) => r.ok(
                "本机",
                format!("{cpus} 核；内存 可用 {} / 共 {}", mb(avail_kb), mb(total_kb)),
            ),
            None => r.warn("本机", format!("{cpus} 核；读不到 /proc/meminfo"), "内存信息只是参考，不影响能不能跑。"),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0);
        r.warn("本机", format!("{cpus} 核；非 Linux，不读 /proc"), "被控端正式部署在麒麟上。");
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn mb(kb: u64) -> String {
    format!("{} MB", kb / 1024)
}

/// 按「终端显示宽度」算长度：中日韩全角字符占两列。
///
/// 报告里几项名字有长有短（`ffmpeg` 与 `身份`），直接 `{:<6}` 会按**字符数**补空格，
/// 中文两字看着比 `ffmpeg` 短一半，输出就参差不齐 —— 现场是要人眼扫的，对齐不是洁癖。
fn display_width(s: &str) -> usize {
    s.chars().map(|c| if (c as u32) >= 0x1100 { 2 } else { 1 }).sum()
}

fn pad_to(s: &str, width: usize) -> String {
    let n = display_width(s);
    let mut out = String::from(s);
    out.push_str(&" ".repeat(width.saturating_sub(n)));
    out
}

/// 从 `ffmpeg -version` 的输出里取第一行（`ffmpeg version 5.1.2 Copyright ...`）。
///
/// **认准 `ffmpeg version` 这两个词**：`--ffmpeg` 指错路径时（指向了别的程序），那个程序
/// 往往也「成功退出、打一行自己的版本」—— 只看退出码会把 `cmd.exe` 当成好 ffmpeg。
pub fn parse_ffmpeg_version(stdout: &str) -> Option<String> {
    let line = stdout.lines().next()?.trim();
    if !line.contains("ffmpeg version") {
        return None;
    }
    // 只留到版本号那一节，后面那一长串版权声明对现场没用。
    let short: Vec<&str> = line.split_whitespace().take(3).collect();
    Some(short.join(" "))
}

/// `ffmpeg -encoders` 里有没有这个编码器。
///
/// 那一列的格式是 ` V....D libx264  H.264 ...`（六个标志位 + 名字），所以按**整词**
/// 比对，不能 `contains` —— `libx264rgb` 也含 `libx264`，那样会把「只有 rgb 版」
/// 的 ffmpeg 判成能用。
pub fn has_encoder(list: &str, name: &str) -> bool {
    list.lines().any(|l| {
        let mut it = l.split_whitespace();
        match (it.next(), it.next()) {
            (Some(flags), Some(n)) => flags.len() == 6 && n == name,
            _ => false,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report_with(levels: &[(Level, &'static str)]) -> Report {
        let mut r = Report::new();
        for (lv, name) in levels {
            r.push(*lv, name, "详情");
        }
        r
    }

    #[test]
    fn render_marks_every_line_with_a_greppable_tag() {
        let r = report_with(&[(Level::Ok, "身份"), (Level::Warn, "显示"), (Level::Fail, "中继")]);
        let text = r.render();
        assert!(text.contains("[ OK ] 身份"), "{text}");
        assert!(text.contains("[WARN] 显示"), "{text}");
        assert!(text.contains("[FAIL] 中继"), "{text}");
        // 计数与结论
        assert!(text.contains("1 通过 / 1 警告 / 1 失败"), "{text}");
        assert!(text.contains("还不能跑"), "{text}");
    }

    #[test]
    fn warn_does_not_block_but_fail_does() {
        assert!(report_with(&[(Level::Ok, "身份"), (Level::Warn, "显示")]).all_ok());
        assert!(!report_with(&[(Level::Ok, "身份"), (Level::Fail, "显示")]).all_ok());
        // 空报告没有失败项 —— 语义上是「什么都没查」，调用方不该拿它当通过；
        // 这里的实现只保证「不会误报失败」，真正的保证在 run() 永远塞满检查。
        assert!(Report::new().all_ok());
    }

    #[test]
    fn hint_is_rendered_under_its_own_line() {
        let mut r = Report::new();
        r.fail("中继", "连不上", "先起中继");
        let text = r.render();
        let lines: Vec<&str> = text.lines().collect();
        let idx = lines.iter().position(|l| l.contains("[FAIL] 中继")).expect("有那一行");
        assert!(lines[idx + 1].trim_start().starts_with("└─ 先起中继"), "{text}");
    }

    #[test]
    fn has_encoder_matches_whole_word_only() {
        let list = " Encoders:\n V..... = Video\n ------\n V....D libx264              H.264 / AVC\n V....D libx264rgb           H.264 / AVC RGB\n";
        assert!(has_encoder(list, "libx264"));
        assert!(!has_encoder(list, "h264_nvenc"));
        // 只有 rgb 版时不能算有
        let rgb_only = " ------\n V....D libx264rgb           H.264 / AVC RGB\n";
        assert!(!has_encoder(rgb_only, "libx264"), "libx264rgb 不能顶替 libx264");
    }

    #[test]
    fn parse_ffmpeg_version_keeps_only_the_version() {
        let out = "ffmpeg version 5.1.2 Copyright (c) 2000-2022 the FFmpeg developers\nbuilt with gcc\n";
        assert_eq!(parse_ffmpeg_version(out).as_deref(), Some("ffmpeg version 5.1.2"));
        assert_eq!(parse_ffmpeg_version(""), None);
        // 指向别的程序时它也会「成功退出 + 打版本」，不能被当成 ffmpeg
        assert_eq!(parse_ffmpeg_version("Microsoft Windows [Version 10.0.26100]\n"), None);
        assert_eq!(parse_ffmpeg_version("my-encoder 1.0\n"), None);
    }

    #[test]
    fn mb_rounds_down_to_whole_megabytes() {
        assert_eq!(mb(0), "0 MB");
        assert_eq!(mb(1024 * 3800), "3800 MB");
        assert_eq!(mb(1023), "0 MB");
    }

    #[test]
    fn padding_counts_double_width_chars() {
        // 两个汉字 = 4 列，六个拉丁字母 = 6 列。
        assert_eq!(display_width("身份"), 4);
        assert_eq!(display_width("ffmpeg"), 6);
        assert_eq!(display_width("键鼠注入"), 8);
        // 补到 10 列：两个汉字要补 6 个空格，四个汉字补 2 个。
        assert_eq!(pad_to("身份", 10).chars().count(), 8); // 2 字符 + 6 空格
        assert_eq!(pad_to("身份", 10).chars().filter(|c| *c == ' ').count(), 6);
        assert_eq!(pad_to("键鼠注入", 10).chars().filter(|c| *c == ' ').count(), 2);
        // 已经超宽就不截断
        assert_eq!(pad_to("a-very-long-name", 4), "a-very-long-name");
    }

    #[test]
    fn rendered_check_lines_align() {
        let r = report_with(&[(Level::Ok, "身份"), (Level::Ok, "键鼠注入")]);
        let text = r.render();
        let lines: Vec<&str> = text.lines().filter(|l| l.starts_with('[')).collect();
        assert_eq!(lines.len(), 2);
        // 详情在每行里的起点，按显示宽度必须是同一列
        let starts: Vec<usize> = lines
            .iter()
            .map(|l| display_width(&l[..l.find("详情").expect("有详情")]))
            .collect();
        assert_eq!(starts[0], starts[1], "两行的详情不在同一列：{text}");
    }
}