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
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

const DEFAULT_RELAY: &str = "";

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
    /// 这条子进程是「独立终端窗口」还是一条桌面会话。**收尾的话不一样**：终端模式下
    /// 面板手里的退出码是**终端模拟器**的，不是远端 shell 的，不能混着说。
    child_term: bool,
    /// 正在跑的「刷新设备」后台任务。**它必须异步**：那是连中继的外部进程，
    /// 连不上时最长等 10 秒 —— UI 线程卡 10 秒，桌面就会判窗口「无响应」，
    /// 而按这个按钮的时刻恰恰是「地址可能还没填对」的时候。
    refresh: Option<RefreshJob>,
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
            child_term: false,
            refresh: None,
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
        // 面板是 GUI 子系统，没有控制台：写进屏幕的日志会随着关窗一起消失。
        // 会话结束/子进程秒退这类结论**必须能够事后 grep**，否则现场报修时只能凭记忆。
        self.note_file(&line);
        self.log.push(line);
        if self.log.len() > 60 {
            let n = self.log.len() - 60;
            self.log.drain(..n);
        }
    }

    /// 把一条动作记进 `gui-actions.log`（>64 KiB 时轮转一次，防止无限长）。
    fn note_file(&self, line: &str) {
        use std::io::Write as _;
        let dir = Self::cfg_dir();
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("gui-actions.log");
        if let Ok(m) = std::fs::metadata(&p) {
            if m.len() > 64 * 1024 {
                let _ = std::fs::rename(&p, dir.join("gui-actions.log.1"));
            }
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
            let _ = writeln!(f, "{}  {line}", chrono_free_stamp());
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
    fn build_args(&self, pw_file: Option<&Path>, mode: Mode) -> Vec<String> {
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

    /// 开始一次「刷新设备」。**立刻返回**：真正连中继的那条外部进程在后台线程里跑。
    ///
    /// 为什么非改不可（2026-11-21 夜批）：`uulan-client --devices` 要连中继，
    /// `RvConn::connect` 的超时是 **10 秒**。以前它在 UI 线程里 `Command::output()` 等，
    /// 于是「中继地址填错 → 点刷新 → 整个窗口冻 10 秒」，桌面甚至弹「应用程序无响应」；
    /// 而按这个按钮的时刻，恰恰是地址还没填对的时候 —— 反馈越该来的时候越不来。
    fn start_refresh(&mut self) {
        if self.refresh.is_some() {
            // 已经在跑：重复点不排第二次（每次都要等满超时，排队只会更慢）。
            return;
        }
        let Some(client) = self.client_exe() else {
            self.alert = "找不到 uulan-client —— 请把它和面板放在同一目录。".into();
            return;
        };
        let relay = self.relay.trim().to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut cmd = Command::new(&client);
            cmd.args(["--server", &relay, "--devices"]);
            // 设备清单是纯后台查询：不要控制台壳（与桌面会话同一套标志）。
            apply_creation_flags(&mut cmd, &Mode::Desktop);
            let done = match cmd.output() {
                Ok(out) => {
                    let text = String::from_utf8_lossy(&out.stdout).to_string();
                    let err = String::from_utf8_lossy(&out.stderr).to_string();
                    let (rows, unparsed) = parse_devices(&text);
                    let head = text
                        .lines()
                        .find(|l| l.contains("台设备"))
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    RefreshDone::Ran { rows, unparsed, head, err, ok: out.status.success() }
                }
                Err(e) => RefreshDone::SpawnFailed(format!("控制端跑不起来：{e}")),
            };
            // 接收端没了（面板关掉）就算了：发不出去不是错误。
            let _ = tx.send(done);
        });
        self.alert.clear();
        self.status = "正在刷新设备…（连不上中继时最多等 10 秒）".to_string();
        self.refresh = Some(RefreshJob { started: std::time::Instant::now(), rx });
    }

    /// 主线程每帧调一次：结果到了就落地，没到就把「已 N 秒」写进状态栏。
    ///
    /// 「已 N 秒」不是装饰：这条按钮过去的故障形态就是「点了没反应」，
    /// 一个在走的秒数能让人分清「在等」和「死掉了」。
    fn poll_refresh(&mut self) {
        let (got, elapsed) = {
            let Some(job) = self.refresh.as_ref() else { return };
            (job.rx.try_recv(), job.started.elapsed().as_secs_f32())
        };
        match got {
            Ok(RefreshDone::Ran { rows, unparsed, head, err, ok }) => {
                self.refresh = None;
                let o = refresh_outcome(&rows, unparsed, &head, &err, ok);
                self.devices = rows;
                self.status = o.status;
                if let Some(l) = o.log {
                    self.push_log(l);
                }
                if let Some(a) = o.alert {
                    self.alert = a;
                }
            }
            Ok(RefreshDone::SpawnFailed(msg)) => {
                self.refresh = None;
                self.alert = msg.clone();
                self.push_log(msg);
                self.status = "刷新失败：控制端没能启动。".to_string();
            }
            Err(TryRecvError::Empty) => {
                self.status = format!("正在刷新设备… 已 {elapsed:.1} 秒（连不上中继时最多等 10 秒）");
            }
            Err(TryRecvError::Disconnected) => {
                self.refresh = None;
                self.alert = "刷新线程没有回报结果就退出了（面板内部错误，请重开面板）。".into();
            }
        }
    }

    /// 校验表单 → 写凭据文件 → 拼命令行。
    ///
    /// **dry-run（`--print-args`）与真正启动走同一段**：否则「脚本验过的形状」和
    /// 「按钮跑的形状」是两套，验了也不算数。
    fn prepare(&mut self, mode: Mode) -> Result<Vec<String>, String> {
        // 设备 ID 只在经中继时需要：直连走的是地址，没有设备目录语义。
        // 旧写法在这里无条件要 ID，而错误提示自己写着「直连那时不用设备 ID」——
        // 检查比提示更早生效，于是**照着提示做的人反而过不去**。
        if self.direct.trim().is_empty() && self.device.trim().is_empty() {
            return Err("还没填设备 ID —— 点上面列表里那一行，或自己写进来。\n                （想不经中继直接连，就在「直连地址」里填 被控端IP:21117，那时不用设备 ID）".into());
        }
        if self.password.trim().is_empty() && self.os_user.trim().is_empty() {
            // 这一格装的是**一次性临时码**（被控端那一屏大字显示的那张），不是永久口令。
            // 但被控端以「系统账户」认证时它根本不存在 —— 那种形态只填用户名 + 登录密码。
            return Err("请填「临时码/口令」，或填系统账户（用户名 + 登录密码）。\n\
                平时填被控端屏幕上那张**一次性临时码**（用完即废、每整点自动换新）。\n\
                只有长期无人值守接入时才用永久口令：在被控端跑 `uulan-host --show` 能看。".into());
        }
        if !self.os_user.trim().is_empty() && self.os_password.trim().is_empty() {
            return Err("填了系统账户名，但没给它的登录密码（第二因素要用）。".into());
        }
        if !self.os_password.trim().is_empty() {
            // 只在真要带第二因素时写这个文件：不留多余的明文副本在盘上。
            self.write_os_pw_file().map_err(|e| format!("第二因素的密码文件写不了：{e}"))?;
        }
        // **没有访问口令就不写口令文件**（见 route_args 的注释：空文件会让控制端在
        // 参数阶段直接退出来）。顺带也少一份明文副本。
        let pw = if self.password.trim().is_empty() {
            None
        } else {
            Some(self.write_pw_file().map_err(|e| format!("口令文件写不了：{e}"))?)
        };
        Ok(self.build_args(pw.as_deref(), mode))
    }

    fn launch(&mut self, mode: Mode) {
        let Some(client) = self.client_exe() else {
            self.status = "找不到 uulan-client —— 请把它和面板放在同一目录。".into();
            return;
        };
        // 表单没填全必须有显眼回应：旧版只改底部小字，用户报「点了没反应」。
        let args = match self.prepare(mode.clone()) {
            Ok(a) => a,
            Err(msg) => {
                self.alert = msg;
                return;
            }
        };
        let shown = sanitize(&args);
        // 子进程的输出**必须留盘**：改成 GUI 子系统后它没有控制台，
        // 不接住就永远看不到它为什么没起窗。
        //
        // **终端模式例外**（2026-11-22 真机坐实的 bug）：终端要的是一个用户能看见、
        // 能打字的窗口。把 stdout/stderr 接进日志文件、stdin 还是面板的 `/dev/null`，
        // 结果就是「点一下 → 远端 shell 连上了 → 输入立刻 EOF → 进程退出」，
        // 全程一声不响，用户只看到桌面什么都没发生。现在改为：
        // Windows 让控制端自己开一个新控制台，unix 借系统终端模拟器来跑它。
        let log_path = App::cfg_dir().join("gui-session.log");
        let _ = std::fs::create_dir_all(App::cfg_dir());
        let host = match mode {
            Mode::Desktop => TermHost::Direct,
            Mode::Term => match pick_term_host(&client, &args) {
                Ok(h) => h,
                Err(msg) => {
                    // 找不到终端模拟器也要说清「手怎么动」，不许静默。
                    self.alert = msg.clone();
                    self.push_log(msg);
                    return;
                }
            },
        };
        let mut cmd = spawn_command(&client, &args, &host);
        apply_creation_flags(&mut cmd, &mode);
        if redirects_stdout(&mode) {
            // 先清空再以 append(true) 打开：Windows 下 stdout/stderr 共用非 append 句柄
            // 并发写入会互相覆盖并留下 0x00 空洞、劈断 UTF-8 中文。
            let _ = std::fs::write(&log_path, b"");
            match std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
                Ok(f) => match f.try_clone() {
                    Ok(f2) => {
                        cmd.stdout(Stdio::from(f2)).stderr(Stdio::from(f));
                    }
                    Err(e) => {
                        self.alert = format!("会话日志句柄复制失败（{e}），无法诊断子进程。");
                        return;
                    }
                },
                Err(e) => {
                    self.alert = format!("会话日志建不了（{e}），也就无法诊断子进程为什么不起窗。");
                    return;
                }
            }
        }
        self.alert.clear();
        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id();
                self.save_cfg();
                let program = match &host {
                    TermHost::Direct => client
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string(),
                    TermHost::Wrap { program, .. } => format!(
                        "{} -e {}",
                        program.file_name().unwrap_or_default().to_string_lossy(),
                        client.file_name().unwrap_or_default().to_string_lossy()
                    ),
                };
                self.status = format!(
                    "已启动（PID {pid}），窗口应该已经出现。{}  口令走的是文件，没进命令行。",
                    match mode {
                        Mode::Desktop => "远程控制窗口",
                        Mode::Term => "终端窗口（独立窗口，直接在里面敲命令）",
                    }
                );
                self.push_log(format!("执行：{} {}", program, shown));
                self.child = Some((child, std::time::Instant::now(), log_path));
                self.child_term = mode == Mode::Term;
            }
            Err(e) => {
                self.alert = format!("启动失败：{e}\n控制端路径：{}", client.display());
                self.push_log(self.alert.clone());
            }
        }
    }
}

