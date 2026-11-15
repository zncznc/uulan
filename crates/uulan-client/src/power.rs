//! 电源动作（`--power`）的控制端半边（M4 补格，2026-11-16，配被控端 `power::handle`）。
//!
//! 命令行用法：
//!
//! ```text
//! --power <lock|logout|reboot|shutdown>  让被控端执行电源动作，等一句回话就退出
//! ```
//!
//! 与 `--terms`/`--ls` 一样是**独立会话**：不开窗口、不收画面。
//!
//! ## 为什么「超时」和「连接先断」都可能是「成功」
//!
//! `reboot`/`shutdown` 生效的形态就是机器下线 —— 若被控端在把回话写出去之前就被
//! systemd 收掉，控制端看到的是断线而不是 `PowerState`。这不是消息丢了，
//! 而是动作**成功了**。所以这里的两条超时/断线路径都把两种可能**如实并列**，
//! 不冒充确定性（与被控端侧「Pending = 采纳」同一口径）。
//!
//! ## 为什么拒绝也必须回话
//!
//! 「被 power_allow 挡下」和「对面是不认 power 的老版本」排查方向完全不同：
//! 前者改被控端配置，后者升级被控端。被控端对**每次请求必回一句**，
//! 控制端只在老版本/机器已下线这两种形态下才会走到超时/断线分支。

use std::time::Duration;

use anyhow::{Context, Result};
use tokio::time::{timeout_at, Instant};

use uulan_protocol::crypto::SecControl;
use uulan_protocol::frame::{read_frame, write_frame, FrameKind};
use uulan_protocol::msg::{C2H, H2C, PowerAction};

/// 最多等多久要一句 `PowerState`。老被控端不认 `Power`、永远不会回 ——
/// 与其挂死，不如明说两种可能。
const WAIT_LIMIT: Duration = Duration::from_secs(12);

/// 渲染成人读两行 / JSON 单行（纯函数，本机可测）。
pub fn render(action: &str, ok: bool, detail: &str, json: bool) -> String {
    if json {
        let v = serde_json::json!({ "action": action, "ok": ok, "detail": detail });
        return serde_json::to_string(&v).unwrap_or_else(|_| "{}".into());
    }
    let head = if ok {
        format!("电源动作 {action}：已采纳")
    } else {
        format!("电源动作 {action}：未执行")
    };
    if detail.is_empty() {
        head
    } else {
        format!("{head}\n  {detail}")
    }
}

/// 解析 `--power` 的动作名。大小写不敏感；认不出就报错并列出全部有效值。
pub fn parse_action(raw: &str) -> Result<PowerAction> {
    Ok(match raw.trim().to_ascii_lowercase().as_str() {
        "lock" => PowerAction::Lock,
        "logout" => PowerAction::Logout,
        "reboot" => PowerAction::Reboot,
        "shutdown" => PowerAction::Shutdown,
        other => anyhow::bail!(
            "--power 不认识动作 {other:?}（有效值: lock | logout | reboot | shutdown）"
        ),
    })
}

/// 在一条已鉴权的控制连接上发一句 `Power`，等一句 `PowerState`。
///
/// 与 `terms::ask` 同一条纪律：控制通道是**多路复用**的，「等这一句」不等于「只听这一句」
/// —— 别的消息照旧跳过，Ping 必须回，否则慢链路下会被中继当死连接摘掉。
pub async fn ask(control: SecControl, action: PowerAction) -> Result<(bool, String)> {
    let (mut rd, mut wr) = control.into_split();
    let f = C2H::Power { action }
        .to_frame()
        .context("Power 序列化失败（这条路不该发生）")?;
    write_frame(&mut wr, f.kind, &f.payload)
        .await
        .context("控制通道写入失败")?;
    let deadline = Instant::now() + WAIT_LIMIT;
    loop {
        let fr = match timeout_at(deadline, read_frame(&mut rd)).await {
            Err(_) => anyhow::bail!(
                "等电源动作回话超时（{} 秒）：被控端多半是不认 power 的老版本；\
                 若刚发的是 reboot/shutdown/logout，也可能是机器已经下线、回音没赶上车",
                WAIT_LIMIT.as_secs()
            ),
            Ok(Err(e)) => return Err(e).context(
                "回话到达前连接就断了：若刚发的是 reboot/shutdown/logout，\
                 这正是「已生效」的样子",
            ),
            Ok(Ok(f)) => f,
        };
        if fr.kind == FrameKind::Ping {
            let _ = write_frame(&mut wr, FrameKind::Pong, &fr.payload).await;
            continue;
        }
        if fr.kind != FrameKind::Json {
            continue;
        }
        let m: H2C = match fr.decode_json() {
            Ok(m) => m,
            Err(_) => continue,
        };
        match m {
            H2C::PowerState {
                action: echo,
                ok,
                detail,
            } => {
                if echo == action.wire_name() {
                    return Ok((ok, detail));
                }
                // 回显不是我要的那条：如实忽略（理论上只可能是对端 bug）。
            }
            H2C::Bye { reason } => anyhow::bail!("被控端道别：{reason}"),
            _ => continue,
        }
    }
}

/// `--power` 的入口：发出去、等回话、打印。返回「被采纳与否」——
/// 调用方据此决定退出码（未执行 = 非零，脚本要能当断言用）。
pub async fn run(control: SecControl, action: PowerAction, json: bool) -> Result<bool> {
    let (ok, detail) = ask(control, action).await?;
    println!("{}", render(action.wire_name(), ok, &detail, json));
    Ok(ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_says_adoption_and_no_never_looks_the_same() {
        let a = render("reboot", true, "已执行", false);
        let b = render("reboot", false, "不在放行名单", false);
        assert!(a.contains("已采纳") && a.contains("已执行"), "{a}");
        assert!(b.contains("未执行") && b.contains("放行名单"), "{b}");
        assert_ne!(a, b);
    }

    #[test]
    fn json_is_one_greppable_line() {
        let j = render("lock", false, "不在放行名单", true);
        assert!(!j.contains('\n'), "JSON 必须单行：{j}");
        let v: serde_json::Value = serde_json::from_str(&j).expect("合法 JSON");
        assert_eq!(v["action"], "lock");
        assert_eq!(v["ok"], false);
        assert!(v["detail"].as_str().unwrap().contains("放行名单"));
    }

    #[test]
    fn parse_action_is_lenient_on_case_and_strict_on_unknown() {
        assert_eq!(parse_action("LOCK").unwrap(), PowerAction::Lock);
        assert_eq!(parse_action(" Shutdown ").unwrap(), PowerAction::Shutdown);
        let e = parse_action("poweer").unwrap_err().to_string();
        assert!(e.contains("lock | logout | reboot | shutdown"), "{e}");
    }

    #[test]
    fn wire_shape_is_stable() {
        // 线上口径钉住：老端解不出新变体没关系（它不会收到），但新两端的名字不许漂。
        let f = C2H::Power {
            action: PowerAction::Reboot,
        }
        .to_frame()
        .unwrap();
        let s = String::from_utf8_lossy(&f.payload).to_string();
        assert!(s.contains("\"power\""), "{s}");
        assert!(s.contains("\"reboot\""), "{s}");
        let j = serde_json::to_string(&H2C::PowerState {
            action: "reboot".into(),
            ok: true,
            detail: "已执行".into(),
        })
        .unwrap();
        assert!(j.contains("\"power_state\""), "{j}");
    }
}
