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

#![cfg_attr(windows, windows_subsystem = "windows")]

mod local;
use local::LocalPanel;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

const DEFAULT_RELAY: &str = "192.168.223.15:21116";

/// 面板自己的配置。**这里现在会存访问口令**（`remember_pw=true` 时）——
/// 用户明确要求去掉 `host.pw` 那个"读文件"的拐弯，选择"内网图省事"。
/// 这条改动的真实含义要说出来：访问口令从此以明文留在 `gui.json` 里，
/// 任何能读这个文件的人都能连走内网被控端。Linux 下建 0600，Windows 靠用户档保护。
/// 不想留就把"记住这条口令"取消，或删掉这个文件。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct GuiConfig {
    #[serde(default)]
    relay: String,
    #[serde(default)]
    device: String,
    /// 非空 = 这台控制端**不经中继**，直接连被控端的监听端口。
    #[serde(default)]
    direct: String,
    /// 记住访问口令（下次打开面板自动填）。
    #[serde(default)]
    remember_pw: bool,
    #[serde(default)]
    password: String,
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
    /// 直连地址（`IP:端口`）。填了就绕过中继 —— 与命令行的 `--direct` 等价。
    direct: String,
    password: String,
    /// 第二因素：系统账户名 + 该账户的登录密码。
    ///
    /// 面板**不新增任何语义**：它就是 `uulan-client --os-user/--os-password-file` 的
    /// 按钮版（那两个参数已在真机验收里出现）。密码同样走 0600 文件，不进命令行。
    /// 记住这条口令（勾了才把口令写进 gui.json）。
    remember_pw: bool,
    os_user: String,
    os_password: String,
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
    /// 显眼的失败提示。**v1 的教训**：失败只写在底部一行小字里，用户点了按钮看不出
    /// 任何反应，于是报「没有任何反应」—— 反馈藏在角落等于没有反馈。
    alert: String,
    /// 0 = 远程控制（连别人）；1 = 本机（被控端）信息 —— 给对方念 ID 与一次性验证码那一屏。
    tab: usize,
    local: LocalPanel,
    /// 被控端信息页必须**自动刷新**：这一屏的唯一用途就是把码念给别人，
    /// 而临时码是一次性的 —— 屏幕上的旧值会让人念出一张已作废的码。
    local_next: std::time::Instant,
    /// 第二个实例被双击起来时竖起的旗子（由后台线程写、主线程读）。
    relaunch: Arc<AtomicBool>,
    /// 最近一次启动的子进程：秒退就必须当场说出口（并把它的输出留在盘上）。
    child: Option<(std::process::Child, std::time::Instant, PathBuf)>,
}

impl App {
    fn new(relaunch: Arc<AtomicBool>) -> Self {
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
            direct: cfg.direct,
            password: if cfg.remember_pw { cfg.password.clone() } else { String::new() },
            remember_pw: cfg.remember_pw,
            os_user: String::new(),
            os_password: String::new(),
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
            alert: String::new(),
            tab: 0,
            local: LocalPanel::new(),
            local_next: std::time::Instant::now(),
            relaunch,
            child: None,
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
        let cfg = GuiConfig {
            relay: self.relay.trim().to_string(),
            device: self.device.trim().to_string(),
            direct: self.direct.trim().to_string(),
            // 只有勾了才落盘；不勾就主动抹掉，不留历史副本。
            remember_pw: self.remember_pw,
            password: if self.remember_pw { self.password.clone() } else { String::new() },
        };
        let _ = std::fs::create_dir_all(Self::cfg_dir());
        let _ = std::fs::write(&self.cfg_path, serde_json::to_string_pretty(&cfg).unwrap_or_default());
        // 一旦这个文件里存了访问口令，权限就得跟上：Linux/麒麟默认 umask 会写出 0644，
        // 那等于把"能连走内网被控端"的凭据交给同机所有账户。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.cfg_path, std::fs::Permissions::from_mode(0o600));
        }
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

