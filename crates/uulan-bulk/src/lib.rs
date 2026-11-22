//! bulk 连接的多路复用：一条 TCP 上同时跑多条按 `id` 区分的数据流。
//!
//! 为什么需要它：一条 bulk 连接上有三种活要同时干 —— 文件传输（一次一条，几十秒）、
//! 端口映射（一条 SSH 会长时间占着 id）。`fs_loop` 那种「独占连接、串行处理」的结构
//! 会把端口映射堵在队列里，所以把连接拆成三个角色：
//!
//! ```text
//! 读任务   read_frame → BulkFrame::decode → HashMap<id, Sender> 分派
//! 写任务   mpsc::Receiver<BulkFrame> → write_frame（唯一写者，天然有序）
//! 业务任务 每条流一个：文件传输 / 端口映射的 socket 桥，各自在开始时注册 id、结束时注销
//! ```
//!
//! 三条硬规则：
//!
//! 1. **写只有一个任务**。多个任务各拿半条 `TcpStream` 直接写会把帧切碎交错
//!    （帧头是长度前缀，交错一次整条连接就报废），所以所有写出都排队列。
//! 2. **`read_frame()` 只在一个任务里 await**。它不是 cancel-safe 的（两次 `read_exact`），
//!    一旦被 `select!` 取消就会吞掉半个帧头。
//! 3. **`id` 由控制通道的请求分配，同一时刻只属于一条流**。注销靠 [`Stream`] 的 `Drop`，
//!    所以业务任务 panic 也不会在表里留下一个永远收不到数据的 id。
//!
//! ## 按流滑动窗口（[`spawn_windowed`]，2026-11-22 起）
//!
//! 第一版接受过一个代价（写在明处）：读任务是**单线程分派**、每条流队列有界
//! （[`STREAM_QUEUE`] 帧），所以一条流的消费端卡住 → 队列满 → 读任务阻塞 →
//! **整条连接上别的流全部停摆**。2026-11-22 的真机台架把它量成了绝对值：
//! A 流灌满后故意停读 10 s，同会话另一条 1 MiB 往返从 ~100 ms 被拖到 **6892 ms**。
//!
//! 现在的做法是**按流的滑动窗口**：接收方每消费一帧就回一条 `BulkKind::Credit`（窗口 +1），
//! 发送方在窗口用完时等信用。于是读任务**永不因为某一条流而阻塞** —— 对端最多只会
//! 超前发「窗口」帧，正好落在队列容量之内。卡住的只有那一条流自己：
//! 它的业务任务在等本机 socket，发送方在等它消费，其它流照走。
//!
//! 四条纪律：
//!
//! - **窗口只在双方都声明支持时启用**（`msg::ClientHello::bulk_window` /
//!   `HostHelloOk::bulk_window`，双方都发才 `Some`）。老对端不发信用，
//!   新端若单方面等窗口就会永远等下去。
//! - **文件传输用 [`Mux::register_unwindowed`]**：控制端那半边（`fsx`）是**独占整条连接**
//!   的直写实现（不走 mux、不读信用），给它开窗口必然死锁。所以「哪条流走窗口」
//!   由**接收方**注册时逐流决定，不是全局开关。
//! - **信用必须在 `recv()` 里就还**，不能等业务任务「真正写出去」再还：
//!   业务任务手里可能压着一帧在等本机 socket，那一帧的信用已经消耗掉了，
//!   晚还会让对端在窗口边界上多等一个往返。
//! - **`send_data` 会等信用，所以绝不能放在 `select!` 的分支体里**（同 2026-11-21
//!   那个「只发不收就死锁」的老 bug）。一个方向一个任务，用 [`Stream::split`]。

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, Semaphore};
use tracing::{debug, warn};

use uulan_protocol::bulk::{BulkFrame, BulkKind};
use uulan_protocol::frame::{read_frame, write_frame, FrameError, FrameKind};

/// 写队列容量（帧）。够吸收一次文件传输的突发，又不至于在断线时攒几百 KB。
pub const OUT_QUEUE: usize = 64;

/// 每条流自己的接收队列容量（帧）——**不开窗口**时用的值（老行为）。
pub const STREAM_QUEUE: usize = 8;

/// 按流窗口的默认帧数。16 × [`uulan_protocol::bulk::CHUNK`] = 1 MiB 在途，
/// 局域网（RTT < 1 ms）下足够跑满链路，又让每条卡住的流最多占 1 MiB 内存。
pub const STREAM_WINDOW: usize = 16;

/// 协商窗口的下界。**不能是 0**：窗口 0 帧 = 发一帧就要等一份永远不来的信用。
pub const WINDOW_MIN: u16 = 1;

