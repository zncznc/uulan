//! 本机（被控端）信息面板：把「设备 ID + 一次性验证码」摊在屏幕上给对面的人念。
//!
//! # 为什么值得单独一屏
//!
//! UU 远程的被控端就是这样的：坐在被控那台机器前的人，看一眼本机窗口，把
//! 「识别码 + 验证码」念给你，你在这头输入就能连上。要在物理隔离内网里做同一件事，
//! 缺的从来不是协议（设备 ID、口令、一次性临时码、每小时轮换，M6 全都做好了并且验过），
//! 缺的是**把这些显示出来的那一屏**。
//!
//! # 一条安全上的主张：默认念临时码，不是永久口令
//!
//! 临时码**用一次即失效**、每整点轮换，念错人、被旁人听见，损失都只有一次访问；
//! 永久口令一旦被人记下就一直有效，而且连错 5 次还会触发锁定。所以：
//!
//! - 临时码**直接大字显示**（这才是要念出去的东西）；
//! - 永久口令**默认掩码**，必须显式点「显示」才展开，界面上同时说明为什么更该用临时码；
//! - 两者都**绝不写进 `gui.json`、绝不进任何日志**（延续 M6 的脱敏纪律）。
//!
//! 数据只从被控端自己的配置文件读（`host.json`），不新造协议、不新造真相。

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;

/// 单实例锁端口。选一个高位端口而不是文件锁：Windows 与麒麟同一套代码，
/// 而且**第二个实例能当场告诉第一个实例「有人又双击了一次」**，
/// 第一个实例就能在界面上说一句话 —— 否则用户只会觉得「双击没反应」。
pub const LOCK_PORT: u16 = 45579;

/// 启动时调用：抢到锁返回 Some(listener)（由调用方交给后台线程持续 accept），
/// 抢不到说明已有实例 —— 给它发一个信号后自己退出。
pub fn acquire_single_instance() -> Option<TcpListener> {
    match TcpListener::bind(("127.0.0.1", LOCK_PORT)) {
        Ok(l) => Some(l),
        Err(_) => {
            if let Ok(mut st) = TcpStream::connect(("127.0.0.1", LOCK_PORT)) {
                let _ = st.write_all(b"relaunch");
            }
            None
        }
    }
}

/// 后台线程：第二个实例双击启动时会连一下这个端口；收到就把旗子竖起来。
///
/// 这里**故意不传 egui::Context**：线程在主线程建好 Context 之前就得起（否则端口
/// 空着，第二个实例照样能进来）。所以线程只写旗子，主线程按秒轮询 —— 一次每秒的
/// 唤醒代价可以忽略，换来的是「双击第二次」这件事一定会被看见。
pub fn watch_relaunch(
    listener: std::net::TcpListener,
    flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if let Ok(mut st) = stream {
                let mut buf = [0u8; 16];
                let _ = std::io::Read::read(&mut st, &mut buf);
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    });
}

#[derive(Debug, Clone, Default)]
pub struct Info {
    pub path: PathBuf,
    pub id: String,
    pub name: String,
    pub server: String,
    pub password: String,
    pub temp: String,
}

pub struct LocalPanel {
    pub info: Option<Info>,
    pub reveal: bool,
    pub status: String,
}

impl LocalPanel {
    pub fn new() -> Self {
        let mut p = Self { info: None, reveal: false, status: String::new() };
        p.reload();
        p
    }

