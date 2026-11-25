//! PAM 第二因素：口令过了 PAKE 之后，再验一次**这个人的系统登录密码**。
//!
//! # 为什么这一层单独存在
//!
//! PAKE 能证明"对面知道那份共享口令"，但它证明不了"这个人有权登录这台机器"。
//! 现场要的通常是后者（运维流动、口令共用、人走了口令还在）。这一层把远控权限
//! 挂回系统账户本身：改密码 = 吊销远控，禁用账户 = 立即断掉。
//!
//! # 三条不能妥协的规矩（都有具体理由，不是教条）
//!
//! 1. **不在名单里的用户名，绝不喂给 su。**
//!    否则任何人都能拿 `root`、`oracle` 这类账户来试探着锁 —— 用户接受的是
//!    "我自己输错锁我自己"，不是"别人可以替我把别人的账户锁掉"。名单为空 = 拒绝一切
//!    （空名单更可能是没配好，绝不能被理解成"全允许"）。
//! 2. **只给一次机会。**
//!    su 自己会重试三次；我们写完一次口令就不再补输入。三次重试会把系统账户的
//!    失败计数放大，也给爆破多两倍机会。代价是"错口令"这一路表现为**超时**而不是
//!    明确失败 —— 归因略糊，但判定结果正确，这个取舍是故意的。
//! 3. **su 的输出一个字都不留。**
//!    pty 默认**回显输入**，所以它的输出里可能直接含那个登录密码。因此这里读出来就丢，
//!    错误信息里绝不带 su 的任何原文，也不写进日志。

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 校验器：`(用户名, 密码) -> 是否通过`。
///
/// `Err` = 无法判定（超时、开不了 pty、su 不存在），调用方一律按**拒绝**处理 ——
/// 这一层是闸门，判定不了的时候不许放行。
pub type Checker = Arc<dyn Fn(&str, &str) -> Result<bool, String> + Send + Sync>;

/// 名单检查（纯函数，好测）。大小写与空格按精确匹配处理：
/// 系统账户名本来就是大小写敏感的，这里不替它做"贴心"归一化。
pub fn in_allow_list(allow: &[String], user: &str) -> bool {
    let u = user.trim();
    !u.is_empty() && allow.iter().any(|a| a.trim() == u)
}

/// 用系统的 `su` 走一遍真 PAM。不需要 root 权限（`su` 本身是 setuid），
/// 也就不需要把任何口令哈希交到本程序手里。
pub fn system_checker(timeout: Duration) -> Checker {
    Arc::new(move |user: &str, password: &str| {
        let mut cmd = portable_pty::CommandBuilder::new("su");
        cmd.arg(user);
        cmd.arg("-c");
        cmd.arg("/bin/true");
        run_pty_prompt_auth(cmd, password, timeout)
    })
}