/// 协商窗口的上界：96 帧 × 64 KiB = 6 MiB 在途/流。
///
/// 再大就变成「一条流卡死吃掉半台机器的内存」，而 96 帧在 100 ms RTT 上已经能跑到
/// ~60 MB/s，对内网远控足够。要更大先想清楚内存账。
pub const WINDOW_MAX: u16 = 96;

/// 握手协商出的按流窗口帧数；`None` = 这场会话不开窗口（老行为）。
///
/// 口径只有这一份，**两端各算一次、结果必须相同**：
///
/// - 双方都声明 `bulk_window` 才谈得上协商（单方面开窗口 = 等一份永远不来的信用）；
/// - 帧数取两端声明的**较小值** —— 发送方的在途上限必须 ≤ 接收方能缓冲的量，
///   取 min 才两头都对得上（两端口径不一致会让读任务在队列上憋住，等于没修）；
/// - `0` = 没声明（老端或只想用默认值）→ 按 [`STREAM_WINDOW`] 算；
/// - 最后钳在 [`WINDOW_MIN`]..=[`WINDOW_MAX`]：对端发来的数是不可信输入，钳而不拒。
pub fn negotiate_window(client: (bool, u16), host: (bool, u16)) -> Option<usize> {
    if !client.0 || !host.0 {
        return None;
    }
    let pick = |f: u16| if f == 0 { STREAM_WINDOW as u16 } else { f };
    Some(pick(client.1).min(pick(host.1)).clamp(WINDOW_MIN, WINDOW_MAX) as usize)
}

/// 开窗口的流，队列比窗口多留几格：`END` 不占窗口，算错一格也不该把连接憋死。
const WINDOW_SLACK: usize = 4;

struct Entry {
    tx: mpsc::Sender<BulkFrame>,
    /// 本端**发送**的许可（对端每消费一帧就 +1）。`None` = 这条流不走窗口。
    credit: Option<Arc<Semaphore>>,
}

type Registry = Arc<Mutex<HashMap<u32, Entry>>>;

/// bulk 连接的多路复用器。克隆是廉价的（内部 `Arc`）。
///
/// Drop 掉最后一个 `Mux`（且所有 `Stream` 也没了）时写任务自然结束并关闭连接。
#[derive(Clone)]
pub struct Mux {
    inner: Arc<Inner>,
}

struct Inner {
    registry: Registry,
    out: mpsc::Sender<BulkFrame>,
    /// 读端已经结束（对端关了连接或帧解析失败）。之后不再接受新 id。
    closed: Arc<AtomicBool>,
    /// 本会话协商出来的窗口大小（`None` = 老对端 / 不走窗口）。
    window: Option<usize>,
}

impl std::fmt::Debug for Mux {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mux")
            .field("streams", &self.stream_count())
            .field("closed", &self.is_closed())
            .field("window", &self.inner.window)
            .finish()
    }
}

/// 起一个多路复用器（老行为：读任务阻塞式分派）。`rd` / `wr` 就是拆开的 bulk 连接两半
/// （测试里可以是 `duplex` 的两半）。
pub fn spawn<R, W>(rd: R, wr: W) -> Mux
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    spawn_windowed(rd, wr, None)
}

/// 起一个多路复用器，并按 `window` 帧开按流滑动窗口（`None` 等价于 [`spawn`]）。
///
/// **只有协商成功的会话才传 `Some`**：`bulk_window` 是双方共同声明的能力位，
/// 单方面开窗口 = 等一份永远不来的信用。
pub fn spawn_windowed<R, W>(rd: R, wr: W, window: Option<usize>) -> Mux
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out_tx, out_rx) = mpsc::channel::<BulkFrame>(OUT_QUEUE);
    let registry: Registry = Arc::new(Mutex::new(HashMap::new()));

    let inner = Arc::new(Inner {
        registry: registry.clone(),
        out: out_tx,
        closed: Arc::new(AtomicBool::new(false)),
        window: window.filter(|w| *w > 0),
    });

    tokio::spawn(read_task(rd, registry.clone(), inner.closed.clone()));
    tokio::spawn(write_task(wr, out_rx));

    Mux { inner }
}

impl Mux {
    /// 注册一条流（本会话开了窗口就走窗口，否则老行为）。
    ///
    /// 端口映射 / 转发用这个；文件传输请用 [`Self::register_unwindowed`]。
    /// `None` = 这个 id 已被占用，或 bulk 连接已经断了。
    pub fn register(&self, id: u32) -> Option<Stream> {
        self.register_with(id, self.inner.window)
    }

