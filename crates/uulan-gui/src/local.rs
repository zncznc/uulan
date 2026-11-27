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
    /// (用户名, 口令位数)。**口令值绝不在这里** —— 这个结构每帧都被 clone。
    pub users: Vec<(String, usize)>,
    /// 这台机器认哪种凭据（`host.json` 的 `auth_os`）。
    /// **面板必须显示它**：否则坐在这儿的人会照着屏幕念临时码，而对面走的是「系统账户」那条路。
    pub auth_os: bool,
    /// PAM 第二因素是否开着（OS 模式必需；没开 = 系统账户一律登不进来）。
    pub pam_on: bool,
    /// OS 模式允许登录的系统账户名。空 = 谁都不许（PAM 那套纪律：没配好不等于开放）。
    pub pam_users: Vec<String>,
}

/// 「对方该拿什么连我」—— 坐在这台机器前的人要念出去的，就是这几行。
///
/// 为什么抽成纯函数：这段文字是**现场唯一的说明书**（当面协助时对面看不到这台机器的屏幕）。
/// 它错一个词，人就照错的念。所以它不吃任何 IO，判据在本机就能钉死。
#[derive(Debug, PartialEq, Eq)]
pub struct HowTo {
    /// 「系统账户（用户名 + 该账户的登录密码）」/「访问口令 / 一次性临时码」
    pub mode: String,
    /// 要念出去的行（按重要性排）。
    pub lines: Vec<String>,
    /// 配错了就明说（两种静默配错：OS 模式没开 PAM、OS 模式名单为空）。
    pub warn: Option<String>,
}

/// 由「配置 + 直连端口探活结果 + 本机 IP」算出该念的话。**没有 IO、没有全局状态。**
pub fn connect_hint(info: &Info, listen_open: Option<bool>, ips: &[String]) -> HowTo {
    let direct = match listen_open {
        Some(true) => format!(
            "直连：{}（对方用 --direct 或面板「直连地址」填这个）",
            direct_addrs(ips)
        ),
        Some(false) => "直连端口没在听（这台是以 --no-listen 起的）—— 只能经中继连".to_string(),
        None => format!("直连：{}（还没探过端口）", direct_addrs(ips)),
    };

    if info.auth_os {
        let mode = "系统账户（用户名 + 该账户在本机的登录密码）".to_string();
        let mut lines = vec![direct];
        if info.pam_users.is_empty() {
            lines.push("系统账户：（名单为空 —— 没有账户能登录）".to_string());
        } else {
            lines.push(format!(
                "系统账户：{}（密码就是各账户在本机的登录密码，不用另记口令）",
                info.pam_users.join(" / ")
            ));
        }
        let warn = if !info.pam_on {
            Some("这台配了 OS 模式但 PAM 没开：系统账户一律登不进来（临时码还能当备路用）。".to_string())
        } else if info.pam_users.is_empty() {
            Some("这台配了 OS 模式但允许名单为空：谁都不许登录。".to_string())
        } else {
            None
        };
        HowTo { mode, lines, warn }
    } else {
        let mode = "访问口令 / 一次性临时码（PAKE 口令通道）".to_string();
        let lines = vec![
            direct,
            "访问口令：见下面「永久口令」那一块；临时码：见上面那张一次性码。".to_string(),
        ];
        HowTo { mode, lines, warn: None }
    }
}

/// 直连端口到底在不在听 —— **探一次比猜一次强**。面板不读别处的配置（服务怎么起的它不知道），
/// 只信「127.0.0.1:21117 连得上」这个事实。默认监听就是这个端口（`--listen` 的默认值）。
fn probe_direct_port() -> Option<bool> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 21117));
    match TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(400)) {
        Ok(_) => Some(true),
        // 拒绝与超时都意味着「本机这个端口没有人在听」——对面板来说这两个是一回事。
        Err(_) => Some(false),
    }
}

