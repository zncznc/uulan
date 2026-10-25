//! 会话槽：把「窗口」和「一条会话」解耦，让断线之后能在**同一个窗口**上接回。
//!
//! 以前控制端是「一条会话到底」：`VideoWatcher` 一结束，`main` 就打印「画面流已结束」退出。
//! 被控端从 M6 起已经能自己重连中继了，但控制端这边中继重启一次就得用户重新点一次连接 ——
//! 而现场最常见的断线原因恰恰就是「升级 = 重装 + 重启服务」。
//!
//! 现在分三层：
//!
//! ```text
//! window::App  ──读──>  Live（会话槽：最新一帧 / 统计 / 输入口 / 状态）  <──写──  drive（会话循环）
//! ```
//!
//! `Live` 在**整个进程**里只创建一次，每条会话结束时只把「输入口」摘掉、
//! 把统计换成本次会话的。于是窗口完全不知道「重连」这回事 —— 它只是继续画最新一帧。
//!
//! 三条纪律：
//!
//! * **断线瞬间必须摘掉输入口**：留着的话窗口会把鼠标移动塞进一个死通道，
//!   计进 `dropped`，事后看日志像网络丢包，其实是自己在往死管道里灌。
//! * **屏幕上的最后一帧不清空**：断线时留着残影 + 标题写「正在接回」，
//!   比闪成黑屏更像「正在恢复」，也让人知道该等一等而不是以为程序挂了。
//! * **只重试「等一会儿可能就好」的错误**：口令错、协议不匹配这类再试一百次也一样，
//!   而且口令错还会往对面的锁定计数里加数（见 [`worth_retrying`]）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::watch;
use tracing::{info, warn};

use crate::connect::{self, ClientError, Connected, HostInfo};
use crate::control::{ControlLink, HostStatus, InputSender, InputStats};
use crate::watch::{Frame, VideoWatcher, WatchError, WatchStats, WatchStatsSnapshot};
use uulan_protocol::msg::Cap;
use uulan_protocol::rv::ErrCode;
use uulan_protocol::rvclient::RvError;

/// 一条会话当前处在什么阶段。窗口标题直接拿它说话。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    /// 还没连上第一条（窗口已经开了、画面还没有）。
    #[default]
    Connecting,
    /// 会话活着。
    Live,
    /// 画面断了，正在按退避重连。
    Reconnecting,
    /// 不再重连了（放弃，或者用户关掉了窗口）。
    Finished,
}

impl Phase {
    /// 标题里那一小段话。`Live` 不写 —— 标题上本来就没有「正常」这一项。
    pub fn label(self) -> &'static str {
        match self {
            Phase::Connecting => "正在连接",
            Phase::Live => "",
            Phase::Reconnecting => "已断线，正在接回",
            Phase::Finished => "已停止重连",
        }
    }
}

/// 窗口与 headless 之间共享的「当前会话」。
///
/// 字段都藏在 `RwLock` 后面而不是各自一个原子量：一次重连要**成组**换掉
/// （统计 / 输入口 / 状态必须来自同一条会话），各自独立就可能读到「新统计 + 旧输入口」。
pub struct Live {
    tx: watch::Sender<Option<Frame>>,
    rx: watch::Receiver<Option<Frame>>,
    slot: RwLock<Slot>,
    /// 键鼠/剪贴板计数。跨会话**累积** —— 「这次操控一共发了多少」不会因为重连归零。
    input_stats: Arc<InputStats>,
    /// 成功建立过的会话数（含第一条）。0 = 一次都没连上。
    sessions: AtomicU64,
    /// 其中由「断线重连」接回来的次数。
    reconnects: AtomicU64,
    /// 所有会话加起来的帧数。窗口标题只显示**当前**会话，收工报告要的是总量。
    totals: RwLock<Totals>,
}

/// 跨会话累计的帧数。指标口径和 `WatchStats` 一致，只是不分会话。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Totals {
    pub aus: u64,
    pub keyframes: u64,
    pub decoded: u64,
    pub dropped: u64,
    pub bytes: u64,
}