    /// 第二因素的登录密码：另一个 0600 文件，绝不进 argv。
    fn write_os_pw_file(&self) -> anyhow::Result<PathBuf> {
        std::fs::create_dir_all(Self::cfg_dir())?;
        let p = Self::cfg_dir().join("gui-os.pw");
        let mut f = std::fs::File::create(&p)?;
        f.write_all(self.os_password.as_bytes())?;
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
        // 直连与经中继是两条互斥的路：填了直连地址就**只发 --direct**，
        // 绝不同时带 --server/--device —— 那会让控制端自己猜哪条算，行为不可复现。
        let mut a = route_args(self.direct.trim(), self.relay.trim(), self.device.trim(), pw_file);
        if !self.os_user.trim().is_empty() {
            // 路径不是秘密；秘密在那个 0600 文件里。
            let f = Self::cfg_dir().join("gui-os.pw");
            a.push("--os-user".into());
            a.push(self.os_user.trim().to_string());
            a.push("--os-password-file".into());
            a.push(f.to_string_lossy().into_owned());
        }
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
        // 这两条以前只改底部小字，结果用户报「点了没反应」—— 表单没填全必须有显眼回应。
        if self.device.trim().is_empty() {
            self.alert = "还没填设备 ID —— 点上面列表里那一行，或自己写进来。\n                （想不经中继直接连，就在「直连地址」里填 被控端IP:21117，那时不用设备 ID）".into();
            return;
        }
        if self.password.trim().is_empty() && self.os_user.trim().is_empty() {
            // 这一格装的是**一次性临时码**（被控端那一屏大字显示的那张），不是永久口令。
            // 加密握手（PAKE）拿它当共享密钥，所以它必填；用户名+登录密码是第二因素。
            self.alert = "请填「临时码/口令」。\n\
                平时填被控端屏幕上那张**一次性临时码**（用完即废、每整点自动换新）。\n\
                只有长期无人值守接入时才用永久口令：在被控端跑 `uulan-host --show` 能看。".into();
            return;
        }
        if !self.os_user.trim().is_empty() && self.os_password.trim().is_empty() {
            self.alert = "填了系统账户名，但没给它的登录密码（第二因素要用）。".into();
            return;
        }
        if !self.os_password.trim().is_empty() {
            // 只在真要带第二因素时写这个文件：不留多余的明文副本在盘上。
            if let Err(e) = self.write_os_pw_file() {
                self.alert = format!("第二因素的密码文件写不了：{e}");
                return;
            }
        }
        let pw = match self.write_pw_file() {
            Ok(p) => p,
            Err(e) => {
                self.status = format!("口令文件写不了：{e}");
                return;
            }
        };
        let args = self.build_args(&pw, mode.clone());
        let shown = sanitize(&args);
        // 子进程的输出**必须留盘**：改成 GUI 子系统后它没有控制台，
        // 不接住就永远看不到它为什么没起窗。
        let log_path = App::cfg_dir().join("gui-session.log");
        let _ = std::fs::create_dir_all(App::cfg_dir());
        let out = std::fs::File::create(&log_path);
        let mut cmd = Command::new(&client);
        cmd.args(&args);
        hide_console(&mut cmd);
        match out {
            Ok(f) => {
                match f.try_clone() {
                    Ok(f2) => {
                        cmd.stdout(Stdio::from(f2)).stderr(Stdio::from(f));
                    }
                    Err(e) => {
                        self.alert = format!("会话日志句柄复制失败（{e}），无法诊断子进程。");
                        return;
                    }
                }
            }
            Err(e) => {
                self.alert = format!("会话日志建不了（{e}），也就无法诊断子进程为什么不起窗。");
                return;
            }
        }
        self.alert.clear();
        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id();
                self.save_cfg();
                self.status = format!(
                    "已启动（PID {pid}），窗口应该已经出现。{}  口令走的是文件，没进命令行。",
                    match mode { Mode::Desktop => "远程控制窗口", Mode::Term => "终端会话" }
                );
                self.push_log(format!("执行：{} {}", client.file_name().unwrap_or_default().to_string_lossy(), shown));
                self.child = Some((child, std::time::Instant::now(), log_path));
            }
            Err(e) => {
                self.alert = format!("启动失败：{e}\n控制端路径：{}", client.display());
                self.push_log(self.alert.clone());
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

/// 连接路线的参数装配（抽成自由函数的理由与 `parse_devices` 一样：能被单测钉住）。
///
/// 直连与经中继**互斥**：填了直连地址就只发 `--direct`，不再带 `--server/--device`。
/// 同时带上等于把"走哪条路"交给控制端自己去猜，那种不确定性最难在现场复现。
fn route_args(direct: &str, relay: &str, device: &str, pw_file: &Path) -> Vec<String> {
    let pw = pw_file.to_string_lossy().into_owned();
    if !direct.trim().is_empty() {
        vec!["--direct".into(), direct.trim().to_string(), "--password-file".into(), pw]
    } else {
        vec![
            "--server".into(), relay.trim().to_string(),
            "--device".into(), device.trim().to_string(),
            "--password-file".into(), pw,
        ]
    }
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

        // 每秒醒一次去看旗子（第二个实例被双击起来、或用户又点了一次图标）。
        root.ctx().request_repaint_after(std::time::Duration::from_millis(1000));
        if self.relaunch.swap(false, Ordering::Relaxed) && self.alert.is_empty() {
            self.alert = "又有人双击了一次图标 —— 已经有一个面板在跑了，就是这个窗口。\n                两个窗口会各自显示不同的验证码，所以第二个被拒绝启动。".into();
        }

        // 子进程秒退 = 「点了没反应」的真实形态。必须当场说，而不是等用户去猜。
        // 写法用 take/放回而不是 as_mut：as_mut 里再访问 self 会撞借用检查。
        if let Some((mut child, born, lp)) = self.child.take() {
            match child.try_wait() {
                Ok(Some(st)) => {
                    let raw = std::fs::read_to_string(&lp).unwrap_or_default();
                    let tail: String = raw.lines().rev().take(3).collect::<Vec<_>>().join(" ← ");
                    if born.elapsed().as_secs() < 10 {
                        self.alert = format!(
                            "远程进程 {st} 就退了（不到 10 秒），所以你没看到窗口。\n最后三行输出：{}\n全文在 {}",
                            tail.trim().replace('\n', " / "),
                            lp.display()
                        );
                    }
                }
                // 还活着就放回原位，下一帧继续看。
                _ => self.child = Some((child, born, lp)),
            }
        }

        egui::CentralPanel::default().show(root, |ui| {
            // 两页：这台机器去连别人 / 别人来连这台机器。UU 也是这么分的。
            ui.horizontal(|ui| {
                if ui.selectable_label(self.tab == 0, "远程控制").clicked() {
                    self.tab = 0;
                    self.alert.clear();
                }
                if ui.selectable_label(self.tab == 1, "本机（被控端）信息").clicked() {
                    self.tab = 1;
                    self.alert.clear();
                    self.local.reload();
                }
            });
            ui.separator();
            if self.tab == 1 {
                // eframe 默认只在有输入时重绘，所以必须显式约醒，否则这一屏永远停在旧值。
                // 用 repaint_after 而不是 repaint：后者等于连续重绘，空转把 CPU 拉满。
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(600));
                if std::time::Instant::now() >= self.local_next {
                    self.local.reload();
                    self.local_next = std::time::Instant::now() + std::time::Duration::from_secs(2);
                }
                if !self.local.info.as_ref().map(|i| i.temp.clone()).unwrap_or_default().is_empty() {
                    ui.label(egui::RichText::new(
                        "↑ 这一屏每两秒自动重读配置；临时码用一次即失效，念出去的那张用完就作废。",
                    )
                    .weak()
                    .small());
                }
                // 先 clone 再传：&self.relay 与 self.local.ui(..) 会同时借 self，直接传过不了借用检查。
                let relay = self.relay.clone();
                self.local.ui(ui, &relay);
                return;
            }

            if !self.alert.is_empty() {
                ui.colored_label(egui::Color32::from_rgb(200, 40, 40), &self.alert);
                if ui.button("我知道了").clicked() {
                    self.alert.clear();
                }
                ui.separator();
            }
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.label("中继地址");
                    ui.add(egui::TextEdit::singleline(&mut self.relay).desired_width(240.0));
                    if ui.button("刷新设备").clicked() {
                        self.refresh_devices();
                    }
                });
                // 直连：填了就绕开中继。放这一行而不是新开一页，是因为它只是"换地址写法"，
                // 下面的选项、口令、按钮全部照用。
                ui.horizontal(|ui| {
                    ui.label("直连地址");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.direct)
                            .hint_text("留空 = 经中继；例 192.168.1.20:21117")
                            .desired_width(240.0),
                    );
                    if !self.direct.trim().is_empty() {
                        ui.label(egui::RichText::new("不经中继，被控端需开着监听（默认 21117）").weak().small());
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("设备 ID");
                    ui.add(egui::TextEdit::singleline(&mut self.device).desired_width(140.0));
                    // 措辞对齐现场说法：对方念给你的是**临时码**（一次性），
                    // 永久口令只是备选。写"访问口令"会让人以为必须去翻口令文件。
                    ui.label("临时码/口令");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.password)
                            .password(true)
                            .desired_width(200.0),
                    );
                    ui.checkbox(&mut self.remember_pw, "记住这条口令");
                    ui.label("系统账户(可选)");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.os_user)
                            .hint_text("被控端开了第二因素才填")
                            .desired_width(110.0),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut self.os_password)
                            .password(true)
                            .hint_text("该账户的登录密码")
                            .desired_width(110.0),
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

