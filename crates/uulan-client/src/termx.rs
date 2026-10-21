//! 控制端的终端（M4 的客户端半边）。
//!
//! 命令行用法（`uulan-client`）：
//!
//! ```text
//! --term                       在远端开一个 shell（交互）
//! --term-size 140x40           窗口行列（不写就按本机终端大小，问不到用 120x30）
//! --term-shell /bin/bash       指定远端 shell（不写用被控端配置/环境里的）
//! ```
//!
//! 和 `--ls/--put` 一样**不开窗口、不收画面**：连上、鉴权、开 PTY、把 stdin/stdout 接上去。
//!
//! ## 两端是怎么分工的
//!
//! 协议上终端只有「控制通道上的 JSON」一条路（DESIGN 5.2）：输出 `term_out` 带 base64，
//! 输入 `term_input` 带 base64，窗口大小 `term_resize`。原因是 PTY 的输出可能是任意字节
//! （半截 UTF-8、ANSI 转义），塞不进 JSON 的裸字符串。
//!
//! [`TermLink`] 是**可测的那一半**：它只管「字节 ↔ 协议消息」，不碰本机 stdin/stdout。
//! `run()` 才把真终端接上去。端到端测试用 `TermLink`，所以「协议接得对不对」是能自动验的。
//!
//! ## 本机终端要进 raw 模式
//!
//! 否则方向键、Ctrl-C、`vim` 全都动不了（行缓冲会把它们吃掉，等到回车才发）。
//! unix 上用 `cfmakeraw`；**Windows 控制端暂未接**（要 `SetConsoleMode`，见 DESIGN 的已知限制）。

use std::io::{Read, Write};

use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, info};

use uulan_protocol::frame::{read_frame, write_frame, FrameKind};
use uulan_protocol::msg::{C2H, H2C, TermOp};

/// 开会话时用的参数。`cols`/`rows` 为 0 表示「按本机终端大小来」。
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub shell: Option<String>,
    pub cols: u16,
    pub rows: u16,
}

impl Options {
    /// 分辨率定下来：给了就用，没给就问本机终端，问不到用 120x30。
    fn resolved(mut self) -> Self {
        if self.cols == 0 || self.rows == 0 {
            let (c, r) = detect_size().unwrap_or((120, 30));
            self.cols = c;
            self.rows = r;
        }
        self
    }
}

/// 终端会话里会发生的事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ev {
    /// shell 的原始输出（一般直接写 stdout）。
    Output(Vec<u8>),
    /// 会话结束，带退出码。
    Exit(i32),
    /// 被控端报错（打不开 shell、id 不认识、输入不合法…）。
    Error(String),
}

/// 要发给被控端的东西：控制消息，或者心跳回应。
enum ToHost {
    Json(C2H),
    Pong(bytes::Bytes),
}

/// 一条已建立的终端会话（可测的一半：只做字节 ↔ 协议消息）。
pub struct TermLink {
    id: u32,
    tx: mpsc::Sender<ToHost>,
    msgs: mpsc::Receiver<H2C>,
    reader: tokio::task::JoinHandle<()>,
    writer: tokio::task::JoinHandle<()>,
}

