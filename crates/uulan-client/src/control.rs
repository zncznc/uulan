//! 控制通道：窗口里的键鼠事件从这里上报，被控端的回报（`InputError` / `Bye`）也从这里回来。
//!
//! 之前这条连接握手完就没人读了 —— 那是个隐患：被控端一发东西，TCP 接收缓冲迟早堆满。
//! 现在收发各占一半：
//!
//! ```text
//! 窗口线程 --(unbounded mpsc)--> 写任务 --> control socket
//!                              读任务 <-- control socket
//! ```
//!
//! 用**无界**队列是有意的：输入事件很小、频率有限（鼠标 100+ 次/秒也就几 KB/s），
//! 而**丢一个松开事件就会让被控端的键卡住**。宁可多占几十 KB，也不能丢。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use uulan_protocol::frame::{read_frame, write_frame, FrameKind};
use uulan_protocol::msg::{C2H, H2C};

/// 上报与回报的计数。收工时打印，作为「输入真的发出去了」的机器可查证据。
#[derive(Debug, Default)]
pub struct InputStats {
    pub moves: AtomicU64,
    pub keys: AtomicU64,
    pub chars: AtomicU64,
    pub buttons: AtomicU64,
    pub wheels: AtomicU64,
    /// 控制通道已关而没发出去的消息数（正常退出时应当接近 0）。
    pub dropped: AtomicU64,
    /// 被控端报告的输入错误数（比如被控端不是 X11、XTest 被拦）。
    pub host_errors: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputStatsSnapshot {
    pub moves: u64,
    pub keys: u64,
    pub chars: u64,
    pub buttons: u64,
    pub wheels: u64,
    pub dropped: u64,
    pub host_errors: u64,
}

impl InputStatsSnapshot {
    pub fn total(&self) -> u64 {
        self.moves + self.keys + self.chars + self.buttons + self.wheels
    }
}

impl InputStats {
    pub fn snapshot(&self) -> InputStatsSnapshot {
        InputStatsSnapshot {
            moves: self.moves.load(Ordering::Relaxed),
            keys: self.keys.load(Ordering::Relaxed),
            chars: self.chars.load(Ordering::Relaxed),
            buttons: self.buttons.load(Ordering::Relaxed),
            wheels: self.wheels.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            host_errors: self.host_errors.load(Ordering::Relaxed),
        }
    }
}

/// 给窗口线程用的发送端（`send` 是同步的，窗口事件循环里可以直接调）。
#[derive(Clone)]
pub struct InputSender {
    tx: mpsc::UnboundedSender<C2H>,
    stats: Arc<InputStats>,
}

impl InputSender {
    /// 上报一条输入消息。返回 false 表示控制通道已经没了。
    pub fn send(&self, m: C2H) -> bool {
        match &m {
            C2H::MouseMoveAbs { .. } | C2H::MouseMoveRel { .. } => {
                self.stats.moves.fetch_add(1, Ordering::Relaxed);
            }
            C2H::MouseButton { .. } => {
                self.stats.buttons.fetch_add(1, Ordering::Relaxed);
            }
            C2H::MouseWheel { .. } => {
                self.stats.wheels.fetch_add(1, Ordering::Relaxed);
            }
            C2H::Key { .. } => {
                self.stats.keys.fetch_add(1, Ordering::Relaxed);
            }
            C2H::KeyChar(_) => {
                self.stats.chars.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
        if self.tx.send(m).is_ok() {
            true
        } else {
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            false
        }
    }
}

/// 控制通道的宿主。**必须活到会话结束**：Drop 会结束读写任务。
pub struct ControlLink {
    input: InputSender,
    stats: Arc<InputStats>,
    handle: tokio::task::JoinHandle<()>,
}

impl ControlLink {
    /// 接管已鉴权的控制连接。
    pub fn start(control: TcpStream) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<C2H>();
        let stats = Arc::new(InputStats::default());
        let st = stats.clone();

        let handle = tokio::spawn(async move {
            let (mut rd, mut wr) = control.into_split();
            loop {
                tokio::select! {
                    msg = rx.recv() => {
                        let Some(m) = msg else { break };
                        let bytes = match serde_json::to_vec(&m) {
                            Ok(b) => b,
                            Err(e) => { debug!(error = %e, "输入消息序列化失败"); continue }
                        };
                        if write_frame(&mut wr, FrameKind::Json, &bytes).await.is_err() {
                            debug!("控制通道写入失败，输入上报结束");
                            break;
                        }
                    }
                    frame = read_frame(&mut rd) => {
                        let f = match frame {
                            Ok(f) => f,
                            Err(e) => { debug!(error = %e, "控制通道读取结束"); break }
                        };
                        match f.kind {
                            FrameKind::Ping => {
                                let _ = write_frame(&mut wr, FrameKind::Pong, &f.payload).await;
                            }
                            FrameKind::Json => match f.decode_json::<H2C>() {
                                Ok(H2C::InputError { message }) => {
                                    st.host_errors.fetch_add(1, Ordering::Relaxed);
                                    warn!(message = %message, "被控端报告输入注入失败");
                                }
                                Ok(H2C::Bye { reason }) => {
                                    info!(reason = %reason, "被控端道别");
                                    break;
                                }
                                Ok(H2C::Metrics(_)) => debug!("收到被控端指标"),
                                Ok(_) => {}
                                Err(e) => debug!(error = %e, "控制通道上收到无法解析的 JSON"),
                            },
                            FrameKind::Bye => break,
                            _ => {}
                        }
                    }
                }
            }
        });

        Self {
            input: InputSender { tx, stats: stats.clone() },
            stats,
            handle,
        }
    }

    pub fn input(&self) -> InputSender {
        self.input.clone()
    }

    pub fn stats(&self) -> Arc<InputStats> {
        self.stats.clone()
    }

    /// 道别并等写出任务把 Bye 发出去（最多等 500ms）。
    pub async fn close(mut self, reason: &str) {
        let _ = self.input.send(C2H::Bye { reason: reason.to_string() });
        let _ = tokio::time::timeout(Duration::from_millis(500), &mut self.handle).await;
        self.handle.abort();
    }
}

impl Drop for ControlLink {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_count_by_message_kind() {
        let s = Arc::new(InputStats::default());
        let (tx, _rx) = mpsc::unbounded_channel();
        let sender = InputSender { tx, stats: s.clone() };
        assert!(sender.send(C2H::MouseMoveAbs { x: 0.5, y: 0.5 }));
        assert!(sender.send(C2H::Key { vk: 0x41, down: true, repeat: false }));
        assert!(sender.send(C2H::Ping { seq: 1, sent_ms: 0 }));
        let n = s.snapshot();
        assert_eq!((n.moves, n.keys), (1, 1));
        assert_eq!(n.total(), 2, "Ping 不属于输入，不该计数");
    }

    #[test]
    fn closed_channel_counts_drops_instead_of_panicking() {
        let s = Arc::new(InputStats::default());
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx); // 写出任务没了
        let sender = InputSender { tx, stats: s.clone() };
        assert!(!sender.send(C2H::Key { vk: 0x41, down: false, repeat: false }));
        assert_eq!(s.snapshot().dropped, 1);
    }
}