    /// 注册一条**不走窗口**的流：读任务对它仍是阻塞式分派（老行为）。
    ///
    /// 为什么文件传输必须走这条：控制端的 `fsx` 是「独占整条连接直接写」的实现，
    /// 它既不读信用、也没法在等窗口时继续干活 —— 给它开窗口就是互相等死。
    pub fn register_unwindowed(&self, id: u32) -> Option<Stream> {
        self.register_with(id, None)
    }

    fn register_with(&self, id: u32, window: Option<usize>) -> Option<Stream> {
        if self.inner.closed.load(Ordering::Acquire) {
            return None;
        }
        let cap = window.map_or(STREAM_QUEUE, |w| w + WINDOW_SLACK);
        let (tx, rx) = mpsc::channel::<BulkFrame>(cap);
        let credit = window.map(|w| Arc::new(Semaphore::new(w)));
        {
            let mut reg = self.inner.registry.lock().unwrap_or_else(|e| e.into_inner());
            if reg.contains_key(&id) {
                return None;
            }
            reg.insert(id, Entry { tx, credit: credit.clone() });
        }
        let core = Arc::new(Core { id, registry: self.inner.registry.clone() });
        Some(Stream {
            tx: StreamSender {
                id,
                out: self.inner.out.clone(),
                credit,
                core: core.clone(),
            },
            rx: StreamReceiver {
                id,
                rx,
                out: self.inner.out.clone(),
                windowed: window.is_some(),
                core,
            },
        })
    }

    /// 当前注册了几条流（日志/测试用）。
    pub fn stream_count(&self) -> usize {
        self.inner.registry.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// bulk 连接是否已经断了。
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }

    /// 本会话的窗口大小（`None` = 不走窗口）。
    pub fn window(&self) -> Option<usize> {
        self.inner.window
    }

    /// 主动关掉所有流（会话收尾时用）。之后的 `register` 一律失败。
    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::Release);
        // 丢掉所有发送端 → 各流的 `recv()` 返回 None → 业务任务自己收尾；
        // 同时关掉各流的许可 → 卡在窗口上的 `send_data` 立刻返回错误。
        close_all(&self.inner.registry);
    }
}

/// 摘掉所有流，并**叫醒正在等窗口的发送方**（`Semaphore::close` 让 `acquire` 立刻返回错误）。
///
/// 少了这一句，连接断了以后一个卡在窗口上的 `send_data` 会挂到会话收尾为止 ——
/// 业务任务看起来「没反应」，而不是「连接断了」。
fn close_all(registry: &Registry) {
    let mut reg = registry.lock().unwrap_or_else(|e| e.into_inner());
    for (_, e) in reg.drain() {
        if let Some(c) = e.credit {
            c.close();
        }
    }
}

/// 注销：流的**两半都没了**才把 id 从表里摘掉。
struct Core {
    id: u32,
    registry: Registry,
}

impl Drop for Core {
    fn drop(&mut self) {
        let mut reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = reg.remove(&self.id) {
            if let Some(c) = e.credit {
                c.close();
            }
        }
        debug!(id = self.id, streams = reg.len(), "bulk 流已注销");
    }
}

/// 一条按 `id` 区分的数据流。
///
/// 同一时刻一条流只有**一个**使用方：要么串行地用（文件传输：收完发完就走），
/// 要么拆成两半（[`Stream::split`]）给两个任务同时收发 —— 端口映射的双工桥就是这么干的。
pub struct Stream {
    tx: StreamSender,
    rx: StreamReceiver,
}

impl std::fmt::Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stream").field("id", &self.tx.id).finish()
    }
}

impl Stream {
    pub fn id(&self) -> u32 {
        self.tx.id
    }

    /// 收一帧。`None` = 这条流已经结束（对端发完 END 后注销，或者 bulk 连接断了）。
    ///
    /// 注意：`None` 也可能是「连接断了」，业务方要么当 EOF 处理、要么看 [`Mux::is_closed`]。
    pub async fn recv(&mut self) -> Option<BulkFrame> {
        self.rx.recv().await
    }

    /// 发一段数据。超过 [`uulan_protocol::bulk::CHUNK`] 请调用方自己切 ——
    /// 这里不静默切分，否则「发了 1 MiB、对端按 64 KiB 收到 16 帧」的语义差别会被藏起来。
    ///
    /// **开了窗口的流上这会等信用**：调用方必须在「不会因此错过别的方向」的位置 await 它
    /// （一个方向一个任务，见 [`Stream::split`]）。放在 `select!` 的分支体里就会复现
    /// 2026-11-21 那个「只发不收就死锁」的老 bug。
    pub async fn send_data(&mut self, data: &[u8]) -> io::Result<()> {
        self.tx.send_data(data).await
    }

