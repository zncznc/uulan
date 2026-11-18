//! uulan-gui —— 内网远控的图形面板。
//!
//! # 为什么 v1 是「面板 + 复用已验证的控制端二进制」，而不是把画面/输入重实现一遍
//!
//! 控制端的画面渲染、键鼠映射、剪贴板、加密握手、断线接回这些路径，都在真机上一条条
//! 验收过（见 docs/DESIGN.md 的状态表）。如果在 GUI 里另起一套事件循环去接管它们，
//! 那些验收**当场作废** —— 换了实现就得重新验一遍，而重新验的成本比写面板高得多。
//! 所以 v1 只做一件事：**把命令行参数变成按钮**，真正干活的还是同一个 uulan-client。
//! 形状确认之后，v2 再把两者合并成单进程（画面作为 egui 纹理），那时才动渲染路径。
//!
//! # 三条不许破的规矩
//!
//! 1. **口令不上命令行**。进程表是内网里最容易泄露口令的地方（`ps` / 任务管理器
//!    都看得见），所以口令写进用户目录下 0600 的文件，用 `--password-file` 传。
//! 2. **设备列表不重实现协议**。直接跑 `uulan-client --devices` 并读它的文本输出 ——
//!    那条路径本身已经真机验过，GUI 再写一遍协议就是给自己造第二个真相。
//! 3. **配置里永不落口令**。只存中继地址与设备 ID（`gui.json`）。
//!
//! # 能跑起来的最小闭环
//!
//! 填中继 → 刷新设备 → 选中一台 → 输口令 → 点「开始远程控制」。中间任何一步失败，
//! 面板都会把对面程序的**原话**显示出来（而不是替它编一句好话）。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

const DEFAULT_RELAY: &str = "192.168.223.15:21116";

/// 持久化的只有这两样 —— 口令永远不进这个文件。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct GuiConfig {
    #[serde(default)]
    relay: String,
    #[serde(default)]
    device: String,
}

#[derive(Debug, Clone)]
struct DeviceRow {
    id: String,
    name: String,
    online: bool,
    last: bool,
}

pub struct App {
    cfg_path: PathBuf,
    relay: String,
    device: String,
    password: String,
    // 选项（每一项都对应控制端一个已验过的开关）
    audio: bool,
    mic: bool,
    capture: bool,
    clipboard: bool,
    quality: usize, // 0=默认 1=流畅 2=清晰
    fps: String,
    scale: String,
    devices: Vec<DeviceRow>,
    status: String,
    log: Vec<String>,
}

impl App {
    fn new() -> Self {
        let cfg_path = config_path();
        let cfg = load_cfg(&cfg_path);
        let relay = if cfg.relay.is_empty() {
            DEFAULT_RELAY.to_string()
        } else {
            cfg.relay
        };
        Self {
            cfg_path,
            relay,
            device: cfg.device,
            password: String::new(),
            audio: false,
            mic: false,
            capture: false,
            clipboard: true,
            quality: 0,
            fps: String::new(),
            scale: String::new(),
            devices: Vec::new(),
            status: "就绪。填中继地址，点「刷新设备」。".to_string(),
            log: Vec::new(),
        }
    }

    /// 配置目录：Windows 用 %LOCALAPPDATA%\uulan，其它平台用 ~/.config/uulan。
    fn cfg_dir() -> PathBuf {
        #[cfg(windows)]
        let base = std::env::var("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::home_dir().unwrap_or_default().join("AppData"));
        #[cfg(not(windows))]
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::home_dir().unwrap_or_default().join(".config"));
        base.join("uulan")
    }

    /// 控制端二进制：优先取 GUI 旁边的同名文件（安装包形态），其次取构建目录。
    fn client_exe(&self) -> Option<PathBuf> {
        let exe_name = if cfg!(windows) { "uulan-client.exe" } else { "uulan-client" };
        let mut cand: Vec<PathBuf> = Vec::new();
        if let Ok(cur) = std::env::current_exe() {
            if let Some(dir) = cur.parent() {
                cand.push(dir.join(exe_name));
                cand.push(dir.join("..").join(exe_name));
            }
        }
        // 开发期：仓库里的 target/release
        cand.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/release").join(exe_name));
        cand.into_iter().map(|p| p.canonicalize().unwrap_or(p)).find(|p| p.is_file())
    }

