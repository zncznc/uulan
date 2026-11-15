//! 电源动作（`C2H::Power` → `H2C::PowerState`）的被控端半边（M4 补格，2026-11-16）。
//!
//! `Power` 在协议里躺了很久，两端从没碰过 —— 又一例「协议先行、两端未接」
//! （与 `VideoSet`、音频状态、画面结束原因、`--terms` 同款）。接线时把三条纪律钉死：
//!
//! - **默认全拒绝**：`host.json` 的 `power_allow` 不写 = 四种动作一个都不许。
//!   与 `portmap_allow` 同一条思路：能关机/重启的开关就是安全边界 ——
//!   拿到口令的人本来就能看桌面、传文件，「顺便把机器关了」必须另开一道 explicit 的闸。
//! - **每一次请求必回一句 `PowerState`**（采纳/拒绝/失败都回）：
//!   不回的话控制端只能靠超时猜「没放行」还是「机器已经下线」—— 这两种排查方向完全不同。
//! - **真执行在 spawn 出来的任务里**：systemctl 之类可能要几秒才回话，
//!   绝不在 control_loop 里同步等 —— 那会卡住键鼠和剪贴板（`--forward` 当年踩过的坑）。
//!
//! 执行的命令按平台挑（见 [`plan`]），子进程的 stderr 摘要会带进 `detail`：
//! 「polkit 要认证」「这台没有 loginctl」这类现场答案，一句 `detail` 就该说清，
//! 不该让人再翻被控端日志。

use std::process::Stdio;
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{info, warn};

use uulan_protocol::msg::{H2C, PowerAction};

use crate::session::{send_out, Out};

/// 子进程「启动之后最多看它几秒」。
///
/// `systemctl reboot` / `shutdown /s` 这类命令**立即返回 0**，真正倒下是几秒后的事，
/// 所以退出码 0 就回「已执行」；超过宽限期还没退出的回「仍在执行」——
/// 同样算采纳，但把「机器多半正在下线」说在前面，别让控制端把断线当故障。
const EXEC_GRACE: Duration = Duration::from_secs(3);

/// `detail` 里带回来的 stderr 最多留几个字（控制通道不是日志搬运工）。
const STDERR_TAIL: usize = 200;

/// 把 `power_allow` 的原始字符串翻成动作集合。`*` = 四种全放行。
///
/// 认不出的词**必须报错**（与 `portmap_allow` 同一条纪律）：拼错一个词，
/// 「以为已经放开」的现场会变成永久拒绝，那种问题比误放行还难查。
pub fn parse_allow(raw: &[String]) -> Result<Vec<PowerAction>, String> {
    let mut out = Vec::new();
    for item in raw {
        let w = item.trim().to_ascii_lowercase();
        match w.as_str() {
            "*" => {
                return Ok(vec![
                    PowerAction::Lock,
                    PowerAction::Logout,
                    PowerAction::Reboot,
                    PowerAction::Shutdown,
                ])
            }
            "lock" => out.push(PowerAction::Lock),
            "logout" => out.push(PowerAction::Logout),
            "reboot" => out.push(PowerAction::Reboot),
            "shutdown" => out.push(PowerAction::Shutdown),
            other => {
                return Err(format!(
                    "不认识的动作词 {other:?}（有效值: lock / logout / reboot / shutdown / *）"
                ))
            }
        }
    }
    out.dedup();
    Ok(out)
}

/// 一行说明当前放行范围（进日志、也进拒绝理由，照着它就能改配置）。
pub fn describe(allow: &[PowerAction]) -> String {
    if allow.is_empty() {
        return "全部拒绝（power_allow 未配置）".to_string();
    }
    let names: Vec<&str> = allow.iter().map(|a| a.wire_name()).collect();
    format!("放行 {}", names.join(", "))
}

/// 这一拍到底执行什么（纯函数，跨平台分支在这里收口）。
pub enum Exec {
    Spawn(Vec<String>),
    /// 这台机器上没有安全执行这条路（例如拿不到当前用户名）。拒绝并把原因说清。
    Impossible(String),
}