/// 每个地址都带端口。
///
/// 只给最后一个加端口是**真机面板上抓到的错**（2026-11-20）：那台机器有三块网卡，
/// 屏幕上是 `192.168.223.15 / 192.168.231.1 / 172.16.96.1:21117` —— 照着念的人
/// 会把第一个（唯一能用的那个）念成没有端口的地址，对面填进去连不上。
fn direct_addrs(ips: &[String]) -> String {
    if ips.is_empty() {
        return "本机 IP:21117".to_string();
    }
    ips.iter()
        .map(|ip| format!("{ip}:21117"))
        .collect::<Vec<_>>()
        .join(" / ")
}

/// 虚拟网卡上的地址**对面连不到**，念出去只会让人白试一次。
///
/// 真机实测（2026-11-20）：`hostname -I` 给出 `192.168.223.15 192.168.231.1 172.16.96.1`，
/// 后两个是 VMware 的 vmnet1/vmnet8 —— 只有第一个是对方能路由到的。
/// 按接口名过滤比按网段猜可靠（网段是现场自己定的，名字是软件定的）。
///
/// Windows 上 `local_ips()` 恒为空（那一屏只在麒麟上有人看），所以这两个函数
/// 在 Windows 构建里只有测试用得到 —— 别让 dead_code 警告把零警告基线弄红。
#[cfg_attr(windows, allow(dead_code))]
fn is_virtual_iface(name: &str) -> bool {
    const PREFIXES: [&str; 8] = ["vmnet", "docker", "virbr", "br-", "veth", "tun", "tap", "lo"];
    PREFIXES.iter().any(|p| name.starts_with(p))
}

/// 从「(接口名, 地址)」里挑出该念出去的地址。纯函数，判据在本机就能钉死。
#[cfg_attr(windows, allow(dead_code))]
pub fn lan_ips_from(entries: &[(String, String)]) -> Vec<String> {
    let kept: Vec<String> = entries
        .iter()
        .filter(|(iface, _)| !is_virtual_iface(iface))
        .map(|(_, ip)| ip.clone())
        .collect();
    if kept.is_empty() {
        // 全被滤掉时宁可原样给出来：一块地址都不给，现场连试都没得试。
        return entries.iter().map(|(_, ip)| ip.clone()).collect();
    }
    kept
}