    /// 发结束标记。之后这个 id 不应再发数据（协议规定）。`END` **不占窗口**。
    pub async fn send_end(&mut self) -> io::Result<()> {
        self.tx.send_end().await
    }

    /// 拆成「只发」和「只收」两半，给两个任务同时用。
    ///
    /// 注销时机靠两半共享的 [`Core`]：**两半都没了**才摘 id —— 只要还有一半活着，
    /// 这个 id 就仍然是自己的。
    pub fn split(self) -> (StreamSender, StreamReceiver) {
        (self.tx, self.rx)
    }
}

/// 流的发送半边。拿它就不能再收帧（同一个 id 只有一份接收队列）。
pub struct StreamSender {
    id: u32,
    out: mpsc::Sender<BulkFrame>,
    /// 本端发送许可（对端每消费一帧 +1）。`None` = 不走窗口。
    credit: Option<Arc<Semaphore>>,
    /// 只为了 `Drop`：两半都没了才注销 id。
    #[allow(dead_code)]
    core: Arc<Core>,
}

impl StreamSender {
    pub fn id(&self) -> u32 {
        self.id
    }

    pub async fn send_data(&mut self, data: &[u8]) -> io::Result<()> {
        if let Some(sem) = &self.credit {
            // 拿一张「可以再发一帧」的票，然后把它**用掉**（不是还回去）：
            // 还回去等于窗口永不满，对端就会无限超前发。
            let permit = sem.clone().acquire_owned().await.map_err(|_| {
                io::Error::new(io::ErrorKind::BrokenPipe, "bulk 流已关闭（等发送窗口时连接断了）")
            })?;
            permit.forget();
        }
        self.send_frame(BulkFrame::data(self.id, Bytes::copy_from_slice(data))).await
    }

    pub async fn send_end(&mut self) -> io::Result<()> {
        self.send_frame(BulkFrame::end(self.id)).await
    }

    async fn send_frame(&mut self, f: BulkFrame) -> io::Result<()> {
        self.out
            .send(f)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "bulk 连接已关闭"))
    }
}

/// 流的接收半边。`recv()` 在开窗口的流上会**顺手把信用还给对端**。
pub struct StreamReceiver {
    id: u32,
    rx: mpsc::Receiver<BulkFrame>,
    out: mpsc::Sender<BulkFrame>,
    /// 这条流走不走窗口（决定收一帧要不要还信用）。
    windowed: bool,
    /// 只为了 `Drop`：两半都没了才注销 id。
    #[allow(dead_code)]
    core: Arc<Core>,
}

impl StreamReceiver {
    pub fn id(&self) -> u32 {
        self.id
    }

    pub async fn recv(&mut self) -> Option<BulkFrame> {
        let f = self.rx.recv().await?;
        if self.windowed && f.kind == BulkKind::Data {
            // 信用在这里还，而不是等业务任务真写完 —— 见模块注释第三条纪律。
            // 还失败（连接已断）也把这一帧交给调用方：让它下一拍自然看到 EOF。
            if self.out.send(BulkFrame::credit(self.id)).await.is_err() {
                debug!(id = self.id, "还信用失败（连接已断）");
            }
        }
        Some(f)
    }
}

/// 读任务：只做「读帧 → 解析 → 按 id 分派」。信用帧在分派之前就被消化掉。
async fn read_task<R>(mut rd: R, registry: Registry, closed: Arc<AtomicBool>)
where
    R: AsyncRead + Unpin,
{
    loop {
        let f = match read_frame(&mut rd).await {
            Ok(f) => f,
            Err(FrameError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                debug!("bulk 连接已被对端关闭");
                break;
            }
            Err(e) => {
                warn!(error = %e, "读 bulk 帧失败，连接收尾");
                break;
            }
        };
        if f.kind != FrameKind::Blob {
            // 心跳之类：这条连接只承载数据，别的帧直接跳过（与控制连接的行为一致）。
            continue;
        }
        let bf = match BulkFrame::decode(&f.payload) {
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, "bulk 帧解析失败（帧头损坏），连接收尾");
                break;
            }
        };
        let entry = {
            let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
            reg.get(&bf.id).map(|e| (e.tx.clone(), e.credit.clone()))
        };
        let Some((tx, credit)) = entry else {
            // 没见过这个 id 的帧：多半是「控制端已经放弃、被控端还在发」的残帧。
            // 丢掉 + 记一笔，绝不能让整条连接因为一个陌生 id 就断。
            debug!(id = bf.id, kind = ?bf.kind, "收到未注册 id 的 bulk 帧，已丢弃");
            continue;
        };
        if bf.kind == BulkKind::Credit {
            // 信用帧**不是数据**：进业务队列会被当成「一个 0 字节的数据段」。
            match credit {
                Some(sem) => sem.add_permits(1),
                None => debug!(id = bf.id, "这条流不走窗口，却收到信用帧（忽略）"),
            }
            continue;
        }
        // 开窗口的流：对端最多超前「窗口」帧，所以这里不该满 —— 满了说明对端没守窗口
        // （老对端、或两端注册模式不一致）。退回阻塞式分派是唯一不丢数据的做法，
        // 代价就是老行为的老毛病：这条流一卡，整条连接跟着停。
        match tx.try_send(bf) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(bf)) => {
                debug!(id = bf.id, "流队列已满，退回阻塞式分派（对端没守窗口？）");
                if tx.send(bf).await.is_err() {
                    debug!("流已被注销，收到的帧丢弃");
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                debug!("流已被注销，收到的帧丢弃");
            }
        }
    }

    closed.store(true, Ordering::Release);
    // 摘掉所有流（并叫醒等窗口的发送方）→ 所有 `recv()` 立刻返回 None，业务任务不必等超时。
    close_all(&registry);
}