/// 平台命令表。**每条都是 argv 数组、不经 shell** —— 动作名之外没有任何拼接面。
#[cfg_attr(windows, allow(unused_variables))] // `user` 只有 unix 的 Logout 分支用得到
pub fn plan(action: PowerAction, test_program: Option<&str>, user: Option<&str>) -> Exec {
    // 测试钩子（host.json 的 power_test_program）：本机验收台用它把
    // 「命令真的被执行、退出码真的回了话」钉死，而不真去碰这台机器的电源。
    // 与 audio_source 的 lavfi 合成源同一思路。参数固定为动作名，无别的输入。
    if let Some(p) = test_program {
        return Exec::Spawn(vec![p.to_string(), action.wire_name().to_string()]);
    }
    match action {
        PowerAction::Lock => {
            #[cfg(windows)]
            {
                Exec::Spawn(vec![
                    "rundll32.exe".into(),
                    "user32.dll,LockWorkStation".into(),
                ])
            }
            #[cfg(not(windows))]
            Exec::Spawn(vec!["loginctl".into(), "lock-session".into()])
        }
        PowerAction::Logout => {
            #[cfg(windows)]
            {
                Exec::Spawn(vec![
                    "shutdown.exe".into(),
                    "/l".into(),
                    "/t".into(),
                    "0".into(),
                ])
            }
            #[cfg(not(windows))]
            match user.map(str::trim).filter(|u| !u.is_empty()) {
                Some(u) => Exec::Spawn(vec![
                    "loginctl".into(),
                    "terminate-user".into(),
                    u.to_string(),
                ]),
                None => Exec::Impossible("拿不到当前用户名，Logout 没有安全执行路径".into()),
            }
        }
        PowerAction::Reboot => {
            #[cfg(windows)]
            {
                Exec::Spawn(vec![
                    "shutdown.exe".into(),
                    "/r".into(),
                    "/t".into(),
                    "0".into(),
                ])
            }
            #[cfg(not(windows))]
            Exec::Spawn(vec!["systemctl".into(), "reboot".into()])
        }
        PowerAction::Shutdown => {
            #[cfg(windows)]
            {
                Exec::Spawn(vec![
                    "shutdown.exe".into(),
                    "/s".into(),
                    "/t".into(),
                    "0".into(),
                ])
            }
            #[cfg(not(windows))]
            Exec::Spawn(vec!["systemctl".into(), "poweroff".into()])
        }
    }
}

/// 执行走到哪一步了（喂给 [`compose`]，回话的「算不算成功」只在那一个口径里）。
pub enum Outcome {
    /// 策略没放行（含 power_allow 配置有误时按全拒绝处理）。
    Denied(String),
    /// 这台机器没有这条路。
    Impossible(String),
    /// 命令压根没起来（找不到程序 / 起进程被拒）。
    SpawnFail(String),
    /// 子进程退出：code 为 None = 被信号终止。第二段是 stderr 原文（未截断）。
    Exited(Option<i32>, String),
    /// 宽限期内还没退出 —— 多半是关机/重启已经在路上了。
    Pending,
}

/// stderr 取**尾部**：systemd/polkit 的话都在最后一段，开头往往是空行。
pub fn stderr_tail(raw: &str, limit: usize) -> String {
    let s: String = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" | ");
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= limit {
        return s;
    }
    chars[chars.len() - limit..].iter().collect()
}

/// 结果 → 回给控制端的那一句。纯函数，口径钉死在这里：
///
/// - 退出码 0 / 仍在跑 = **采纳**（ok=true）；
/// - 策略拒绝、起不来、非零退出 = **没成**（ok=false），`detail` 给原因与改法；
/// - 「非零退出但 stderr 是空的」也要明说，别让 detail 变成 `退出码 3：` 后面没话。
pub fn compose(action: PowerAction, outcome: Outcome) -> H2C {
    let name = action.wire_name().to_string();
    let (ok, detail) = match outcome {
        Outcome::Denied(why) => (false, why),
        Outcome::Impossible(why) => (false, why),
        Outcome::SpawnFail(e) => (
            false,
            format!("命令起不来：{e}（这台机器上没有它，或不在 PATH）"),
        ),
        Outcome::Exited(Some(0), _) => (true, "已执行".to_string()),
        Outcome::Exited(Some(code), err) => {
            let t = stderr_tail(&err, STDERR_TAIL);
            let why = if t.is_empty() {
                "子进程没留话说".to_string()
            } else {
                t
            };
            (false, format!("退出码 {code}：{why}"))
        }
        Outcome::Exited(None, err) => (
            false,
            format!("被信号终止：{}", stderr_tail(&err, STDERR_TAIL)),
        ),
        Outcome::Pending => (
            true,
            "命令仍在执行（若刚才是 reboot/shutdown/logout，被控端多半正在下线）".to_string(),
        ),
    };
    H2C::PowerState {
        action: name,
        ok,
        detail,
    }
}