impl TermLink {
    /// 在一条已鉴权的控制连接上开会话。
    ///
    /// 会话 id 固定为 1：`--term` 是独立会话（跟 `--forward`/`--ls` 一样），一条连接上只有一个终端，
    /// 不需要协商编号。
    pub async fn open(control: TcpStream, opts: Options) -> Result<Self> {
        let opts = opts.resolved();
        let (rd, mut wr) = control.into_split();
        let (tx, mut rx) = mpsc::channel::<ToHost>(32);
        let (msg_tx, msg_rx) = mpsc::channel::<H2C>(32);

        let writer = tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                let r = match m {
                    ToHost::Json(m) => match m.to_frame() {
                        Ok(f) => write_frame(&mut wr, f.kind, &f.payload).await,
                        Err(e) => {
                            debug!(error = %e, "终端消息序列化失败");
                            continue;
                        }
                    },
                    // 心跳必须回，否则长会话会被中继当成死连接。
                    ToHost::Pong(p) => write_frame(&mut wr, FrameKind::Pong, &p).await,
                };
                if r.is_err() {
                    debug!("控制通道写入失败，终端会话结束");
                    break;
                }
            }
        });

        let pong_tx = tx.clone();
        let reader = tokio::spawn(async move {
            let mut rd = rd;
            loop {
                // 读帧只能在**这一个**任务里 await：它不是取消安全的。
                let f = match read_frame(&mut rd).await {
                    Ok(f) => f,
                    Err(e) => {
                        debug!(error = %e, "控制通道读取结束");
                        break;
                    }
                };
                match f.kind {
                    FrameKind::Json => match f.decode_json::<H2C>() {
                        Ok(m) => {
                            if msg_tx.send(m).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => debug!(error = %e, "终端消息解析失败，已忽略"),
                    },
                    FrameKind::Ping => {
                        let _ = pong_tx.send(ToHost::Pong(f.payload)).await;
                    }
                    _ => {}
                }
            }
        });

        let link = Self { id: 1, tx, msgs: msg_rx, reader, writer };
        link.send(C2H::Term(TermOp::Open {
            id: 1,
            cols: opts.cols,
            rows: opts.rows,
            shell: opts.shell,
        }))
        .await?;
        info!(cols = opts.cols, rows = opts.rows, "已请求开启远端终端");
        Ok(link)
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    pub async fn input(&self, data: &[u8]) -> Result<()> {
        self.send(C2H::Term(TermOp::Input { id: self.id, data: B64.encode(data) })).await
    }

    pub async fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.send(C2H::Term(TermOp::Resize { id: self.id, cols, rows })).await
    }

    pub async fn close(&self) -> Result<()> {
        self.send(C2H::Term(TermOp::Close { id: self.id })).await
    }

    async fn send(&self, m: C2H) -> Result<()> {
        self.tx
            .send(ToHost::Json(m))
            .await
            .map_err(|_| anyhow::anyhow!("控制通道已关闭"))
    }

    /// 等下一条与会话有关的事件。别的消息（剪贴板、指标、画面参数）直接跳过。
    pub async fn next_event(&mut self) -> Option<Ev> {
        loop {
            let m = self.msgs.recv().await?;
            match m {
                H2C::TermOut { id, data } if id == self.id => match B64.decode(data.as_bytes()) {
                    Ok(b) => return Some(Ev::Output(b)),
                    // 不静默：显示成乱码比什么都不显示好定位。
                    Err(e) => {
                        return Some(Ev::Error(format!("被控端发来的输出不是合法 base64：{e}")))
                    }
                },
                H2C::TermExit { id, code } if id == self.id => return Some(Ev::Exit(code)),
                H2C::TermError { id, error } if id == self.id => return Some(Ev::Error(error)),
                H2C::Bye { reason } => return Some(Ev::Error(format!("被控端道别：{reason}"))),
                other => debug!(kind = ?std::mem::discriminant(&other), "终端会话收到无关消息，已忽略"),
            }
        }
    }

    /// 收摊：先关连接，再把两个任务收回来（别让它们悬在运行时里）。
    pub async fn finish(mut self) {
        drop(self.tx);
        self.reader.abort();
        self.writer.abort();
        let _ = (&mut self.reader).await;
    }
}

