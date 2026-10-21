//! 被控端配置：设备身份、访问密码、中继地址、采集源。
//!
//! 配置落在 `~/.config/uulan/host.json`（Unix 下 0600）。里面**只有本机自己的**东西：
//! 访问密码、设备 ID、中继地址。中继服务器不持有其中任何一项，
//! 所以即使中继被拿下，也拿不到能连上这台机器的凭据。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use uulan_protocol::msg::Quality;

use crate::capture::CaptureInput;

/// 默认中继端口，与 `uulan-protocol::DEFAULT_PORT` 保持一致。
pub use uulan_protocol::DEFAULT_PORT;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostConfig {
    /// 中继服务器地址（`ip:port`）。
    #[serde(default = "default_server")]
    pub server: String,

    /// 本机显示名，会出现在控制端的设备列表里。
    #[serde(default = "default_name")]
    pub name: String,

    /// 设备 ID。首次向中继注册成功后由中继下发，之后一直复用。
    #[serde(default)]
    pub device_id: Option<String>,

    /// 永久访问密码。控制端要靠它才能连上。
    #[serde(default = "gen_password")]
    pub password: String,

    /// 一次性临时验证码（可选）。用掉一次即失效。
    #[serde(default)]
    pub temp_code: Option<String>,

    /// 是否允许他人用「设备 ID + 验证码」发起协助（对应 UU 的远程协助开关）。
    #[serde(default)]
    pub assist_enabled: bool,

    /// 采集源。生产环境是 `x11`，自测/演示可以是 `lavfi`。
    #[serde(default)]
    pub capture: CaptureInput,

    /// 是否把光标画进视频流。默认否 —— 光标由控制端本地绘制，移动零延迟、无双光标。
    #[serde(default)]
    pub draw_mouse: bool,

    /// 视频编码质量档（kbps 或 auto）。
    #[serde(default = "default_bitrate")]
    pub max_bitrate_kbps: u32,

    /// 画面档位。`Custom` 时把 `max_bitrate_kbps` 当作码率上限；其余档位用
    /// `uulan_protocol::msg::quality_preset` 里的换算表。
    #[serde(default = "default_quality")]
    pub quality: Quality,

    /// 帧率。
    #[serde(default = "default_fps")]
    pub fps: u32,

    /// 采集区域宽度。`0` 表示启动时用 `xdpyinfo` 探测，探测不到就用 1920。
    #[serde(default)]
    pub screen_width: u32,

    /// 采集区域高度。`0` 表示启动时探测，探测不到就用 1080。
    #[serde(default)]
    pub screen_height: u32,

    /// 文件传输是否允许客户端用**绝对路径**浏览整个磁盘（默认允许）。
    ///
    /// 远控产品（UU / 向日葵）的用户预期就是能翻全盘，所以默认开着；
    /// 只想让人看家目录就设成 `false`。注意 `~/` 开头的路径任何时候都出不了家目录。
    #[serde(default = "default_true")]
    pub fs_allow_absolute: bool,

    /// 端口映射的目标白名单。空列表 = 只允许本机回环（`127.0.0.0/8`、`::1`）。
    ///
    /// 支持 `*`（不限制）、`10.0.0.0/8`、`192.168.1.5`（单机）。**这是安全边界**：
    /// 放开到网段就等于把这台机器变成内网跳板，只给信任的控制端开。
    #[serde(default)]
    pub portmap_allow: Vec<String>,
}

fn default_true() -> bool {
    true
}

fn default_server() -> String {
    format!("127.0.0.1:{DEFAULT_PORT}")
}
fn default_name() -> String {
    hostname_fallback()
}
fn default_fps() -> u32 {
    30
}
fn default_bitrate() -> u32 {
    0 // 0 = 恒定质量
}

fn default_quality() -> Quality {
    Quality::Auto
}

fn hostname_fallback() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "uulan-host".to_string())
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            server: default_server(),
            name: default_name(),
            device_id: None,
            password: gen_password(),
            temp_code: None,
            assist_enabled: false,
            capture: CaptureInput::default(),
            draw_mouse: false,
            max_bitrate_kbps: default_bitrate(),
            quality: default_quality(),
            fps: default_fps(),
            screen_width: 0,
            screen_height: 0,
            fs_allow_absolute: true,
            portmap_allow: Vec::new(),
        }
    }
}