/// 本机对外地址（给对面填 IP 用）。Windows 上返回空 —— 那一屏只在麒麟上有人看。
#[cfg(not(windows))]
fn local_ips() -> Vec<String> {
    // `ip -4 -o addr show scope global` 带接口名，才能把 vmnet/docker 这类滤掉；
    // `hostname -I` 只有地址，滤不了（真机上就是这么把两块 VMware 网卡念出去的）。
    if let Ok(o) = std::process::Command::new("ip")
        .args(["-4", "-o", "addr", "show", "scope", "global"])
        .output()
    {
        if o.status.success() {
            let text = String::from_utf8_lossy(&o.stdout);
            let entries: Vec<(String, String)> = text
                .lines()
                .filter_map(|l| {
                    let mut it = l.split_whitespace();
                    let _idx = it.next()?; // "2:"
                    let iface = it.next()?.to_string();
                    let cidr = it.find(|t| t.contains('/'))?;
                    let ip = cidr.split('/').next()?.to_string();
                    Some((iface, ip))
                })
                .collect();
            let ips = lan_ips_from(&entries);
            if !ips.is_empty() {
                return ips;
            }
        }
    }
    // 没有 `ip` 命令（或它失败）时退回老路：至少给点东西。
    std::process::Command::new("hostname")
        .arg("-I")
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .filter(|s| !s.starts_with("127.") && !s.contains(':'))
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(windows)]
fn current_os_user() -> Option<String> {
    std::env::var("USERNAME").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

#[cfg(not(windows))]
fn current_os_user() -> Option<String> {
    std::env::var("USER").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

#[cfg(windows)]
fn local_ips() -> Vec<String> {
    use std::os::windows::process::CommandExt;
    let Ok(o) = std::process::Command::new("ipconfig")
        .creation_flags(0x0800_0000)
        .output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&o.stdout);
    let mut out = Vec::new();
    for line in text.lines() {
        if !(line.contains("IPv4") || line.contains("IP Address")) {
            continue;
        }
        if let Some((_, rhs)) = line.rsplit_once(':') {
            let ip = rhs.trim().trim_end_matches("(首选)").trim_end_matches("(Preferred)").trim();
            if !ip.is_empty()
                && !ip.starts_with("127.")
                && !ip.starts_with("169.254.")
                && ip.parse::<std::net::Ipv4Addr>().is_ok()
                && !out.iter().any(|x| x == ip)
            {
                out.push(ip.to_string());
            }
        }
    }
    out
}

pub struct LocalPanel {
    pub info: Option<Info>,
    pub reveal: bool,
    pub status: String,
    /// 「加一个人」输入的名字。
    new_user: String,
    /// 由本面板拉起的被控端进程。**关掉面板就下线**靠这个句柄实现 ——
    /// 否则"打开程序当被控端"会变成"退出程序后本机还在被人连"，那是个惊喜的反面。
    pub host_child: Option<std::process::Child>,
    /// 上线的时刻，用来区分"正在向中继注册"和"注册失败"。
    host_since: Option<std::time::Instant>,
    /// 直连端口探活结果（`reload()` 时探一次；每帧探等于每秒一次无谓连接）。
    listen_open: Option<bool>,
}


impl Drop for LocalPanel {
    /// 面板退出 ⇒ 自己拉起的被控端一起走。系统服务那种不归我们管（也不该管）。
    fn drop(&mut self) {
        if let Some(mut c) = self.host_child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl LocalPanel {
    /// 本机现在的被控端状态。区分三种，因为**下线语义完全不同**：
    /// systemd 常驻服务关掉面板也不会下线；面板拉起的才会。
    fn host_state(&mut self) -> (&'static str, bool) {
        // 自己拉起的进程先问它本人。
        if let Some(c) = self.host_child.as_mut() {
            match c.try_wait() {
                Ok(None) => return ("由本程序拉起（关掉面板即下线）", true),
                Ok(Some(_)) | Err(_) => {
                    self.host_child = None;
                    self.host_since = None;
                }
            }
        }
        // 再看系统里有没有别的 uulan-host 在跑（root 装的系统/用户服务就是这种）。
        let found = || -> bool {
            // 闭包里不能用 `?`（它得返回 bool），所以全程 map + unwrap_or：
            // 探不到进程就当"没在跑"，绝不为这个把面板弄崩。
            #[cfg(windows)]
            {
                std::process::Command::new("tasklist")
                    .args(["/FI", "IMAGENAME eq lanlink-host.exe", "/NH"])
                    .output()
                    .map(|o| String::from_utf8_lossy(&o.stdout).contains("lanlink-host.exe"))
                    .unwrap_or(false)
            }
            #[cfg(not(windows))]
            {
                std::process::Command::new("ps")
                    .args(["-eo", "args"])
                    .output()
                    .map(|o| {
                        String::from_utf8_lossy(&o.stdout)
                            .lines()
                            .any(|l| l.contains("lanlink-host") && !l.contains("ps -eo args"))
                    })
                    .unwrap_or(false)
            }
        };
        match found() {
            true => ("作为服务常驻（关掉本程序不会下线）", true),
            false => ("未上线", false),
        }
    }

    /// 让本机现在就能被连接：直接跑被控端，**不需要 root、不需要 install.sh**。
    /// 设备 ID 由中继首次注册时下发（config.rs 里写明），口令由被控端自己生成并落盘，
    /// 所以这里只是把它拉起来，不替它造任何身份。
    pub fn start_host(&mut self, relay: &str) {
        let Some(host) = Self::host_exe() else {
            self.status = "找不到 lanlink-host 二进制 —— 装了客户端还得带上被控端可执行文件才行。".into();
            return;
        };
        if self.host_state().1 {
            self.status = "本机已经在被控状态了，不需要再拉一个（两个被控端会抢同一个设备 ID）。".into();
            return;
        }
        // 首次启动被控端（尚无 host.json）：自动生成配置并为当前登录用户开启系统账户认证，
        // 保留临时码作为备路（与 Linux deploy/install.sh 口径完全一致）。
        if Self::config_path().is_none() {
            if let Some(cur_user) = current_os_user() {
                let mut init = Command::new(&host);
                init.arg("--show");
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    init.creation_flags(0x0800_0000);
                }
                let _ = init.output();
                let mut enable = Command::new(&host);
                enable.args(["--pam", "--pam-allow", &cur_user, "--auth", "os"]);
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    enable.creation_flags(0x0800_0000);
                }
                let _ = enable.output();
            }
        }
        let dir = host.parent().map(|p| p.to_path_buf()).unwrap_or_default();
        let _ = std::fs::create_dir_all(&dir);
        let log = dir.join("gui-host.log");
        let mut cmd = Command::new(&host);
        if !relay.trim().is_empty() {
            cmd.args(["--server", relay.trim()]);
        }
        // 输出必须接住：没有控制台的时候，它是唯一能看到被控端为什么起不来的地方。
        match std::fs::File::create(&log) {
            Ok(f) => match f.try_clone() {
                Ok(f2) => {
                    cmd.stdout(std::process::Stdio::from(f2)).stderr(std::process::Stdio::from(f));
                }
                Err(_) => {
                    cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::from(f));
                }
            },
            Err(_) => {
                cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        match cmd.spawn() {
            Ok(c) => {
                self.host_child = Some(c);
                self.host_since = Some(std::time::Instant::now());
                self.status = "已拉起被控端，正在向中继注册…（设备 ID 由中继下发，口令写在本机配置里）".to_string();
            }
            Err(e) => self.status = format!("起不来 {}：{e}", host.display()),
        }
    }

    pub fn stop_host(&mut self) {
        // 先把子进程**取出来**再动 self，否则 as_mut 的借用会罩住后面的赋值。
        if let Some(mut c) = self.host_child.take() {
            let _ = c.kill();
            let _ = c.wait();
            self.host_since = None;
            self.status = "本机已下线（由本程序拉起的被控端已停止）。".into();
            return;
        }
        self.status = if self.host_state().1 {
            "本机是被控端服务拉起来的，面板不越权去停它 —— 请用 systemctl。".into()
        } else {
            "本机本来就没在线。".into()
        };
    }
}

impl LocalPanel {
    pub fn new() -> Self {
        let mut p = Self {
            info: None,
            reveal: false,
            status: String::new(),
            new_user: String::new(),
            host_child: None,
            host_since: None,
            listen_open: None,
        };
        p.reload();
        p
    }

    /// 本机被控端配置文件：XDG 是正式位置，另外认一下安装前缀里可能存在的副本。
    fn config_path() -> Option<PathBuf> {
        let mut cands: Vec<PathBuf> = Vec::new();
        if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
            cands.push(PathBuf::from(x).join("lanlink").join("host.json"));
        }
        if let Some(h) = std::env::var_os("HOME") {
            let home = PathBuf::from(h);
            cands.push(home.join(".config").join("lanlink").join("host.json"));
            cands.push(home.join(".lanlink").join("host.json"));
        }
        if let Some(p) = std::env::var_os("USERPROFILE") {
            let home = PathBuf::from(p);
            cands.push(home.join(".config").join("lanlink").join("host.json"));
        }
        for p in ["/opt/lanlink/etc/host.json", "/etc/lanlink/host.json"] {
            cands.push(PathBuf::from(p));
        }
        cands.into_iter().find(|p| p.is_file())
    }

    /// 被控端二进制（生成临时码要用它，别自己造码 —— 造了中继也不认）。
    fn host_exe() -> Option<PathBuf> {
        let name = if cfg!(windows) { "lanlink-host.exe" } else { "lanlink-host" };
        let mut cands: Vec<PathBuf> = Vec::new();
        if let Ok(cur) = std::env::current_exe() {
            if let Some(dir) = cur.parent() {
                cands.push(dir.join(name));
                cands.push(dir.join("..").join(name));
            }
        }
        cands.push(PathBuf::from("/opt/lanlink/bin").join(name));
        if let Some(h) = std::env::var_os("HOME") {
            let home = PathBuf::from(h);
            cands.push(home.join(".local/lanlink/bin").join(name));
            cands.push(home.join(".lanlink/bin").join(name));
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
                    let users: Vec<(String, usize)> = v
                        .get("users")
                        .and_then(|x| x.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|u| {
                                    let n = u.get("name")?.as_str()?.to_string();
                                    let l = u.get("password")?.as_str()?.chars().count();
                                    Some((n, l))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let auth_os = v.get("auth_os").and_then(|x| x.as_bool()).unwrap_or(false);
                    let pam = v.get("pam");
                    let pam_on = pam
                        .and_then(|p| p.get("enabled"))
                        .and_then(|x| x.as_bool())
                        .unwrap_or(false);
                    let pam_users: Vec<String> = pam
                        .and_then(|p| p.get("allow_users"))
                        .and_then(|x| x.as_array())
                        .map(|a| a.iter().filter_map(|u| u.as_str().map(|s| s.to_string())).collect())
                        .unwrap_or_default();
                    self.status.clear();
                    self.listen_open = probe_direct_port();
                    Some(Info {
                        path: path.clone(),
                        id: g("device_id"),
                        name: g("name"),
                        server: g("server"),
                        password: g("password"),
                        temp,
                        users,
                        auth_os,
                        pam_on,
                        pam_users,
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
            self.status = "找不到 lanlink-host 二进制 —— 临时码必须由被控端自己生成，中继才认。".into();
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


    /// 借被控端自己的 CLI 改用户表 —— 面板**不自己造口令、不自己写配置**：
    /// 谁生成身份，谁才负责它的格式、强度校验与落盘（这里全在 `uulan-host` 里）。
    /// 返回被控端 stdout 的尾部（`--user-add` 会在这里打印一次口令，正是给人看的）。
    fn run_host_cli(&mut self, args: &[&str]) -> String {
        let Some(host) = Self::host_exe() else {
            self.status = "找不到 lanlink-host 二进制，管不了用户表。".into();
            return String::new();
        };
        let Some(path) = Self::config_path() else {
            self.status = "没有配置文件，管不了用户表。".into();
            return String::new();
        };
        let mut cmd = Command::new(&host);
        for a in args {
            cmd.arg(a);
        }
        cmd.arg("--config").arg(&path);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        match cmd.output() {
            Ok(o) if o.status.success() => {
                self.reload();
                String::from_utf8_lossy(&o.stdout).to_string()
            }
            Ok(o) => {
                let e = String::from_utf8_lossy(&o.stderr);
                self.status = format!(
                    "被控端拒绝执行（退出码 {:?}）：{}",
                    o.status.code(),
                    e.lines().rev().take(2).collect::<Vec<_>>().join(" ← ")
                );
                String::new()
            }
            Err(e) => {
                self.status = format!("跑不了 {}：{e}", host.display());
                String::new()
            }
        }
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

    pub fn ui(&mut self, root: &mut egui::Ui, relay: &str) {
        // Context 是 Arc 型的，克隆出来用；直接 root.ctx() 会把 root 借成只读，
        // 后面再 root.group(..) 就撞借用检查。
        let ctx = root.ctx().clone();
        // 第一组就是「本机能不能被连」——这是"装了客户端就能当被控端"的落点。
        let (state, online) = self.host_state();
        root.group(|ui| {
            ui.label(egui::RichText::new("本机被控状态").strong());
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(if online { "● 在线" } else { "○ 未上线" })
                    .color(if online { egui::Color32::from_rgb(60, 190, 90) } else { egui::Color32::GRAY })
                    .strong());
                ui.label(state);
            });
            ui.horizontal(|ui| {
                if !online {
                    let label = if relay.trim().is_empty() { "让本机成为被控端" } else { "让本机成为被控端（上线）" };
                    if ui.button(label).clicked() {
                        self.start_host(relay);
                    }
                } else if ui.button("下线").clicked() {
                    self.stop_host();
                }
                if self.host_child.is_none() && online {
                    ui.label(egui::RichText::new("下线请在本机执行：systemctl --user stop lanlink-host").small().weak());
                }
            });
            ui.label(egui::RichText::new(
                "不需要 root、不需要安装脚本：起来的就是本机的被控端。设备 ID 由中继首次注册时下发，\n\
                 访问口令由被控端自己生成并写入本机配置 —— 面板不替机器编造身份。",
            )
            .small()
            .weak());
        });
        root.add_space(6.0);

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
                ui.label(egui::RichText::new("对方怎么连我（当面念这几行）").strong());
                ui.separator();
                let how = connect_hint(&info, self.listen_open, &local_ips());
                ui.label(egui::RichText::new(format!("认证方式：{}", how.mode)).strong());
                for l in &how.lines {
                    ui.label(egui::RichText::new(l.clone()).monospace());
                }
                if let Some(w) = &how.warn {
                    // 配错必须是红的，不能和说明文字一个颜色 —— 现场一眼要能分出「配置错了」。
                    ui.label(
                        egui::RichText::new(w.clone())
                            .color(egui::Color32::from_rgb(220, 120, 40))
                            .strong(),
                    );
                }
                if (!info.auth_os || !info.pam_on || info.pam_users.is_empty())
                    && current_os_user().is_some()
                {
                    if let Some(u) = current_os_user() {
                        if ui.button(format!("启用系统账户登录（仅允许 {u}）")).clicked() {
                            let _ = self.run_host_cli(&["--pam", "--pam-allow", &u, "--auth", "os"]);
                            if self.host_child.is_some() {
                                self.stop_host();
                                self.start_host(relay);
                            }
                            self.status = format!("已开启系统账户登录（允许 {u} 使用本机登录密码连入）。");
                        }
                    }
                }
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
                ui.label(egui::RichText::new("额外登录身份（每人一份独立口令）").strong());
                ui.separator();
                if info.users.is_empty() {
                    ui.label(egui::RichText::new("还没有额外用户 —— 现在只有主人口令与临时码能连。").weak());
                } else {
                    for (n, l) in info.users.clone() {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(&n).monospace());
                            ui.label(egui::RichText::new(format!("口令 {l} 位")).weak());
                            if ui.button("吊销").clicked() {
                                // 吊销走被控端自己的 CLI：身份归它生成、归它落盘、归它校验格式。
                                let out = self.run_host_cli(&["--user-del", &n]);
                                if !out.is_empty() {
                                    self.status = format!("已吊销 {n}。他的口令下一次就通不过。");
                                }
                            }
                        });
                    }
                }
                ui.horizontal(|ui| {
                    ui.label("新用户名");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.new_user)
                            .hint_text("例 ops（别用空格和逗号）")
                            .desired_width(150.0),
                    );
                    if ui.button("加一个人").clicked() {
                        let name = self.new_user.trim().to_string();
                        if name.is_empty() {
                            self.status = "先起个名字。".into();
                        } else {
                            let out = self.run_host_cli(&["--user-add", &name]);
                            if !out.is_empty() {
                                // 被控端只在 stdout 打印这一次口令，抄下来就没了 ——
                                // 所以这里原样转述，并当场提醒别往聊天窗口里贴。
                                self.status =
                                    format!("{}（只显示这一次，关掉界面就再也看不到）", out.trim());
                            }
                            self.new_user.clear();
                        }
                    }
                });
                ui.label(
                    egui::RichText::new(
                        "身份名不在线上送：被控端看命中哪一份凭据就知道是谁，报假名没用。\n\
                         吊销当场生效；某人泄露只换他那一行，别人不受影响。",
                    )
                    .small()
                    .weak(),
                );
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
                    egui::RichText::new("点上面「让本机成为被控端」，注册成功后这里就会出现设备 ID 与验证码。")
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

#[cfg(test)]
mod hint_tests {
    use super::*;

    fn ips() -> Vec<String> {
        vec!["192.168.223.15".to_string()]
    }

    fn base() -> Info {
        Info {
            id: "805747189".into(),
            name: "麒麟真机".into(),
            server: "192.168.223.15:21116".into(),
            password: "a".repeat(12),
            ..Default::default()
        }
    }

    #[test]
    fn pake_mode_points_at_the_password_and_the_temp_code() {
        let mut i = base();
        i.temp = "123456".into();
        let h = connect_hint(&i, Some(true), &ips());
        assert!(h.mode.contains("访问口令"), "{}", h.mode);
        assert!(h.lines.iter().any(|l| l.contains("192.168.223.15:21117")), "{:?}", h.lines);
        assert!(h.lines.iter().any(|l| l.contains("访问口令")), "{:?}", h.lines);
        assert_eq!(h.warn, None, "正常形态不许有告警");
    }

    #[test]
    fn os_mode_names_the_accounts_and_says_the_password_is_the_login_one() {
        let mut i = base();
        i.auth_os = true;
        i.pam_on = true;
        i.pam_users = vec!["mis".into(), "ops".into()];
        let h = connect_hint(&i, Some(true), &ips());
        assert!(h.mode.contains("系统账户"), "{}", h.mode);
        let accounts = h.lines.iter().find(|l| l.contains("系统账户")).expect("要列出账户");
        assert!(accounts.contains("mis") && accounts.contains("ops"), "{accounts}");
        assert!(accounts.contains("登录密码"), "要说明密码就是系统登录密码：{accounts}");
        assert_eq!(h.warn, None);
    }

    #[test]
    fn os_mode_without_pam_warns_that_nothing_can_log_in() {
        let mut i = base();
        i.auth_os = true;
        i.pam_on = false;
        i.pam_users = vec!["mis".into()];
        let h = connect_hint(&i, Some(true), &ips());
        let w = h.warn.expect("必须告警");
        assert!(w.contains("PAM 没开"), "{w}");
    }

    #[test]
    fn os_mode_with_an_empty_allow_list_warns_that_nobody_may_log_in() {
        let mut i = base();
        i.auth_os = true;
        i.pam_on = true;
        let h = connect_hint(&i, Some(true), &ips());
        let w = h.warn.expect("必须告警");
        assert!(w.contains("名单为空"), "{w}");
        assert!(h.lines.iter().any(|l| l.contains("名单为空")), "{:?}", h.lines);
    }

    #[test]
    fn a_closed_direct_port_is_never_sold_as_available() {
        let i = base();
        let h = connect_hint(&i, Some(false), &ips());
        let direct = h.lines.first().expect("第一行就是直连");
        assert!(direct.contains("没在听"), "{direct}");
        assert!(!direct.contains("21117"), "没在听就不该给出一个像个能用的地址：{direct}");
        // 还没有 IP 时也不能编一个地址出来。
        let h2 = connect_hint(&i, Some(true), &[]);
        assert!(h2.lines[0].contains("本机 IP"), "{:?}", h2.lines);
    }

    /// 真机面板上抓到的错（2026-11-20）：三个地址只有最后一个带端口，
    /// 照着念的人会把第一个（唯一能用的那个）念成没有端口的地址。
    #[test]
    fn every_direct_address_carries_the_port() {
        let i = base();
        let three = vec![
            "192.168.223.15".to_string(),
            "192.168.231.1".to_string(),
            "172.16.96.1".to_string(),
        ];
        let h = connect_hint(&i, Some(true), &three);
        let direct = h.lines.first().expect("第一行就是直连");
        for ip in &three {
            assert!(
                direct.contains(&format!("{ip}:21117")),
                "每个地址都要带端口，缺 {ip}:21117 —— {direct}"
            );
        }
    }

    /// 虚拟网卡（VMware / docker / libvirt）上的地址对面连不到，不许念出去。
    #[test]
    fn virtual_interfaces_are_not_read_out() {
        let entries = vec![
            ("eno1".to_string(), "192.168.223.15".to_string()),
            ("vmnet1".to_string(), "192.168.231.1".to_string()),
            ("vmnet8".to_string(), "172.16.96.1".to_string()),
            ("docker0".to_string(), "172.17.0.1".to_string()),
        ];
        assert_eq!(lan_ips_from(&entries), vec!["192.168.223.15".to_string()]);
    }

    /// 全被滤掉时宁可原样给出来 —— 一块地址都不给，现场连试都没得试。
    #[test]
    fn all_virtual_falls_back_to_what_we_have() {
        let entries = vec![("vmnet8".to_string(), "172.16.96.1".to_string())];
        assert_eq!(lan_ips_from(&entries), vec!["172.16.96.1".to_string()]);
    }
}