/// 一次「刷新设备」的后台任务：一进来就带时间戳，状态栏靠它算「已 N 秒」。
struct RefreshJob {
    started: std::time::Instant,
    rx: Receiver<RefreshDone>,
}

/// 后台线程回报的东西。控制端跑完了就连退出码一起带回来 ——
/// 「一个设备都没有」和「控制端自己报错退出」必须由 [`refresh_outcome`] 分开说。
enum RefreshDone {
    Ran { rows: Vec<DeviceRow>, unparsed: usize, head: String, err: String, ok: bool },
    SpawnFailed(String),
}

/// 刷新结果要怎么讲给人听（纯函数：不碰 IO，本机单测就能钉住措辞）。
#[derive(Debug, PartialEq, Eq)]
struct RefreshOutcome {
    /// 顶到眼前的那行红字（失败时要显眼 —— 这是这条按钮的老教训）。
    alert: Option<String>,
    /// 底部状态栏那一句。
    status: String,
    /// 写进「最近的动作」的补充行（没有就不写）。
    log: Option<String>,
}

/// 三种结果是三句不同的话：**控制端说连不上**、**连上了但一台都没有**、**读到了设备**。
/// 把它们糊成一句「没解析到设备」正是过去那种「查不出原因」的形态。
fn refresh_outcome(
    rows: &[DeviceRow],
    unparsed: usize,
    head: &str,
    err: &str,
    ok: bool,
) -> RefreshOutcome {
    let err = err.trim();
    let head = head.trim();
    if !ok && !err.is_empty() {
        // 控制端自己的原话最准（`连不上中继 10.255.255.1:21116：连接超时（10s）`），
        // 面板只负责把它原样顶到眼前，不替它编一句好话。
        let first = err.lines().next().unwrap_or(err).trim().to_string();
        return RefreshOutcome {
            alert: Some(first),
            status: "刷新失败 —— 原因见上面那行红字。".to_string(),
            log: Some(format!("刷新设备失败：{err}")),
        };
    }
    if rows.is_empty() {
        let why = if head.is_empty() {
            "中继连上了，但设备目录是空的（被控端还没上线？）".to_string()
        } else {
            head.to_string()
        };
        return RefreshOutcome {
            alert: Some(format!("没解析到设备。{why}")),
            status: "刷新失败 —— 一个设备都没读到。".to_string(),
            log: Some(format!("刷新设备：没解析到设备（{why}）")),
        };
    }
    RefreshOutcome {
        alert: None,
        status: if head.is_empty() {
            format!("共 {} 台设备", rows.len())
        } else {
            format!("{head}（共 {} 行）", rows.len())
        },
        log: if unparsed > 0 {
            // 认不出来的行必须说出来，而不是悄悄当成「离线」——
            // 「列表里全是灰点」和「解析器读不懂这个格式」是两回事。
            Some(format!("有 {unparsed} 行没能解析成设备（格式变了？）"))
        } else {
            None
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
fn route_args(direct: &str, relay: &str, device: &str, pw_file: Option<&Path>) -> Vec<String> {
    let mut a = if !direct.trim().is_empty() {
        vec!["--direct".into(), direct.trim().to_string()]
    } else {
        vec![
            "--server".into(), relay.trim().to_string(),
            "--device".into(), device.trim().to_string(),
        ]
    };
    // **没有访问口令就不发 --password-file**。OS 模式（用户名 + 该账户的登录密码）
    // 压根没有共享口令这回事；旧写法总会写一个**空文件**再把路径递过去，控制端在
    // 解析参数时就以 `--password-file 的第一行是空的` 退出 —— 面板还在、子进程立刻死、
    // GUI 子系统没有控制台，现场只看到「点了没反应」。这是本面板最贵的一条接缝。
    if let Some(p) = pw_file {
        a.push("--password-file".into());
        a.push(p.to_string_lossy().into_owned());
    }
    a
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

/// 一条会话怎么起：直接跑控制端，还是让终端模拟器去跑它。
///
/// 抽成一个枚举（而不是两个分支里各写一遍 `Command`）是为了让「拼出来的命令行」
/// 能在本机单测里钉住 —— 这一段在现场唯一的表现就是「窗口有没有出现」。
// Windows 上只用得到 Direct（新控制台就是它的「终端宿主」），Wrap 只在 unix 构造；
// 但这两个分支的单测必须在开发机（Windows）上跑，所以类型本身保持跨平台，只压死代码警告。
#[cfg_attr(windows, allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
enum TermHost {
    /// 直接跑控制端：桌面模式，以及 Windows 上的终端模式（新控制台由它自己开）。
    Direct,
    /// 让终端模拟器跑控制端：`<终端> -e <控制端> <参数…>`（unix 终端模式）。
    Wrap { program: PathBuf, argv: Vec<String> },
}

/// 桌面模式的输出落盘（GUI 子系统没有控制台）；终端模式**必须留在窗口里**。
fn redirects_stdout(mode: &Mode) -> bool {
    matches!(mode, Mode::Desktop)
}

/// 按宿主形态拼出真正要 spawn 的命令（纯函数：不碰 IO，本机单测能钉住 program/args）。
fn spawn_command(client: &Path, args: &[String], host: &TermHost) -> Command {
    match host {
        TermHost::Direct => {
            let mut c = Command::new(client);
            c.args(args);
            c
        }
        TermHost::Wrap { program, argv } => {
            let mut c = Command::new(program);
            c.args(argv);
            c
        }
    }
}

#[cfg(windows)]
fn apply_creation_flags(cmd: &mut Command, mode: &Mode) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(creation_flags_for(mode));
}

#[cfg(windows)]
fn creation_flags_for(mode: &Mode) -> u32 {
    // CREATE_NEW_CONSOLE(0x10)：终端模式必须给用户一个**能看见、能打字**的控制台。
    // CREATE_NO_WINDOW(0x0800_0000)：桌面模式只要窗口，不要那个黑色控制台壳。
    // （以前两种模式都套 CREATE_NO_WINDOW，于是「开一个远端终端」在 Windows 上
    // 同样是个死按钮 —— 结构上跟麒麟那次是同一个错。）
    match mode {
        Mode::Desktop => 0x0800_0000,
        Mode::Term => 0x0000_0010,
    }
}

#[cfg(not(windows))]
fn apply_creation_flags(_cmd: &mut Command, _mode: &Mode) {}

/// 终端模拟器的候选表，**顺序就是优先级**。麒麟 V10 上 `x-terminal-emulator` 与
/// `mate-terminal` 实测都在；其余几种是别的发行版的常见项，排后面不吃亏。
#[cfg(not(windows))]
const TERM_CANDIDATES: &[&str] = &[
    "x-terminal-emulator",
    "ukui-terminal",
    "mate-terminal",
    "gnome-terminal",
    "xfce4-terminal",
    "konsole",
    "lxterminal",
    "xterm",
];

#[cfg(windows)]
#[allow(dead_code)]
const TERM_CANDIDATES: &[&str] = &[];

/// 终端模拟器的调法：`-e` 后面跟完整命令行（x-terminal-emulator / mate-terminal 都认）。
#[cfg_attr(windows, allow(dead_code))]
fn term_wrap_argv(client: &Path, args: &[String]) -> Vec<String> {
    let mut v = vec!["-e".to_string(), client.display().to_string()];
    v.extend(args.iter().cloned());
    v
}

/// 在若干目录里按名字优先级找第一个存在的程序（纯函数，`is_file` 由调用方给：
/// 本机单测不碰盘，真调用给 `Path::is_file`）。
#[cfg_attr(windows, allow(dead_code))]
fn find_program_in(
    dirs: &[PathBuf],
    names: &[&str],
    is_file: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    for n in names {
        for d in dirs {
            let p = d.join(n);
            if is_file(&p) {
                return Some(p);
            }
        }
    }
    None
}

/// 终端模式的外壳程序：unix 找一个终端模拟器；Windows 不需要（新控制台就够）。
#[cfg(not(windows))]
fn pick_term_host(client: &Path, args: &[String]) -> Result<TermHost, String> {
    let dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    match find_program_in(&dirs, TERM_CANDIDATES, &|p| p.is_file()) {
        Some(program) => Ok(TermHost::Wrap { program, argv: term_wrap_argv(client, args) }),
        None => Err(format!(
            "找不到可用的终端模拟器（试过：{}）。\n可以自己在终端里跑这一行：\n  {} {}",
            TERM_CANDIDATES.join(" / "),
            client.display(),
            args.join(" ")
        )),
    }
}

#[cfg(windows)]
fn pick_term_host(_client: &Path, _args: &[String]) -> Result<TermHost, String> {
    Ok(TermHost::Direct)
}

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

        // 会话在跑的时候，状态栏要能看出「还在跑、跑了多久」——
        // 以前的 footer 只在启动那一刻写过一句 PID，之后整场都是死的。
        let running_for = self
            .child
            .as_ref()
            .map(|(_, born, _)| born.elapsed().as_secs());
        egui::Panel::bottom("foot").show(root, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(&self.status);
                if let Some(secs) = running_for {
                    ui.label(
                        egui::RichText::new(format!("· 会话进行中（已 {}）", fmt_dur(secs)))
                            .weak()
                            .small(),
                    );
                }
            });
        });

        // 每秒醒一次去看旗子（第二个实例被双击起来、或用户又点了一次图标）。
        root.ctx().request_repaint_after(std::time::Duration::from_millis(1000));
        // 刷新任务的结果在这一帧落地；在跑的时候 100 ms 醒一次，让「已 N 秒」真的在走。
        self.poll_refresh();
        if self.refresh.is_some() {
            root.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
        if self.relaunch.swap(false, Ordering::Relaxed) && self.alert.is_empty() {
            self.alert = "又有人双击了一次图标 —— 已经有一个面板在跑了，就是这个窗口。\n                两个窗口会各自显示不同的验证码，所以第二个被拒绝启动。".into();
        }

        // 子进程秒退 = 「点了没反应」的真实形态。必须当场说，而不是等用户去猜。
        // 写法用 take/放回而不是 as_mut：as_mut 里再访问 self 会撞借用检查。
        if let Some((mut child, born, lp)) = self.child.take() {
            match child.try_wait() {
                Ok(Some(st)) => {
                    let secs = born.elapsed().as_secs();
                    let code = st.code();
                    let raw = std::fs::read(&lp)
                        .map(|b| String::from_utf8_lossy(&b).into_owned())
                        .unwrap_or_default();
                    let tail: String = raw
                        .lines()
                        .filter(|l| !l.trim().is_empty())
                        .rev()
                        .take(3)
                        .collect::<Vec<_>>()
                        .join(" ← ");
                    if secs < 10 {
                        let what = if self.child_term { "终端窗口" } else { "远程进程" };
                        // 终端模式没有日志文件（输出留在那个窗口里），别再指向一个
                        // 上一轮桌面会话留下的旧文件 —— 现场会照着它去读别人的历史。
                        let where_to_look = if self.child_term {
                            "（终端模式不落盘：那行原因在终端窗口里）".to_string()
                        } else {
                            format!("全文在 {}", lp.display())
                        };
                        self.alert = format!(
                            "{what} {st} 就退了（不到 10 秒），所以你没看到窗口。\n最后三行输出：{}\n{}",
                            tail.trim().replace('\n', " / "),
                            where_to_look
                        );
                    } else {
                        // **正常关窗也是「会话结束」**：面板以前只在「不到 10 秒秒退」时说话，
                        // 于是长会话结束时一声不吭 —— 现场分不清「我自己关的」「对面断了」还是「崩溃了」。
                        // 退出码的含义（尤其 4 = 放弃重连）由 session_end_note 负责说人话。
                        let note = if self.child_term {
                            term_end_note(code, secs)
                        } else {
                            session_end_note(code, secs)
                        };
                        self.push_log(note.clone());
                        self.status = note.clone();
                        if code.unwrap_or(-1) != 0 {
                            self.alert = format!(
                                "{note}\n最后三行输出：{}\n全文在 {}",
                                tail.trim().replace('\n', " / "),
                                lp.display()
                            );
                        }
                    }
                }
                // 还活着就放回原位，下一帧继续看。
                _ => self.child = Some((child, born, lp)),
            }
        }

        egui::CentralPanel::default().show(root, |ui| {
            // 整页可滚动：内容（很长的失败提示 + 全部表单）在 720x520 的默认窗口里
            // 会超过可视高度 —— 真机实测「开始远程控制」会被切在窗口外，点不到。
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                // 两页：这台机器去连别人 / 别人来连这台机器。UU 也是这么分的。
                ui.horizontal(|ui| {
                    // 切页签**不清 alert**：刷新失败的红字是在这一页发起的，切过去看就该还在
                    // （真机踩过：清了以后状态栏那句「原因见上面那行红字」指向一行不存在的字）。
                    // 提示由「我知道了」显式关掉。
                    if ui.selectable_label(self.tab == 0, "远程控制").clicked() {
                        self.tab = 0;
                    }
                    if ui.selectable_label(self.tab == 1, "本机（被控端）信息").clicked() {
                        self.tab = 1;
                        self.local.reload();
                    }
                });
                ui.separator();
                // 失败提示**两页都渲染**：刷新是在这一页按的，结果不该因为切了页就不见。
                if !self.alert.is_empty() {
                    // 提示可能很长（带日志尾部的退出码说明，真机实测 6~8 行）：**给它自己的、
                    // 封顶高度的滚动区**。否则 720x520 的窗口里「开始远程控制」会被顶到窗口外，
                    // 用户连再试一次都点不到 —— 真机上就是一行 panic 尾巴把按钮挤出可视区的。
                    egui::ScrollArea::vertical()
                        .id_salt("alert-scroll")
                        .max_height(110.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            ui.colored_label(egui::Color32::from_rgb(200, 40, 40), &self.alert);
                        });
                    // 「我知道了」留在滚动区外面：关掉提示的办法不能被提示自己挤走。
                    if ui.button("我知道了").clicked() {
                        self.alert.clear();
                    }
                    ui.separator();
                }
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

                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        ui.label("中继地址");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.relay)
                                .desired_width(240.0)
                                .hint_text("例 192.168.1.10:21116"),
                        );
                        // 刷新在后台线程里跑：这里只负责「按下」和「显示在跑」。
                        let busy = self.refresh.is_some();
                        let label = if busy { "刷新中…" } else { "刷新设备" };
                        if ui.add_enabled(!busy, egui::Button::new(label)).clicked() {
                            self.start_refresh();
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
                    // 这一行控件最多（设备 ID / 临时码 / 记住口令 / 系统账户 + 密码）。用
                    // horizontal_wrapped：窗口压到最小 560x420 时（真机实测）不换行会把
                    // 「记住这…」截断、系统账户框整个看不见 —— 那不是「挤」，是功能消失。
                    ui.horizontal_wrapped(|ui| {
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
                                .hint_text("用户名：被控端用「系统账户」认证时填这里")
                                .desired_width(150.0),
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut self.os_password)
                                .password(true)
                                .hint_text("该账户的登录密码")
                                .desired_width(110.0),
                        );
                        if !self.os_user.trim().is_empty() && self.password.trim().is_empty() {
                            // 这一句是给现场的人看的：填了系统账户就可以不填临时码。
                            ui.label(egui::RichText::new("只用系统账户认证（临时码可留空）").weak().small());
                        }
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

        });
    }
}

