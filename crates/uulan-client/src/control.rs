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

use uulan_protocol::crypto::SecControl;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use uulan_clip::{poll_outbound, ClipSync, Clipboard, POLL_INTERVAL};
use uulan_protocol::frame::{read_frame, write_frame, FrameKind};
use uulan_protocol::msg::{C2H, H2C, Metrics};

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
    /// 剪贴板：本端 → 被控端，真的送出去的条数。
    pub clips_sent: AtomicU64,
    /// 剪贴板：被控端 → 本端，真的写进本地剪贴板的条数。
    pub clips_recv: AtomicU64,
    /// 写本地剪贴板失败次数。
    pub clip_errors: AtomicU64,
    /// 读本地剪贴板失败次数（打不开，不是「内容没变」）。
    ///
    /// 单独计这个数：以前「发送 0」既可能是「本机剪贴板一直是旧的」，
    /// 也可能是「根本读不到」，现场分不清（2026-10-31 就被误判成后端没重试）。
    pub clip_read_errors: AtomicU64,
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
    pub clips_sent: u64,
    pub clips_recv: u64,
    pub clip_errors: u64,
    pub clip_read_errors: u64,
}

impl InputStatsSnapshot {
    pub fn total(&self) -> u64 {
        self.moves + self.keys + self.chars + self.buttons + self.wheels
    }

    /// 把另一份计数累加进来（headless 跨会话汇总用：每条会话一个 `ControlLink`）。
    pub fn accumulate(&mut self, o: &InputStatsSnapshot) {
        self.moves += o.moves;
        self.keys += o.keys;
        self.chars += o.chars;
        self.buttons += o.buttons;
        self.wheels += o.wheels;
        self.dropped += o.dropped;
        self.host_errors += o.host_errors;
        self.clips_sent += o.clips_sent;
        self.clips_recv += o.clips_recv;
        self.clip_errors += o.clip_errors;
        self.clip_read_errors += o.clip_read_errors;
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
            clips_sent: self.clips_sent.load(Ordering::Relaxed),
            clips_recv: self.clips_recv.load(Ordering::Relaxed),
            clip_errors: self.clip_errors.load(Ordering::Relaxed),
            clip_read_errors: self.clip_read_errors.load(Ordering::Relaxed),
        }
    }
}

/// 被控端上报的运行状态（`H2C::Metrics` / `H2C::SystemState`）。
///
/// 这两个消息以前**被控端一条都没发过**（协议里有、主循环里没人收），于是控制端
/// 既看不到对面的负载，也不知道对面锁屏了 —— 键鼠继续往锁屏里送。
///
/// 原子量存整数：`cpu` 存「百分比 ×100」（`f32` 放进 `AtomicU64` 要 `to_bits`，
/// 读的时候还得自己解释；×100 取整后误差 0.01%，对显示没有意义损失）。
#[derive(Debug, Default)]
pub struct HostStatus {
    locked: AtomicBool,
    /// 是否收到过 `SystemState`。没收到过时**不能**当作「没锁」去判断，
    /// 但也不能当作「锁着」把键鼠掐掉（老版本被控端根本不发这条）。
    known: AtomicBool,
    cpu_x100: AtomicU64,
    mem_used_mb: AtomicU64,
    mem_total_mb: AtomicU64,
    rtt_ms: AtomicU64,
    /// 控制端**自己**量的链路往返时延（微秒）：`C2H::Ping` → `H2C::Pong`。
    /// 被控端报的 `rtt_ms` 恒为 0（它没有可比的时钟），所以「延迟多少」只能这边量。
    rtt_local_us: AtomicU64,
    /// 量到过多少次。0 = 还没测出来（标题上就不显示，免得写个假 0.0ms）。
    rtt_samples: AtomicU64,
    /// 收到多少条 `Metrics`。
    metrics_recv: AtomicU64,
    /// 状态变化次数（每次 `SystemState` 变化 +1）。
    state_changes: AtomicU64,
    /// 因为「被控端锁屏」而没发出去的输入消息数。
    dropped_locked: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostStatusSnapshot {
    pub locked: bool,
    pub known: bool,
    pub cpu_pct_x100: u64,
    pub mem_used_mb: u64,
    pub mem_total_mb: u64,
    pub rtt_ms: u64,
    /// 控制端自测的往返时延（微秒）与采样次数。
    pub rtt_local_us: u64,
    pub rtt_samples: u64,
    pub metrics_recv: u64,
    pub state_changes: u64,
    pub dropped_locked: u64,
}

impl HostStatus {
    pub fn locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }

    pub fn snapshot(&self) -> HostStatusSnapshot {
        HostStatusSnapshot {
            locked: self.locked(),
            known: self.known.load(Ordering::Relaxed),
            cpu_pct_x100: self.cpu_x100.load(Ordering::Relaxed),
            mem_used_mb: self.mem_used_mb.load(Ordering::Relaxed),
            mem_total_mb: self.mem_total_mb.load(Ordering::Relaxed),
            rtt_ms: self.rtt_ms.load(Ordering::Relaxed),
            rtt_local_us: self.rtt_local_us.load(Ordering::Relaxed),
            rtt_samples: self.rtt_samples.load(Ordering::Relaxed),
            metrics_recv: self.metrics_recv.load(Ordering::Relaxed),
            state_changes: self.state_changes.load(Ordering::Relaxed),
            dropped_locked: self.dropped_locked.load(Ordering::Relaxed),
        }
    }

    fn apply_metrics(&self, m: &Metrics) {
        let cpu = if m.cpu.is_finite() { m.cpu.clamp(0.0, 100.0) } else { 0.0 };
        self.cpu_x100.store((cpu * 100.0) as u64, Ordering::Relaxed);
        self.mem_used_mb.store(m.mem_used / (1024 * 1024), Ordering::Relaxed);
        self.mem_total_mb.store(m.mem_total / (1024 * 1024), Ordering::Relaxed);
        self.rtt_ms.store(m.rtt_ms as u64, Ordering::Relaxed);
        self.metrics_recv.fetch_add(1, Ordering::Relaxed);
    }

    /// 记一条自测时延。「这条 Pong 是不是刚发出去那条的回应」由 [`RttProbe`] 判。
    fn note_rtt(&self, us: u64) {
        self.rtt_local_us.store(us, Ordering::Relaxed);
        self.rtt_samples.fetch_add(1, Ordering::Relaxed);
    }

    /// 返回 `Some(变了没)`：第一次收到也算「变了」（要打一行日志）。
    fn apply_state(&self, locked: bool, has_desktop: bool) -> bool {
        let first = !self.known.swap(true, Ordering::Relaxed);
        let prev = self.locked.swap(locked, Ordering::Relaxed);
        if first || prev != locked {
            self.state_changes.fetch_add(1, Ordering::Relaxed);
        }
        let _ = has_desktop;
        first || prev != locked
    }
}

/// 控制通道的时延探针间隔。1 秒：内网开销可忽略，又能跟上「延迟刚刚变差」这件事。
const PING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// 进程内的单调时钟（微秒）。
///
/// 用 `Instant` 而不是系统时间：墙上时钟被 NTP 拽一下，时延就会算出负数或者几十秒。
/// 只要取时间的两处都走这里，基准点一致，差值就是真的往返时间。
fn now_us() -> u64 {
    use std::sync::OnceLock;
    static T0: OnceLock<std::time::Instant> = OnceLock::new();
    T0.get_or_init(std::time::Instant::now).elapsed().as_micros() as u64
}

/// 时延探针：发 `C2H::Ping`，按 `H2C::Pong` 回到本机的时间算往返。
///
/// 为什么由控制端自己算：被控端那条 `Metrics.rtt_ms` 恒为 0（被控端没有可比的时钟，
/// 它在协议里如实填 0 而不是编个数）。「这条链路来回多少毫秒」只有控制端量得出来 ——
/// 它同时拥有发出时刻和到达时刻。
///
/// **只认最近一次发出的 seq**：网络乱序时旧 Pong 可能后到，不能让它把新数字冲掉；
/// 同一条 Pong 重复到达也只结算一次。
#[derive(Debug, Default)]
pub struct RttProbe {
    seq: u32,
    last: Option<(u32, u64)>,
}