/// 拒绝理由要写成**能照抄的改法**（现场不该翻文档才知道配置叫什么）。
pub fn denial_detail(action: PowerAction, allow: &[PowerAction], cfg_error: Option<String>) -> String {
    if let Some(e) = cfg_error {
        return format!("power_allow 配置有误（{e}），本次按全部拒绝处理");
    }
    format!(
        "不在放行名单（当前：{}）。要放开：在被控端 host.json 里把 \"power_allow\" 加上 \"{}\"，再重启被控端。",
        describe(allow),
        action.wire_name()
    )
}

/// 执行一条电源动作并把结果回上控制通道。
///
/// 在**独立任务**里跑（control_loop 只负责 spawn 它），配置快照由调用方在
/// 收到请求那一刻取 —— 与「热加载凭据」同一条纪律：以请求那一刻的配置为准。
pub(crate) async fn handle(
    action: PowerAction,
    raw_allow: Vec<String>,
    test_program: Option<String>,
    user: Option<String>,
    out: mpsc::UnboundedSender<Out>,
) {
    let report = |outcome: Outcome| {
        let m = compose(action, outcome);
        if let H2C::PowerState { ok, detail, .. } = &m {
            info!(action = action.wire_name(), ok, detail = %detail, "电源动作回话（已告知控制端）");
        }
        let _ = send_out(&out, m);
    };

    let allow = match parse_allow(&raw_allow) {
        Ok(a) => a,
        Err(e) => {
            warn!(error = %e, "power_allow 配置有误，电源动作按全部拒绝处理");
            report(Outcome::Denied(denial_detail(
                action,
                &[],
                Some(e),
            )));
            return;
        }
    };
    if !allow.contains(&action) {
        report(Outcome::Denied(denial_detail(action, &allow, None)));
        return;
    }

    let argv = match plan(action, test_program.as_deref(), user.as_deref()) {
        Exec::Spawn(v) => v,
        Exec::Impossible(why) => {
            report(Outcome::Impossible(why));
            return;
        }
    };
    info!(action = action.wire_name(), cmd = %argv.join(" "), "执行电源动作");

    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            report(Outcome::SpawnFail(e.to_string()));
            return;
        }
    };
    // kill_on_drop 默认 false：宽限期到点就**放手**，让关机命令把机器关掉。
    match tokio::time::timeout(EXEC_GRACE, child.wait_with_output()).await {
        Err(_) => report(Outcome::Pending),
        Ok(Err(e)) => report(Outcome::SpawnFail(format!("等结果失败：{e}"))),
        Ok(Ok(outs)) => {
            let code = outs.status.code();
            let err = String::from_utf8_lossy(&outs.stderr).to_string();
            report(Outcome::Exited(code, err));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uulan_protocol::msg::PowerAction::*;

    fn acts(a: &[PowerAction]) -> Vec<&'static str> {
        a.iter().map(|x| x.wire_name()).collect()
    }

    #[test]
    fn allowlist_parses_valid_words_and_dedups() {
        let raw: Vec<String> = ["lock", " LOCK ", "lock", "*"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // `*` 出现即全放行，不必再解析后面的。
        assert_eq!(acts(&parse_allow(&raw).unwrap()).len(), 4);
        let raw2: Vec<String> = ["lock", "logout"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(acts(&parse_allow(&raw2).unwrap()), vec!["lock", "logout"]);
        assert_eq!(parse_allow(&[]).unwrap(), Vec::<PowerAction>::new());
    }

    #[test]
    fn allowlist_rejects_unknown_words_instead_of_silently_ignoring() {
        // 拼错词必须当场报错：静默忽略会把「以为放开了」变成永久拒绝。
        let raw: Vec<String> = ["lock", "poweer"].iter().map(|s| s.to_string()).collect();
        let e = parse_allow(&raw).unwrap_err();
        assert!(e.contains("poweer"), "{e}");
        assert!(e.contains("lock"), "报错要顺带告诉人家有效值：{e}");
    }

    #[test]
    fn wire_names_match_the_serde_spellings() {
        // 白名单、回显、执行参数全用 wire_name —— 它必须与线上（serde snake_case）一致。
        for a in [Lock, Logout, Reboot, Shutdown] {
            let s = serde_json::to_string(&a).unwrap();
            assert_eq!(s, format!("\"{}\"", a.wire_name()));
        }
    }

    #[test]
    fn compose_is_the_single_place_deciding_what_counts_as_accepted() {
        // 退出码 0 = 采纳；仍在跑 = 采纳但话说在前面；非零/起不来/拒绝 = 不采纳。
        let m = compose(Lock, Outcome::Exited(Some(0), String::new()));
        assert!(matches!(&m, H2C::PowerState { ok: true, detail, .. } if detail == "已执行"));
        let m = compose(Reboot, Outcome::Pending);
        assert!(matches!(&m, H2C::PowerState { ok: true, detail, .. } if detail.contains("仍在执行")));
        let m = compose(Reboot, Outcome::Exited(Some(1), "polkit 要认证".into()));
        match m {
            H2C::PowerState { action, ok, detail } => {
                assert_eq!(action, "reboot");
                assert!(!ok);
                assert!(detail.contains("退出码 1"), "{detail}");
                assert!(detail.contains("polkit"), "stderr 要带回来：{detail}");
            }
            other => panic!("不是 PowerState：{other:?}"),
        }
        // 非零退出 + stderr 空白：detail 不能秃成「退出码 3：」。
        let m = compose(Shutdown, Outcome::Exited(Some(3), "   ".into()));
        match m {
            H2C::PowerState { detail, .. } => assert!(detail.contains("没留话说"), "{detail}"),
            _ => unreachable!(),
        }
    }

    #[test]
    fn stderr_tail_keeps_the_end_and_flattens_blank_lines() {
        let raw = "\nfirst line\n\n";
        assert_eq!(stderr_tail(raw, 200), "first line");
        let long = "a".repeat(300);
        let t = stderr_tail(&long, 100);
        assert_eq!(t.chars().count(), 100);
        assert!(t.starts_with('a') && t.ends_with('a'));
    }

    #[test]
    fn denial_detail_teaches_the_fix_not_just_the_no() {
        let d = denial_detail(Reboot, &[Lock], None);
        assert!(d.contains("power_allow"), "{d}");
        assert!(d.contains("reboot"), "改法里要给出动作名：{d}");
        assert!(d.contains("host.json"), "要说出配置在哪：{d}");
        let d2 = denial_detail(Lock, &[], Some("不认识的动作词 poweer".into()));
        assert!(d2.contains("按全部拒绝"), "配置坏了要明说降级口径：{d2}");
    }

    #[test]
    fn test_program_overrides_the_platform_table_but_keeps_the_shape() {
        // 验收台形态：argv = [program, 动作名]，除动作名外没有任何输入面。
        match plan(Reboot, Some("D:/fake.exe"), None) {
            Exec::Spawn(v) => {
                assert_eq!(v.len(), 2);
                assert_eq!(v[0], "D:/fake.exe");
                assert_eq!(v[1], "reboot");
            }
            Exec::Impossible(_) => panic!("测试钩子不该有 Impossible 路"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn production_commands_follow_the_service_manager_conventions() {
        // 麒麟/通用 Linux：loginctl / systemctl，不经 shell、没有拼接面。
        match plan(Lock, None, None) {
            Exec::Spawn(v) => assert_eq!(v, vec!["loginctl", "lock-session"]),
            _ => panic!(),
        }
        match plan(Reboot, None, None) {
            Exec::Spawn(v) => assert_eq!(v, vec!["systemctl", "reboot"]),
            _ => panic!(),
        }
        match plan(Shutdown, None, None) {
            Exec::Spawn(v) => assert_eq!(v, vec!["systemctl", "poweroff"]),
            _ => panic!(),
        }
        match plan(Logout, None, Some("mis")) {
            Exec::Spawn(v) => assert_eq!(v, vec!["loginctl", "terminate-user", "mis"]),
            _ => panic!(),
        }
        // 拿不到用户名 = Impossible，绝不猜一个。
        assert!(matches!(plan(Logout, None, None), Exec::Impossible(_)));
    }

    #[cfg(windows)]
    #[test]
    fn production_commands_on_windows_use_shutdown_exe_flags() {
        match plan(Reboot, None, None) {
            Exec::Spawn(v) => assert_eq!(v, ["shutdown.exe", "/r", "/t", "0"]),
            _ => panic!(),
        }
        match plan(Shutdown, None, None) {
            Exec::Spawn(v) => assert_eq!(v, ["shutdown.exe", "/s", "/t", "0"]),
            _ => panic!(),
        }
        match plan(Lock, None, None) {
            Exec::Spawn(v) => assert_eq!(v, ["rundll32.exe", "user32.dll,LockWorkStation"]),
            _ => panic!(),
        }
    }

    async fn run_handle(action: PowerAction, allow: &[&str], test_program: Option<&str>) -> H2C {
        let (tx, mut rx) = mpsc::unbounded_channel::<Out>();
        let raw: Vec<String> = allow.iter().map(|s| s.to_string()).collect();
        handle(
            action,
            raw,
            test_program.map(str::to_string),
            Some("tester".to_string()),
            tx,
        )
        .await;
        match rx.recv().await.expect("必须回一句") {
            Out::Json(m) => m,
            other => panic!("电源回话应当是 Json，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn denied_action_never_reaches_the_spawn_even_with_a_working_program() {
        // 默认（不配置）+ 一个真能跑起来的测试程序：必须**死在闸门口**，
        // 回话里的原因是配置而不是退出码 —— 顺序钉住。
        let fake = if cfg!(windows) {
            "C:/Windows/System32/cmd.exe"
        } else {
            "/bin/true"
        };
        let m = run_handle(Lock, &[], Some(fake)).await;
        match m {
            H2C::PowerState { ok, detail, action } => {
                assert_eq!(action, "lock");
                assert!(!ok);
                assert!(detail.contains("power_allow"), "闸门理由必须指向配置：{detail}");
                assert!(!detail.contains("退出码"), "绝不能跑到子进程那一步：{detail}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn allowed_action_really_spawns_and_the_child_result_comes_back() {
        // 真起一个真进程（不碰电源）：where.exe 找不到 lock / /bin/false 退出 1。
        // 「执行机制真的走通」靠这条钉，而不是纯函数的绿。
        let fake = if cfg!(windows) {
            "C:/Windows/System32/where.exe"
        } else {
            "/bin/false"
        };
        let m = run_handle(Lock, &["lock"], Some(fake)).await;
        match m {
            H2C::PowerState { ok, detail, .. } => {
                assert!(!ok, "非零退出不能算采纳：{detail}");
                assert!(detail.contains("退出码 1"), "子进程退出码要带回线上：{detail}");
            }
            other => panic!("{other:?}"),
        }
        #[cfg(unix)]
        {
            // 退出 0 = 采纳。
            let m = run_handle(Lock, &["lock"], Some("/bin/true")).await;
            assert!(matches!(m, H2C::PowerState { ok: true, .. }));
        }
    }

    #[tokio::test]
    async fn broken_allowlist_falls_back_to_denial_with_the_reason() {
        // host.json 拼错了词：按全拒绝处理，且 detail 里带上「配置有误」。
        let m = run_handle(Reboot, &["poweer"], None).await;
        match m {
            H2C::PowerState { ok, detail, .. } => {
                assert!(!ok);
                assert!(detail.contains("配置有误"), "{detail}");
            }
            other => panic!("{other:?}"),
        }
    }
}