impl HostConfig {
    /// 默认配置路径：`$UULAN_CONFIG` 优先，否则 `~/.config/uulan/host.json`。
    pub fn default_path() -> PathBuf {
        if let Some(p) = std::env::var_os("UULAN_CONFIG") {
            return PathBuf::from(p);
        }
        config_home().join("uulan").join("host.json")
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)?;
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        Ok(serde_json::from_str(&text)?)
    }

    /// 读配置；文件不存在就用默认值并把新配置写下去（含新生成的密码）。
    pub fn load_or_create(path: &Path) -> Result<Self, ConfigError> {
        if path.exists() {
            let cfg = Self::load(path)?;
            cfg.check()?;
            return Ok(cfg);
        }
        let cfg = Self::default();
        cfg.save(path)?;
        Ok(cfg)
    }

    pub fn check(&self) -> Result<(), ConfigError> {
        validate_password(&self.password).map_err(ConfigError::BadPassword)?;
        if let Some(t) = &self.temp_code {
            validate_password(t).map_err(ConfigError::BadPassword)?;
        }
        if self.server.trim().is_empty() {
            return Err(ConfigError::Empty("server".into()));
        }
        if !(1..=144).contains(&self.fps) {
            return Err(ConfigError::Empty(format!("fps={} 超出 1..=144", self.fps)));
        }
        Ok(())
    }

    /// 保存并把文件权限收紧到 0600（Unix）。
    ///
    /// 这很重要：文件里是访问这台机器的密码，同机其他用户不该读到。
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let text = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &text)?;
        restrict_permissions(&tmp)?;
        // Windows 上裸的 rename 会在目标不存在时偶发失败：MoveFile 撞上杀毒软件 /
        // 索引器对刚写完的临时文件的瞬时句柄，报 os error 5「拒绝访问」。
        // 实测：首次运行（配置文件还不存在）100% 失败，第二次就好了 —— 这种
        // 「第一次装好跑不起来、再跑一次就行」是最伤信任的 bug。
        // 所以失败要重试；实在换不过去就退回直接写目标文件（另一次写入而已）。
        let mut last: Option<std::io::Error> = None;
        for attempt in 0..5u32 {
            match std::fs::rename(&tmp, path) {
                Ok(()) => {
                    last = None;
                    break;
                }
                Err(e) => {
                    last = Some(e);
                    std::thread::sleep(std::time::Duration::from_millis(20 * (attempt as u64 + 1)));
                }
            }
        }
        if let Some(e) = last {
            // 退路：直接把内容写到目标文件，再收拾掉临时文件。
            std::fs::write(path, &text).map_err(|_| ConfigError::Io(e))?;
            let _ = std::fs::remove_file(&tmp);
        }
        restrict_permissions(path)?;
        Ok(())
    }

    /// 换一个新的一次性临时验证码。
    pub fn refresh_temp_code(&mut self) -> String {
        let c = gen_password();
        self.temp_code = Some(c.clone());
        c
    }
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut p = std::fs::metadata(path)?.permissions();
    p.set_mode(0o600);
    std::fs::set_permissions(path, p)
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> std::io::Result<()> {
    // Windows 上没有等价的简单做法，交给 ACL 继承。文档里说明这一点。
    //
    // 注意：这里必须**不报错**。文件里是访问密码，但收紧 ACL 要动 `icacls` /
    // Win32 安全描述符，一旦失败就会把「配置写不进去」牵连进来 —— 宁可权限宽一点，
    // 也不能让被控端起不来。真正的收紧交给部署脚本（DEPLOY-KYLIN.md 里是 0600）。
    Ok(())
}

#[cfg(all(test, not(unix)))]
mod windows_save_tests {
    use super::*;

    /// 每个测试一个独立目录；**环境写不了文件时返回 None，让调用方跳过**。
    ///
    /// 这台开发机上，位于 `D:/default-workspace/` 下的可执行文件无法写任何文件，
    /// 换到 `D:/` 下立刻正常 —— 那是机器级的执行策略，不是本代码的问题。
    /// 遇到这种情况要明确跳过并说明，而不是红一片让人误��为代码坏了。
    fn test_dir(tag: &str) -> Option<std::path::PathBuf> {
        let dir = std::env::temp_dir().join(format!("uulan-{tag}-{}", std::process::id()));
        if !probe_writable(&dir) {
            eprintln!(
                "跳过：本环境不允许可执行文件写文件（{} 建不出来）。\
                 这是机器级限制（exe 位于 D:/default-workspace 下时会出现），\
                 与代码无关；请在真麒麟上验证写盘逻辑。",
                dir.display()
            );
            return None;
        }
        Some(dir)
    }

    /// 真的建个目录并写个文件试试 —— 只 create_dir_all 不够：目录可能已经存在而写入仍被拒。
    fn probe_writable(dir: &std::path::Path) -> bool {
        if std::fs::create_dir_all(dir).is_err() {
            return false;
        }
        let f = dir.join(".probe");
        let ok = std::fs::write(&f, b"x").is_ok();
        let _ = std::fs::remove_file(&f);
        ok
    }