    fn save_cfg(&mut self) {
        let cfg = GuiConfig { relay: self.relay.trim().to_string(), device: self.device.trim().to_string() };
        let _ = std::fs::create_dir_all(Self::cfg_dir());
        let _ = std::fs::write(&self.cfg_path, serde_json::to_string_pretty(&cfg).unwrap_or_default());
    }

    fn push_log(&mut self, s: impl Into<String>) {
        let line = s.into();
        if line.trim().is_empty() {
            return;
        }
        self.log.push(line);
        if self.log.len() > 60 {
            let n = self.log.len() - 60;
            self.log.drain(..n);
        }
    }

    /// 口令落到 0600 的文件里；返回给命令行用的只有路径。
    fn write_pw_file(&self) -> anyhow::Result<PathBuf> {
        std::fs::create_dir_all(Self::cfg_dir())?;
        let p = Self::cfg_dir().join("gui-current.pw");
        let mut f = std::fs::File::create(&p)?;
        f.write_all(self.password.as_bytes())?;
        f.write_all(b"\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
        }
        Ok(p)
    }

    /// 组命令行。**这里出现过的每个参数都必须已经在真机验收里出现过**，
    /// GUI 不发明新语义。
    fn build_args(&self, pw_file: &Path, mode: Mode) -> Vec<String> {
        let mut a: Vec<String> = vec![
            "--server".into(), self.relay.trim().to_string(),
            "--device".into(), self.device.trim().to_string(),
            "--password-file".into(), pw_file.to_string_lossy().into_owned(),
        ];
        match mode {
            Mode::Desktop => {}
            Mode::Term => a.push("--term".into()),
        }
        if !self.clipboard {
            a.push("--no-clipboard".into());
        }
        if self.audio {
            a.push("--audio".into());
        }
        if self.mic {
            a.push("--mic".into());
        }
        if self.capture {
            a.push("--capture".into());
        }
        match self.quality {
            1 => a.extend(["--quality".into(), "fast".into()]),
            2 => a.extend(["--quality".into(), "best".into()]),
            _ => {}
        }
        let fps = self.fps.trim();
        if !fps.is_empty() && fps.parse::<u32>().is_ok() {
            a.extend(["--fps".into(), fps.to_string()]);
        }
        let sc = self.scale.trim();
        if !sc.is_empty() && sc.parse::<f64>().is_ok() {
            a.extend(["--scale".into(), sc.to_string()]);
        }
        a
    }

