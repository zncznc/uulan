//! 电源动作验收台的**假执行程序**（不碰任何真电源）。
//!
//! 被控端配置 `power_test_program` 指向它；被调形态固定为 `fake_power <动作名>`。
//! 退出码表是被 e2e 脚本断言的口径，改动要同步脚本：
//!
//! - `lock`     → 退出 0（= 采纳路「已执行」）
//! - `shutdown` → stderr 一句话 + 退出 3（= 非零退出要带着原因回上线）
//! - `logout`   → 睡 15 秒（超过被控端 3 秒宽限 = 采纳路「命令仍在执行」）
//! - 其它       → 退出 9（= 参数形状坏了）
//!
//! 为什么要有这条：「消息两端接上」的最硬证据不是纯函数绿，而是
//! **子进程真的被执行、它的退出码与 stderr 真的回到了控制端屏幕上**。

use std::{env, process::exit, thread::sleep, time::Duration};

fn main() {
    let Some(action) = env::args().nth(1) else {
        eprintln!("fake-power: 缺参数（用法: fake_power <动作名>）");
        exit(9);
    };
    println!("fake-power: 收到 {action}");
    match action.as_str() {
        "lock" => exit(0),
        "shutdown" => {
            eprintln!("fake-power: 模拟拒绝切断电源");
            exit(3);
        }
        "logout" => {
            // 宽限期(3s)内不退出，让被控端回「命令仍在执行」。别睡太久——它是孤儿子进程。
            sleep(Duration::from_secs(15));
            exit(0);
        }
        other => {
            eprintln!("fake-power: 不认识 {other:?}");
            exit(9);
        }
    }
}