    /// 回归：目标文件**不存在**时也一定要保存成功。
    ///
    /// 这正是首次运行的情形，也是原实现在 Windows 上必挂的那种情形
    /// （rename 撞上瞬时文件锁 → os error 5）。
    #[test]
    fn save_creates_a_brand_new_file() {
        let Some(dir) = test_dir("cfg-new") else { return };
        let p = dir.join(format!("new-{}.json", rand::random::<u64>()));
        assert!(!p.exists(), "前提：文件必须还不存在");

        let cfg = HostConfig::default();
        cfg.save(&p).expect("首次保存必须成功");
        assert!(p.exists(), "保存后文件应存在");

        let back = HostConfig::load(&p).expect("读回来");
        assert_eq!(back.password, cfg.password, "密码应原样往返");
        assert_eq!(back.server, cfg.server);

        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_dir(&dir);
    }

    /// 回归：连续保存多次不出错，也不留下临时文件。
    #[test]
    fn save_is_repeatable_and_leaves_no_tmp() {
        let Some(dir) = test_dir("cfg-repeat") else { return };
        let p = dir.join("host.json");

        let mut cfg = HostConfig::default();
        for i in 0..3 {
            cfg.name = format!("第{i}次");
            cfg.save(&p).expect("重复保存必须成功");
        }
        let back = HostConfig::load(&p).unwrap();
        assert_eq!(back.name, "第2次", "最后一次写入应生效");

        let tmp = p.with_extension("json.tmp");
        assert!(!tmp.exists(), "不应残留临时文件 {}", tmp.display());

        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_dir(&dir);
    }
}

fn config_home() -> PathBuf {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(x);
    }
    if let Some(h) = std::env::var_os("HOME") {
        return PathBuf::from(h).join(".config");
    }
    if let Some(p) = std::env::var_os("USERPROFILE") {
        return PathBuf::from(p).join(".config");
    }
    PathBuf::from(".")
}

/// 生成访问密码：12 位，含大小写字母与数字。
///
/// 刻意排除容易看错/听错的字符（`0O1lI`），因为这种密码经常是要**口头念给**
/// 远端的同事、或者手抄的。
///
/// **必须保证至少有一个字母和一个数字**：这是 [`validate_password`] 的硬性要求，
/// 而本函数生成的就是首次启动时的默认密码。早先纯随机从池子里取，
/// 会出现 `QzzyBJnqJRrP` 这种全字母的结果，于是配置文件一写下去就通不过
/// [`HostConfig::check`]，被控端下次启动直接起不来 —— 自己生成的密码过不了自己的校验。
pub fn gen_password() -> String {
    use rand::seq::SliceRandom;
    use rand::Rng;

    const LETTERS: &[u8] = b"abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ";
    const DIGITS: &[u8] = b"23456789";
    const POOL: &[u8] = b"abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    const LEN: usize = 12;

    let mut rng = rand::thread_rng();
    let mut out: Vec<u8> = Vec::with_capacity(LEN);
    out.push(LETTERS[rng.gen_range(0..LETTERS.len())]);
    out.push(DIGITS[rng.gen_range(0..DIGITS.len())]);
    while out.len() < LEN {
        out.push(POOL[rng.gen_range(0..POOL.len())]);
    }
    // 打乱，免得「字母总在第 1 位、数字总在第 2 位」暴露结构。
    out.shuffle(&mut rng);
    String::from_utf8(out).expect("字符池全是 ASCII")
}