    fn refresh_devices(&mut self) {
        let Some(client) = self.client_exe() else {
            self.status = "找不到 uulan-client —— 请把它和面板放在同一目录。".into();
            return;
        };
        let mut cmd = Command::new(&client);
        cmd.args(["--server", self.relay.trim(), "--devices"]);
        hide_console(&mut cmd);
        match cmd.output() {
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout);
                let err = String::from_utf8_lossy(&out.stderr);
                self.devices.clear();
                let (rows, unparsed) = parse_devices(&text);
                self.devices = rows;
                if unparsed > 0 {
                    // 认不出来的行必须说出来，而不是悄悄当成「离线」——
                    // 「列表里全是灰点」和「解析器读不懂这个格式」是两回事。
                    self.push_log(format!("有 {unparsed} 行没能解析成设备（格式变了？）"));
                }
                let head = text.lines().find(|l| l.contains("台设备")).unwrap_or("").trim().to_string();
                self.status = if self.devices.is_empty() {
                    format!("没解析到设备。{} {}", head, err.trim()).trim().to_string()
                } else {
                    format!("{}（共 {} 行）", head, self.devices.len())
                };
                self.push_log(format!("刷新设备：{}", self.status));
            }
            Err(e) => {
                self.status = format!("控制端跑不起来：{e}");
                self.push_log(self.status.clone());
            }
        }
    }

    fn launch(&mut self, mode: Mode) {
        let Some(client) = self.client_exe() else {
            self.status = "找不到 uulan-client —— 请把它和面板放在同一目录。".into();
            return;
        };
        if self.device.trim().is_empty() {
            self.status = "先选一台设备（或手填设备 ID）。".into();
            return;
        }
        if self.password.trim().is_empty() {
            self.status = "需要访问口令。被控端首次启动时终端上会打印一次。".into();
            return;
        }
        let pw = match self.write_pw_file() {
            Ok(p) => p,
            Err(e) => {
                self.status = format!("口令文件写不了：{e}");
                return;
            }
        };
        let args = self.build_args(&pw, mode.clone());
        let mut cmd = Command::new(&client);
        cmd.args(&args);
        hide_console(&mut cmd);
        let shown = sanitize(&args);
        match cmd.spawn() {
            Ok(child) => {
                self.save_cfg();
                self.status = format!(
                    "已启动（PID {}）。{}  口令走的是文件，没进命令行。",
                    child.id(),
                    match mode { Mode::Desktop => "远程控制窗口", Mode::Term => "终端会话" }
                );
                self.push_log(format!("执行：{} {}", client.file_name().unwrap_or_default().to_string_lossy(), shown));
            }
            Err(e) => {
                self.status = format!("启动失败：{e}");
                self.push_log(self.status.clone());
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode { Desktop, Term }

/// 解析 `uulan-client --devices` 的文本输出。
///
/// 已知形态：`  805747189  麒麟真机  在线  空闲 8s  ← 上次`
/// 故意**不**用「第二个 token 就是名字」这种写法：设备名可以带空格
/// （`运维 值班机`），按位置切会把名字后半段当成状态，于是**在线被读成离线** ——
/// 那是最坏的错法：它看起来完全正常。改成找状态词本身，名字取它之前的全部。
/// 返回（解析出的行，认不出来的行数）；认不出来的数量必须让调用方说出来。
fn parse_devices(text: &str) -> (Vec<DeviceRow>, usize) {
    let mut out = Vec::new();
    let mut unparsed = 0;
    for line in text.lines() {
        let t = line.trim();
        let mut it = t.split_whitespace();
        let Some(id) = it.next() else { continue };
        // 表头、提示行都以非数字开头，直接跳过，不计入「认不出来」。
        if id.len() < 6 || !id.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let rest: Vec<&str> = it.collect();
        let Some(pos) = rest.iter().position(|w| *w == "在线" || *w == "离线") else {
            unparsed += 1;
            continue;
        };
        let name = rest[..pos].join(" ");
        if name.is_empty() {
            unparsed += 1;
            continue;
        }
        out.push(DeviceRow {
            id: id.to_string(),
            name,
            online: rest[pos] == "在线",
            last: t.contains("上次"),
        });
    }
    (out, unparsed)
}

/// 把参数里的口令文件路径脱敏掉 —— 日志里只留结构，不留可还原的信息。
fn sanitize(args: &[String]) -> String {
    let mut out = String::new();
    let mut skip_next = false;
    for a in args {
        if skip_next {
            out.push_str("‹口令文件› ");
            skip_next = false;
            continue;
        }
        if a == "--password-file" {
            out.push_str("--password-file ");
            skip_next = true;
            continue;
        }
        out.push_str(a);
        out.push(' ');
    }
    out
}

fn config_path() -> PathBuf {
    App::cfg_dir().join("gui.json")
}

fn load_cfg(p: &Path) -> GuiConfig {
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

#[cfg(windows)]
fn hide_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW：控制端的**窗口**照旧显示，只是不要那个黑色控制台壳。
    cmd.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn hide_console(_cmd: &mut Command) {}

impl eframe::App for App {
    // eframe 0.37 起 App 交出的是**一棵 Ui 根**而不是 Context（旧写法 `update(ctx, frame)`
    // 会报「method `update` is not a member of trait」）。面板仍然挂在 ctx 上，所以从
    // ui.ctx() 拿回来即可 —— 语义没变，只是入口形状变了。
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("head").show(root, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("uulan 内网远控");
                ui.separator();
                ui.label("物理隔离内网 · 端到端加密 · 无云服务");
            });
            ui.add_space(4.0);
        });

        egui::Panel::bottom("foot").show(root, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(&self.status);
            });
        });

        egui::CentralPanel::default().show(root, |ui| {
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.label("中继地址");
                    ui.add(egui::TextEdit::singleline(&mut self.relay).desired_width(240.0));
                    if ui.button("刷新设备").clicked() {
                        self.refresh_devices();
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("设备 ID");
                    ui.add(egui::TextEdit::singleline(&mut self.device).desired_width(140.0));
                    ui.label("访问口令");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.password)
                            .password(true)
                            .desired_width(200.0),
                    );
                });
            });

            ui.add_space(6.0);
            ui.group(|ui| {
                ui.label("找到的设备（点一行就填进上面）");
                if self.devices.is_empty() {
                    ui.label("（还没有 —— 点「刷新设备」）");
                }
                let mut picked: Option<String> = None;
                for d in &self.devices {
                    ui.horizontal(|ui| {
                        let dot = if d.online { "●" } else { "○" };
                        let color = if d.online { egui::Color32::from_rgb(60, 200, 120) } else { egui::Color32::GRAY };
                        ui.colored_label(color, dot);
                        if ui.selectable_label(false, format!("{}  {}{}", d.id, d.name, if d.last { "  ← 上次" } else { "" })).clicked() {
                            picked = Some(d.id.clone());
                        }
                    });
                }
                if let Some(id) = picked {
                    self.device = id;
                    self.save_cfg();
                }
            });

            ui.add_space(6.0);
            ui.group(|ui| {
                ui.label("会话选项");
                ui.horizontal_wrapped(|ui| {
                    ui.checkbox(&mut self.clipboard, "同步剪贴板");
                    ui.checkbox(&mut self.audio, "听对面的声音");
                    ui.checkbox(&mut self.mic, "把本机麦克风送过去");
                    ui.checkbox(&mut self.capture, "捕获指针（F8 开关）");
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label("画质");
                    ui.radio_value(&mut self.quality, 0, "默认");
                    ui.radio_value(&mut self.quality, 1, "流畅优先");
                    ui.radio_value(&mut self.quality, 2, "清晰优先");
                    ui.separator();
                    ui.label("帧率");
                    ui.add(egui::TextEdit::singleline(&mut self.fps).desired_width(50.0));
                    ui.label("缩放");
                    ui.add(egui::TextEdit::singleline(&mut self.scale).desired_width(60.0));
                });
            });

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let b1 = ui.add_sized([190.0, 40.0], egui::Button::new(egui::RichText::new("开始远程控制").strong().size(16.0)));
                if b1.clicked() {
                    self.launch(Mode::Desktop);
                }
                let b2 = ui.add_sized([150.0, 40.0], egui::Button::new("开一个远端终端"));
                if b2.clicked() {
                    self.launch(Mode::Term);
                }
            });

            ui.add_space(8.0);
            if !self.log.is_empty() {
                ui.collapsing("最近的动作", |ui| {
                    for l in self.log.iter().rev().take(12) {
                        ui.label(egui::RichText::new(l.clone()).small());
                    }
                });
            }
        });
    }
}