impl RttProbe {
    /// 该发下一个 Ping 了：返回要发的消息，并记下发出时刻。
    pub fn next(&mut self, now_us: u64) -> C2H {
        self.seq = self.seq.wrapping_add(1);
        self.last = Some((self.seq, now_us));
        // 协议里的 `sent_ms` 是毫秒（被控端原样回带，不影响这边的计算）。
        C2H::Ping { seq: self.seq, sent_ms: now_us / 1000 }
    }

    /// 收到 Pong：对得上最近一次就返回往返微秒数，否则 `None`（旧包/重复包）。
    pub fn on_pong(&mut self, seq: u32, now_us: u64) -> Option<u64> {
        let (want, sent) = self.last?;
        if seq != want {
            return None;
        }
        self.last = None;
        Some(now_us.saturating_sub(sent))
    }
}

#[cfg(test)]
impl InputSender {
    /// 单测用的构造：通道由调用方给，统计从零开始。
    pub fn for_test(tx: mpsc::UnboundedSender<C2H>, status: Arc<HostStatus>) -> Self {
        Self { tx, stats: Arc::new(InputStats::default()), status }
    }
}

/// 给窗口线程用的发送端（`send` 是同步的，窗口事件循环里可以直接调）。
#[derive(Clone)]
pub struct InputSender {
    tx: mpsc::UnboundedSender<C2H>,
    stats: Arc<InputStats>,
    status: Arc<HostStatus>,
}

impl InputSender {
    /// 上报一条输入消息。返回 false 表示控制通道已经没了。
    ///
    /// 被控端锁屏时**在这里就短路**：被控端自己也会丢（那是权威的一侧），
    /// 但控制端不该把每秒上百条鼠标移动白送过去。返回 `true` 是有意的 ——
    /// 这条消息不是「通道坏了」，而是「按状态不发」，混成一个 `false` 会让
    /// 窗口以为连接断了。
    pub fn send(&self, m: C2H) -> bool {
        if m.is_input() && self.status.locked() {
            self.status.dropped_locked.fetch_add(1, Ordering::Relaxed);
            return true;
        }
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
    status: Arc<HostStatus>,
    handle: tokio::task::JoinHandle<()>,
}

impl ControlLink {
    /// 接管已鉴权的控制连接（不启用剪贴板同步）。
    pub fn start(control: SecControl) -> Self {
        Self::start_with_clipboard(control, None)
    }

    /// 同上，但带上剪贴板后端。
    ///
    /// 剪贴板轮询和键鼠上报**共用这一条控制连接、这一个任务**：
    /// 三件事（等上报 → 读回报 → 定时轮询剪贴板）都是「有就干、没有就等」，
    /// 拆成多个任务反而要额外加锁去抢同一个写半边。
    pub fn start_with_clipboard(control: SecControl, clipboard: Option<Box<dyn Clipboard>>) -> Self {
        Self::start_with(control, clipboard, Arc::new(InputStats::default()))
    }

    /// 同上，但计数用**调用方给的**那一份。
    ///
    /// 为什么需要：控制端断线重连之后每条会话都是新的 `ControlLink`，
    /// 而「这次操控一共发了多少条键鼠」是跨会话的量。窗口模式的会话槽持有一份
    /// 累积的 [`InputStats`]，每条会话往里加。
    pub fn start_with(
        control: SecControl,
        clipboard: Option<Box<dyn Clipboard>>,
        stats: Arc<InputStats>,
    ) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<C2H>();
        let status = Arc::new(HostStatus::default());
        let st = stats.clone();
        let status_rx = status.clone();