/// 交互式终端：把本机 stdin/stdout 接到远端 PTY 上，返回远端 shell 的退出码。
pub async fn run(control: TcpStream, opts: Options) -> Result<i32> {
    let mut link = TermLink::open(control, opts).await?;

    // raw 模式必须在**本函数作用域**里进出：Drop 时还原，否则崩溃留下一个不回显的终端。
    let _raw = RawMode::enter();

    // stdin 用 std 线程读（tokio 的 stdin 在 Windows 上不可用，而且这里本来就是阻塞语义）。
    let (in_tx, mut in_rx) = mpsc::channel::<Option<Vec<u8>>>(32);
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let stdin = std::io::stdin();
        let mut h = stdin.lock();
        loop {
            match h.read(&mut buf) {
                Ok(0) => {
                    let _ = in_tx.blocking_send(None);
                    return;
                }
                Ok(n) => {
                    if in_tx.blocking_send(Some(buf[..n].to_vec())).is_err() {
                        return;
                    }
                }
                Err(_) => {
                    let _ = in_tx.blocking_send(None);
                    return;
                }
            }
        }
    });

    let mut stdin_done = false;
    let code = loop {
        tokio::select! {
            ev = link.next_event() => match ev {
                Some(Ev::Output(b)) => {
                    let out = std::io::stdout();
                    let mut out = out.lock();
                    out.write_all(&b).context("写本机终端失败")?;
                    out.flush().ok();
                }
                Some(Ev::Exit(code)) => break code,
                Some(Ev::Error(e)) => {
                    eprintln!("\r\nuulan 终端：{e}");
                    break 1;
                }
                None => {
                    eprintln!("\r\nuulan 终端：连接已断开");
                    break 1;
                }
            },
            // stdin 关掉之后这一支就不再参与 select（否则会一直抢到 EOF）。
            inp = in_rx.recv(), if !stdin_done => match inp {
                Some(Some(b)) => link.input(&b).await?,
                Some(None) | None => {
                    stdin_done = true;
                    // 本机 stdin 没了（管道结束 / 用户 Ctrl-D）：礼貌地关远端会话，
                    // 然后继续收尾 —— 退出码还是要报出来的。
                    link.close().await?;
                }
            },
        }
    };

    link.finish().await;
    Ok(code)
}

// ───────────────────────────── 本机终端 raw 模式 ─────────────────────────────

/// 进 raw 模式，`Drop` 时还原（`saved` 为 `None` 表示没改过，什么都没得还原）。
struct RawMode {
    #[cfg(unix)]
    saved: Option<libc::termios>,
}

impl RawMode {
    fn enter() -> Self {
        #[cfg(unix)]
        {
            // SAFETY: 只碰 fd 0 的 termios；`saved` 是读回来的原值，Drop 时原样写回。
            unsafe {
                let mut t: libc::termios = std::mem::zeroed();
                if libc::tcgetattr(0, &mut t) == 0 {
                    let saved = t;
                    libc::cfmakeraw(&mut t);
                    if libc::tcsetattr(0, libc::TCSANOW, &t) == 0 {
                        return RawMode { saved: Some(saved) };
                    }
                }
            }
            // 不是 tty（管道 / 重定向 / Windows）：不进 raw，也不报错 ——
            // 否则脚本化验证根本没法跑（这正是端到端测试用的路径）。
            debug!("stdin 不是 tty，跳过 raw 模式");
            RawMode { saved: None }
        }
        #[cfg(not(unix))]
        {
            // Windows 控制端暂未接 SetConsoleMode。键盘仍可用，但方向键/Ctrl-C
            // 要等回车才发出（见 DESIGN 的已知限制）。
            debug!("Windows 控制端暂未进入原始模式");
            RawMode {}
        }
    }
}

#[cfg(unix)]
impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(t) = self.saved {
            // SAFETY: `saved` 是 enter() 从 fd 0 读出来的原值。
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &t);
            }
        }
    }
}

/// 问本机终端有多大。问不到返回 `None`（调用方用 120x30 兜底）。
fn detect_size() -> Option<(u16, u16)> {
    #[cfg(unix)]
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(0, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
            return Some((ws.ws_col, ws.ws_row));
        }
        None
    }
    #[cfg(not(unix))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_auto_size_never_returns_zero() {
        // 0x0 = 「按本机终端来」。测试环境里问不到大小，必须是兜底值而不是 0 ——
        // 0 行列会让远端 PTY 里的程序排版崩掉。
        let o = Options { shell: None, cols: 0, rows: 0 }.resolved();
        assert!(o.cols >= 1 && o.rows >= 1, "auto 之后的尺寸不能是 0：{o:?}");
        assert_eq!(o.cols, 120);
        assert_eq!(o.rows, 30);
    }

    #[test]
    fn options_keep_explicit_size() {
        let o = Options { shell: Some("/bin/sh".into()), cols: 100, rows: 40 }.resolved();
        assert_eq!((o.cols, o.rows), (100, 40));
    }

    #[test]
    fn events_are_comparable() {
        // 测试里要按值比较事件（`assert_eq!(ev, Ev::Exit(0))`）。
        assert_eq!(Ev::Output(b"hi".to_vec()), Ev::Output(b"hi".to_vec()));
        assert_ne!(Ev::Exit(0), Ev::Exit(1));
    }
}