/// 会话结束时给现场看的一句话。
///
/// 退出码不是内部细节：控制端用 **4 = 放弃重连**（`session::drive` 的约定）、**3 = 被控端
/// 拒绝键鼠注入**（`--inject` 的约定）。窗口自己消失时，现场最需要的正是这一句 ——
/// 以前面板对正常结束完全沉默，用户只能猜「是断线还是我关的」。
fn session_end_note(code: Option<i32>, secs: u64) -> String {
    let d = fmt_dur(secs);
    match code {
        Some(0) => format!("远程会话已结束（正常收工，用时 {d}）"),
        Some(4) => format!(
            "远程会话已断开：控制端放弃了重连（退出码 4，用时 {d}）—— 被控端可能关机/断网，可再点一次「开始远程控制」"
        ),
        Some(3) => format!("远程会话结束：被控端没接受键鼠注入（退出码 3，用时 {d}）"),
        Some(n) => format!("远程会话异常结束（退出码 {n}，用时 {d}）—— 最后三行输出见下方提示"),
        None => format!("远程会话被强制结束（拿不到退出码，用时 {d}）"),
    }
}

/// 终端模式的收尾话术。
///
/// **退出码的归属必须说清**：unix 上这条进程是终端模拟器，它的退出码不是远端 shell 的；
/// 远端 shell 的退出码只在那个窗口里看得到。混着说会让现场拿着一个 0 去怀疑「shell 明明
/// 退 5 了」。
fn term_end_note(code: Option<i32>, secs: u64) -> String {
    let d = fmt_dur(secs);
    match code {
        Some(0) => format!("远端终端窗口已关闭（用时 {d}）"),
        Some(n) => format!(
            "远端终端窗口已关闭（终端程序退出码 {n}，用时 {d}）—— 远端 shell 自己的退出码在那个窗口里"
        ),
        None => format!("远端终端窗口已关闭（拿不到终端程序的退出码，用时 {d}）"),
    }
}