        let handle = tokio::spawn(async move {
            let (mut rd, mut wr) = control.into_split();
            let mut clip = clipboard;
            let status = status_rx;
            let mut clip_sync = ClipSync::new();
            // 基线：连接这一刻本机剪贴板里已有的内容不算「新内容」，不推给对面。
            // 在任务开头就读一次（而不是等第一拍轮询），把「刚连上就复制」的窗口压到最小。
            let mut clip_armed = false;
            let baseline = clip.as_mut().and_then(|c| c.get_text());
            let _ = poll_outbound(&mut clip_sync, baseline.as_deref(), &mut clip_armed);

            let mut tick = tokio::time::interval(POLL_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // 时延探针。interval 的第一拍立刻到点，先吃掉它 —— 否则刚握手完就发一条，
            // 量到的是握手的尾巴，不是稳态延迟。
            let mut rtt = RttProbe::default();
            let mut ping = tokio::time::interval(PING_INTERVAL);
            ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ping.tick().await;

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
                                Ok(H2C::Clip { text }) => {
                                    // 写进本机剪贴板。写入失败不算致命：对面内容不会丢，
                                    // 只是这台机器粘不出来 —— 所以计数后继续。
                                    match clip.as_mut() {
                                        None => debug!(bytes = text.len(), "没有剪贴板后端，收到的内容已丢弃"),
                                        Some(c) => match clip_sync.inbound(&text) {
                                            None => debug!("收到的剪贴板内容与已知内容相同，忽略"),
                                            Some(t) => match c.set_text(&t) {
                                                Ok(()) => {
                                                    st.clips_recv.fetch_add(1, Ordering::Relaxed);
                                                    debug!(bytes = t.len(), "已写入本机剪贴板");
                                                }
                                                Err(e) => {
                                                    st.clip_errors.fetch_add(1, Ordering::Relaxed);
                                                    warn!(error = %e, "写本机剪贴板失败");
                                                }
                                            },
                                        },
                                    }
                                }
                                Ok(H2C::ClipOffer { len }) => {
                                    debug!(len, "被控端声明有新剪贴板内容（第一版只支持直接内容）");
                                }
                                Ok(H2C::Metrics(m)) => {
                                    status.apply_metrics(&m);
                                    debug!(cpu = m.cpu, mem_total = m.mem_total, "收到被控端指标");
                                }
                                Ok(H2C::SystemState { locked, has_desktop }) => {
                                    // 变化才打日志，否则每秒一条会把窗口模式的终端刷满。
                                    if status.apply_state(locked, has_desktop) {
                                        if locked {
                                            warn!(
                                                has_desktop,
                                                "被控端已锁屏：暂停发送键鼠（解锁后自动恢复）"
                                            );
                                        } else {
                                            info!(has_desktop, "被控端已解锁：恢复发送键鼠");
                                        }
                                    }
                                }
                                Ok(_) => {}
                                Err(e) => debug!(error = %e, "控制通道上收到无法解析的 JSON"),
                            },
                            FrameKind::Pong => {
                                // 被控端对上面那条 Ping 的回应（带 seq）。
                                match f.decode_json::<H2C>() {
                                    Ok(H2C::Pong { seq, .. }) => {
                                        if let Some(us) = rtt.on_pong(seq, now_us()) {
                                            status.note_rtt(us);
                                        }
                                    }
                                    _ => debug!("Pong 帧里的内容解不出来，忽略"),
                                }
                            }
                            FrameKind::Bye => break,
                            _ => {}
                        }
                    }
                    _ = ping.tick() => {
                        // 兼作控制通道的心跳：这条通道半开时，这里的写入迟早会失败，
                        // 比「等用户下次点鼠标」更早发现。
                        let m = rtt.next(now_us());
                        let Ok(bytes) = serde_json::to_vec(&m) else { continue };
                        // **帧级 Ping，不是 Json 帧**：被控端只对 `FrameKind::Ping` 走回声那条路，
                        // 包成 Json 会被当成「尚未实现的控制消息」静默丢掉（第一版就这么错的）。
                        if write_frame(&mut wr, FrameKind::Ping, &bytes).await.is_err() {
                            debug!("控制通道写入失败，时延探针结束");
                            break;
                        }
                    }
                    _ = tick.tick() => {
                        let Some(c) = clip.as_mut() else { continue };
                        let cur = c.get_text();
                        // 读失败与「内容没变」必须分开：`None` 本身两者都可能，
                        // 后端能给出原因时才算「读失败」并计数。
                        if cur.is_none() {
                            if let Some(why) = c.last_error() {
                                let n = st.clip_read_errors.fetch_add(1, Ordering::Relaxed) + 1;
                                // 每 100 次才再打一条，否则 2Hz 会把日志刷满。
                                if n == 1 || n % 100 == 0 {
                                    warn!(
                                        error = %why,
                                        count = n,
                                        "读本机剪贴板失败（不是「内容没变」；本次会话的发送数会一直是 0）"
                                    );
                                }
                            }
                        }
                        let Some(text) = clip_sync.outbound(cur.as_deref()) else {
                            continue;
                        };
                        let m = C2H::Clip { text };
                        let Ok(bytes) = serde_json::to_vec(&m) else { continue };
                        if write_frame(&mut wr, FrameKind::Json, &bytes).await.is_err() {
                            break;
                        }
                        st.clips_sent.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        });

