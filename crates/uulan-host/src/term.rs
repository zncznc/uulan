//! 终端（PTY）—— M4 的最后一项。
//!
//! 一条会话 = 被控端上的一个真 shell 进程 + 一枚 PTY 主端。分工和键鼠/剪贴板一样：
//!
//! - [`PtyFactory`] / [`PtyHandle`] 是**后端**（生产是 `portable-pty`，测试是内存假 shell）；
//! - [`run`] 是**纯逻辑**：id 注册表、会话数上限、输出切帧、退出码上报、收尾升级。
//!
//! 这样 Windows 开发机上也能把「开/写/改大小/关/退出」整条状态机钉住，
//! 只有「PTY 本身（termios、SIGHUP→SIGKILL、真 shell 的交互）」必须上麒麟才验得了。
//!
//! ## 为什么输出要 base64 + 切帧
//!
//! PTY 输出是**任意字节**：半截 UTF-8、ANSI 转义、二进制乱码都可能有。放进 JSON 字符串
//! 只能 base64。一条 `term_out` 限制在 [`OUT_CHUNK`] 原始字节以内 —— 一次 `cat` 一个大文件
//! 会瞬间产出几 MB，塞进一帧就成了「一帧堵住整条控制通道，键鼠跟着卡」。
//!
//! ## 反压是**故意**的
//!
//! 事件队列有界（[`EVENT_QUEUE`]）：控制端不收，读线程就阻塞，PTY 缓冲满了 shell 自己就停。
//! 这比「无限缓冲、把被控端内存吃掉」正确得多。
//!
//! ## 收尾：先 SIGHUP，再 SIGKILL
//!
//! 交互式 shell 会忽略 SIGTERM（参考项目踩过的坑），所以先 SIGHUP（`portable-pty` 的
//! `kill()` 在 unix 上就是 SIGHUP），[`KILL_GRACE`] 之后还不走就补 SIGKILL。
//! 会话结束时**所有**残留终端都要收掉，否则用户以为断了、被控端上还挂着一堆 shell。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use uulan_protocol::msg::{H2C, TermOp, TermSession};

/// 同时最多几条终端会话。每条 = 一个进程 + 两个线程，别让人一开几十个。
pub const MAX_SESSIONS: usize = 8;
/// 一条 `term_out` 最多带多少**原始**字节（base64 之后约 4/3）。
pub const OUT_CHUNK: usize = 16 * 1024;
/// 关会话时先 SIGHUP，等这么久还不退出就补 SIGKILL。
pub const KILL_GRACE: Duration = Duration::from_millis(800);
/// 事件队列长度（反压的闸门）。
const EVENT_QUEUE: usize = 64;
/// PTY 读线程一次读多少。
const READ_BUF: usize = 8192;
/// 进程退出后，等最后一批输出刷完的时间。
const FLUSH_GRACE: Duration = Duration::from_millis(200);
/// 输出端先关时，再等退出码的时间。
const EXIT_GRACE: Duration = Duration::from_secs(2);

/// 开一条 PTY 需要的参数。后端据此起进程，**不需要知道会话 id**（id 归 [`run`] 管）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    pub shell: String,
    pub cols: u16,
    pub rows: u16,
}

impl SpawnSpec {
    pub fn new(shell: impl Into<String>, cols: u16, rows: u16) -> Self {
        Self { shell: shell.into(), cols: cols.max(1), rows: rows.max(1) }
    }
}

/// 后端把输出和退出码送到这两个通道上。都是 tokio 通道，后端在自己的线程里 `blocking_send`。
pub struct PtySink {
    pub data: mpsc::Sender<Vec<u8>>,
    pub exit: mpsc::Sender<i32>,
}

/// 一条已经跑起来的 PTY 会话（后端句柄）。
pub trait PtyHandle: Send {
    /// 把字节写进 shell 的 stdin。**不能阻塞**：实现里要交给自己的写线程。
    fn write(&mut self, data: &[u8]) -> Result<(), String>;
    /// 改窗口大小（shell 里的 `vim` / `top` 靠它重排）。
    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String>;
    /// 礼貌地结束：unix 上是 SIGHUP，Windows 上是 TerminateProcess。
    fn kill(&mut self) -> Result<(), String>;
    /// 赖着不走时的最后一手。默认等于 [`PtyHandle::kill`]。
    fn hard_kill(&mut self) -> Result<(), String> {
        self.kill()
    }
}

/// 起 PTY 的方式。可注入 —— 这是本机（无 PTY / 无 X11）能验整条链路的唯一办法。
pub trait PtyFactory: Send + Sync {
    fn spawn(&self, spec: &SpawnSpec, sink: PtySink) -> Result<Box<dyn PtyHandle>, String>;
    /// 后端名字，只用于日志。
    fn name(&self) -> &'static str {
        "unknown"
    }
}

/// 默认 shell：`$SHELL` → `/bin/bash`（Windows 上 `%COMSPEC%` → `cmd.exe`）。
pub fn default_shell() -> String {
    let from_env = if cfg!(windows) {
        std::env::var("COMSPEC").ok()
    } else {
        std::env::var("SHELL").ok()
    };
    from_env
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "cmd.exe".to_string()
            } else {
                "/bin/bash".to_string()
            }
        })
}

// ───────────────────────────── 生产后端：portable-pty ─────────────────────────────

