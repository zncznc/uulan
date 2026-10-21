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
//! 代价（写在明处）：一条流的消费端卡住会把整条 bulk 连接的反压传上去，因为读任务是
//! 单线程分派。流队列有界（[`STREAM_QUEUE`] 帧），所以卡住的是「所有流」而不是「内存」。
//! 第一版接受这个代价 —— 文件传输和端口映射的消费端都是本地 socket，不会无故停读。

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use uulan_protocol::bulk::BulkFrame;
use uulan_protocol::frame::{read_frame, write_frame, FrameError, FrameKind};

/// 写队列容量（帧）。够吸收一次文件传输的突发，又不至于在断线时攒几百 KB。
pub const OUT_QUEUE: usize = 64;

/// 每条流自己的接收队列容量（帧）。
pub const STREAM_QUEUE: usize = 8;

type Registry = Arc<Mutex<HashMap<u32, mpsc::Sender<BulkFrame>>>>;

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
}

impl std::fmt::Debug for Mux {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mux")
            .field("streams", &self.stream_count())
            .field("closed", &self.is_closed())
            .finish()
    }
}

/// 起一个多路复用器：`rd` / `wr` 就是拆开的 bulk 连接两半（测试里可以是 `duplex` 的两半）。
pub fn spawn<R, W>(rd: R, wr: W) -> Mux
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
    });

    tokio::spawn(read_task(rd, registry.clone(), inner.closed.clone()));
    tokio::spawn(write_task(wr, out_rx));

    Mux { inner }
}

impl Mux {
    /// 注册一条流。`None` = 这个 id 已被占用，或 bulk 连接已经断了。
    pub fn register(&self, id: u32) -> Option<Stream> {
        if self.inner.closed.load(Ordering::Acquire) {
            return None;
        }
        let (tx, rx) = mpsc::channel::<BulkFrame>(STREAM_QUEUE);
        {
            let mut reg = self.inner.registry.lock().unwrap_or_else(|e| e.into_inner());
            if reg.contains_key(&id) {
                return None;
            }
            reg.insert(id, tx);
        }
        Some(Stream {
            id,
            rx,
            out: self.inner.out.clone(),
            core: Arc::new(Core {
                id,
                registry: self.inner.registry.clone(),
            }),
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

    /// 主动关掉所有流（会话收尾时用）。之后的 `register` 一律失败。
    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::Release);
        // 丢掉所有发送端 → 各流的 `recv()` 返回 None → 业务任务自己收尾。
        self.inner.registry.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

/// 注销：`Stream` 一没了就把 id 从表里摘掉。
struct Core {
    id: u32,
    registry: Registry,
}

impl Drop for Core {
    fn drop(&mut self) {
        let mut reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        reg.remove(&self.id);
        debug!(id = self.id, streams = reg.len(), "bulk 流已注销");
    }
}

/// 一条按 `id` 区分的数据流。
///
/// 同一时刻一条流只有**一个**使用方：要么串行地用（文件传输：收完发完就走），
/// 要么在一个 `select!` 里同时收和发（端口映射的 socket 桥）。不提供 `split()`，
/// 因为「两个任务各持一半」会让注销时机变得含糊（一半还活着时 id 该不该释放？）。
pub struct Stream {
    id: u32,
    rx: mpsc::Receiver<BulkFrame>,
    out: mpsc::Sender<BulkFrame>,
    /// 只为了 `Drop`：`Stream` 一没了就把这个 id 从注册表里摘掉。
    #[allow(dead_code)]
    core: Arc<Core>,
}

impl std::fmt::Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stream").field("id", &self.id).finish()
    }
}

impl Stream {
    pub fn id(&self) -> u32 {
        self.id
    }

    /// 收一帧。`None` = 这条流已经结束（对端发完 END 后注销，或者 bulk 连接断了）。
    ///
    /// 注意：`None` 也可能是「连接断了」，业务方要么当 EOF 处理、要么看 [`Mux::is_closed`]。
    pub async fn recv(&mut self) -> Option<BulkFrame> {
        self.rx.recv().await
    }

    /// 发一段数据。超过 [`uulan_protocol::bulk::CHUNK`] 请调用方自己切 ——
    /// 这里不静默切分，否则「发了 1 MiB、对端按 64 KiB 收到 16 帧」的语义差别会被藏起来。
    pub async fn send_data(&mut self, data: &[u8]) -> io::Result<()> {
        self.send_frame(BulkFrame::data(self.id, Bytes::copy_from_slice(data)))
            .await
    }

    /// 发结束标记。之后这个 id 不应再发数据（协议规定）。
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

/// 读任务：只做「读帧 → 解析 → 按 id 分派」。
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
        let tx = {
            let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
            reg.get(&bf.id).cloned()
        };
        match tx {
            Some(tx) => {
                // 有界队列：这一条流的消费端慢下来时，反压就停在这里（见文件头说明）。
                if tx.send(bf).await.is_err() {
                    debug!("流已被注销，收到的帧丢弃");
                }
            }
            None => {
                // 没见过这个 id 的帧：多半是「控制端已经放弃、被控端还在发」的残帧。
                // 丢掉 + 记一笔，绝不能让整条连接因为一个陌生 id 就断。
                debug!(id = bf.id, kind = ?bf.kind, "收到未注册 id 的 bulk 帧，已丢弃");
            }
        }
    }

    closed.store(true, Ordering::Release);
    // 清空注册表 → 所有流的 recv() 立刻返回 None，业务任务不必等超时。
    registry.lock().unwrap_or_else(|e| e.into_inner()).clear();
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
    use uulan_protocol::bulk::BulkKind;

    /// 一对通过内存管道相连的 mux（a 的写 → b 的读，b 的写 → a 的读）。
    fn linked() -> (Mux, Mux) {
        let (a_out, b_in) = duplex(64 * 1024);
        let (b_out, a_in) = duplex(64 * 1024);
        (spawn(a_in, a_out), spawn(b_in, b_out))
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
}