/// 时长写成现场能念的样子。不引时间库：这里只做整除，任何依赖都是多余。
fn fmt_dur(secs: u64) -> String {
    if secs < 60 {
        format!("{secs} 秒")
    } else if secs < 3600 {
        format!("{} 分 {} 秒", secs / 60, secs % 60)
    } else {
        format!("{} 小时 {} 分", secs / 3600, (secs % 3600) / 60)
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

/// `--print-args`：把「面板会怎么跑控制端」原样打出来，不建窗口、不连网、不抢单实例锁。
///
/// 两个用途，都不是调试玩具：
///   · 现场：报「点了没反应」时，第一件要知道的就是子进程拿到的到底是什么参数
///     （GUI 子系统没有控制台，参数错了只能靠猜）；
///   · 验收：脚本可以直接拿这份 argv 跑真控制端，于是「面板拼出来的命令行能不能用」
///     第一次成为机器可查的量，而不是靠人点一下看窗口有没有出现。
///
/// 口令与系统密码从环境变量 `UULAN_GUI_PASSWORD` / `UULAN_GUI_OS_PASSWORD` 取 ——
/// **不进 argv**，与面板本身同一条纪律。
fn dry_run() -> Option<i32> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if !argv.iter().any(|a| a == "--print-args") {
        return None;
    }
    let mut direct = String::new();
    let mut relay = String::new();
    let mut device = String::new();
    let mut os_user = String::new();
    let mut term = false;
    let mut argv_out: Option<PathBuf> = None;
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--direct" => { direct = argv.get(i + 1).cloned().unwrap_or_default(); i += 2; }
            "--relay" => { relay = argv.get(i + 1).cloned().unwrap_or_default(); i += 2; }
            "--device" => { device = argv.get(i + 1).cloned().unwrap_or_default(); i += 2; }
            "--os-user" => { os_user = argv.get(i + 1).cloned().unwrap_or_default(); i += 2; }
            "--argv-out" => { argv_out = argv.get(i + 1).map(PathBuf::from); i += 2; }
            "--term" => { term = true; i += 1; }
            _ => i += 1,
        }
    }
    let mut app = App::new(Arc::new(AtomicBool::new(false)));
    if !direct.is_empty() { app.direct = direct; }
    if !relay.is_empty() { app.relay = relay; }
    if !device.is_empty() { app.device = device; }
    app.os_user = os_user;
    app.password = std::env::var("UULAN_GUI_PASSWORD").unwrap_or_default();
    app.os_password = std::env::var("UULAN_GUI_OS_PASSWORD").unwrap_or_default();
    let mode = if term { Mode::Term } else { Mode::Desktop };
    match app.prepare(mode) {
        Ok(args) => {
            println!("模式：{}", if term { "终端" } else { "远程控制" });
            // 打的是脱敏后的形状：口令文件路径抹成占位符，口令本身从不上屏。
            println!("{}", sanitize(&args));
            if let Some(p) = argv_out {
                // 一行一个参数：脚本直接读，不用再去猜引号。
                if let Err(e) = std::fs::write(&p, format!("{}\n", args.join("\n"))) {
                    eprintln!("写不了 --argv-out：{e}");
                    return Some(3);
                }
            }
            Some(0)
        }
        Err(msg) => {
            eprintln!("表单不完整：{msg}");
            Some(2)
        }
    }
}

