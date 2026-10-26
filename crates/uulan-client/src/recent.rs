//! 控制端记住「上次连的是哪台」。
//!
//! 现场最烦的重复劳动不是连不上，而是那个 9 位设备 ID 每次都要从工单、便签、聊天记录里
//! 再抄一遍。`--last` 把上一次**鉴权成功**的「中继地址 + 设备 ID」存在本机，
//! 下次只写 `uulan-client --last --password-file ~/.uulan-pw` 就能连回去。
//!
//! 三条硬规矩：
//!
//! 1. **口令绝不落盘。** 这里只有地址、设备 ID、对端显示名和被连上的时间；
//!    口令继续走 `--password-file` / `UULAN_PASSWORD`（`saved_file_carries_no_credential`
//!    那条测试盯着序列化后的文本，谁往里加凭据字段它立刻红）。
//! 2. **只在鉴权成功之后写。** 「TCP 连上了」不算 —— 设备 ID 打错、口令打错时也会连上中继，
//!    把那种参数记下来，下次 `--last` 就是记下一个错答案。
//! 3. **坏了就当没有。** 文件缺了、JSON 坏了、目录写不进去，一律降级成「没记住」，
//!    绝不让一个缓存文件挡住连机器这件正事。

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// 覆盖配置文件路径的环境变量（测试与「多套现场配置」用）。
pub const ENV_PATH: &str = "UULAN_CLIENT_CONFIG";

/// 本机记住的上一次连接。
///
/// 字段故意很少：多一个字段就多一个「现场两台机器配置文件不一样」的理由。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recent {
    /// 中继地址（`host:port`）。
    pub server: String,
    /// 被控端设备 ID（9 位数字）。
    pub device: String,
    /// 被控端上次自报的显示名。只用来把那行提示说清楚，不参与连接。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host_name: String,
    /// 上次鉴权成功的 Unix 秒。0 = 老文件里没有这一项，不是「1970 年」。
    #[serde(default)]
    pub saved_unix: u64,
}

/// 配置文件路径：`$UULAN_CLIENT_CONFIG` 优先，否则 `~/.config/uulan/client.json`。
///
/// 与 `uulan-host` 的 `host.json` 同一套约定（`XDG_CONFIG_HOME` → `$HOME/.config` →
/// `%USERPROFILE%\.config`）。控制端和被控端在一台机器上时，现场只需要记一个目录。
pub fn default_path() -> PathBuf {
    if let Some(p) = std::env::var_os(ENV_PATH) {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    config_home().join("uulan").join("client.json")
}

fn config_home() -> PathBuf {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        if !x.is_empty() {
            return PathBuf::from(x);
        }
    }
    if let Some(h) = std::env::var_os("HOME") {
        if !h.is_empty() {
            return PathBuf::from(h).join(".config");
        }
    }
    if let Some(p) = std::env::var_os("USERPROFILE") {
        if !p.is_empty() {
            return PathBuf::from(p).join(".config");
        }
    }
    PathBuf::from(".")
}

/// 读一条记录。任何问题（文件不在、JSON 坏了、没权限）都返回 `None`。
///
/// **不返回 `Result`** 是刻意的：调用点要的是「有没有记住」，而不是「为什么没记住」。
/// 但坏文件会在 stderr 留一句，免得「明明存过却不生效」变成一个查不出来的悬案。
pub fn load_at(path: &Path) -> Option<Recent> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return None, // 第一次用，正常路径
    };
    match serde_json::from_str::<Recent>(&raw) {
        Ok(mut r) => {
            // 空记录等于没有记录：`--last` 拿一个空设备 ID 去连，会把「没记住」变成
            // 「中继说设备 ID 不合法」，两种错法现场完全不是一回事。
            if r.device.trim().is_empty() || r.server.trim().is_empty() {
                return None;
            }
            r.device = r.device.trim().to_string();
            r.server = r.server.trim().to_string();
            Some(r)
        }
        Err(e) => {
            eprintln!("（配置 {} 读不出来，当作没记住：{e}）", path.display());
            None
        }
    }
}