    /// 本机被控端配置文件：XDG 是正式位置，另外认一下安装前缀里可能存在的副本。
    fn config_path() -> Option<PathBuf> {
        let mut cands: Vec<PathBuf> = Vec::new();
        if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
            cands.push(PathBuf::from(x).join("uulan").join("host.json"));
        }
        if let Some(h) = std::env::var_os("HOME") {
            let home = PathBuf::from(h);
            cands.push(home.join(".config").join("uulan").join("host.json"));
            cands.push(home.join(".uulan").join("host.json"));
        }
        for p in ["/opt/uulan/etc/host.json", "/etc/uulan/host.json"] {
            cands.push(PathBuf::from(p));
        }
        cands.into_iter().find(|p| p.is_file())
    }

    /// 被控端二进制（生成临时码要用它，别自己造码 —— 造了中继也不认）。
    fn host_exe() -> Option<PathBuf> {
        let name = if cfg!(windows) { "uulan-host.exe" } else { "uulan-host" };
        let mut cands: Vec<PathBuf> = Vec::new();
        if let Ok(cur) = std::env::current_exe() {
            if let Some(dir) = cur.parent() {
                cands.push(dir.join(name));
                cands.push(dir.join("..").join(name));
            }
        }
        cands.push(PathBuf::from("/opt/uulan/bin").join(name));
        if let Some(h) = std::env::var_os("HOME") {
            let home = PathBuf::from(h);
            cands.push(home.join(".local/uulan/bin").join(name));
            cands.push(home.join(".uulan/bin").join(name));
        }
        cands.into_iter().find(|p| p.is_file())
    }

    pub fn reload(&mut self) {
        self.info = match Self::config_path() {
            Some(path) => match std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).map_err(|e| e.to_string()))
            {
                Ok(v) => {
                    let g = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
                    let temp = v.get("temp_code").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    self.status.clear();
                    Some(Info {
                        path: path.clone(),
                        id: g("device_id"),
                        name: g("name"),
                        server: g("server"),
                        password: g("password"),
                        temp,
                    })
                }
                Err(e) => {
                    self.status = format!("配置文件读不到（{}）：{e}", path.display());
                    None
                }
            },
            None => {
                self.status = "这台机器上没有被控端配置（host.json）—— 本机不是被控端，或者还没首次运行过。".to_string();
                None
            }
        };
    }

    /// 生成一次性临时码：交给被控端自己去生成与落盘，我们只负责之后重读。
    pub fn gen_temp(&mut self) {
        let Some(path) = Self::config_path() else {
            self.status = "没有配置文件，生成不了。".into();
            return;
        };
        let Some(host) = Self::host_exe() else {
            self.status = "找不到 uulan-host 二进制 —— 临时码必须由被控端自己生成，中继才认。".into();
            return;
        };
        let mut cmd = Command::new(&host);
        cmd.args(["--new-temp-code", "--config"]);
        cmd.arg(&path);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        match cmd.output() {
            Ok(out) => {
                if out.status.success() {
                    self.reload();
                    self.status = "已生成一次性临时码（用一次即失效，每整点轮换）。".into();
                } else {
                    let e = String::from_utf8_lossy(&out.stderr);
                    // 只截尾部，别把整段（可能含口令的行）搬到界面上。
                    let tail: String = e.lines().rev().take(2).collect::<Vec<_>>().join(" ← ");
                    self.status = format!("被控端拒绝生成（退出码 {:?}）：{}", out.status.code(), tail);
                }
            }
            Err(e) => self.status = format!("跑不了 {}：{e}", host.display()),
        }
    }

    /// 让这一屏不被空闲锁屏/黑屏抢走。**只在麒麟/UKUI 上有意义**，而且做成
    /// 用户显式按的按钮 —— 面板不该在背后悄悄改桌面配置。
    ///
    /// 为什么必须做：这一屏的唯一用途是把码**念出来**，而 UKUI 默认
    /// `lock-blank-screen=true`（黑屏即锁）、唤醒即锁 —— 锁屏页一盖，对面的人
    /// 面对的是登录框，什么都念不出来，还会误以为软件坏了。
    pub fn keep_awake(&mut self) {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            self.status = "当前是 Wayland 会话，这些命令管不到锁屏。".into();
            return;
        }
        // 数组元素长度不一致（gsettings 五个、xset 三个），所以这里必须是切片的切片，
        // 不能写成 [[&str; 5]; 6] —— 那种写法在混长度时直接编译不过。
        let cmds: [&[&str]; 6] = [
            &["gsettings", "set", "org.ukui.screensaver", "idle-activation-enabled", "false"],
            &["gsettings", "set", "org.ukui.screensaver", "lock-enabled", "false"],
            &["gsettings", "set", "org.ukui.power-manager", "lock-blank-screen", "false"],
            &["gsettings", "set", "org.ukui.power-manager", "lock-use-screensaver", "false"],
            &["xset", "s", "off"],
            &["xset", "-dpms"],
        ];
        let mut done: Vec<String> = Vec::new();
        for k in cmds {
            if let Some(o) = std::process::Command::new(k[0]).args(&k[1..]).output().ok() {
                if o.status.success() {
                    done.push(k.join(" "));
                }
            }
        }
        self.status = if done.is_empty() {
            "一条都没改成 —— 这台机器可能不是 UKUI/麒麟桌面，或命令不在 PATH。".to_string()
        } else {
            format!("已关闭空闲锁屏与黑屏锁（{} 项生效）。恢复系统默认：重新登录或把这几项设回 true。", done.len())
        };
    }

    fn kv(ui: &mut egui::Ui, ctx: &egui::Context, label: &str, value: &str, big: bool, copy: bool) {
        ui.horizontal(|ui| {
            ui.label(label);
            let text = if big {
                egui::RichText::new(value).monospace().size(22.0).strong()
            } else {
                egui::RichText::new(value).monospace()
            };
            let resp = ui.add(egui::Label::new(text).selectable(true));
            if copy && ui.add(egui::Button::new("复制")).on_hover_text("复制到本机剪贴板").clicked() {
                ctx.copy_text(value.to_string());
            }
            let _ = resp;
        });
    }

    pub fn ui(&mut self, root: &mut egui::Ui) {
        // Context 是 Arc 型的，克隆出来用；直接 root.ctx() 会把 root 借成只读，
        // 后面再 root.group(..) 就撞借用检查。
        let ctx = root.ctx().clone();
        if let Some(info) = self.info.clone() {
            root.group(|ui| {
                ui.label(egui::RichText::new("这台机器的被控端身份").strong());
                ui.separator();
                ui.label(egui::RichText::new(format!("配置文件：{}", info.path.display())).small().weak());
                ui.add_space(6.0);
                Self::kv(ui, &ctx, "设备 ID", &info.id, true, true);
                Self::kv(ui, &ctx, "名称", &info.name, false, false);
                Self::kv(ui, &ctx, "中继地址", &info.server, false, false);
            });

            root.add_space(6.0);
            root.group(|ui| {
                ui.label(egui::RichText::new("给对方念这个：一次性临时验证码").strong());
                ui.separator();
                if info.temp.is_empty() {
                    ui.label("本机还没有临时验证码。");
                    if ui.button("生成一次性临时码").clicked() {
                        self.gen_temp();
                    }
                } else {
                    Self::kv(ui, &ctx, "临时码", &info.temp, true, true);
                    ui.label(
                        egui::RichText::new("用一次即失效；每整点自动换一张新的。念错、被旁人听到，损失都只有一次访问。")
                            .weak(),
                    );
                    if ui.button("换一张新的").clicked() {
                        self.gen_temp();
                    }
                }
            });

            root.add_space(6.0);
            root.group(|ui| {
                ui.label(egui::RichText::new("永久口令（一般别给）").strong());
                ui.separator();
                ui.label(
                    egui::RichText::new("永久口令一直有效，而且连错 5 次会锁 60 秒。只有在确实需要长期无人值守接入时才用它。")
                        .weak(),
                );
                let shown = if self.reveal {
                    info.password.clone()
                } else {
                    "●".repeat(info.password.chars().count().min(24))
                };
                Self::kv(ui, &ctx, "口令", &shown, false, self.reveal);
                if ui.button(if self.reveal { "隐藏" } else { "显示永久口令" }).clicked() {
                    self.reveal = !self.reveal;
                }
                if self.reveal {
                    ui.label(egui::RichText::new("已经显示在屏幕上了 —— 注意身后有没有人。").weak());
                }
            });

            // 锁屏一盖，这一屏就念不出来 —— 所以把它做成一个看得见的开关。
            root.group(|ui| {
                ui.label(egui::RichText::new("让这一屏一直可见").strong());
                ui.separator();
                ui.label(egui::RichText::new(
                    "麒麟/UKUI 默认「黑屏即锁」，锁屏页会盖住验证码，对面的人什么也念不出来。",
                )
                .weak());
                if ui.button("关闭本机空闲锁屏与休眠锁").clicked() {
                    self.keep_awake();
                }
            });
        } else {
            root.group(|ui| {
                ui.label(egui::RichText::new("本机不是被控端").strong());
                ui.separator();
                ui.label(
                    egui::RichText::new("没找到被控端配置（host.json），所以没有本机身份可展示。")
                        .weak(),
                );
                ui.label(
                    egui::RichText::new(
                        "装了被控端的那台机器上运行本程序时，这一页会显示它的设备 ID 与一次性验证码。",
                    )
                    .weak(),
                );
            });
        }
        if !self.status.is_empty() {
            root.add_space(6.0);
            root.label(egui::RichText::new(self.status.clone()).color(egui::Color32::from_rgb(60, 140, 220)));
        }
    }
}