fn main() {
    // `--print-args` 必须在单实例锁**之前**：现场诊断往往正是面板开着的时候。
    if let Some(code) = dry_run() {
        std::process::exit(code);
    }
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
        let a = route_args("192.168.1.20:21117", "10.0.0.9:21116", "805747189", Some(pw));
        assert_eq!(a[0], "--direct");
        assert_eq!(a[1], "192.168.1.20:21117");
        assert!(!a.contains(&"--server".to_string()), "直连还带了 --server：{a:?}");
        assert!(!a.contains(&"--device".to_string()), "直连还带了 --device：{a:?}");
        let i = a.iter().position(|x| x == "--password-file").unwrap();
        assert_eq!(a[i + 1], "PW", "口令文件必须紧跟在 --password-file 后，否则 sanitize 漏脱敏");

        let b = route_args("", "10.0.0.9:21116", "805747189", Some(pw));
        assert!(b.contains(&"--server".to_string()) && b.contains(&"--device".to_string()));
        assert!(!b.contains(&"--direct".to_string()));

        // 只填空白也算没直连（并把空白擦掉，不发一个空参数给控制端）
        let c = route_args("   ", " 10.0.0.9:21116 ", " 805747189 ", Some(pw));
        assert_eq!(c[1], "10.0.0.9:21116");
        assert_eq!(c[3], "805747189");
    }

    #[test]
    fn no_access_password_means_no_password_file_argument() {
        // OS 模式（用户名 + 系统登录密码）根本没有访问口令。旧写法总会写一个**空文件**
        // 并把 --password-file 递过去，控制端在参数阶段就 `第一行是空的` 退出 ——
        // 面板在、子进程立刻死、GUI 没有控制台，现场只看到「点了没反应」。
        let a = route_args("192.168.1.20:21117", "", "", None);
        assert!(!a.contains(&"--password-file".to_string()), "没口令却给了口令文件：{a:?}");
        assert_eq!(a[0], "--direct");
        // 直连不需要设备 ID（这条以前被表单拦在前面，和它自己的提示自相矛盾）。
        assert!(!a.contains(&"--device".to_string()), "直连还带了 --device：{a:?}");
    }

    fn row(id: &str, online: bool) -> DeviceRow {
        DeviceRow { id: id.into(), name: "麒麟真机".into(), online, last: false }
    }

    #[test]
    fn refresh_success_reports_the_head_line_not_a_guess() {
        let rows = vec![row("805747189", true)];
        let o = refresh_outcome(&rows, 0, "中继 192.168.223.15:21116：1 台设备（在线 1）", "", true);
        assert!(o.alert.is_none(), "正常刷新不该弹红字：{o:?}");
        assert!(o.status.contains("1 台设备"), "状态栏要带控制端自己那句：{o:?}");
        assert_eq!(o.log, None);
    }

    #[test]
    fn refresh_failure_shows_the_client_own_words() {
        // 这条钉的是「查不出原因」的老形态：连不上中继时，面板必须把控制端的原话
        // （连不上中继 …：连接超时（10s））顶到眼前，而不是笼统的「没解析到设备」。
        let o = refresh_outcome(&[], 0, "", "连不上中继 10.255.255.1:21116：连接超时（10s）", false);
        let a = o.alert.as_ref().expect("失败必须显眼");
        assert!(a.contains("连不上中继"), "{a}");
        assert!(a.contains("10.255.255.1:21116"), "地址要原样带出来，现场才知道改哪儿：{a}");
        assert!(o.status.contains("刷新失败"), "{o:?}");
    }

    #[test]
    fn refresh_with_zero_devices_says_which_kind_of_empty_it_is() {
        // 中继连上了、目录是空的：这不是「解析器读不懂」，是这台中继上现在没有机器。
        let o = refresh_outcome(&[], 0, "中继 10.0.0.9:21116：0 台设备（在线 0）", "", true);
        assert!(o.alert.is_some());
        let a = o.alert.unwrap();
        assert!(a.contains("0 台设备"), "要把控制端那句带出来：{a}");
        assert!(!a.contains("连不上"), "别把「空的」说成「连不上」：{a}");
    }

    #[test]
    fn unparsable_lines_are_counted_in_the_log_not_silently_dropped() {
        let rows = vec![row("805747189", true)];
        let o = refresh_outcome(&rows, 2, "中继 x：1 台设备（在线 1）", "", true);
        assert!(o.alert.is_none());
        let l = o.log.expect("认不出来的行必须记账");
        assert!(l.contains('2'), "{l}");
    }

    #[test]
    fn a_normal_end_is_reported_with_its_duration() {
        let n = session_end_note(Some(0), 605);
        assert!(n.contains("正常收工"), "{n}");
        assert!(n.contains("10 分 5 秒"), "{n}");
    }

    #[test]
    fn give_up_on_reconnect_says_exit_code_4_out_loud() {
        let n = session_end_note(Some(4), 30);
        assert!(n.contains("放弃"), "{n}");
        assert!(n.contains("退出码 4"), "{n}");
    }

    #[test]
    fn unknown_exit_code_is_not_dressed_up_as_normal() {
        let n = session_end_note(Some(1), 61);
        assert!(n.contains("异常结束"), "{n}");
        assert!(n.contains("退出码 1"), "{n}");
        assert!(!n.contains("正常"), "{n}");
    }

    #[test]
    fn killed_without_a_code_is_still_reported() {
        let n = session_end_note(None, 5);
        assert!(n.contains("强制结束"), "{n}");
    }

    #[test]
    fn durations_read_like_a_person_would_say_them() {
        assert_eq!(fmt_dur(0), "0 秒");
        assert_eq!(fmt_dur(59), "59 秒");
        assert_eq!(fmt_dur(65), "1 分 5 秒");
        assert_eq!(fmt_dur(7200), "2 小时 0 分");
        assert_eq!(fmt_dur(3661), "1 小时 1 分");
    }

    #[test]
    fn missing_password_file_only_shortens_the_argv_it_does_not_reorder_it() {
        // 拼装顺序不是风格问题：`--password-file` 必须紧跟在它的值前面（sanitize()
        // 靠这个位置脱敏），附加参数只能往后加。
        let a = route_args("", "10.0.0.9:21116", "805747189", None);
        assert!(a.contains(&"--device".to_string()), "经中继必须带设备 ID：{a:?}");
        assert!(!a.contains(&"--password-file".to_string()));
        assert_eq!(a[0], "--server");
        assert_eq!(a[1], "10.0.0.9:21116");
    }

    // ---- 终端模式：窗口到底怎么起（2026-11-22） ----

    #[test]
    fn terminal_mode_hands_the_client_over_to_a_terminal_emulator() {
        // 真机 bug 的形态：以前终端模式也把输出接进日志、stdin 是 /dev/null，
        // 于是远端 shell 一连上就 EOF —— 用户只看到桌面什么都没发生。
        // 修法就是这行 `-e`：让终端模拟器去跑控制端，输入输出都留在那个窗口里。
        let args: Vec<String> = ["--server", "192.168.223.15:21116", "--device", "805747189", "--password-file", "/tmp/x.pw", "--term"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let argv = term_wrap_argv(Path::new("/opt/uulan/bin/uulan-client"), &args);
        assert_eq!(argv[0], "-e", "终端模拟器只认 -e 后面那条完整命令");
        assert_eq!(argv[1], "/opt/uulan/bin/uulan-client");
        assert_eq!(&argv[2..], &args[..], "控制端的参数必须原样透传，一个都不许改");
    }

    #[test]
    fn terminal_emulator_is_picked_by_candidate_order_then_path_order() {
        // 名字顺序优先于目录顺序：x-terminal-emulator 在哪个目录里先找到就用它，
        // 而不是「PATH 里第一个目录里碰巧存在的那个」。
        let dirs = vec![PathBuf::from("/usr/local/bin"), PathBuf::from("/usr/bin")];
        let have = ["/usr/local/bin/mate-terminal", "/usr/bin/x-terminal-emulator"];
        let got = find_program_in(&dirs, &["x-terminal-emulator", "mate-terminal"], &|p| {
            have.iter().any(|h| Path::new(h) == p)
        });
        assert_eq!(got, Some(PathBuf::from("/usr/bin/x-terminal-emulator")));

        // 一个都没有 = None（调用方据此报「找不到终端模拟器」，而不是静默起个死进程）
        assert_eq!(
            find_program_in(&dirs, &["x-terminal-emulator", "mate-terminal"], &|_| false),
            None
        );
    }

    #[test]
    fn wrap_host_runs_the_terminal_as_program_and_the_client_as_argument() {
        // spawn 出去的到底是哪个程序，是本机唯一能钉住的部分（窗口长什么样钉不住）。
        let host = TermHost::Wrap {
            program: PathBuf::from("/usr/bin/mate-terminal"),
            argv: vec!["-e".into(), "/opt/uulan/bin/uulan-client".into(), "--term".into()],
        };
        let cmd = spawn_command(Path::new("/opt/uulan/bin/uulan-client"), &["--term".to_string()], &host);
        assert_eq!(cmd.get_program(), Path::new("/usr/bin/mate-terminal"));
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().to_string()).collect();
        assert_eq!(args, vec!["-e", "/opt/uulan/bin/uulan-client", "--term"]);
    }

    #[test]
    fn direct_host_runs_the_client_itself() {
        let cmd = spawn_command(
            Path::new("C:/uulan/uulan-client.exe"),
            &["--server".to_string(), "10.0.0.9:21116".into()],
            &TermHost::Direct,
        );
        assert_eq!(cmd.get_program(), Path::new("C:/uulan/uulan-client.exe"));
        assert_eq!(cmd.get_args().count(), 2);
    }

    #[test]
    fn desktop_output_goes_to_the_log_but_a_terminal_keeps_it_in_its_window() {
        assert!(redirects_stdout(&Mode::Desktop), "桌面模式必须留盘，否则排不了错");
        assert!(!redirects_stdout(&Mode::Term), "终端模式接走输出 = 窗口里什么都没有");
    }

    #[cfg(windows)]
    #[test]
    fn windows_terminal_mode_gets_its_own_console() {
        // CREATE_NO_WINDOW 对终端模式就是「没有控制台可打字」，与麒麟那次同族。
        assert_eq!(creation_flags_for(&Mode::Desktop), 0x0800_0000);
        assert_eq!(creation_flags_for(&Mode::Term), 0x0000_0010);
    }

    #[test]
    fn terminal_conclusion_says_whose_exit_code_it_is() {
        // 终端模式的退出码是**终端模拟器**的，不是远端 shell 的 —— 措辞里必须写清，
        // 否则现场会拿着一个 0 去怀疑「shell 明明退 5 了」。
        assert_eq!(term_end_note(Some(0), 12), "远端终端窗口已关闭（用时 12 秒）");
        let n = term_end_note(Some(7), 65);
        assert!(n.contains("退出码 7") && n.contains("1 分 5 秒"), "{n}");
        assert!(n.contains("远端 shell 自己的退出码"), "{n}");
        assert!(term_end_note(None, 3).contains("拿不到"));
    }

}