impl Totals {
    /// 累进一个会话的统计。
    pub fn add(&mut self, s: &WatchStatsSnapshot) {
        self.aus += s.aus;
        self.keyframes += s.keyframes;
        self.decoded += s.decoded;
        self.dropped += s.dropped;
        self.bytes += s.bytes;
    }
}

struct Slot {
    phase: Phase,
    /// 给标题用的一句话（断线原因 / 重连第几次）。**不含口令类内容**。
    note: String,
    host: Option<HostInfo>,
    stats: Arc<WatchStats>,
    status: Arc<HostStatus>,
    input: Option<InputSender>,
}

impl Live {
    pub fn new() -> Self {
        let (tx, rx) = watch::channel::<Option<Frame>>(None);
        Self {
            tx,
            rx,
            slot: RwLock::new(Slot {
                phase: Phase::Connecting,
                note: String::new(),
                host: None,
                stats: Arc::new(WatchStats::default()),
                status: Arc::new(HostStatus::default()),
                input: None,
            }),
            input_stats: Arc::new(InputStats::default()),
            sessions: AtomicU64::new(0),
            reconnects: AtomicU64::new(0),
            totals: RwLock::new(Totals::default()),
        }
    }

    /// 累积的键鼠/剪贴板计数（跨会话）。
    pub fn input_stats(&self) -> Arc<InputStats> {
        self.input_stats.clone()
    }

    /// 给会话用的帧发送端（跨会话不变，窗口那边握着对应的接收端）。
    pub fn frames_tx(&self) -> watch::Sender<Option<Frame>> {
        self.tx.clone()
    }

    /// 给窗口用的帧接收端。**同一个接收端要一直用到进程结束**。
    pub fn frames(&self) -> watch::Receiver<Option<Frame>> {
        self.rx.clone()
    }

    pub fn stats(&self) -> Arc<WatchStats> {
        self.read().stats.clone()
    }

    pub fn status(&self) -> Arc<HostStatus> {
        self.read().status.clone()
    }

    /// 当前会话的输入口。断线期间是 `None` —— 窗口照常调用，消息直接丢掉。
    pub fn input(&self) -> Option<InputSender> {
        self.read().input.clone()
    }

    pub fn host(&self) -> Option<HostInfo> {
        self.read().host.clone()
    }

    pub fn phase(&self) -> Phase {
        self.read().phase
    }

    pub fn note(&self) -> String {
        self.read().note.clone()
    }

    pub fn sessions(&self) -> u64 {
        self.sessions.load(Ordering::Relaxed)
    }

    pub fn reconnects(&self) -> u64 {
        self.reconnects.load(Ordering::Relaxed)
    }

    /// 一条会话收尾时把它的帧数累进来。
    pub fn add_totals(&self, s: &WatchStatsSnapshot) {
        let mut t = self.totals.write().unwrap_or_else(|e| e.into_inner());
        t.add(s);
    }

    pub fn totals(&self) -> Totals {
        *self.totals.read().unwrap_or_else(|e| e.into_inner())
    }

    /// 收工报告用的总量：已结算的会话 + **当前还在跑的那条**。
    ///
    /// 为什么要后者：窗口关掉时 `drive` 被 abort，最后一条会话走不到
    /// 「结算」那一行。少了它，报告里就只有第一条会话的帧数 —— 看起来像
    /// 「接回之后再也没出过画面」，正好和事实相反。
    pub fn totals_now(&self) -> Totals {
        let mut t = self.totals();
        let g = self.read();
        if g.phase == Phase::Live {
            t.add(&g.stats.snapshot());
        }
        t
    }

    /// 一条会话起来了：换上它的统计 / 输入口 / 状态。
    pub fn mark_live(
        &self,
        host: HostInfo,
        stats: Arc<WatchStats>,
        status: Arc<HostStatus>,
        input: Option<InputSender>,
    ) {
        let first = self.sessions.fetch_add(1, Ordering::Relaxed) == 0;
        if !first {
            self.reconnects.fetch_add(1, Ordering::Relaxed);
        }
        let mut g = self.write();
        g.phase = Phase::Live;
        g.note.clear();
        g.host = Some(host);
        g.stats = stats;
        g.status = status;
        g.input = input;
    }