/// 在伪终端里跑一次口令校验进程：等它吐出提示符后再写一次口令，全程不保留任何输出。
///
/// 为什么**必须等提示符出来再写**：util-linux `su` / `pam_unix` 在打印 `Password:` / `输入密码`
/// 之前会调用 `tcsetattr(..., TCSAFLUSH, ...)` 关回显并**清空 pty 输入缓冲**。
/// `spawn` 完立刻写密码会被 `TCSAFLUSH` 一把冲掉，导致 `su` 干等到 10 秒超时
/// （2026-11-25 麒麟真机 `su 2.34` 实测：`dt=0.016s echo=False prompt='输入密码'`）。
fn run_pty_prompt_auth(
    cmd: portable_pty::CommandBuilder,
    password: &str,
    timeout: Duration,
) -> Result<bool, String> {
    use portable_pty::PtySize;
    use std::sync::mpsc;

    let sys = portable_pty::native_pty_system();
    let pair = sys
        .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
        .map_err(|e| format!("开伪终端失败：{e}"))?;

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("起 su 失败（这台机器上没有 su？）：{e}"))?;
    drop(pair.slave);

    let mut killer = child.clone_killer();
    let (prompt_tx, prompt_rx) = mpsc::channel::<()>();

    // 规矩 3：另起线程把输出读干丢掉。pty 可能回显输入，任何字节都不许留下；
    // 仅在首次收到非空输出时发一个空信号，告诉主线程「提示符已出、TCSAFLUSH 已过」。
    if let Ok(mut rd) = pair.master.try_clone_reader() {
        std::thread::spawn(move || {
            let mut sink = [0u8; 4096];
            let mut notified = false;
            loop {
                match rd.read(&mut sink) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if !notified {
                            notified = true;
                            let _ = prompt_tx.send(());
                        }
                    }
                }
            }
        });
    }

    let mut writer = Some(
        pair.master
            .take_writer()
            .map_err(|e| format!("伪终端写端拿不到：{e}"))?,
    );

    let deadline = Instant::now() + timeout;
    let mut wrote = false;
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return Ok(st.success()),
            Ok(None) => {}
            Err(e) => return Err(format!("等 su 退出失败：{e}")),
        }
        // 规矩 2：收到提示符后只写这一次，绝不补写第二次。
        if !wrote && prompt_rx.try_recv().is_ok() {
            wrote = true;
            std::thread::sleep(Duration::from_millis(10));
            if let Some(mut w) = writer.take() {
                let sent = w
                    .write_all(password.as_bytes())
                    .and_then(|_| w.write_all(b"\n"))
                    .and_then(|_| w.flush());
                drop(w);
                if let Err(e) = sent {
                    // 子进程可能在报错退出（如账户不存在）时刚好关闭从端，先复查退出码。
                    if let Ok(Some(st)) = child.try_wait() {
                        return Ok(st.success());
                    }
                    let _ = killer.kill();
                    return Err(format!("写不进伪终端：{e}"));
                }
            }
        }
        if Instant::now() >= deadline {
            let _ = killer.kill();
            // 超时按拒绝处理（见模块头：Err 一律视为不放行）。
            return Err("PAM 校验超时（su 还在等输入 —— 按一次失败处理）".to_string());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 测试用的假校验器：按表放行，别的都拒。
pub fn fake_checker(known: &[(&str, &str)]) -> Checker {
    let table: Vec<(String, String)> = known.iter().map(|(u, p)| (u.to_string(), p.to_string())).collect();
    Arc::new(move |user: &str, password: &str| {
        Ok(table.iter().any(|(u, p)| u == user && p == password))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_allow_list_denies_everyone() {
        // 空名单必须是"拒绝一切"而不是"全允许"：没配好 ≠ 开放。
        assert!(!in_allow_list(&v(&[]), "mis"));
        assert!(!in_allow_list(&v(&[]), "root"));
    }

    #[test]
    fn off_list_users_never_reach_su() {
        // 这条保护的是**别人**的账户：拿 root 来试探着锁不是运维该有的能力。
        assert!(!in_allow_list(&v(&["ops", "mis"]), "root"));
        assert!(!in_allow_list(&v(&["ops", "mis"]), ""));
        assert!(!in_allow_list(&v(&["ops", "mis"]), "   "));
        assert!(in_allow_list(&v(&["ops", "mis"]), "  ops  "));
        assert!(!in_allow_list(&v(&["ops"]), "OPS"), "系统账户名大小写敏感，别替它归一化");
    }

    #[test]
    fn fake_checker_is_the_test_seam() {
        let c = fake_checker(&[("mis", "pw1234")]);
        assert_eq!(c("mis", "pw1234"), Ok(true));
        assert_eq!(c("mis", "wrong"), Ok(false));
        assert_eq!(c("root", "pw1234"), Ok(false));
    }

    #[cfg(unix)]
    #[test]
    fn waits_for_prompt_so_tcsaflush_does_not_swallow_password() {
        // 模拟 util-linux su：启动 50ms 后先清空提前写进缓冲区的字节（TCSAFLUSH），
        // 再打印提示符并读取一行口令。若 spawn 后立刻写密码，会被清空并超时。
        let script = r#"sleep 0.05; while read -t 0.02 -r _; do :; done; printf 'Password: '; IFS= read -r pw; [ "$pw" = "ok-pass" ]"#;

        let mut cmd_ok = portable_pty::CommandBuilder::new("bash");
        cmd_ok.args(["-c", script]);
        assert_eq!(
            run_pty_prompt_auth(cmd_ok, "ok-pass", Duration::from_secs(3)),
            Ok(true)
        );

        let mut cmd_bad = portable_pty::CommandBuilder::new("bash");
        cmd_bad.args(["-c", script]);
        assert_eq!(
            run_pty_prompt_auth(cmd_bad, "wrong-pass", Duration::from_secs(3)),
            Ok(false)
        );
    }

    #[cfg(unix)]
    #[test]
    fn hung_prompt_times_out_cleanly() {
        let mut cmd = portable_pty::CommandBuilder::new("sh");
        cmd.args(["-c", "sleep 5"]);
        let t0 = Instant::now();
        let res = run_pty_prompt_auth(cmd, "any", Duration::from_millis(250));
        assert!(res.is_err(), "卡死的子进程必须按超时拒绝：{res:?}");
        assert!(t0.elapsed() < Duration::from_secs(2), "超时强杀耗时过长");
    }
}