fn main() -> eframe::Result<()> {
    // 日志一律走 stderr（项目纪律）；GUI 里没有终端，就让它安静地不打印。
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([720.0, 520.0]).with_min_inner_size([560.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        "uulan 内网远控",
        opts,
        Box::new(|_cc| Ok(Box::new(App::new()))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_known_device_line() {
        let text = "中继 192.168.223.15:21116：1 台设备（在线 1）\n  805747189  麒麟真机  在线  空闲 8s  ← 上次\n";
        let (rows, bad) = parse_devices(text);
        assert_eq!(bad, 0, "表头不该被算成解析失败");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "805747189");
        assert_eq!(rows[0].name, "麒麟真机");
        assert!(rows[0].online);
        assert!(rows[0].last, "「← 上次」必须认出来");
    }

    #[test]
    fn device_names_with_spaces_do_not_become_offline() {
        // 这一条钉的是最坏的错法：按位置切分时，「值班」会被当成语义词后的状态，
        // 于是**在线设备显示成离线**，而且界面上一切正常。
        let text = "  123456789  运维 值班机  在线  空闲 3s\n";
        let (rows, bad) = parse_devices(text);
        assert_eq!(bad, 0);
        assert_eq!(rows[0].name, "运维 值班机");
        assert!(rows[0].online, "名字带空格绝不能被判成离线");
    }

    #[test]
    fn offline_and_unparsable_are_told_apart() {
        let (rows, bad) = parse_devices("  700000001  机器甲  离线  42s 前\n");
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].online);
        assert_eq!(bad, 0);
        // 状态词整个缺失：必须报「解析不出来」，不许假装离线。
        let (rows2, bad2) = parse_devices("  700000002  机器乙  怪格式\n");
        assert!(rows2.is_empty());
        assert_eq!(bad2, 1);
    }

    #[test]
    fn sanitizer_never_leaks_the_password_file_path() {
        let a = vec![
            "--server".to_string(), "192.168.223.15:21116".into(),
            "--device".to_string(), "805747189".into(),
            "--password-file".to_string(), "C:/Users/x/AppData/uulan/gui-current.pw".into(),
            "--audio".to_string(),
        ];
        let s = sanitize(&a);
        assert!(!s.contains("gui-current.pw"), "脱敏后还剩路径：{s}");
        assert!(s.contains("805747189"), "设备 ID 是要给人看的，别一起抹掉：{s}");
        assert!(s.contains("--audio"));
    }
}