    /// 断了，正在退避重连。输入口当场摘掉。
    pub fn mark_reconnecting(&self, attempt: u32, why: &str) {
        let mut g = self.write();
        g.phase = Phase::Reconnecting;
        g.note = format!("第 {attempt} 次 · {why}");
        g.input = None;
    }

    /// 不再重连了。`reason` 会进日志和标题（不含口令类内容）。
    pub fn mark_finished(&self, reason: &str) {
        let mut g = self.write();
        g.phase = Phase::Finished;
        g.note = reason.to_string();
        g.input = None;
    }

    /// 锁中毒时照旧取内层：这里的临界区只有字段赋值，没有可能「半途 panic 留下坏状态」的逻辑。
    /// 为了一个显示用的槽把整个进程崩掉不值得。
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Slot> {
        self.slot.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Slot> {
        self.slot.write().unwrap_or_else(|e| e.into_inner())
    }
}

impl Default for Live {
    fn default() -> Self {
        Self::new()
    }
}

/// 一条会话的完整配置。窗口模式与 headless 共用同一份。
#[derive(Debug, Clone)]
pub struct Plan {
    pub server: String,
    pub device: String,
    /// 口令。**不进日志**：只有 `connect` 用，其余地方一律不碰。
    pub password: String,
    pub client_name: String,
    pub caps: Vec<Cap>,
    pub ffmpeg: Option<std::path::PathBuf>,
    /// 是否在这条会话上启用剪贴板同步。每条会话都要**新建**一个后端：
    /// X11 那个后端是「常驻线程当剪贴板所有者」，跨会话复用会把上一次的 owner 状态带进来。
    pub clipboard: bool,
    pub no_input: bool,
    /// 断线后最多连续重连几次（0 = 断线即收工）。
    pub max_reconnects: u32,
}

impl Plan {
    async fn connect(&self) -> Result<Connected, ClientError> {
        connect::connect(
            &self.server,
            &self.device,
            &self.password,
            &self.client_name,
            self.caps.clone(),
        )
        .await
    }
}

/// 重连退避（秒）：1、2、4、8、15，之后一直 30。
///
/// 为什么不从 0 开始立刻连：中继重启要 2–3 秒才起得来，被控端那边也要重新注册
/// （它的退避同样从 1 秒起）。立刻重试只会连撞三次「设备不在线」，然后才退到 30 秒 ——
/// 反而更慢。
const STEPS: [u64; 5] = [1, 2, 4, 8, 15];

/// 退避策略本身。纯逻辑（不碰网络、不睡），所以能在本机用 `cargo test` 钉住。
#[derive(Debug)]
pub struct Reconnect {
    max: u32,
    used: u32,
}

impl Reconnect {
    pub fn new(max: u32) -> Self {
        Self { max, used: 0 }
    }

    /// 已经连续重连了几次。
    pub fn used(&self) -> u32 {
        self.used
    }

    /// 上个会话是好的（真的出过画面 / 撑够了一段时间）：连续失败计数清零。
    /// 不清零的话「用得越久、下次断线等得越久」，完全说不通。
    pub fn recovered(&mut self) {
        self.used = 0;
    }