        Self {
            input: InputSender { tx, stats: stats.clone(), status: status.clone() },
            stats,
            status,
            handle,
        }
    }

    /// 被控端运行状态（CPU / 内存 / 锁屏）。窗口标题与收工统计都用它。
    pub fn status(&self) -> Arc<HostStatus> {
        self.status.clone()
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
        let sender = InputSender {
            tx,
            stats: s.clone(),
            status: Arc::new(HostStatus::default()),
        };
        assert!(sender.send(C2H::MouseMoveAbs { x: 0.5, y: 0.5 }));
        assert!(sender.send(C2H::Key { vk: 0x41, down: true, repeat: false }));
        assert!(sender.send(C2H::Ping { seq: 1, sent_ms: 0 }));
        let n = s.snapshot();
        assert_eq!((n.moves, n.keys), (1, 1));
        assert_eq!(n.total(), 2, "Ping 不属于输入，不该计数");
    }

    /// headless 跨会话汇总靠它：两段会话的计数必须逐项相加，别漏字段。
    #[test]
    fn snapshot_accumulates_every_field() {
        let mut a = InputStatsSnapshot {
            moves: 1, keys: 2, chars: 3, buttons: 4, wheels: 5, dropped: 6,
            host_errors: 7, clips_sent: 8, clips_recv: 9, clip_errors: 10,
            clip_read_errors: 11,
        };
        let b = InputStatsSnapshot {
            moves: 100, keys: 200, chars: 300, buttons: 400, wheels: 500, dropped: 600,
            host_errors: 700, clips_sent: 800, clips_recv: 900, clip_errors: 1000,
            clip_read_errors: 1100,
        };
        a.accumulate(&b);
        assert_eq!(a.moves, 101);
        assert_eq!(a.clips_sent, 808);
        assert_eq!(a.clips_recv, 909);
        assert_eq!(a.clip_errors, 1010);
        assert_eq!(a.clip_read_errors, 1111);
        assert_eq!(a.total(), 101 + 202 + 303 + 404 + 505);
    }

    #[test]
    fn closed_channel_counts_drops_instead_of_panicking() {
        let s = Arc::new(InputStats::default());
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx); // 写出任务没了
        let sender = InputSender {
            tx,
            stats: s.clone(),
            status: Arc::new(HostStatus::default()),
        };
        assert!(!sender.send(C2H::Key { vk: 0x41, down: false, repeat: false }));
        assert_eq!(s.snapshot().dropped, 1);
    }

    #[test]
    fn locked_host_stops_input_at_the_client_and_keeps_the_channel_healthy() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let st = Arc::new(HostStatus::default());
        st.apply_state(true, true);
        let sender = InputSender { tx, stats: Arc::new(InputStats::default()), status: st.clone() };

        // 锁屏期间：输入类消息一律不发，但**返回 true**（通道没坏，别让窗口以为断线了）
        assert!(sender.send(C2H::MouseMoveAbs { x: 0.5, y: 0.5 }));
        assert!(sender.send(C2H::Key { vk: 0x41, down: true, repeat: false }));
        assert!(rx.try_recv().is_err(), "锁屏时不该往控制通道里塞输入");
        assert_eq!(st.snapshot().dropped_locked, 2);
        assert_eq!(sender.stats.snapshot().total(), 0, "没发出去就不该算进上报计数");

        // 非输入消息照常走：锁屏时还得能传文件/发剪贴板/道别
        assert!(sender.send(C2H::Clip { text: "还在工作".into() }));
        assert!(matches!(rx.try_recv(), Ok(C2H::Clip { .. })));

        // 解锁后立刻恢复
        st.apply_state(false, true);
        assert!(sender.send(C2H::MouseMoveAbs { x: 0.1, y: 0.1 }));
        assert!(matches!(rx.try_recv(), Ok(C2H::MouseMoveAbs { .. })));
    }

    #[test]
    fn rtt_probe_only_settles_the_matching_pong() {
        let mut p = RttProbe::default();
        assert!(matches!(p.next(1_000_000), C2H::Ping { seq: 1, sent_ms: 1000 }));
        // 4.5 ms 的往返
        assert_eq!(p.on_pong(1, 1_004_500), Some(4_500));
        // 同一条 Pong 又来一次：不重复结算
        assert_eq!(p.on_pong(1, 1_005_000), None);
        // 旧 seq 的 Pong 不能把新的一次冲掉
        assert!(matches!(p.next(2_000_000), C2H::Ping { seq: 2, .. }));
        assert_eq!(p.on_pong(1, 2_000_100), None);
        assert_eq!(p.on_pong(2, 2_000_800), Some(800));
    }

    #[test]
    fn rtt_probe_never_returns_a_negative_round_trip() {
        // 时间戳比发出时刻还早（只可能是时钟基准被换过）：不许冒出天量数字。
        let mut p = RttProbe::default();
        let _ = p.next(5_000_000);
        assert_eq!(p.on_pong(1, 4_999_000), Some(0));
    }

    #[test]
    fn metrics_are_stored_and_clamped_for_display() {
        let st = HostStatus::default();
        assert_eq!(st.snapshot().metrics_recv, 0);
        st.apply_metrics(&Metrics {
            cpu: 1234.5,           // 脏数据（不该出现）：显示成 100%
            mem_used: 3 * 1024 * 1024 * 1024,
            mem_total: 8 * 1024 * 1024 * 1024,
            net_rx: 1,
            net_tx: 2,
            rtt_ms: 7,
        });
        let s = st.snapshot();
        assert_eq!(s.cpu_pct_x100, 10_000, "超过 100% 要夹住，别把标题撑爆");
        assert_eq!((s.mem_used_mb, s.mem_total_mb), (3072, 8192));
        assert_eq!(s.rtt_ms, 7);
        assert_eq!(s.metrics_recv, 1);
        // NaN 也不能让标题出现 "NaN%"
        st.apply_metrics(&Metrics {
            cpu: f32::NAN,
            mem_used: 0,
            mem_total: 0,
            net_rx: 0,
            net_tx: 0,
            rtt_ms: 0,
        });
        assert_eq!(st.snapshot().cpu_pct_x100, 0);
    }

    #[test]
    fn first_state_message_always_counts_as_a_change() {
        let st = HostStatus::default();
        assert!(!st.snapshot().known);
        assert!(st.apply_state(false, true), "第一条 SystemState 要打日志");
        assert!(!st.apply_state(false, true), "同状态不重复打日志");
        assert!(st.apply_state(true, true));
        assert_eq!(st.snapshot().state_changes, 2);
        assert!(st.snapshot().locked);
    }
}