/// 写一条记录：先写 `*.tmp` 再 `rename`，中途断电不会留下半截 JSON。
///
/// Unix 下权限压到 0600 —— 里面虽然没有口令，但「这台控制端连哪台机器」在隔离网里
/// 也是值得少说一句的信息，而 0600 的成本是零。
pub fn save_to(path: &Path, r: &Recent) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_string_pretty(r).map_err(io::Error::other)?;
    std::fs::write(&tmp, body.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// 记下这次连上的（中继, 设备）。返回写盘结果，调用方自己决定要不要吵。
pub fn remember_at(path: &Path, server: &str, device: &str, host_name: &str) -> io::Result<()> {
    save_to(
        path,
        &Recent {
            server: server.trim().to_string(),
            device: device.trim().to_string(),
            host_name: host_name.trim().to_string(),
            saved_unix: now_unix(),
        },
    )
}

/// 忘掉：返回「本来有没有」。文件不在不是错误 —— 现场会连点两次。
pub fn forget_at(path: &Path) -> io::Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// `--last` 的补空缺规则：**只补命令行没给的**。
///
/// 命令行优先不是礼貌问题：`--last --device 111111111` 必须连 111111111。
/// 反过来的实现（记住的覆盖命令行）会让「临时连另一台」变成不可能，
/// 而且现象是「我明明写了 ID，它连的是另一台」——最难信任的那类 bug。
pub fn apply(r: &Recent, server: &str, server_explicit: bool, device: &str) -> (String, String) {
    let server = if server_explicit { server.to_string() } else { r.server.clone() };
    let device = if device.trim().is_empty() { r.device.clone() } else { device.trim().to_string() };
    (server, device)
}

/// 「上次连的是哪台」那行提示：设备 ID、对端名字、多久以前。
pub fn describe(r: &Recent, now: u64) -> String {
    let id = if r.device.is_empty() { "（没有记录）" } else { r.device.as_str() };
    let name = if r.host_name.is_empty() { String::new() } else { format!("「{}」", r.host_name) };
    format!("{id}{name}，{}", ago(now, r.saved_unix))
}

/// 把「多久以前」说成人话。
///
/// 单调时间不做，只用 Unix 秒的差：这只用于一句提示，时钟往回跳一格最多说成「刚刚」。
pub fn ago(now: u64, then: u64) -> String {
    if then == 0 || now <= then {
        return "时间未知".to_string();
    }
    let d = now - then;
    match d {
        0..=59 => "刚刚".to_string(),
        60..=3599 => format!("{} 分钟前", d / 60),
        3600..=86_399 => format!("{} 小时前", d / 3600),
        _ => format!("{} 天前", d / 86_400),
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 写盘测试要先用真写一个文件探一次可写性 —— 这台开发机上 exe 位于
    /// `D:\default-workspace` 下时**任何写盘都被拒**（见 skill「坑 2」）。
    /// 硬断言会让环境一变就是一片假红，把真问题埋掉。
    fn test_dir(tag: &str) -> Option<PathBuf> {
        let dir = std::env::temp_dir().join(format!("uulan-recent-{tag}-{}", std::process::id()));
        if std::fs::create_dir_all(&dir).is_err() {
            eprintln!("跳过：建不出测试目录 {}", dir.display());
            return None;
        }
        let probe = dir.join(".probe");
        if std::fs::write(&probe, b"x").is_err() {
            eprintln!("跳过：本环境不允许写文件（{}）—— 与代码无关", dir.display());
            return None;
        }
        let _ = std::fs::remove_file(&probe);
        Some(dir)
    }

    fn sample() -> Recent {
        Recent {
            server: "192.168.223.15:21116".into(),
            device: "805747189".into(),
            host_name: "麒麟真机".into(),
            saved_unix: 1_700_000_000,
        }
    }

    #[test]
    fn roundtrip_saves_and_loads() {
        let Some(dir) = test_dir("roundtrip") else { return };
        let p = dir.join("client.json");
        let _ = std::fs::remove_file(&p);
        assert!(load_at(&p).is_none(), "文件不在时必须是 None，不是报错");
        save_to(&p, &sample()).expect("保存");
        assert_eq!(load_at(&p).unwrap(), sample());
        assert!(!p.with_extension("json.tmp").exists(), "不该留下临时文件");
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_dir(&dir);
    }

    /// 老版本文件（没有 host_name / saved_unix）也必须能读 —— 否则每次升级
    /// 都要现场删一次缓存，而现场不会知道要删。
    #[test]
    fn older_file_without_new_fields_still_loads() {
        let Some(dir) = test_dir("older") else { return };
        let p = dir.join("client.json");
        std::fs::write(&p, r#"{"server":"10.0.0.1:21116","device":"123456789"}"#).unwrap();
        let r = load_at(&p).expect("老文件要能读");
        assert_eq!(r.device, "123456789");
        assert!(r.host_name.is_empty());
        assert_eq!(r.saved_unix, 0);
        assert_eq!(ago(1_700_000_000, r.saved_unix), "时间未知");
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn corrupt_or_empty_file_reads_as_nothing_remembered() {
        let Some(dir) = test_dir("corrupt") else { return };
        let p = dir.join("client.json");

        std::fs::write(&p, "{ 这不是 JSON").unwrap();
        assert!(load_at(&p).is_none(), "坏文件必须降级成 None，不能 panic");

        // 「有文件但设备 ID 是空的」同样算没记住。
        std::fs::write(&p, r#"{"server":"10.0.0.1:21116","device":"  "}"#).unwrap();
        assert!(load_at(&p).is_none());

        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_dir(&dir);
    }

    /// 护栏：磁盘上不许出现任何凭据。谁以后想「顺手把口令也存上」，这条立刻红。
    #[test]
    fn saved_file_carries_no_credential() {
        let r = Recent {
            server: "10.0.0.1:21116".into(),
            device: "805747189".into(),
            host_name: "麒麟真机".into(),
            saved_unix: 1,
        };
        let body = serde_json::to_string(&r).unwrap().to_lowercase();
        for bad in ["password", "passwd", "口令", "密码", "temp_code", "secret", "token"] {
            assert!(!body.contains(bad), "配置里出现了凭据字样 {bad}：{body}");
        }
        // 字段自己也不能有「value」这种能塞任意东西的兜底。
        assert!(!body.contains("\"value\""), "{body}");
    }

    #[test]
    fn apply_only_fills_what_the_command_line_left_empty() {
        let r = sample();
        // 什么都没给：全部取自记住的
        let (s, d) = apply(&r, "127.0.0.1:21116", false, "");
        assert_eq!((s.as_str(), d.as_str()), ("192.168.223.15:21116", "805747189"));
        // 设备给了一半：命令行的设备胜出，中继仍取自记住的
        let (s, d) = apply(&r, "127.0.0.1:21116", false, "111111111");
        assert_eq!((s.as_str(), d.as_str()), ("192.168.223.15:21116", "111111111"));
        // 中继显式给了：不覆盖它（`--server` 的默认值不算显式，靠 server_explicit 区分）
        let (s, _) = apply(&r, "10.1.2.3:21116", true, "");
        assert_eq!(s, "10.1.2.3:21116");
    }

    #[test]
    fn describe_says_the_device_and_how_long_ago() {
        let r = sample();
        let now = r.saved_unix + 12 * 60;
        let s = describe(&r, now);
        assert!(s.contains("805747189") && s.contains("麒麟真机") && s.contains("12 分钟前"), "{s}");
        assert_eq!(ago(1_000, 1_000), "时间未知", "同一秒不该说成「0 秒前」");
        assert_eq!(ago(80, 40), "刚刚");
        assert_eq!(ago(100, 40), "1 分钟前");
        assert_eq!(ago(3 * 3600, 0), "时间未知");
        assert_eq!(ago(10 * 86_400 + 100, 100), "10 天前");
    }

    #[test]
    fn forget_reports_whether_there_was_anything() {
        let Some(dir) = test_dir("forget") else { return };
        let p = dir.join("client.json");
        let _ = std::fs::remove_file(&p);
        assert!(!forget_at(&p).unwrap(), "本来没有 → false，且不是错误");
        save_to(&p, &sample()).unwrap();
        assert!(forget_at(&p).unwrap());
        assert!(load_at(&p).is_none());
        let _ = std::fs::remove_dir(&dir);
    }

    /// 默认路径落在 `…/uulan/client.json`（与 `host.json` 同一个目录）。
    /// 环境变量存在时跳过：那是真的改过环境，测不到默认值。
    #[test]
    fn default_path_lives_next_to_host_json() {
        if std::env::var_os(ENV_PATH).is_some() {
            eprintln!("跳过：环境里设了 {}", ENV_PATH);
            return;
        }
        let p = default_path();
        assert!(p.ends_with("client.json"), "{}", p.display());
        assert_eq!(p.parent().and_then(|d| d.file_name()), Some(std::ffi::OsStr::new("uulan")));
    }
}