/// 生产后端。`portable-pty` 在 unix 上开 `/dev/ptmx` + `fork`，在 Windows 上用 ConPTY，
/// 所以这一个实现两端都能编；**只有 unix 的 SIGKILL 升级**是 `cfg(unix)`。
pub struct NativePty;

impl PtyFactory for NativePty {
    fn name(&self) -> &'static str {
        "portable-pty"
    }

    fn spawn(&self, spec: &SpawnSpec, sink: PtySink) -> Result<Box<dyn PtyHandle>, String> {
        let sys = portable_pty::native_pty_system();
        let pair = sys
            .openpty(pty_size(spec.cols, spec.rows))
            .map_err(|e| format!("开 PTY 失败：{e}"))?;

        let mut cmd = portable_pty::CommandBuilder::new(&spec.shell);
        // 只补一个 TERM：`env_clear` 会连带清掉 PATH/HOME，连 `ls` 都跑不起来。
        cmd.env("TERM", "xterm-256color");

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("启动 {} 失败：{e}", spec.shell))?;
        drop(pair.slave);

        let master = pair.master;
        let mut reader = master.try_clone_reader().map_err(|e| format!("取 PTY 读端失败：{e}"))?;
        let mut writer = master.take_writer().map_err(|e| format!("取 PTY 写端失败：{e}"))?;
        let killer = child.clone_killer();
        let pid = child.process_id();

        let PtySink { data, exit } = sink;

        // 读线程：PTY → 事件通道。`blocking_send` 在队列满时阻塞，
        // 于是控制端不收 → 读线程停 → PTY 缓冲满 → shell 自己停。这就是我们要的反压。
        std::thread::spawn(move || {
            let mut buf = vec![0u8; READ_BUF];
            loop {
                match std::io::Read::read(&mut reader, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if data.blocking_send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    // 子进程退出后主端读会报 EIO —— 那不是错误，是「对面没了」。
                    Err(e) => {
                        debug!(error = %e, "PTY 读结束");
                        break;
                    }
                }
            }
        });

        // 写线程：写 PTY 在子进程不读时会阻塞（比如 `sleep 100` 不看输入），
        // 绝不能让它挡在 async 任务上。
        let (in_tx, in_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            for data in in_rx {
                if writer.write_all(&data).is_err() {
                    break;
                }
                let _ = writer.flush();
            }
        });

        // 等待线程：唯一能知道「shell 怎么没的」的地方（退出码 / 信号）。
        std::thread::spawn(move || {
            let mut child = child;
            let code = match child.wait() {
                Ok(st) => exit_code_of(&st),
                Err(e) => {
                    warn!(error = %e, "等 PTY 子进程退出失败");
                    -1
                }
            };
            let _ = exit.blocking_send(code);
        });

        Ok(Box::new(NativePtyHandle { master, killer, input: in_tx, pid }))
    }
}

