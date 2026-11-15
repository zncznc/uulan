//! 「远端终端会话列表」的客户端半边（M4 终端；配被控端 `TermOp::List` / `H2C::TermSessions`）。
//!
//! 命令行用法（`uulan-client`）：
//!
//! ```text
//! --terms              列远端当前的终端会话（id / 名称）；配 --json 出单行 JSON
//! ```
//!
//! 与 `--term`/`--ls` 一样，这是**独立会话**：不开窗口、不收画面，干完就退出。
//!
//! ## 为什么这一半到现在才接上
//!
//! `TermOp::List` → `H2C::TermSessions` 在被控端早就实现好了（`term::run` 的 List 分支），
//! 但控制端从没发过 `List`，也从没接住 `TermSessions` —— 又一次「协议先行、两端未接」
//! （与 `VideoSet`、音频状态、画面结束原因同款）。本模块把控制端这半接上：
//! 发一句 List，等一句 TermSessions，打印，退出。
//!
//! ## 清单的作用域是谁定的（被控端的注册表结构决定的，控制端改不了）
//!
//! 被控端的 `term` 注册表记的是**这条控制连接**开的会话（注册表按控制会话分，不按整机）。
//! 所以独立进程跑 `--terms` 永远是「没有」—— 那不是 bug，是老实的答案；要看见会话，
//! 得在**开着会话的那条连接**上用 `TermLink::list()`。跨连接、跨机器的「到底谁开着会话」
//! 是被控端的后续项（注册表要从按会话改成按机器，PTY 生命周期作用域得一起想清楚 ——
//! 不是今晚一块搬砖的事）。见 DESIGN 的「终端会话清单」一节。
//!
//! 单条会话报错（`TermError`）不毁整张清单 —— 只随带打到 stderr：判「这台有没有会话开着」
//! 靠的是「这一问有没有回音」，不是「回的东西干不干净」。

use std::time::Duration;

use anyhow::{Context, Result};
use tokio::time::{timeout_at, Instant};

use uulan_protocol::crypto::SecControl;
use uulan_protocol::frame::{read_frame, write_frame, FrameKind};
use uulan_protocol::msg::{C2H, H2C, TermOp, TermSession};

/// 最多等多久要一句 `TermSessions`。老被控端认不出 `List`、永远不会回 ——
/// 与其挂死，不如明说「对面多半没这个能力」。（独立会话没有看门狗兜着。）
const WAIT_LIMIT: Duration = Duration::from_secs(8);

/// 把会话清单渲染成人读的一行块 / JSON 的一行（纯函数，本机可测）。
pub fn render(sessions: &[TermSession], json: bool) -> String {
    if json {
        let items: Vec<serde_json::Value> = sessions
            .iter()
            .map(|s| serde_json::json!({ "id": s.id, "title": s.title }))
            .collect();
        serde_json::to_string(&items).unwrap_or_else(|_| "[]".into())
    } else if sessions.is_empty() {
        "远端当前没有终端会话".to_string()
    } else {
        let mut out = format!("远端共有 {} 条终端会话:", sessions.len());
        for s in sessions {
            let title = if s.title.is_empty() {
                "(没有名字)"
            } else {
                s.title.as_str()
            };
            out.push_str(&format!("\n  #{}\t{}", s.id, title));
        }
        out
    }
}

/// 在一条已鉴权的控制连接上发一句 `List`，等一句 `TermSessions`。
///
/// 中途飘过来的别的消息（指标、锁屏状态、剪贴板）照旧跳过 —— 与 fsx 的 `wait` 同一条纪律：
/// 控制通道是**多路复用**的，「等这一句」不等于「只听这一句」。Ping 必须回，
/// 否则慢链路下这条会话会被中继当死连接摘掉。
pub async fn ask(control: SecControl) -> Result<Vec<TermSession>> {
    let (mut rd, mut wr) = control.into_split();
    let f = C2H::Term(TermOp::List)
        .to_frame()
        .context("List 序列化失败（这条路不该发生）")?;
    write_frame(&mut wr, f.kind, &f.payload)
        .await
        .context("控制通道写入失败")?;
    let deadline = Instant::now() + WAIT_LIMIT;
    loop {
        let fr = match timeout_at(deadline, read_frame(&mut rd)).await {
            Err(_) => anyhow::bail!(
                "等终端会话清单超时（{} 秒）：被控端多半是不认 TermOp::List 的老版本",
                WAIT_LIMIT.as_secs()
            ),
            Ok(Err(e)) => return Err(e).context("控制通道读失败（被控端断线了？）"),
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
            H2C::TermSessions { sessions } => return Ok(sessions),
            H2C::TermError { id, error } => {
                eprintln!("  （会话 #{id} 报错：{error}）");
            }
            H2C::Bye { reason } => anyhow::bail!("被控端道别：{reason}"),
            _ => continue,
        }
    }
}

/// `--terms` 的入口：列出会话并打印（`--json` 时出 JSON 单行）。
pub async fn run(control: SecControl, json: bool) -> Result<()> {
    let sessions = ask(control).await?;
    println!("{}", render(&sessions, json));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(id: u32, title: &str) -> TermSession {
        TermSession {
            id,
            title: title.into(),
        }
    }

    #[test]
    fn an_empty_list_says_so_rather_than_printing_nothing() {
        // 「什么都不印」与「一台都没有」必须分得开，否则脚本会以为清单没送到。
        let out = render(&[], false);
        assert!(out.contains("没有终端会话"), "{out}");
    }

    #[test]
    fn a_named_session_prints_id_and_title() {
        let out = render(&[s(1, "/bin/bash — 主窗口"), s(7, "top")], false);
        assert!(out.contains("共有 2 条"), "{out}");
        assert!(out.contains("#1") && out.contains("/bin/bash — 主窗口"), "{out}");
        assert!(out.contains("#7") && out.contains("top"), "{out}");
    }

    #[test]
    fn an_untitled_session_is_labeled_not_left_blank() {
        // 「(没有名字)」要明说，不然看着像机器吞了一段输出。
        let out = render(&[s(3, "")], false);
        assert!(out.contains("(没有名字)"), "{out}");
        assert!(out.contains("#3"), "{out}");
    }

    #[test]
    fn json_output_survives_cjk_and_round_trips() {
        // --json 的唯一读者是脚本：必须一行可解析，中文标题一字不许变。
        let out = render(&[s(1, "运维终端"), s(2, "")], true);
        let back: Vec<serde_json::Value> = serde_json::from_str(&out).expect(&out);
        assert_eq!(back.len(), 2);
        assert_eq!(back[0]["id"], 1);
        assert_eq!(back[0]["title"], "运维终端");
        assert_eq!(back[1]["title"], "");
    }

    #[test]
    fn counts_cannot_be_inferred_from_a_single_entry() {
        // 条数必须来自清单本身：一条与零条要分得开，不能靠「有没有换行」猜。
        let one = render(&[s(1, "a")], false);
        let none = render(&[], false);
        assert!(one.contains("共有 1 条"), "{one}");
        assert!(!none.contains("共有"), "{none}");
    }
}