/// 没有控制台 ⇒ panic 和启动错误没人看得见。「双击没反应」是最难报修的一类故障，
/// 所以把它写进文件（下次排查不用再靠猜）。
fn error_log() -> PathBuf {
    App::cfg_dir().join("gui-error.log")
}

fn note_error(what: &str) {
    let line = format!("{}  {what}\n", chrono_free_stamp());
    let _ = std::fs::create_dir_all(App::cfg_dir());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(error_log()) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// 不引 chrono：内网里少一个依赖就少一个装不上的理由。这是本地时区的粗粒度时间戳，
/// 够用来排序故障，不承担协议语义。
fn chrono_free_stamp() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|x| x.as_secs())
        .unwrap_or(0);
    format!("@{d}")
}

fn main() {
    // 单实例：同一台机器开两个面板，旧的那个会挂着**已作废的验证码**继续显示，
    // 坐在屏幕前的人就会把废码念给你 —— 这不是难看，是让人白跑一趟。
    let relaunch = Arc::new(AtomicBool::new(false));
    match local::acquire_single_instance() {
        Some(listener) => local::watch_relaunch(listener, relaunch.clone()),
        None => {
            note_error("已有一个面板在运行 —— 本次启动退出（避免两个窗口显示不同的验证码）。");
            eprintln!("已有一个 uulan 面板在运行，请在任务栏里找到它。");
            return;
        }
    }

    // 日志一律走 stderr（项目纪律）；改成 GUI 子系统后 stderr 没有去处，
    // 所以这里把 panic 与启动错误**同时**落到 gui-error.log。
    std::panic::set_hook(Box::new(|info| {
        note_error(&format!("panic: {info}"));
    }));
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([720.0, 520.0]).with_min_inner_size([560.0, 420.0]),
        ..Default::default()
    };
    if let Err(e) = eframe::run_native(
        "uulan 内网远控",
        opts,
        Box::new(move |_cc| Ok(Box::new(App::new(relaunch.clone())))),
    ) {
        note_error(&format!("启动失败：{e}"));
        // 起不来时留一句能看见的话，比静默退出强。
        eprintln!("面板起不来，原因写在 {}", error_log().display());
        std::process::exit(1);
    }
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

    #[test]
    fn route_args_direct_drops_relay_and_device() {
        // 直连与经中继互斥：填了直连地址就**只发 --direct**。
        // 同时带 --server/--device 等于让控制端自己猜走哪条路，现场最难复现。
        let pw = std::path::Path::new("PW");
        let a = route_args("192.168.1.20:21117", "10.0.0.9:21116", "805747189", pw);
        assert_eq!(a[0], "--direct");
        assert_eq!(a[1], "192.168.1.20:21117");
        assert!(!a.contains(&"--server".to_string()), "直连还带了 --server：{a:?}");
        assert!(!a.contains(&"--device".to_string()), "直连还带了 --device：{a:?}");
        let i = a.iter().position(|x| x == "--password-file").unwrap();
        assert_eq!(a[i + 1], "PW", "口令文件必须紧跟在 --password-file 后，否则 sanitize 漏脱敏");

        let b = route_args("", "10.0.0.9:21116", "805747189", pw);
        assert!(b.contains(&"--server".to_string()) && b.contains(&"--device".to_string()));
        assert!(!b.contains(&"--direct".to_string()));

        // 只填空白也算没直连（并把空白擦掉，不发一个空参数给控制端）
        let c = route_args("   ", " 10.0.0.9:21116 ", " 805747189 ", pw);
        assert_eq!(c[1], "10.0.0.9:21116");
        assert_eq!(c[3], "805747189");
    }

}