/// 写任务：bulk 连接上**唯一**的写者。
async fn write_task<W>(mut wr: W, mut rx: mpsc::Receiver<BulkFrame>)
where
    W: AsyncWrite + Unpin,
{
    while let Some(f) = rx.recv().await {
        let payload = f.encode();
        if let Err(e) = write_frame(&mut wr, FrameKind::Blob, &payload).await {
            debug!(error = %e, "写 bulk 帧失败，写任务结束");
            break;
        }
    }
    // 尽力关掉写半边，让对端立刻看到 EOF 而不是等 TCP 超时。
    let _ = tokio::io::AsyncWriteExt::shutdown(&mut wr).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::duplex;

    /// 协商口径：两端各算一次必须得同一个数，否则一端窗口大、一端队列小，
    /// 「读任务永不因为某条流阻塞」这条承诺当场失效。
    #[test]
    fn window_negotiation_is_symmetric_and_min_takes_precedence() {
        // 双方都没声明 → 不开窗口（老行为）。
        assert_eq!(negotiate_window((false, 0), (true, 0)), None);
        assert_eq!(negotiate_window((true, 64), (false, 0)), None);
        // 都声明但都没写帧数 → 默认 16（老端组合：新老混跑仍是 16）。
        assert_eq!(negotiate_window((true, 0), (true, 0)), Some(STREAM_WINDOW));
        // 一边没写帧数 = 它那端的队列还是按默认 16 建的 → 取 min 之后就是 16。
        // 这正是「调大窗口必须两端都写」的原因：只调一端没有任何效果。
        assert_eq!(negotiate_window((true, 64), (true, 0)), Some(STREAM_WINDOW));
        assert_eq!(negotiate_window((true, 0), (true, 48)), Some(STREAM_WINDOW));
        // 两边都写才谈得上放大/缩小，取小。
        assert_eq!(negotiate_window((true, 64), (true, 24)), Some(24));
        assert_eq!(negotiate_window((true, 64), (true, 96)), Some(64));
        // 对称：谁报的是控制端不影响结果。
        assert_eq!(
            negotiate_window((true, 64), (true, 24)),
            negotiate_window((true, 24), (true, 64))
        );
        // 对端发来的数不可信：越界钳住而不是拒绝会话（min 之后仍可能越界，所以钳在最后）。
        assert_eq!(negotiate_window((true, 60_000), (true, 60_000)), Some(WINDOW_MAX as usize));
        assert_eq!(negotiate_window((true, 4096), (true, 4096)), Some(WINDOW_MAX as usize));
        assert_eq!(negotiate_window((true, 1), (true, 1)), Some(WINDOW_MIN as usize));
    }

    /// 协商出来的窗口必须真的落到信用上：窗口 W 的流，发送端最多超前 W 帧；
    /// 对端每消费一帧放一帧过去，一帧都不丢、顺序不乱。
    #[tokio::test]
    async fn negotiated_window_really_gates_the_sender() {
        let w = negotiate_window((true, 24), (true, 96)).expect("双方都声明了就应当开窗口");
        assert_eq!(w, 24);
        let (a_out, b_in) = duplex(256 * 1024);
        let (b_out, a_in) = duplex(4096);
        let a = spawn_windowed(a_in, a_out, Some(w));
        let b = spawn_windowed(b_in, b_out, Some(w));
        let mut sa = a.register(1).unwrap();
        let mut sb = b.register(1).unwrap();
        // 发送端跑在独立任务里：窗口用完后它会等信用，不能和「收」串在一个任务里（那就是死锁）。
        let sender = tokio::spawn(async move {
            for i in 0..(w * 2) {
                sa.send_data(&[i as u8]).await.unwrap();
            }
        });
        for i in 0..(w * 2) {
            let f = recv_data(&mut sb).await;
            assert_eq!(&f.data[..], &[i as u8][..]);
        }
        tokio::time::timeout(Duration::from_secs(2), sender)
            .await
            .expect("2W 帧全收完，发送端就该退出")
            .unwrap();
    }

    /// 一对通过内存管道相连的 mux（a 的写 → b 的读，b 的写 → a 的读）。
    fn linked() -> (Mux, Mux) {
        let (a_out, b_in) = duplex(64 * 1024);
        let (b_out, a_in) = duplex(64 * 1024);
        (spawn(a_in, a_out), spawn(b_in, b_out))
    }

    /// 同上，但两端都开按流窗口。
    fn linked_windowed(w: usize) -> (Mux, Mux) {
        let (a_out, b_in) = duplex(64 * 1024);
        let (b_out, a_in) = duplex(64 * 1024);
        (spawn_windowed(a_in, a_out, Some(w)), spawn_windowed(b_in, b_out, Some(w)))
    }

    async fn recv_data(s: &mut Stream) -> BulkFrame {
        tokio::time::timeout(Duration::from_secs(2), s.recv())
            .await
            .expect("等帧超时")
            .expect("流已结束")
    }

    #[tokio::test]
    async fn two_streams_do_not_mix() {
        let (a, b) = linked();
        let mut a1 = a.register(1).expect("注册 1");
        let mut a2 = a.register(2).expect("注册 2");
        let mut b1 = b.register(1).expect("对端注册 1");
        let mut b2 = b.register(2).expect("对端注册 2");

        // 交错发：1 号流两段、2 号流一段，顺序不能串。
        a1.send_data(b"one-").await.unwrap();
        a2.send_data(b"two").await.unwrap();
        a1.send_data(b"one").await.unwrap();

        assert_eq!(&recv_data(&mut b1).await.data[..], b"one-");
        assert_eq!(&recv_data(&mut b2).await.data[..], b"two");
        assert_eq!(&recv_data(&mut b1).await.data[..], b"one");
        assert_eq!(b.stream_count(), 2);
    }

    #[tokio::test]
    async fn end_is_delivered_per_stream() {
        let (a, b) = linked();
        let mut a1 = a.register(7).unwrap();
        let mut a2 = a.register(8).unwrap();
        let mut b1 = b.register(7).unwrap();
        let mut b2 = b.register(8).unwrap();

        a1.send_end().await.unwrap();
        let f = recv_data(&mut b1).await;
        assert_eq!(f.kind, BulkKind::End);
        assert!(f.data.is_empty());

        // 7 号流结束了，8 号流不该受影响。
        a2.send_data(b"still here").await.unwrap();
        assert_eq!(&recv_data(&mut b2).await.data[..], b"still here");
    }

    #[tokio::test]
    async fn unregistered_id_is_dropped_and_connection_survives() {
        let (a, b) = linked();
        let mut a_ghost = a.register(99).unwrap();
        let mut a_ok = a.register(1).unwrap();
        let mut b_ok = b.register(1).unwrap();

        a_ghost.send_data(b"nobody listens").await.unwrap();
        a_ok.send_data(b"real").await.unwrap();
        // b 只注册了 1 号流：99 的帧被丢掉，1 号的帧照常到达。
        assert_eq!(&recv_data(&mut b_ok).await.data[..], b"real");
        assert_eq!(b.stream_count(), 1);
    }

    #[tokio::test]
    async fn duplicate_id_is_refused() {
        let (a, _b) = linked();
        let _s = a.register(3).expect("第一次注册应当成功");
        assert!(a.register(3).is_none(), "同一个 id 不能注册两次");
    }

    #[tokio::test]
    async fn dropping_a_stream_frees_its_id() {
        let (a, _b) = linked();
        {
            let _s = a.register(5).unwrap();
            assert_eq!(a.stream_count(), 1);
        }
        assert_eq!(a.stream_count(), 0, "Drop 之后 id 必须被释放");
        assert!(a.register(5).is_some(), "释放后可以重新注册同一个 id");
    }

    #[tokio::test]
    async fn peer_eof_wakes_every_stream() {
        let (a_out, b_in) = duplex(4096);
        let (b_out, a_in) = duplex(4096);
        let a = spawn(a_in, a_out);
        let mut s = a.register(1).unwrap();

        // 对端整个消失（连两半管道一起丢掉）= bulk 连接断了。
        drop(b_in);
        drop(b_out);
        let got = tokio::time::timeout(Duration::from_secs(2), s.recv()).await;
        assert!(matches!(got, Ok(None)), "对端断开后流必须结束，不能一直挂着");
        assert!(a.is_closed(), "读端结束后 Mux 必须标记为已关闭");
    }

    #[tokio::test]
    async fn close_fails_new_registrations_and_ends_streams() {
        let (a, b) = linked();
        let mut s = a.register(1).unwrap();
        let _b1 = b.register(1).unwrap();

        a.close();
        assert!(a.is_closed());
        assert!(a.register(2).is_none(), "关闭后不能再注册");
        let got = tokio::time::timeout(Duration::from_secs(2), s.recv()).await;
        assert!(matches!(got, Ok(None)), "关闭后流应当立刻结束");
    }

    #[tokio::test]
    async fn large_payload_roundtrip() {
        let (a, b) = linked();
        let mut sa = a.register(1).unwrap();
        let mut sb = b.register(1).unwrap();
        let payload: Vec<u8> = (0..64 * 1024).map(|i| (i % 251) as u8).collect();
        sa.send_data(&payload).await.unwrap();
        let got = recv_data(&mut sb).await;
        assert_eq!(got.data.len(), payload.len());
        assert_eq!(&got.data[..], &payload[..]);
    }

    // ─────────────── 按流窗口（2026-11-22） ───────────────

    #[tokio::test]
    async fn window_caps_in_flight_frames_until_the_peer_consumes() {
        let (a, b) = linked_windowed(4);
        assert_eq!(a.window(), Some(4));
        let mut sa = a.register(1).unwrap();
        let mut sb = b.register(1).unwrap();

        // 窗口 4：一帧信用都还没来时也能先发 4 帧（这是初始窗口）。
        for i in 0..4u8 {
            tokio::time::timeout(Duration::from_secs(1), sa.send_data(&[i]))
                .await
                .expect("窗口内的发送不该等")
                .unwrap();
        }
        // 第 5 帧必须卡住 —— 对端一帧都还没消费。
        let fifth = tokio::time::timeout(Duration::from_millis(150), sa.send_data(b"x")).await;
        assert!(fifth.is_err(), "窗口用完后发送方必须等信用，否则窗口形同虚设");

        // 对端消费一帧 → 窗口 +1 → 第 5 帧立刻能出去（排在前 4 帧后面，顺序不变）。
        let f = recv_data(&mut sb).await;
        assert_eq!(&f.data[..], &[0u8]);
        tokio::time::timeout(Duration::from_secs(1), sa.send_data(b"x"))
            .await
            .expect("消费一帧后必须放行一帧")
            .unwrap();
        for i in 1..4u8 {
            assert_eq!(&recv_data(&mut sb).await.data[..], &[i]);
        }
        assert_eq!(&recv_data(&mut sb).await.data[..], b"x");
    }

    #[tokio::test]
    async fn credit_frames_never_reach_the_business_queue() {
        let (a, b) = linked_windowed(2);
        let mut sa = a.register(1).unwrap();
        let mut sb = b.register(1).unwrap();
        sa.send_data(b"one").await.unwrap();
        sa.send_data(b"two").await.unwrap();
        // b 消费两帧 → 会往 a 回两条信用；a 侧不该把它们当数据塞进队列。
        assert_eq!(&recv_data(&mut sb).await.data[..], b"one");
        assert_eq!(&recv_data(&mut sb).await.data[..], b"two");
        // 窗口靠信用补满了，a 能继续发；b 收到的必须还是 Data。
        sa.send_data(b"three").await.unwrap();
        let f = recv_data(&mut sb).await;
        assert_eq!(f.kind, BulkKind::Data);
        assert_eq!(&f.data[..], b"three");
    }

    /// 这就是 2026-11-22 真机量出来的那个场景：一条流停读不能拖住别的流。
    #[tokio::test]
    async fn a_stalled_stream_does_not_hold_up_another() {
        let (a, b) = linked_windowed(4);
        let mut a_stuck = a.register(1).unwrap();
        let mut a_ok = a.register(2).unwrap();
        let mut b_stuck = b.register(1).unwrap();
        let mut b_ok = b.register(2).unwrap();

        // 1 号流灌满窗口（4 帧），然后**一帧都不消费**。
        for _ in 0..4 {
            a_stuck.send_data(&[0u8; 1024]).await.unwrap();
        }
        // 2 号流做一次正常往返 —— 必须和「1 号流卡住」无关。
        let t0 = std::time::Instant::now();
        a_ok.send_data(b"ping").await.unwrap();
        assert_eq!(&recv_data(&mut b_ok).await.data[..], b"ping");
        let dt = t0.elapsed();
        assert!(dt < Duration::from_millis(500), "别的流被拖住了：{dt:?}");

        // 1 号流开始消费，积压的帧照旧到齐（没有丢数据）。
        for _ in 0..4 {
            assert_eq!(recv_data(&mut b_stuck).await.data.len(), 1024);
        }
        // 消费完 4 帧后窗口回来了，1 号流可以继续发。
        tokio::time::timeout(Duration::from_secs(1), a_stuck.send_data(&[1u8; 1024]))
            .await
            .expect("对端消费后 1 号流必须能继续发")
            .unwrap();
        assert_eq!(recv_data(&mut b_stuck).await.data.len(), 1024);
    }

    /// 老行为必须原样保留：一端不开窗口（或对端是老版本）时，读任务仍是阻塞式分派。
    /// 文件传输的直写对端就靠这条 —— 它不读信用，给它开窗口就是互相等死。
    #[tokio::test]
    async fn unwindowed_receiver_keeps_blocking_dispatch() {
        let (a_out, b_in) = duplex(64 * 1024);
        let (b_out, a_in) = duplex(64 * 1024);
        // 发送侧是老行为，接收侧开了窗口 —— 「两端注册模式不一致」的真实形态。
        let a = spawn(a_in, a_out);
        let b = spawn_windowed(b_in, b_out, Some(2));
        let mut sa = a.register(1).unwrap();
        let mut sb = b.register_unwindowed(1).unwrap();
        let mut sa2 = a.register(2).unwrap();
        let mut sb2 = b.register(2).unwrap();

        // 1 号流在接收端不走窗口：灌 10 帧（超过窗口 2 + 队列余量 4），
        // 读任务会退回阻塞式分派 —— 但数据一帧不少、顺序不乱。
        for i in 0..10u8 {
            tokio::time::timeout(Duration::from_secs(1), sa.send_data(&[i]))
                .await
                .expect("发送侧只是排队，不该等")
                .unwrap();
        }
        for i in 0..10u8 {
            assert_eq!(recv_data(&mut sb).await.data[0], i);
        }
        // 2 号流（两边都走窗口）照常跑。
        sa2.send_data(b"hi").await.unwrap();
        assert_eq!(&recv_data(&mut sb2).await.data[..], b"hi");
    }

    #[tokio::test]
    async fn split_halves_send_and_receive_at_the_same_time() {
        let (a, b) = linked_windowed(4);
        let (mut atx, mut arx) = a.register(1).unwrap().split();
        let (mut btx, mut brx) = b.register(1).unwrap().split();

        let up = tokio::spawn(async move {
            for i in 0..3u8 {
                atx.send_data(&[i]).await.unwrap();
            }
            atx.send_end().await.unwrap();
        });
        let down = tokio::spawn(async move {
            for i in 0..3u8 {
                btx.send_data(&[10 + i]).await.unwrap();
            }
            btx.send_end().await.unwrap();
        });
        up.await.unwrap();
        down.await.unwrap();

        // 两个方向各 3 帧，谁也没被对方堵住。
        for i in 0..3u8 {
            assert_eq!(recv_split(&mut brx).await.data[0], i);
        }
        for i in 0..3u8 {
            assert_eq!(recv_split(&mut arx).await.data[0], 10 + i);
        }
        assert_eq!(recv_split(&mut brx).await.kind, BulkKind::End);
        assert_eq!(recv_split(&mut arx).await.kind, BulkKind::End);
    }

    async fn recv_split(r: &mut StreamReceiver) -> BulkFrame {
        tokio::time::timeout(Duration::from_secs(2), r.recv())
            .await
            .expect("等帧超时")
            .expect("流已结束")
    }

    #[tokio::test]
    async fn blocked_sender_is_woken_when_the_connection_dies() {
        let (a_out, b_in) = duplex(64 * 1024);
        let (b_out, a_in) = duplex(4096);
        let a = spawn_windowed(a_in, a_out, Some(2));
        let mut sa = a.register(1).unwrap();
        sa.send_data(b"1").await.unwrap();
        sa.send_data(b"2").await.unwrap();
        let wait = tokio::spawn(async move {
            let r = sa.send_data(b"3").await;
            (r.is_err(), sa)
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        // 对端的读端消失 = 连接断了（写端还留着，免得发送队列自己先断）。
        drop(b_out);
        let (failed, _keep) = tokio::time::timeout(Duration::from_secs(2), wait)
            .await
            .expect("连接断了以后等窗口的发送必须醒过来")
            .unwrap();
        assert!(failed, "连接断了应当返回 Err（BrokenPipe）");
        drop(b_in);
    }
}