/// 校验密码强度：8–16 位，且必须同时含字母和数字。
///
/// 这条规则刻意与官方 UU 远程的自定义验证码一致 —— 用户已经习惯了那一套，
/// 也免得出现「全是数字」这种能被穷举的密码。
pub fn validate_password(p: &str) -> Result<(), String> {
    let n = p.chars().count();
    if !(8..=16).contains(&n) {
        return Err(format!("长度必须是 8–16 位，当前 {n} 位"));
    }
    if !p.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err("只能包含大小写字母和数字".into());
    }
    if !p.chars().any(|c| c.is_ascii_alphabetic()) {
        return Err("必须至少含一个字母".into());
    }
    if !p.chars().any(|c| c.is_ascii_digit()) {
        return Err("必须至少含一个数字".into());
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("配置不是合法 JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("配置项不合法: {0}")]
    BadPassword(String),
    #[error("配置项为空或越界: {0}")]
    Empty(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("testtmp");
        std::fs::create_dir_all(&p).unwrap();
        p.push(format!("hostcfg-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn generated_password_always_valid_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..500 {
            let p = gen_password();
            validate_password(&p).unwrap_or_else(|e| panic!("生成的密码不合法: {p} -> {e}"));
            assert_eq!(p.chars().count(), 12);
            // 容易看错的字符必须被排除
            for bad in ['0', 'O', '1', 'l', 'I'] {
                assert!(!p.contains(bad), "密码 {p} 含易混淆字符 {bad}");
            }
            seen.insert(p);
        }
        assert_eq!(seen.len(), 500, "生成了重复密码");
    }

    #[test]
    fn password_rules_match_the_documented_contract() {
        assert!(validate_password("aB3dE5fG").is_ok());
        assert!(validate_password("abcdefgh").is_err(), "全字母应被拒");
        assert!(validate_password("12345678").is_err(), "全数字应被拒");
        assert!(validate_password("aB3dE5f").is_err(), "7 位太短");
        assert!(validate_password("aB3dE5fG7hJ9kL0mX").is_err(), "17 位太长");
        assert!(validate_password("aB3dE5f!").is_err(), "含符号应被拒");
        assert!(validate_password("aB3dE5f中").is_err(), "含中文应被拒");
        // 边界：恰好 8 和 16 位
        assert!(validate_password("aB3dE5fG").is_ok(), "8 位应通过");
        assert!(validate_password("aB3dE5fG7hJ9kL0m").is_ok(), "16 位应通过");
    }

    #[test]
    fn generated_password_always_has_both_letter_and_digit() {
        // 回归：早先纯随机取样会生成全字母密码，而那是**首次启动的默认密码**，
        // 于是配置文件写下去就通不过 check()，被控端下次直接起不来。
        for _ in 0..2000 {
            let p = gen_password();
            assert!(
                p.chars().any(|c| c.is_ascii_digit()),
                "生成的密码 {p} 没有数字"
            );
            assert!(
                p.chars().any(|c| c.is_ascii_alphabetic()),
                "生成的密码 {p} 没有字母"
            );
            validate_password(&p).unwrap_or_else(|e| panic!("{p} 不合法: {e}"));
        }
    }

    #[test]
    fn save_then_load_roundtrip() {
        let p = tmp("rt.json");
        let mut c = HostConfig::default();
        c.device_id = Some("123456789".into());
        c.name = "麒麟-01".into();
        c.fps = 60;
        c.max_bitrate_kbps = 8000;
        c.save(&p).unwrap();

        let back = HostConfig::load(&p).unwrap();
        assert_eq!(back.device_id.as_deref(), Some("123456789"));
        assert_eq!(back.name, "麒麟-01");
        assert_eq!(back.fps, 60);
        assert_eq!(back.max_bitrate_kbps, 8000);
        assert_eq!(back.password, c.password, "密码必须原样带回");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn load_or_create_makes_a_file_with_a_password() {
        let p = tmp("create.json");
        let c = HostConfig::load_or_create(&p).unwrap();
        assert!(p.exists(), "首次运行必须把配置写下来，否则密码每次重启都变");
        validate_password(&c.password).unwrap();
        let again = HostConfig::load_or_create(&p).unwrap();
        assert_eq!(again.password, c.password, "第二次读到的密码必须一致");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn empty_file_is_treated_as_default() {
        let p = tmp("empty.json");
        std::fs::write(&p, "   ").unwrap();
        let c = HostConfig::load(&p).unwrap();
        validate_password(&c.password).unwrap();
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn corrupt_config_is_an_error_not_a_silent_reset() {
        let p = tmp("corrupt.json");
        std::fs::write(&p, "{ not json").unwrap();
        assert!(HostConfig::load(&p).is_err(), "配置损坏必须报错，不能悄悄换掉密码");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn check_rejects_bad_values() {
        let mut c = HostConfig::default();
        c.fps = 0;
        assert!(c.check().is_err());
        c.fps = 200;
        assert!(c.check().is_err());
        c.fps = 30;
        c.password = "short".into();
        assert!(c.check().is_err());
        c.password = gen_password();
        assert!(c.check().is_ok());
        c.server = "  ".into();
        assert!(c.check().is_err());
    }

    #[test]
    fn refresh_temp_code_changes_it() {
        let mut c = HostConfig::default();
        let a = c.refresh_temp_code();
        let b = c.refresh_temp_code();
        assert_ne!(a, b);
        assert_eq!(c.temp_code.as_deref(), Some(b.as_str()));
        validate_password(&a).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn saved_config_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let p = tmp("perm.json");
        HostConfig::default().save(&p).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "配置文件里有访问密码，必须是 0600，实际 {mode:o}");
        let _ = std::fs::remove_file(&p);
    }
}