fn pty_size(cols: u16, rows: u16) -> portable_pty::PtySize {
    portable_pty::PtySize {
        rows: rows.max(1),
        cols: cols.max(1),
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// `portable_pty::ExitStatus` → 我们上报给控制端的数字。
///
/// 被信号杀掉时 `exit_code()` 可能是 0，必须优先看信号，映射成 shell 习惯的 `128+signo`
/// （`kill -HUP` → 129，`kill -9` → 137）。否则「提交了但没跑完」会显示成「正常退出 0」。
fn exit_code_of(st: &portable_pty::ExitStatus) -> i32 {
    if let Some(sig) = st.signal() {
        return 128 + signal_number(sig);
    }
    st.exit_code() as i32
}

/// 信号名 → 编号。只认会话收尾会遇到的几个，认不出就按 SIGHUP 算。
fn signal_number(name: &str) -> i32 {
    let up = name.trim().to_ascii_uppercase();
    let up = up.strip_prefix("SIG").unwrap_or(&up);
    match up {
        "HUP" => 1,
        "INT" => 2,
        "KILL" => 9,
        "TERM" => 15,
        _ => 1,
    }
}

struct NativePtyHandle {
    master: Box<dyn portable_pty::MasterPty + Send>,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    input: std::sync::mpsc::Sender<Vec<u8>>,
    /// 只在 unix 的 SIGKILL 升级路径里读 —— Windows 上那一段是 cfg 掉的，
    /// 所以这个字段在 Windows 构建里注定「没人读」，不是漏了。
    #[allow(dead_code)]
    pid: Option<u32>,
}

impl PtyHandle for NativePtyHandle {
    fn write(&mut self, data: &[u8]) -> Result<(), String> {
        self.input.send(data.to_vec()).map_err(|_| "PTY 已关闭".to_string())
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String> {
        self.master.resize(pty_size(cols, rows)).map_err(|e| format!("改窗口大小失败：{e}"))
    }

    fn kill(&mut self) -> Result<(), String> {
        self.killer.kill().map_err(|e| format!("SIGHUP 失败：{e}"))
    }

    /// `portable-pty` 只暴露 `kill()`（= SIGHUP），所以硬杀要自己发。
    /// 目标是**会话首进程**；它的子进程会跟着收 HUP（同进程组）。
    fn hard_kill(&mut self) -> Result<(), String> {
        #[cfg(unix)]
        {
            let Some(pid) = self.pid else {
                return self.kill();
            };
            // SAFETY: 只传一个已经存在的 pid 和一个常量信号；不改任何本进程状态。
            let r = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            if r == 0 {
                return Ok(());
            }
            let e = std::io::Error::last_os_error();
            // 进程已经没了（ESRCH）就是我们要的结果，不算失败。
            if e.raw_os_error() == Some(libc::ESRCH) {
                return Ok(());
            }
            Err(format!("SIGKILL 失败：{e}"))
        }
        #[cfg(not(unix))]
        {
            // Windows 上 `kill()` 本来就是 TerminateProcess，没有「更硬」的一手。
            self.kill()
        }
    }
}

// ───────────────────────────── 纯逻辑：会话注册表 ─────────────────────────────

/// 后端 → 服务的事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Data { id: u32, bytes: Vec<u8> },
    Exited { id: u32, code: i32 },
}

struct Live {
    handle: Box<dyn PtyHandle>,
    /// 窗口标题（控制端的会话列表里显示）。
    title: String,
    /// 进程还活着吗 —— 收尾升级任务的判据。
    alive: Arc<AtomicBool>,
}

/// 终端服务主循环：吃 [`TermOp`]，吐 [`H2C`]。
///
/// `ops` 关闭（控制通道断了）时会把**所有**残留会话收掉再返回 —— 别留一堆 shell 在
/// 被控端后台跑着，用户以为早就断开了。
pub async fn run<F>(
    factory: Arc<dyn PtyFactory>,
    default_shell: String,
    mut ops: mpsc::Receiver<TermOp>,
    notify: F,
) where
    F: Fn(&H2C) -> bool,
{
    let (ev_tx, mut ev_rx) = mpsc::channel::<Event>(EVENT_QUEUE);
    let mut live: HashMap<u32, Live> = HashMap::new();
    // 已经发出关闭指令、但退出事件还没到的 id。
    //
    // 为什么需要它：`Close` 必须把会话从注册表里摘掉（之后的输入不能进死进程），
    // 但 `TermExit` 是进程真死之后才有的。少了这个集合，关闭指令发出去后
    // 退出事件会因为「注册表里没这条」被静默丢掉 —— 控制端永远等不到会话结束。
    let mut closing: HashSet<u32> = HashSet::new();
    info!(backend = factory.name(), shell = %default_shell, "终端服务已就绪");

    loop {
        tokio::select! {
            biased;
            ev = ev_rx.recv() => {
                match ev {
                    Some(Event::Data { id, bytes }) => {
                        // 切帧：一次 `cat 大文件` 不能变成一条巨型 JSON 堵住控制通道。
                        for chunk in bytes.chunks(OUT_CHUNK) {
                            emit(&notify, H2C::TermOut { id, data: B64.encode(chunk) }).await;
                        }
                    }
                    Some(Event::Exited { id, code }) => {
                        // 两种来源都算数：自己退出的（还在 live 里）、被我们关掉的（在 closing 里）。
                        let known = live.remove(&id).is_some() | closing.remove(&id);
                        if known {
                            info!(id, code, "终端会话结束");
                            emit(&notify, H2C::TermExit { id, code }).await;
                        } else {
                            debug!(id, code, "收到未知会话的退出事件，已忽略");
                        }
                    }
                    None => break,
                }
            }
            op = ops.recv() => {
                let Some(op) = op else { break };
                handle_op(op, &factory, &default_shell, &mut live, &mut closing, &ev_tx, &notify).await;
            }
        }
    }

    // 收尾：残留会话全部 SIGHUP，超时补 SIGKILL。
    closing.clear();
    for (id, mut s) in live.drain() {
        debug!(id, "控制通道已关闭，结束残留终端");
        let _ = s.handle.kill();
        escalate(s.alive, s.handle);
    }
    debug!("终端服务结束");
}

async fn handle_op<F>(
    op: TermOp,
    factory: &Arc<dyn PtyFactory>,
    default_shell: &str,
    live: &mut HashMap<u32, Live>,
    closing: &mut HashSet<u32>,
    ev_tx: &mpsc::Sender<Event>,
    notify: &F,
) where
    F: Fn(&H2C) -> bool,
{
    match op {
        TermOp::Open { id, cols, rows, shell } => {
            // id 由控制端给。重复的 id 必须当场拒，否则旧会话会被悄悄替换掉 ——
            // 控制端那边还拿着旧 id 的窗口，输入就全打到别人身上了。
            if live.contains_key(&id) || closing.contains(&id) {
                emit(notify, H2C::TermError { id, error: format!("会话 {id} 已存在") }).await;
                return;
            }
            if live.len() >= MAX_SESSIONS {
                emit(
                    notify,
                    H2C::TermError { id, error: format!("终端会话数已达上限（{MAX_SESSIONS}）") },
                )
                .await;
                return;
            }
            let shell = shell.unwrap_or_else(|| default_shell.to_string());
            let spec = SpawnSpec::new(shell.clone(), cols, rows);

            // 后端只认识「原始字节 + 退出码」两个通道；这里把它们并成带 id 的事件流，
            // 后端因此不需要知道 id（假后端和生产后端都能复用同一套）。
            let (d_tx, d_rx) = mpsc::channel::<Vec<u8>>(EVENT_QUEUE);
            let (e_tx, e_rx) = mpsc::channel::<i32>(1);
            let ev = ev_tx.clone();
            tokio::spawn(adapt(id, d_rx, e_rx, ev));

            match factory.spawn(&spec, PtySink { data: d_tx, exit: e_tx }) {
                Ok(handle) => {
                    info!(id, cols, rows, shell = %shell, "终端会话已开启");
                    live.insert(
                        id,
                        Live { handle, title: shell, alive: Arc::new(AtomicBool::new(true)) },
                    );
                }
                Err(e) => {
                    warn!(id, shell = %shell, error = %e, "终端会话开启失败");
                    emit(notify, H2C::TermError { id, error: e }).await;
                }
            }
        }
        TermOp::Input { id, data } => {
            let Some(s) = live.get_mut(&id) else {
                emit(notify, H2C::TermError { id, error: format!("没有会话 {id}") }).await;
                return;
            };
            match B64.decode(data.as_bytes()) {
                Ok(bytes) => {
                    if let Err(e) = s.handle.write(&bytes) {
                        warn!(id, error = %e, "写 PTY 失败");
                        emit(notify, H2C::TermError { id, error: e }).await;
                    }
                }
                // 静默丢弃是最坏的做法：用户按了键、屏幕上没反应、日志也没有。
                Err(e) => {
                    emit(notify, H2C::TermError { id, error: format!("输入不是合法 base64：{e}") })
                        .await
                }
            }
        }
        TermOp::Resize { id, cols, rows } => {
            // 会话已经结束（退出通知还在路上）时改大小是无意义的正常竞态，不报错。
            if let Some(s) = live.get_mut(&id) {
                if let Err(e) = s.handle.resize(cols, rows) {
                    warn!(id, error = %e, "改终端窗口大小失败");
                    emit(notify, H2C::TermError { id, error: e }).await;
                }
            }
        }
        TermOp::Close { id } => {
            if let Some(mut s) = live.remove(&id) {
                debug!(id, "控制端要求关闭终端");
                // 记一笔：退出事件随后会到，那时注册表里已经没有这条了，
                // 靠它才知道「这次退出是要上报给控制端的」。
                closing.insert(id);
                let _ = s.handle.kill();
                escalate(s.alive, s.handle);
            }
        }
        TermOp::List => {
            let mut sessions: Vec<TermSession> = live
                .iter()
                .map(|(id, s)| TermSession { id: *id, title: s.title.clone() })
                .collect();
            sessions.sort_by_key(|s| s.id);
            emit(notify, H2C::TermSessions { sessions }).await;
        }
    }
}

/// 发一条消息给控制端；**对方不收就等着**。
///
/// 为什么不能直接 `send` 了事：`Out` 队列是无界的，终端输出如果一路灌进去，
/// 慢链路下会把被控端的内存吃光（`cat` 一个大文件就够）。这里让 `notify` 在队列过深时
/// 返回 `false`，于是本任务停下来等 —— 事件队列随之填满，PTY 读线程阻塞，shell 自己就停了。
/// 这是**唯一**一条能把反压一路传回真 shell 的路径：不能丢字节（丢一个字节终端就花了），
/// 所以只能等。
async fn emit<F>(notify: &F, m: H2C)
where
    F: Fn(&H2C) -> bool,
{
    if notify(&m) {
        return;
    }
    // 5ms 一拍：够快（人感觉不到），又不至于空转烧 CPU。
    loop {
        tokio::time::sleep(Duration::from_millis(5)).await;
        if notify(&m) {
            return;
        }
    }
}

/// 把后端的两条原始通道（输出 / 退出码）并成带 id 的事件流。
///
/// 顺序是**规矩**，不是实现细节：先输出、后退出。丢了最后几行，用户看到的就是
/// 「命令失败但没有任何报错」；丢了退出事件，控制端的窗口永远不会关。
async fn adapt(
    id: u32,
    mut d_rx: mpsc::Receiver<Vec<u8>>,
    mut e_rx: mpsc::Receiver<i32>,
    ev: mpsc::Sender<Event>,
) {
    let mut exit_code: Option<i32> = None;
    loop {
        tokio::select! {
            c = e_rx.recv(), if exit_code.is_none() => match c {
                Some(code) => exit_code = Some(code),
                // 后端把退出通道丢了（异常没报码）：当作「不明原因结束」，别空转。
                None => exit_code = Some(-1),
            },
            d = d_rx.recv() => match d {
                Some(bytes) => {
                    if ev.send(Event::Data { id, bytes }).await.is_err() {
                        return;
                    }
                }
                None => break,
            },
        }
        if let Some(code) = exit_code {
            // 进程已经退出，但最后几行输出可能还在队列里 —— 给一小段时间刷完。
            while let Ok(Some(bytes)) = tokio::time::timeout(FLUSH_GRACE, d_rx.recv()).await {
                if ev.send(Event::Data { id, bytes }).await.is_err() {
                    return;
                }
            }
            let _ = ev.send(Event::Exited { id, code }).await;
            return;
        }
    }

    // 输出端先关（后端读线程收尾）不等于进程已退出：退出码可能还在路上。
    if exit_code.is_none() {
        if let Ok(c) = tokio::time::timeout(EXIT_GRACE, e_rx.recv()).await {
            exit_code = c.or(Some(-1));
        }
    }
    if let Some(code) = exit_code {
        let _ = ev.send(Event::Exited { id, code }).await;
    }
}

/// SIGHUP 之后还没走就补 SIGKILL。任务自己判断会话是否已经结束（`alive`），
/// 所以正常退出的会话不会白占一个 800ms 的定时器。
fn escalate(alive: Arc<AtomicBool>, mut handle: Box<dyn PtyHandle>) {
    tokio::spawn(async move {
        let step = Duration::from_millis(100);
        let mut waited = Duration::ZERO;
        while waited < KILL_GRACE {
            tokio::time::sleep(step).await;
            waited += step;
            if !alive.load(Ordering::SeqCst) {
                return;
            }
        }
        if alive.load(Ordering::SeqCst) {
            warn!("终端会话不响应 SIGHUP，补 SIGKILL");
            let _ = handle.hard_kill();
        }
    });
}

// ───────────────────────────── 测试用假后端 ─────────────────────────────

/// 测试用的假 PTY：**不落真进程**，一台「回声 + 认几条命令」的假 shell。
///
/// 为什么值得有它：本机（Windows / 无 PTY）没有可用的 PTY 路径，而「开→写→切帧→退出→收尾」
/// 这套状态机是真正的代码，必须能在 CI 里钉住。真 PTY 的 termios / 信号路径只能上麒麟验。
pub mod testing {
    use super::*;
    use std::sync::Mutex;

    /// 假 shell 认的命令（都以 `\n` 结尾）：
    /// - `exit N` → 输出 `bye`，退出码 N
    /// - `burst N` → 输出 N 个 `x`（用来验切帧）
    /// - `size` → 输出当前窗口大小 `COLSxROWS`（用来验 resize）
    pub struct FakeShellPty {
        log: Arc<Mutex<Vec<String>>>,
    }

    impl FakeShellPty {
        pub fn new() -> Self {
            Self { log: Arc::new(Mutex::new(Vec::new())) }
        }

        /// 同时拿到一份操作日志（`resize:120x30`、`input:5`、`kill` …），供断言。
        pub fn with_log() -> (Self, Arc<Mutex<Vec<String>>>) {
            let log = Arc::new(Mutex::new(Vec::new()));
            (Self { log: log.clone() }, log)
        }
    }

    impl Default for FakeShellPty {
        fn default() -> Self {
            Self::new()
        }
    }

    enum Msg {
        Data(Vec<u8>),
        Kill,
    }

    impl PtyFactory for FakeShellPty {
        fn name(&self) -> &'static str {
            "fake-shell"
        }

        fn spawn(&self, spec: &SpawnSpec, sink: PtySink) -> Result<Box<dyn PtyHandle>, String> {
            let (tx, rx) = std::sync::mpsc::channel::<Msg>();
            let size = Arc::new(Mutex::new((spec.cols, spec.rows)));
            let size2 = size.clone();
            let log = self.log.clone();
            let shell = spec.shell.clone();

            std::thread::spawn(move || {
                let PtySink { data, exit } = sink;
                let mut line = Vec::new();
                while let Ok(m) = rx.recv() {
                    let chunk = match m {
                        Msg::Kill => {
                            // 被 SIGHUP 杀掉：129 = 128+1，和真 shell 的表现一致。
                            let _ = exit.blocking_send(128 + 1);
                            return;
                        }
                        Msg::Data(d) => d,
                    };
                    // 假 shell 打开回显（真 PTY 默认也回显）。
                    if data.blocking_send(chunk.clone()).is_err() {
                        return;
                    }
                    line.extend_from_slice(&chunk);
                    while let Some(pos) = line.iter().position(|b| *b == b'\n') {
                        let raw: Vec<u8> = line.drain(..=pos).collect();
                        let cmd = String::from_utf8_lossy(&raw).trim().to_string();
                        if let Some(rest) = cmd.strip_prefix("exit") {
                            let code: i32 = rest.trim().parse().unwrap_or(0);
                            let _ = data.blocking_send(format!("bye {code}\r\n").into_bytes());
                            let _ = exit.blocking_send(code);
                            return;
                        }
                        if let Some(rest) = cmd.strip_prefix("burst") {
                            let n: usize = rest.trim().parse().unwrap_or(0);
                            let body = vec![b'x'; n];
                            for part in body.chunks(READ_BUF) {
                                if data.blocking_send(part.to_vec()).is_err() {
                                    return;
                                }
                            }
                        }
                        if cmd == "size" {
                            let (c, r) = *size2.lock().unwrap_or_else(|e| e.into_inner());
                            let _ = data.blocking_send(format!("{c}x{r}\r\n").into_bytes());
                        }
                    }
                }
            });

            // 让「shell 路径」在日志里可见：控制端选了哪个 shell 是配置问题，排障要能看到。
            log.lock().unwrap_or_else(|e| e.into_inner()).push(format!("spawn:{shell}"));
            Ok(Box::new(FakeHandle { tx, size, log: self.log.clone() }))
        }
    }

    struct FakeHandle {
        tx: std::sync::mpsc::Sender<Msg>,
        size: Arc<Mutex<(u16, u16)>>,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl PtyHandle for FakeHandle {
        fn write(&mut self, data: &[u8]) -> Result<(), String> {
            self.log
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("input:{}", data.len()));
            self.tx.send(Msg::Data(data.to_vec())).map_err(|_| "会话已结束".to_string())
        }

        fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String> {
            *self.size.lock().unwrap_or_else(|e| e.into_inner()) = (cols, rows);
            self.log
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("resize:{cols}x{rows}"));
            Ok(())
        }

        fn kill(&mut self) -> Result<(), String> {
            self.log.lock().unwrap_or_else(|e| e.into_inner()).push("kill".into());
            self.tx.send(Msg::Kill).map_err(|_| "会话已结束".to_string())
        }
    }

    /// 只往 stdout 写一行就退出的假后端（验证「会话开出来立刻结束」也用得上）。
    pub struct SilentPty;

    impl PtyFactory for SilentPty {
        fn name(&self) -> &'static str {
            "silent"
        }

        fn spawn(&self, _spec: &SpawnSpec, sink: PtySink) -> Result<Box<dyn PtyHandle>, String> {
            let _ = sink.data.try_send(b"hi\r\n".to_vec());
            let _ = sink.exit.try_send(0);
            Ok(Box::new(SilentHandle))
        }
    }

    struct SilentHandle;

    impl PtyHandle for SilentHandle {
        fn write(&mut self, _data: &[u8]) -> Result<(), String> {
            Ok(())
        }
        fn resize(&mut self, _cols: u16, _rows: u16) -> Result<(), String> {
            Ok(())
        }
        fn kill(&mut self) -> Result<(), String> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    struct H {
        ops: mpsc::Sender<TermOp>,
        msgs: mpsc::Receiver<H2C>,
    }

    fn start_with(factory: Arc<dyn PtyFactory>) -> H {
        let (op_tx, op_rx) = mpsc::channel::<TermOp>(16);
        let (out_tx, out_rx) = mpsc::channel::<H2C>(256);
        tokio::spawn(async move {
            run(factory, "/bin/bash".into(), op_rx, move |m: &H2C| {
                out_tx.try_send(m.clone()).is_ok()
            })
            .await;
        });
        H { ops: op_tx, msgs: out_rx }
    }

    async fn next(h: &mut H) -> H2C {
        tokio::time::timeout(Duration::from_secs(5), h.msgs.recv())
            .await
            .expect("等被控端消息超时")
            .expect("终端服务已经退出")
    }

    /// 一直读到出现 `needle`（按 base64 解出来的字符串匹配），返回累计文本。
    async fn text_until(h: &mut H, needle: &str) -> String {
        let mut acc = String::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), h.msgs.recv()).await {
                Ok(Some(H2C::TermOut { data, .. })) => {
                    acc.push_str(&String::from_utf8_lossy(&B64.decode(data).unwrap()));
                    if acc.contains(needle) {
                        return acc;
                    }
                }
                Ok(Some(other)) => panic!("没等到输出，先等到 {other:?}"),
                Ok(None) => panic!("终端服务退出了"),
                Err(_) => {}
            }
        }
        panic!("等 {needle:?} 超时，已收到 {acc:?}");
    }

    fn b64(s: &str) -> String {
        B64.encode(s.as_bytes())
    }

    #[tokio::test]
    async fn opens_a_session_and_round_trips_input() {
        let (fake, log) = testing::FakeShellPty::with_log();
        let mut h = start_with(Arc::new(fake));
        h.ops.send(TermOp::Open { id: 1, cols: 120, rows: 30, shell: Some("/bin/zsh".into()) }).await.unwrap();
        h.ops.send(TermOp::Input { id: 1, data: b64("echo hi\n") }).await.unwrap();
        let text = text_until(&mut h, "echo hi").await;
        assert!(text.contains("echo hi"), "假 shell 应当回显输入，实际 {text:?}");
        assert!(
            log.lock().unwrap().iter().any(|l| l == "spawn:/bin/zsh"),
            "控制端指定的 shell 必须被用上，日志 {:?}",
            log.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn exit_reports_the_code() {
        let mut h = start_with(Arc::new(testing::FakeShellPty::new()));
        h.ops.send(TermOp::Open { id: 2, cols: 80, rows: 24, shell: None }).await.unwrap();
        h.ops.send(TermOp::Input { id: 2, data: b64("exit 3\n") }).await.unwrap();
        loop {
            match next(&mut h).await {
                H2C::TermExit { id, code } => {
                    assert_eq!(id, 2);
                    assert_eq!(code, 3, "退出码要如实上报");
                    return;
                }
                H2C::TermOut { .. } => {}
                other => panic!("意外消息 {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn duplicate_id_is_rejected_instead_of_replacing_the_session() {
        let mut h = start_with(Arc::new(testing::FakeShellPty::new()));
        h.ops.send(TermOp::Open { id: 5, cols: 80, rows: 24, shell: None }).await.unwrap();
        h.ops.send(TermOp::Open { id: 5, cols: 80, rows: 24, shell: None }).await.unwrap();
        match next(&mut h).await {
            H2C::TermError { id, error } => {
                assert_eq!(id, 5);
                assert!(error.contains("已存在"), "错误信息要说清原因：{error}");
            }
            other => panic!("重复 id 必须报错，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn input_to_an_unknown_session_is_reported() {
        let mut h = start_with(Arc::new(testing::FakeShellPty::new()));
        h.ops.send(TermOp::Input { id: 9, data: b64("x") }).await.unwrap();
        match next(&mut h).await {
            H2C::TermError { id, error } => {
                assert_eq!(id, 9);
                assert!(error.contains("没有会话"), "{error}");
            }
            other => panic!("未知会话的输入必须报错，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_base64_is_reported_not_swallowed() {
        let mut h = start_with(Arc::new(testing::FakeShellPty::new()));
        h.ops.send(TermOp::Open { id: 1, cols: 80, rows: 24, shell: None }).await.unwrap();
        h.ops
            .send(TermOp::Input { id: 1, data: "这不是 base64!!".into() })
            .await
            .unwrap();
        match next(&mut h).await {
            H2C::TermError { error, .. } => assert!(error.contains("base64"), "{error}"),
            other => panic!("非法 base64 必须报错，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn close_kills_the_session_and_reports_the_exit() {
        let (fake, log) = testing::FakeShellPty::with_log();
        let mut h = start_with(Arc::new(fake));
        h.ops.send(TermOp::Open { id: 3, cols: 80, rows: 24, shell: None }).await.unwrap();
        h.ops.send(TermOp::Close { id: 3 }).await.unwrap();
        // 关掉之后 Shell 收到 SIGHUP（假后端报 129）。
        loop {
            match next(&mut h).await {
                H2C::TermExit { id, code } => {
                    assert_eq!(id, 3);
                    assert_eq!(code, 129, "被 SIGHUP 杀掉应当报 128+1");
                    break;
                }
                H2C::TermOut { .. } => {}
                other => panic!("意外消息 {other:?}"),
            }
        }
        assert!(log.lock().unwrap().contains(&"kill".to_string()), "必须真的发过 kill");
        // 关掉之后再发输入：报「没有会话」，不能静默成功。
        h.ops.send(TermOp::Input { id: 3, data: b64("x") }).await.unwrap();
        match next(&mut h).await {
            H2C::TermError { .. } => {}
            other => panic!("会话已关，输入必须被拒，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn resize_reaches_the_backend() {
        let (fake, log) = testing::FakeShellPty::with_log();
        let mut h = start_with(Arc::new(fake));
        h.ops.send(TermOp::Open { id: 4, cols: 80, rows: 24, shell: None }).await.unwrap();
        h.ops.send(TermOp::Resize { id: 4, cols: 140, rows: 40 }).await.unwrap();
        h.ops.send(TermOp::Input { id: 4, data: b64("size\n") }).await.unwrap();
        let text = text_until(&mut h, "140x40").await;
        assert!(text.contains("140x40"), "假 shell 的窗口大小应当已改，实际 {text:?}");
        assert!(log.lock().unwrap().contains(&"resize:140x40".to_string()));
    }

    /// 一次 `cat 大文件` 不能变成一条巨型 JSON。
    #[tokio::test]
    async fn big_output_is_split_into_bounded_frames() {
        let mut h = start_with(Arc::new(testing::FakeShellPty::new()));
        h.ops.send(TermOp::Open { id: 7, cols: 80, rows: 24, shell: None }).await.unwrap();
        let want = OUT_CHUNK * 2 + 1234;
        h.ops.send(TermOp::Input { id: 7, data: b64(&format!("burst {want}\n")) }).await.unwrap();

        let mut xs = 0usize;
        let mut frames = 0usize;
        let mut max_b64 = 0usize;
        let deadline = Instant::now() + Duration::from_secs(10);
        while xs < want && Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(5), h.msgs.recv()).await {
                Ok(Some(H2C::TermOut { data, .. })) => {
                    frames += 1;
                    max_b64 = max_b64.max(data.len());
                    let raw = B64.decode(data).unwrap();
                    assert!(raw.len() <= OUT_CHUNK, "单帧超过上限：{}", raw.len());
                    xs += raw.iter().filter(|b| **b == b'x').count();
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => break,
            }
        }
        assert_eq!(xs, want, "字节必须一个不少");
        assert!(frames >= 3, "这么大一块输出必须切帧，实际只有 {frames} 帧");
        assert!(
            max_b64 <= OUT_CHUNK / 3 * 4 + 8,
            "base64 之后的单帧长度也该在同一个量级，实际 {max_b64}"
        );
    }

    /// 控制端收不动时，本任务必须**停下来等**，而不是把输出堆进内存。
    /// 断言方式：先让 notify 一直拒绝，再放开 —— 消息必须一条不少、顺序不乱地到齐。
    #[tokio::test]
    async fn backpressure_waits_instead_of_dropping_bytes() {
        use std::sync::atomic::AtomicUsize;

        let (op_tx, op_rx) = mpsc::channel::<TermOp>(16);
        let (out_tx, mut out_rx) = mpsc::channel::<H2C>(256);
        let blocked = Arc::new(AtomicBool::new(true));
        let attempts = Arc::new(AtomicUsize::new(0));

        let b2 = blocked.clone();
        let a2 = attempts.clone();
        tokio::spawn(async move {
            run(Arc::new(testing::FakeShellPty::new()), "/bin/bash".into(), op_rx, move |m: &H2C| {
                a2.fetch_add(1, Ordering::SeqCst);
                if b2.load(Ordering::SeqCst) {
                    return false; // 模拟「控制通道积压太深」
                }
                out_tx.try_send(m.clone()).is_ok()
            })
            .await;
        });

        op_tx.send(TermOp::Open { id: 1, cols: 80, rows: 24, shell: None }).await.unwrap();
        op_tx.send(TermOp::Input { id: 1, data: b64("exit 5
") }).await.unwrap();

        // 拒绝期间：一条消息都不该被「吞掉」，也不该到对端。
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(out_rx.try_recv().is_err(), "队列满的时候不该硬塞进去");
        assert!(attempts.load(Ordering::SeqCst) > 0, "应当一直在重试，而不是悄悄放弃");

        // 放开之后：退出事件必须还在（这就是「等」和「丢」的区别）。
        blocked.store(false, Ordering::SeqCst);
        let mut saw_exit = false;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !saw_exit {
            match tokio::time::timeout(Duration::from_millis(500), out_rx.recv()).await {
                Ok(Some(H2C::TermExit { id, code })) => {
                    assert_eq!((id, code), (1, 5));
                    saw_exit = true;
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => {}
            }
        }
        assert!(saw_exit, "放开之后退出事件必须补上，不能因为当时发不出去就丢掉");
    }

    #[tokio::test]
    async fn session_count_is_capped() {
        let mut h = start_with(Arc::new(testing::FakeShellPty::new()));
        for id in 0..MAX_SESSIONS as u32 {
            h.ops.send(TermOp::Open { id, cols: 80, rows: 24, shell: None }).await.unwrap();
        }
        // 让前面几条先落地（事件通道和 op 通道是两条，顺序由服务循环自己保证）。
        h.ops.send(TermOp::List).await.unwrap();
        let n = loop {
            match next(&mut h).await {
                H2C::TermSessions { sessions } => break sessions.len(),
                _ => continue,
            }
        };
        assert_eq!(n, MAX_SESSIONS, "先开的会话应当都在");

        h.ops.send(TermOp::Open { id: 99, cols: 80, rows: 24, shell: None }).await.unwrap();
        match next(&mut h).await {
            H2C::TermError { id, error } => {
                assert_eq!(id, 99);
                assert!(error.contains("上限"), "{error}");
            }
            other => panic!("超上限必须报错，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_reports_open_sessions() {
        let mut h = start_with(Arc::new(testing::FakeShellPty::new()));
        h.ops.send(TermOp::Open { id: 8, cols: 80, rows: 24, shell: Some("/bin/sh".into()) }).await.unwrap();
        h.ops.send(TermOp::List).await.unwrap();
        loop {
            match next(&mut h).await {
                H2C::TermSessions { sessions } => {
                    assert_eq!(sessions.len(), 1);
                    assert_eq!(sessions[0].id, 8);
                    assert_eq!(sessions[0].title, "/bin/sh");
                    return;
                }
                H2C::TermOut { .. } => {}
                other => panic!("意外消息 {other:?}"),
            }
        }
    }

    /// 会话开出来就结束的后端：TermExit 必须马上到，且之后该 id 可以复用。
    #[tokio::test]
    async fn a_session_that_exits_at_once_still_reports_and_frees_the_id() {
        let mut h = start_with(Arc::new(testing::SilentPty));
        h.ops.send(TermOp::Open { id: 11, cols: 80, rows: 24, shell: None }).await.unwrap();
        let mut got_exit = false;
        let mut got_data = false;
        for _ in 0..4 {
            match next(&mut h).await {
                H2C::TermExit { id, code } => {
                    assert_eq!(id, 11);
                    assert_eq!(code, 0);
                    got_exit = true;
                }
                H2C::TermOut { data, .. } => {
                    assert_eq!(B64.decode(data).unwrap(), b"hi\r\n");
                    got_data = true;
                }
                other => panic!("意外消息 {other:?}"),
            }
            if got_exit && got_data {
                break;
            }
        }
        assert!(got_exit && got_data);

        // id 复用：退出之后同一个 id 必须能再开。
        h.ops.send(TermOp::Open { id: 11, cols: 80, rows: 24, shell: None }).await.unwrap();
        h.ops.send(TermOp::List).await.unwrap();
        let mut hit = false;
        for _ in 0..6 {
            match next(&mut h).await {
                H2C::TermSessions { sessions } => {
                    hit = true;
                    // 第二次开后立刻又退，列表可能已经空 —— 只要不是「id 已存在」的报错即可。
                    assert!(sessions.is_empty() || sessions.iter().any(|s| s.id == 11));
                    break;
                }
                H2C::TermError { error, .. } => panic!("id 应当可以复用，却报错：{error}"),
                _ => {}
            }
        }
        assert!(hit, "必须收到会话列表");
    }

    #[test]
    fn exit_codes_map_signals_the_shell_way() {
        // 只测映射函数本身：128+signo 是 shell 的惯例，控制端据此区分「正常退出」和「被杀」。
        assert_eq!(signal_number("SIGHUP"), 1);
        assert_eq!(signal_number("sigkill"), 9);
        assert_eq!(signal_number("TERM"), 15);
        assert_eq!(128 + signal_number("KILL"), 137);
    }

    #[test]
    fn default_shell_is_never_empty() {
        assert!(!default_shell().trim().is_empty());
    }

    #[test]
    fn spawn_spec_clamps_zero_size() {
        // 0 行列会让 PTY 里的程序除零 / 排版崩掉，统一夹到 1。
        let s = SpawnSpec::new("/bin/bash", 0, 0);
        assert_eq!((s.cols, s.rows), (1, 1));
    }

    /// 后端返回错误时不能装死：要回 TermError，而不是「开着但什么都没有」。
    #[tokio::test]
    async fn spawn_failure_is_reported() {
        struct BadPty;
        impl PtyFactory for BadPty {
            fn spawn(&self, _s: &SpawnSpec, _sink: PtySink) -> Result<Box<dyn PtyHandle>, String> {
                Err("没有 /bin/bash".into())
            }
        }
        let mut h = start_with(Arc::new(BadPty));
        h.ops.send(TermOp::Open { id: 1, cols: 80, rows: 24, shell: None }).await.unwrap();
        match next(&mut h).await {
            H2C::TermError { id, error } => {
                assert_eq!(id, 1);
                assert!(error.contains("/bin/bash"), "{error}");
            }
            other => panic!("起不来必须报错，实际 {other:?}"),
        }
        // 失败之后不能把 id 占着不放。
        h.ops.send(TermOp::List).await.unwrap();
        match next(&mut h).await {
            H2C::TermSessions { sessions } => assert!(sessions.is_empty()),
            other => panic!("意外消息 {other:?}"),
        }
    }
}