    /// 断了（或没连上）：返回「等多久再试」，`None` = 该收工了。
    ///
    /// `why` 为 `None` 表示「会话中途断了」——那时没有 `ClientError` 可看，
    /// 一律当作值得接回（我们连错误都不知道，先试一次再说）。
    pub fn retry(&mut self, why: Option<&ClientError>) -> Option<Duration> {
        if self.used >= self.max {
            return None;
        }
        if let Some(e) = why {
            if !worth_retrying(e) {
                return None;
            }
        }
        let i = (self.used as usize).min(STEPS.len() - 1);
        self.used += 1;
        Some(Duration::from_secs(STEPS[i]))
    }
}

/// 会话中途断开之后，这个错误还值得再等一会儿吗？
///
/// 和连接阶段的 `is_retryable` **有意不同**：
///
/// * **「设备不在线」在中途重连时是正常现象** —— 中继刚重启，被控端还在退避里、
///   没重新注册，这时报 Offline 恰恰说明「等一下就有了」。连接阶段相反：
///   用户把 ID 打错了，原样告诉他才是对的。
/// * **被拒**（口令错 / 被锁 / 忙）一律不重试：那是对面明确表态，
///   再试只会往它的失败计数里加数，把「输错一次」升级成「锁一分钟」。
pub fn worth_retrying(e: &ClientError) -> bool {
    match e {
        ClientError::Rv(RvError::Io(_)) | ClientError::Rv(RvError::Timeout(_)) => true,
        // 中继还在重启（连得上但服务端内部没准备好）、设备还没重新注册、
        // 会合票据过期（我们重连得太慢）—— 三种都是「再等一拍」。
        ClientError::Rv(RvError::Server { code, .. }) => {
            matches!(code, ErrCode::Offline | ErrCode::Internal | ErrCode::BadSession)
        }
        // 会话级握手超时：被控端正忙着收尾上一条会话。
        ClientError::Timeout => true,
        _ => false,
    }
}

/// 起会话失败。分两类，因为「要不要重连」的答案完全不同。
#[derive(Debug, Clone)]
pub enum OpenError {
    /// 对面明确表态（拒绝推流、没报画面参数、对面没有图形会话）：
    /// 重连一百次也是同一个答案，还会把用户晾两分钟。
    Refused(String),
    /// 传输/解码一侧的问题：可能只是这条连接坏了，值得再试。
    Transient(String),
}

impl OpenError {
    pub fn why(&self) -> &str {
        match self {
            OpenError::Refused(s) | OpenError::Transient(s) => s,
        }
    }

    pub fn retryable(&self) -> bool {
        matches!(self, OpenError::Transient(_))
    }
}

/// 一条已经起来的会话。**必须活到这条会话结束**：Drop 会结束控制通道的任务。
pub struct Open {
    pub host: HostInfo,
    pub watcher: VideoWatcher,
    pub link: ControlLink,
    /// 输入口。headless 不用它，但**必须留着**：全掉了控制通道的写任务会退出，
    /// 被控端看到控制通道 EOF 就会把整条会话收掉（画面跟着断）——
    /// 那就成了「客户端自己把会话关掉，然后怪网络」。
    _input: Option<InputSender>,
}

/// 起来一条会话：视频流水线 + 控制通道。
///
/// `live` 为 `Some` 时把结果挂到会话槽上（窗口模式）；headless 传 `None`，
/// 自己拿着 `watcher` 收帧。
pub async fn open(
    live: Option<&Arc<Live>>,
    plan: &Plan,
    c: Connected,
) -> Result<Open, OpenError> {
    // 帧通道：窗口模式必须用会话槽里那一条（跨会话不变），
    // 否则窗口手里那个接收端会在重连后指向一条再也没人写的通道 —— 画面永久冻住。
    let started = match live {
        Some(l) => VideoWatcher::start_into(c.video, plan.ffmpeg.clone(), l.frames_tx()).await,
        None => VideoWatcher::start(c.video, plan.ffmpeg.clone()).await,
    };
    let watcher = started.map_err(|e| match e {
        // 被控端说「我不推流了」/压根没报画面参数：那是它的表态，重连没用。
        WatchError::HostRefused(_) | WatchError::NoVideoCfg => {
            OpenError::Refused(format!("开视频通道失败：{e}"))
        }
        _ => OpenError::Transient(format!("开视频通道失败：{e}")),
    })?;

    let clip = if plan.clipboard {
        Some(uulan_clip::default_clipboard(None))
    } else {
        None
    };
    let link = match live {
        Some(l) => ControlLink::start_with(c.control, clip, l.input_stats()),
        None => ControlLink::start_with_clipboard(c.control, clip),
    };
    let input = if c.host.has_desktop && !plan.no_input {
        Some(link.input())
    } else {
        None
    };

    if let Some(l) = live {
        l.mark_live(c.host.clone(), watcher.stats(), link.status(), input.clone());
    }
    info!(
        session = %uulan_protocol::redact::tag(&c.session),
        host = %c.host.name,
        os = %c.host.os,
        "会话已建立"
    );

    Ok(Open { host: c.host, watcher, link, _input: input })
}

/// 会话循环（窗口模式）：连 → 跑 → 断了就退避重连，直到不再重连（或任务被 abort）。
///
/// 「用户关窗」不走到这里：窗口一关，`main` 就把这个任务 abort 掉，
/// 整条流水线连同解码器一起收掉（`VideoWatcher::drop`）。所以这里不需要
/// 「被要求停下」的分支 —— 少一个分支就少一处「停不下来」的可能。
pub async fn drive(live: Arc<Live>, plan: Plan, first: Connected) {
    let mut next = Some(first);
    let mut rc = Reconnect::new(plan.max_reconnects);

    loop {
        // ── 1) 拿一条连接 ──
        let c = match next.take() {
            Some(c) => c,
            None => match plan.connect().await {
                Ok(c) => {
                    info!(reconnect = rc.used(), "已接回被控端");
                    c
                }
                Err(e) => {
                    let wait = rc.retry(Some(&e));
                    live.mark_reconnecting(rc.used(), &e.to_string());
                    match wait {
                        Some(d) => {
                            warn!(
                                error = %e,
                                attempt = rc.used(),
                                wait_s = d.as_secs(),
                                "接回失败，稍后再试"
                            );
                            tokio::time::sleep(d).await;
                            continue;
                        }
                        None => {
                            live.mark_finished(&format!("重连放弃：{e}"));
                            warn!(error = %e, "不再重连，控制端退出");
                            return;
                        }
                    }
                }
            },
        };

        // ── 2) 跑一条会话，直到画面断 ──
        let reason = match open(Some(&live), &plan, c).await {
            Ok(mut o) => {
                let since = std::time::Instant::now();
                o.watcher.wait_end().await;
                let s = o.watcher.snapshot();
                live.add_totals(&s);
                o.link.close("控制端准备重连").await;
                // 撑够一段时间（或真的出过画面）才算「这条会话是好的」，清掉退避计数。
                if s.decoded > 0 && since.elapsed() >= Duration::from_secs(2) {
                    rc.recovered();
                }
                if s.silent_s > 0 {
                    // 半开连接判死：这条会话不是「对面道别」结束的，是看门狗掐掉的。
                    // 原因要原样带进标题/日志，否则现场只看到「画面流结束」，像正常收尾。
                    format!(
                        "视频通道静默 {} 秒（连接半开？），本次会话解出 {} 帧",
                        s.silent_s, s.decoded
                    )
                } else {
                    format!("画面流结束（本次会话解出 {} 帧）", s.decoded)
                }
            }
            Err(e) if !e.retryable() => {
                live.mark_finished(e.why());
                warn!(why = e.why(), "这条会话起不来，且不值得重连，控制端退出");
                return;
            }
            Err(e) => e.why().to_string(),
        };

        // ── 3) 接得回去吗 ──
        match rc.retry(None) {
            Some(d) => {
                live.mark_reconnecting(rc.used(), &reason);
                warn!(
                    reason = %reason,
                    attempt = rc.used(),
                    wait_s = d.as_secs(),
                    "会话断开，准备重连"
                );
                tokio::time::sleep(d).await;
            }
            None => {
                live.mark_finished(&format!("{reason}；已停止重连"));
                warn!(reason = %reason, "不再重连，控制端退出");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    fn rv_server(code: ErrCode) -> ClientError {
        ClientError::Rv(RvError::Server { code, message: "x".into() })
    }

    #[test]
    fn backoff_starts_at_one_second_and_caps_at_fifteen() {
        let mut r = Reconnect::new(10);
        let waits: Vec<u64> = (0..7)
            .map(|_| r.retry(None).expect("还该重试").as_secs())
            .collect();
        assert_eq!(waits, vec![1, 2, 4, 8, 15, 15, 15]);
    }

    #[test]
    fn giving_up_after_the_configured_attempts() {
        let mut r = Reconnect::new(3);
        assert!(r.retry(None).is_some());
        assert!(r.retry(None).is_some());
        assert!(r.retry(None).is_some());
        assert!(r.retry(None).is_none(), "第 4 次就不该再试");
        assert_eq!(r.used(), 3);
    }

    #[test]
    fn zero_means_no_reconnect_at_all() {
        // `--reconnect 0` 要能一次都不试（现场排障时希望「断线就退出」）
        let mut r = Reconnect::new(0);
        assert!(r.retry(None).is_none());
    }

    #[test]
    fn recovery_resets_the_backoff() {
        let mut r = Reconnect::new(10);
        assert_eq!(r.retry(None).unwrap().as_secs(), 1);
        assert_eq!(r.retry(None).unwrap().as_secs(), 2);
        r.recovered();
        assert_eq!(r.used(), 0);
        assert_eq!(r.retry(None).unwrap().as_secs(), 1, "长会话之后再断，不该等 15 秒");
    }

    #[test]
    fn offline_is_worth_retrying_mid_session_but_not_much_else() {
        // 中继重启后、被控端还没重新注册：这时报 Offline 是**预期**，必须继续等
        assert!(worth_retrying(&rv_server(ErrCode::Offline)));
        assert!(worth_retrying(&ClientError::Rv(RvError::Io(io::Error::other("reset")))));
        assert!(worth_retrying(&ClientError::Timeout));

        // 被拒 / 说得清的失败：再试一百次也一样
        assert!(!worth_retrying(&ClientError::Rejected("密码不正确".into())));
        assert!(!worth_retrying(&rv_server(ErrCode::BadDeviceId)));
        assert!(!worth_retrying(&rv_server(ErrCode::ProtoMismatch)));
        assert!(!worth_retrying(&ClientError::Unexpected("x".into())));
    }

    #[test]
    fn a_rejected_error_stops_the_loop_immediately() {
        // 口令错时连一次都不该再试（每试一次都在往对面的锁定计数里加数）
        let mut r = Reconnect::new(10);
        let e = ClientError::Rejected("密码不正确".into());
        assert!(r.retry(Some(&e)).is_none());
        assert_eq!(r.used(), 0, "被拒的情况不该消耗重连次数");
    }

    #[test]
    fn phase_labels_are_what_the_title_shows() {
        assert_eq!(Phase::Live.label(), "", "正常时不往标题里塞字");
        assert!(Phase::Reconnecting.label().contains("接回"));
        assert!(Phase::Finished.label().contains("停止"));
    }

    #[test]
    fn live_slot_swaps_session_fields_together() {
        let live = Live::new();
        assert_eq!(live.phase(), Phase::Connecting);
        assert!(live.input().is_none());

        let host = HostInfo {
            device_id: "123456789".into(),
            os: "kylin".into(),
            version: "0.1.0".into(),
            name: "mis-pc".into(),
            screen: uulan_protocol::msg::Screen { w: 1920, h: 1080 },
            caps: vec![],
            has_desktop: true,
        };
        let stats = Arc::new(WatchStats::default());
        let status = Arc::new(HostStatus::default());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let input = InputSender::for_test(tx, status.clone());

        live.mark_live(host.clone(), stats.clone(), status.clone(), Some(input));
        assert_eq!(live.phase(), Phase::Live);
        assert_eq!(live.sessions(), 1);
        assert_eq!(live.reconnects(), 0);
        assert!(live.input().is_some());
        assert_eq!(live.host().unwrap().name, "mis-pc");

        // 断线：输入口必须当场消失（否则窗口会往死通道里塞鼠标）
        live.mark_reconnecting(1, "画面流结束");
        assert_eq!(live.phase(), Phase::Reconnecting);
        assert!(live.input().is_none());
        assert!(live.note().contains("第 1 次"));

        // 接回：会话数 +1、重连数 +1，统计换成新会话的
        let stats2 = Arc::new(WatchStats::default());
        live.mark_live(host, stats2.clone(), status, None);
        assert_eq!(live.sessions(), 2);
        assert_eq!(live.reconnects(), 1);
        assert!(Arc::ptr_eq(&live.stats(), &stats2), "统计要跟着会话换");
    }

    #[test]
    fn finished_is_sticky_and_drops_the_input() {
        let live = Live::new();
        live.mark_finished("重连放弃");
        assert_eq!(live.phase(), Phase::Finished);
        assert!(live.note().contains("放弃"));
        assert!(live.input().is_none());
    }
}