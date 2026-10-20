//! 一次远程会话。
//!
//! 生命周期：
//!
//! ```text
//! 控制端                                  被控端（这里）
//!   |-- Hello(ClientHello) ------------>|
//!   |<-- HelloOk(HostHelloOk) ----------|
//!   |-- Auth{password} ---------------->|   ← 校验在**被控端**做，中继不知道密码
//!   |<-- AuthOk / AuthFail -------------|
//!   |                                   |-- 启动 FFmpeg 抓屏编码
//!   |<-- VideoCfg{w,h,codec,fps} -------|
//!   |<-- VideoAu(时间戳 + H.264 AU) ----|   （video 通道，持续）
//!   |-- 键鼠 / 剪贴板 / 文件 ... ------->|   （control 通道，M3 起）
//! ```
//!
//! 鉴权失败会累积到 [`AuthState`]，错够 [`AUTH_MAX_FAILURES`] 次开始锁并指数退避。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use uulan_media::ffmpeg::{CaptureOptions, Encoder};
use uulan_media::VideoSource;
use uulan_protocol::frame::{read_frame, write_frame, video, FrameKind};
use uulan_protocol::msg::{
    quality_preset, AuthFailReason, C2H, Cap, H2C, HostHelloOk, HostVideoCfg, Quality, Screen,
};
use uulan_protocol::rvclient::RelaySet;

use uulan_clip::{poll_outbound, ClipSync, Clipboard, POLL_INTERVAL};

use crate::capture::CaptureInput;
use crate::config::HostConfig;
use crate::input::{default_sink, Inject, InputSink, Mapper};

/// 实测帧率：按固定窗口统计编码器**真正产出**的 AU 数。
///
/// 为什么不是「累计帧数 / 总时长」：长会话里累计平均会越算越钝，
/// 用户看不到「刚刚掉到 5 fps」这种事 —— 而那恰恰是最该看到的。
///
/// 为什么不直接信 ffmpeg 报的 fps：x11grab 在静止画面上本来就按「有变化才出帧」工作，
/// 报的是请求帧率，不是实际帧率。真实数字只能靠自己数。
#[derive(Debug)]
pub struct FpsMeter {
    count: u32,
    since: Instant,
    span: Duration,
    last: f32,
    samples: u64,
}

impl FpsMeter {
    pub fn new(span: Duration) -> Self {
        Self::starting_at(Instant::now(), span)
    }

    pub fn starting_at(now: Instant, span: Duration) -> Self {
        Self { count: 0, since: now, span, last: 0.0, samples: 0 }
    }

    /// 记一帧；返回「刚好凑满一个窗口」（此时 [`FpsMeter::fps`] 已更新）。
    pub fn tick(&mut self) -> bool {
        self.tick_at(Instant::now())
    }

    pub fn tick_at(&mut self, now: Instant) -> bool {
        self.count += 1;
        let el = now.saturating_duration_since(self.since);
        if el < self.span {
            return false;
        }
        self.last = self.count as f32 / el.as_secs_f32().max(1e-6);
        self.count = 0;
        self.since = now;
        self.samples += 1;
        true
    }

    /// 最近一个完整窗口的帧率。还没测出来时报 0.0（「还没测」而不是「测得是 0」）。
    pub fn fps(&self) -> f32 {
        self.fps_at(Instant::now())
    }

    /// 同上，但以给定时刻判断「是不是已经停了」。
    ///
    /// 编码器卡住/停止出帧时必须把帧率归零：否则控制端会一直看到「30 fps」，
    /// 而画面其实是冻住的 —— 那是比不显示更坏的误导。
    pub fn fps_at(&self, now: Instant) -> f32 {
        if self.samples == 0 {
            return 0.0;
        }
        if now.saturating_duration_since(self.since) > self.span * 3 {
            return 0.0;
        }
        self.last
    }
}

/// 连续失败多少次开始锁定。
pub const AUTH_MAX_FAILURES: u32 = 5;
/// 首次锁定时长；之后每多错一次翻倍，封顶 [`AUTH_LOCK_CAP`]。
pub const AUTH_LOCK_BASE: Duration = Duration::from_secs(60);
pub const AUTH_LOCK_CAP: Duration = Duration::from_secs(300);

/// 密码比较。用常量时间比较，避免通过响应时间猜密码。
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    // 长度不同也要走完整轮循环，别提前返回。
    let mut diff = (a.len() ^ b.len()) as u8;
    let n = a.len().max(b.len());
    for i in 0..n {
        let x = *a.get(i).unwrap_or(&0);
        let y = *b.get(i).unwrap_or(&0);
        diff |= x ^ y;
    }
    diff == 0
}

#[derive(Debug, Default)]
struct AuthInner {
    failures: u32,
    locked_until: Option<Instant>,
}

/// 跨会话共享的鉴权状态。
///
/// 必须是共享的：如果只按单次会话计数，攻击者每次失败后重连就能重置计数，
/// 锁定形同虚设。
#[derive(Debug, Clone, Default)]
pub struct AuthState {
    inner: Arc<Mutex<AuthInner>>,
}

impl AuthState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前是否处于锁定。是则返回剩余时长。
    pub fn locked_for(&self, now: Instant) -> Option<Duration> {
        let g = self.inner.lock().unwrap();
        match g.locked_until {
            Some(t) if t > now => Some(t - now),
            _ => None,
        }
    }

    /// 记一次失败。返回 `(累计失败次数, 本次锁定时长)`；未达阈值时锁定时长为 0。
    pub fn record_failure(&self, now: Instant) -> (u32, Duration) {
        let mut g = self.inner.lock().unwrap();
        g.failures = g.failures.saturating_add(1);
        if g.failures < AUTH_MAX_FAILURES {
            return (g.failures, Duration::ZERO);
        }
        let extra = (g.failures - AUTH_MAX_FAILURES).min(3);
        let secs = (AUTH_LOCK_BASE.as_secs() << extra).min(AUTH_LOCK_CAP.as_secs());
        let d = Duration::from_secs(secs);
        g.locked_until = Some(now + d);
        (g.failures, d)
    }

    pub fn record_success(&self) {
        let mut g = self.inner.lock().unwrap();
        g.failures = 0;
        g.locked_until = None;
    }

    pub fn failures(&self) -> u32 {
        self.inner.lock().unwrap().failures
    }
}

/// 被控端的持久状态：配置 + 鉴权计数。
pub struct HostState {
    cfg: Mutex<HostConfig>,
    cfg_path: Option<PathBuf>,
    pub auth: AuthState,
}

pub enum AuthResult {
    /// 通过。`used_temp_code` 为真表示吃掉了一次性临时验证码，调用方该去落盘。
    Ok { used_temp_code: bool },
    /// 密码不对。
    Bad,
}

impl HostState {
    pub fn new(cfg: HostConfig, cfg_path: Option<PathBuf>) -> Self {
        Self { cfg: Mutex::new(cfg), cfg_path, auth: AuthState::new() }
    }

    pub fn snapshot(&self) -> HostConfig {
        self.cfg.lock().unwrap().clone()
    }

    pub fn device_id(&self) -> Option<String> {
        self.cfg.lock().unwrap().device_id.clone()
    }

    pub fn set_device_id(&self, id: &str) {
        let mut g = self.cfg.lock().unwrap();
        if g.device_id.as_deref() != Some(id) {
            g.device_id = Some(id.to_string());
            drop(g);
            self.persist();
        }
    }

    pub fn name(&self) -> String {
        self.cfg.lock().unwrap().name.clone()
    }

    /// 校验访问密码，一次性临时码用掉即失效。
    ///
    /// 顺序上先比永久密码再比临时码 —— 临时码是给「临时协助」场景的，
    /// 用它连接不应打断主人自己用永久密码。
    pub fn authenticate(&self, given: &str) -> AuthResult {
        let mut used_temp = false;
        {
            let mut g = self.cfg.lock().unwrap();
            let permanent_ok = ct_eq(given, &g.password);
            if permanent_ok {
                // 永久密码命中，临时码不动
            } else if let Some(t) = g.temp_code.clone() {
                if ct_eq(given, &t) {
                    g.temp_code = None;
                    used_temp = true;
                } else {
                    return AuthResult::Bad;
                }
            } else {
                return AuthResult::Bad;
            }
        }
        if used_temp {
            self.persist();
        }
        AuthResult::Ok { used_temp_code: used_temp }
    }

    fn persist(&self) {
        let Some(p) = &self.cfg_path else { return };
        let cfg = self.cfg.lock().unwrap().clone();
        if let Err(e) = cfg.save(p) {
            // 落盘失败不能把会话搞崩，但要明确告警：临时码可能被重复使用。
            warn!(path = %p.display(), error = %e, "被控端配置落盘失败");
        }
    }
}

/// 视频参数的可变部分。改动它会触发编码器重启。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoSettings {
    pub fps: u32,
    pub quality: Quality,
    /// 仅 [`Quality::Custom`] 时作为码率上限；其余档位由换算表决定。
    pub max_bitrate_kbps: u32,
    pub scale: f32,
}

impl VideoSettings {
    pub fn from_config(c: &HostConfig) -> Self {
        Self {
            fps: if c.fps == 0 { 30 } else { c.fps },
            quality: c.quality,
            max_bitrate_kbps: c.max_bitrate_kbps,
            scale: 1.0,
        }
    }

    /// 换算成编码器参数 `(恒定质量档, 码率上限 kbps)`。
    ///
    /// 上限为 0 表示「不设上限，纯恒定质量」—— 这正是 `Quality::Auto` 的语义。
    /// 换算表在 [`uulan_protocol::msg::quality_preset`]：档位是协议契约，
    /// 两端必须按同一张表理解，所以它不能散落在各自的编码层里。
    pub fn cq_and_maxrate(self) -> (u8, u32) {
        let (cq, preset) = quality_preset(self.quality);
        let maxrate = match self.quality {
            Quality::Custom => self.max_bitrate_kbps,
            _ => preset,
        };
        (cq, maxrate)
    }
}

/// 开一次会话所需的全部输入。
pub struct SessionParams {
    pub state: Arc<HostState>,
    pub ffmpeg: PathBuf,
    pub capture: CaptureInput,
    pub screen: Screen,
    pub encoder: Encoder,
    pub settings: VideoSettings,
    /// 剪贴板后端。`None` = 按采集源自动挑（X11 采集 → X11 剪贴板）。
    ///
    /// 之所以做成可注入的：本机测试要塞一个「内存剪贴板」，才能在没有 X11 的机器上
    /// 断言「控制端复制的文字真的写进了被控端剪贴板、反过来也一样」。
    pub clipboard: Option<Box<dyn Clipboard>>,
}

#[derive(Debug)]
pub enum SessionOutcome {
    /// 对端道别或正常断开。
    Closed(String),
    /// 对端一直不通过鉴权。
    Unauthenticated(String),
    /// 出错。
    Error(String),
}

/// 跑完一次会话。注入后端按采集源自动挑（X11 → XTest，合成源 → 不注入）。
pub async fn run(params: SessionParams, relays: RelaySet, peer: String) -> SessionOutcome {
    let sink = default_sink(&params.capture);
    run_with_sink(params, relays, peer, sink).await
}

/// 同 [`run`]，但注入后端由调用方指定。
///
/// 单独开这个入口是为了让「协议消息 → 注入指令」这段能在没有 X11 的机器上被端到端测到：
/// 测试塞一个记录用的 sink，就能断言被控端到底打算注入什么。生产路径用 [`run`]。
pub async fn run_with_sink(
    params: SessionParams,
    relays: RelaySet,
    peer: String,
    sink: Box<dyn InputSink>,
) -> SessionOutcome {
    let RelaySet { control, video, bulk } = relays;
    let mut control = control;
    // bulk 通道到 M4（文件传输 / 端口映射）才用得上；现在只保持连接不关。
    let _bulk = bulk;

    // 剪贴板后端：抓的是哪个 X 显示，剪贴板就得是哪个 X 显示的。
    let mut params = params;
    let mut clip: Box<dyn Clipboard> = params
        .clipboard
        .take()
        .unwrap_or_else(|| uulan_clip::default_clipboard(params.capture.display().as_deref()));
    let clip_available = clip.available();

    let cfg = params.state.snapshot();
    let client = match handshake(&mut control, &params.state, params.screen, &peer, clip_available).await {
        Ok(Some(c)) => c,
        // 鉴权没过：已经回过 AuthFail 了
        Ok(None) => return SessionOutcome::Unauthenticated(peer.clone()),
        Err(e) => return SessionOutcome::Error(e),
    };

    info!(
        %peer,
        client_os = %client.os,
        client_name = %client.name,
        device_id = ?cfg.device_id,
        "会话已授权，开始推流"
    );

    // 视频参数的热更新通道：控制端改画质/帧率时不需要断开重连。
    let (set_tx, set_rx) = watch::channel(params.settings);
    let video_task = tokio::spawn(video_loop(
        params.ffmpeg.clone(),
        params.capture.clone(),
        params.screen,
        params.encoder,
        params.state.device_id().unwrap_or_default(),
        set_rx,
        video,
    ));

    // 键鼠注入：Mapper 是纯状态机，sink 才是真后端。
    let mut input = InputLoop::new(Mapper::new(params.screen.w, params.screen.h), sink);
    info!(sink = input.sink.name(), "键鼠注入后端");
    info!(backend = clip.name(), available = clip_available, "剪贴板后端");

    // 收发分两半：控制通道现在**同时**要有「读一帧」和「每秒两次轮询剪贴板」
    // 两条独立的时间线，共用一个 `&mut TcpStream` 是借不动的（也是会互相阻塞的）。
    let (mut rd, mut wr) = control.into_split();
    let mut clip_sync = ClipSync::new();
    let mut clip_stats = ClipStats::default();

    let outcome = control_loop(
        &mut rd,
        &mut wr,
        &set_tx,
        &mut input,
        &mut clip,
        &mut clip_sync,
        &mut clip_stats,
    )
    .await;

    // 会话结束（正常/异常/断线）都要把还按着的键松开、把全局状态还原，
    // 否则被控端上会留下一个「一直按住 Ctrl」的键盘。
    input.finish().await;
    info!(
        events = input.events,
        inject_errors = input.errors,
        "键鼠事件统计"
    );
    let (over, empty, same) = clip_sync.skipped();
    info!(
        sent = clip_stats.sent,
        recv = clip_stats.recv,
        errors = clip_stats.errors,
        skipped_oversize = over,
        skipped_empty = empty,
        skipped_same = same,
        "剪贴板同步统计"
    );

    video_task.abort();
    let _ = wr.shutdown().await;

    match outcome {
        Ok(Closed::Bye(reason)) => SessionOutcome::Closed(format!("对端道别: {reason}")),
        Ok(Closed::Eof) => SessionOutcome::Closed("对端关闭了连接".into()),
        Err(e) => SessionOutcome::Error(e),
    }
}

enum Closed {
    Bye(String),
    Eof,
}

/// 握手 + 鉴权。返回 `Ok(None)` 表示鉴权未通过（已回复 AuthFail）。
async fn handshake(
    control: &mut TcpStream,
    state: &HostState,
    screen: Screen,
    peer: &str,
    clip_available: bool,
) -> Result<Option<uulan_protocol::msg::ClientHello>, String> {
    let cfg = state.snapshot();

    // 1) 等 Hello
    let client = match read_c2h(control).await? {
        Some(C2H::Hello(c)) => c,
        Some(other) => return Err(format!("握手时期望 Hello，收到 {other:?}")),
        None => return Err("对端在握手前就断开了".into()),
    };

    // 2) 报自己的信息，让控制端先能显示设备名与屏幕尺寸
    //    能力只声明**真的实现**了的：声明了却没实现，控制端就会出现「点了没反应」的按钮。
    let mut caps = vec![];
    if clip_available {
        caps.push(Cap::Clipboard);
    }
    send_h2c(
        control,
        &H2C::HelloOk(HostHelloOk {
            device_id: cfg.device_id.clone().unwrap_or_default(),
            os: std::env::consts::OS.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            name: cfg.name.clone(),
            screen,
            caps,
            has_desktop: true,
        }),
    )
    .await?;

    // 3) 等 Auth
    let given = match read_c2h(control).await? {
        Some(C2H::Auth { password }) => password,
        Some(other) => return Err(format!("握手时期望 Auth，收到 {other:?}")),
        None => return Err("对端在鉴权前就断开了".into()),
    };

    // 4) 先看锁定
    let now = Instant::now();
    if let Some(left) = state.auth.locked_for(now) {
        warn!(%peer, remaining_ms = left.as_millis() as u64, "鉴权处于锁定期，拒绝");
        send_h2c(
            control,
            &H2C::AuthFail {
                reason: AuthFailReason::Locked,
                retry_after_ms: left.as_millis() as u64,
            },
        )
        .await?;
        return Ok(None);
    }

    // 5) 校验
    match state.authenticate(&given) {
        AuthResult::Ok { used_temp_code } => {
            state.auth.record_success();
            info!(
                %peer,
                used_temp_code,
                "鉴权通过"
            );
            send_h2c(control, &H2C::AuthOk).await?;
            Ok(Some(client))
        }
        AuthResult::Bad => {
            let (n, lock) = state.auth.record_failure(now);
            // 绝不打印密码本身，只记次数。
            warn!(%peer, failures = n, locked_ms = lock.as_millis() as u64, "鉴权失败");
            send_h2c(
                control,
                &H2C::AuthFail {
                    reason: AuthFailReason::BadPassword,
                    retry_after_ms: if lock.is_zero() { 0 } else { lock.as_millis() as u64 },
                },
            )
            .await?;
            Ok(None)
        }
    }
}

/// 注入错误最多多久上报一次。注入不了的时候（比如被控端跑的不是 X11 桌面），
/// 每个鼠标移动都回一条 `InputError` 会把控制通道刷爆。
const INPUT_ERROR_REPORT_EVERY: Duration = Duration::from_millis(2000);

/// 剪贴板同步的计数（会话结束时打印，作为「真的同步过」的机器可查证据）。
#[derive(Debug, Default)]
struct ClipStats {
    /// 被控端 → 控制端：真的发出去的条数。
    sent: u64,
    /// 控制端 → 被控端：真的写进本地剪贴板的条数。
    recv: u64,
    /// 写本地剪贴板失败次数（X11 上没有桌面、被别的程序占着）。
    errors: u64,
}

/// 控制通道上的输入处理：状态机 + 后端 + 错误上报。
struct InputLoop {
    mapper: Mapper,
    sink: Box<dyn InputSink>,
    /// 收到的输入消息条数（排障与验收都看这个数）。
    events: u64,
    /// 注入失败次数（含被节流没上报的）。
    errors: u64,
    last_report: Option<Instant>,
}

impl InputLoop {
    fn new(mapper: Mapper, sink: Box<dyn InputSink>) -> Self {
        Self { mapper, sink, events: 0, errors: 0, last_report: None }
    }

    /// 执行一批注入指令。
    ///
    /// **注入失败不终止会话**：远控里「鼠标点不动」比「直接断线」好 ——
    /// 但必须让人看得见，所以按 [`INPUT_ERROR_REPORT_EVERY`] 节流回一条 `InputError`。
    async fn apply<W: tokio::io::AsyncWrite + Unpin>(
        &mut self,
        batch: Vec<Inject>,
        wr: &mut W,
    ) -> Result<(), String> {
        if batch.is_empty() {
            return Ok(());
        }
        self.events += 1;
        let Err(e) = self.sink.apply(&batch) else {
            return Ok(());
        };
        self.errors += 1;
        let due = self
            .last_report
            .map(|t| t.elapsed() >= INPUT_ERROR_REPORT_EVERY)
            .unwrap_or(true);
        if !due {
            debug!(error = %e, "键鼠注入失败（已节流，不重复上报）");
            return Ok(());
        }
        self.last_report = Some(Instant::now());
        warn!(sink = self.sink.name(), error = %e, total = self.errors, "键鼠注入失败，已通知控制端");
        send_h2c(wr, &H2C::InputError { message: e }).await
    }

    /// 会话收尾：松开所有还按着的键/按钮，恢复 X 的全局状态（autorepeat、键盘映射）。
    async fn finish(&mut self) {
        let batch = self.mapper.release_all();
        if !batch.is_empty() {
            if let Err(e) = self.sink.apply(&batch) {
                debug!(error = %e, "会话收尾释放按键时出错");
            }
        }
        self.sink.finish();
    }
}

/// 控制通道循环。返回对端为何结束。
///
/// 读、写两半是分开传进来的：剪贴板轮询要能在「没有输入消息」的时候也往对面发东西，
/// 而读帧是长期挂起的 —— 同一个 `&mut TcpStream` 上做不到这两件事。
#[allow(clippy::too_many_arguments)]
async fn control_loop(
    rd: &mut tokio::net::tcp::OwnedReadHalf,
    wr: &mut tokio::net::tcp::OwnedWriteHalf,
    set_tx: &watch::Sender<VideoSettings>,
    input: &mut InputLoop,
    clip: &mut Box<dyn Clipboard>,
    clip_sync: &mut ClipSync,
    clip_stats: &mut ClipStats,
) -> Result<Closed, String> {
    let mut tick = tokio::time::interval(POLL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // 基线：会话建立这一刻被控端剪贴板里已有的内容不算「新内容」，
    // 不推给控制端 —— 否则一连上就把对面正在用的剪贴板换掉。
    // 代价说明：X11 上这一读是同步的（最多 400ms 等所有者回应），但只在会话开头做一次，
    // 期间到达的输入帧留在 TCP 缓冲里，不会丢。
    let mut clip_armed = false;
    let baseline = clip.get_text();
    let _ = poll_outbound(clip_sync, baseline.as_deref(), &mut clip_armed);

    loop {
        // 控制帧和心跳都可能从这条连接来，所以直接读帧再分类。
        tokio::select! {
            frame = read_frame(rd) => {
                let frame = match frame {
                    Ok(f) => f,
                    Err(uulan_protocol::frame::FrameError::Io(e))
                        if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                    {
                        return Ok(Closed::Eof)
                    }
                    Err(e) => return Err(format!("读控制帧失败: {e}")),
                };

                match frame.kind {
                    FrameKind::Ping => {
                        write_frame(wr, FrameKind::Pong, &frame.payload)
                            .await
                            .map_err(|e| e.to_string())?;
                    }
                    FrameKind::Json => {
                        let msg: C2H = match frame.decode_json() {
                            Ok(m) => m,
                            Err(e) => {
                                warn!(error = %e, "控制消息解析失败，已忽略");
                                continue;
                            }
                        };
                        match msg {
                            C2H::Bye { reason } => return Ok(Closed::Bye(reason)),
                            C2H::VideoSet(v) => {
                                let mut cur = *set_tx.borrow();
                                let mut changed = false;
                                if v.fps > 0 && v.fps != cur.fps {
                                    cur.fps = v.fps.clamp(1, 144);
                                    changed = true;
                                }
                                if (v.scale - cur.scale).abs() > 0.001 && v.scale > 0.05 && v.scale <= 1.0 {
                                    cur.scale = v.scale;
                                    changed = true;
                                }
                                if v.quality != cur.quality {
                                    cur.quality = v.quality;
                                    changed = true;
                                }
                                if v.max_bitrate_kbps > 0 && v.max_bitrate_kbps != cur.max_bitrate_kbps {
                                    cur.max_bitrate_kbps = v.max_bitrate_kbps;
                                    changed = true;
                                }
                                if changed {
                                    info!(fps = cur.fps, scale = cur.scale, ?cur.quality, "画面参数变更，重启编码器");
                                    let _ = set_tx.send(cur);
                                } else {
                                    debug!("画面参数与当前一致，无需重启");
                                }
                            }
                            // ── 键鼠注入（M2.5/M3）──
                            C2H::MouseMoveAbs { x, y } => {
                                let batch: Vec<Inject> =
                                    input.mapper.mouse_abs(x, y).into_iter().collect();
                                input.apply(batch, wr).await?;
                            }
                            C2H::MouseMoveRel { dx, dy } => {
                                let batch: Vec<Inject> =
                                    input.mapper.mouse_rel(dx, dy).into_iter().collect();
                                input.apply(batch, wr).await?;
                            }
                            C2H::MouseButton { button, down } => {
                                let batch = input.mapper.button(button, down);
                                input.apply(batch, wr).await?;
                            }
                            C2H::MouseWheel { dx, dy } => {
                                let batch = input.mapper.wheel(dx, dy);
                                input.apply(batch, wr).await?;
                            }
                            C2H::Key { vk, down, repeat } => {
                                let batch = input.mapper.key(vk, down, repeat);
                                input.apply(batch, wr).await?;
                            }
                            C2H::KeyChar(k) => {
                                let batch = input.mapper.key_char(&k.target, k.down);
                                input.apply(batch, wr).await?;
                            }
                            // ── 剪贴板 ──
                            C2H::Clip { text } => {
                                if let Some(t) = clip_sync.inbound(&text) {
                                    match clip.set_text(&t) {
                                        Ok(()) => {
                                            clip_stats.recv += 1;
                                            debug!(bytes = t.len(), "已写入本地剪贴板");
                                        }
                                        Err(e) => {
                                            clip_stats.errors += 1;
                                            warn!(error = %e, "写本地剪贴板失败（对面内容不丢，只是这台机器粘不了）");
                                        }
                                    }
                                }
                            }
                            C2H::ClipOffer { len } => {
                                // 第一版只支持「直接带正文」的同步；offer 留给以后做按需拉取，
                                // 现在如实记一笔而不是假装处理过。
                                debug!(len, "收到剪贴板 offer（暂不支持按需拉取）");
                            }
                            // M2 只声明了桌面推送能力，控制端不该发这些。
                            // 收到了就如实忽略并记日志，绝不假装成功。
                            other => {
                                debug!(kind = ?std::mem::discriminant(&other), "收到尚未实现的控制消息，已忽略");
                            }
                        }
                    }
                    FrameKind::Blob => {
                        // bulk 通道才是数据通道，控制连接上不该出现 Blob。
                        debug!("控制连接上收到意外的 Blob 帧，已忽略");
                    }
                    _ => {}
                }
            }
            _ = tick.tick() => {
                // 轮询本机剪贴板。读不到（None）绝不能当成「剪贴板被清空」——
                // 那会在一次 X11 超时之后把对面的剪贴板擦掉。
                let cur = clip.get_text();
                if let Some(text) = poll_outbound(clip_sync, cur.as_deref(), &mut clip_armed) {
                    if send_h2c(wr, &H2C::Clip { text: text.clone() }).await.is_err() {
                        return Ok(Closed::Eof);
                    }
                    clip_stats.sent += 1;
                    debug!(bytes = text.len(), "已把本地剪贴板变化推给控制端");
                }
            }
        }
    }
}

/// 视频推送任务。参数变化时重启编码器。
async fn video_loop(
    ffmpeg: PathBuf,
    capture: CaptureInput,
    screen: Screen,
    encoder: Encoder,
    device_id: String,
    mut set_rx: watch::Receiver<VideoSettings>,
    mut video: TcpStream,
) {
    let mut cur = *set_rx.borrow();
    loop {
        let opts = make_capture_options(&capture, screen, encoder, cur);
        let input = capture.to_ffmpeg_input(&opts);

        let mut src = match VideoSource::start_with_input(&ffmpeg, opts, &input).await {
            Ok(s) => s,
            Err(e) => {
                // 采集起不来时必须**告诉控制端为什么**，而不是静默关掉视频通道。
                // 静默关闭在控制端那边只剩一个光秃秃的 "early eof" ——
                // 排查的人无从判断到底是没装 ffmpeg、DISPLAY 不对，还是参数写错。
                warn!(error = %e, "启动编码源失败");
                let why = H2C::Bye { reason: format!("被控端无法开始采集：{e}") };
                let _ = send_h2c(&mut video, &why).await;
                return;
            }
        };
        info!(?cur, screen_w = screen.w, screen_h = screen.h, "编码源已启动");

        // 实测帧率：真实产出只能自己数。每次重启编码源都重新开一个窗口。
        let mut fps = FpsMeter::new(Duration::from_secs(1));
        // 每 2 秒把实测帧率推给控制端（顺带充当视频通道的保活）。
        let mut report = tokio::time::interval(Duration::from_secs(2));
        report.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        report.tick().await; // 第一拍是立即触发的，先吃掉，免得刚开播就推一条 0 fps

        let cfg_msg = video_cfg(screen, encoder, cur, 0.0);
        if send_h2c(&mut video, &cfg_msg).await.is_err() {
            let _ = src.stop().await;
            return;
        }

        // 用 `break <值>` 表达「为什么退出内层循环」，
        // 免得留下一个永远读不到的初始布尔量。
        let restart = loop {
            tokio::select! {
                au = src.next_au() => match au {
                    Ok(Some(au)) => {
                        let packed = video::pack_au(au.ts_us, &au.data);
                        if write_frame(&mut video, FrameKind::VideoAu, &packed).await.is_err() {
                            debug!("视频通道写入失败，退出");
                            let _ = src.stop().await;
                            return;
                        }
                        if fps.tick() {
                            debug!(fps = fps.fps(), "实测帧率（1 秒窗口）");
                        }
                    }
                    Ok(None) => {
                        // 合成源放完了。抓屏源不会走到这里。
                        info!(
                            frames = src.emitted(),
                            keyframes = src.keyframes(),
                            bytes = src.bytes(),
                            device_id = %device_id,
                            "视频源结束"
                        );
                        let _ = src.stop().await;
                        return;
                    }
                    Err(e) => {
                        warn!(error = %e, "读编码输出失败");
                        let _ = src.stop().await;
                        return;
                    }
                },
                _ = report.tick() => {
                    // 分辨率/编码器没变，只是报一下实测帧率。
                    // 编码器停了的时候 FpsMeter 会自己归零 —— 一直报 30 而画面冻住是最坏的误导。
                    let msg = video_cfg(screen, encoder, cur, fps.fps());
                    if send_h2c(&mut video, &msg).await.is_err() {
                        let _ = src.stop().await;
                        return;
                    }
                }
                changed = set_rx.changed() => {
                    if changed.is_err() {
                        let _ = src.stop().await;
                        return;
                    }
                    break true;
                }
            }
        };
        let _ = src.stop().await;
        if restart {
            cur = *set_rx.borrow();
            info!(?cur, "按新参数重启编码源");
        } else {
            return;
        }
    }
}

/// 组装一条 `VideoCfg`。周期性上报实测帧率时也用它 —— 字段只写一遍，免得两处不一致。
fn video_cfg(screen: Screen, encoder: Encoder, s: VideoSettings, actual_fps: f32) -> H2C {
    H2C::VideoCfg(HostVideoCfg {
        w: screen.w,
        h: screen.h,
        codec: "h264".into(),
        fps: s.fps,
        encoder: encoder.codec_name().to_string(),
        chroma: "420".into(),
        actual_fps,
    })
}

fn make_capture_options(
    capture: &CaptureInput,
    screen: Screen,
    encoder: Encoder,
    s: VideoSettings,
) -> CaptureOptions {
    let display = capture.display().unwrap_or_else(|| ":0.0".to_string());
    let (cq, maxrate) = s.cq_and_maxrate();
    let mut o = CaptureOptions::new(display, screen.w, screen.h);
    o.fps = s.fps;
    o.cq = cq;
    o.max_bitrate_kbps = maxrate;
    o.scale = s.scale;
    o.encoder = encoder;
    o
}

async fn read_c2h(s: &mut TcpStream) -> Result<Option<C2H>, String> {
    let f = match read_frame(s).await {
        Ok(f) => f,
        Err(uulan_protocol::frame::FrameError::Io(e))
            if e.kind() == std::io::ErrorKind::UnexpectedEof =>
        {
            return Ok(None)
        }
        Err(e) => return Err(format!("读帧失败: {e}")),
    };
    if f.kind != FrameKind::Json {
        return Err(format!("期望 JSON 帧，收到 {:?}", f.kind));
    }
    f.decode_json().map(Some).map_err(|e| e.to_string())
}

async fn send_h2c<W: tokio::io::AsyncWrite + Unpin>(s: &mut W, m: &H2C) -> Result<(), String> {
    let bytes = serde_json::to_vec(m).map_err(|e| e.to_string())?;
    write_frame(s, FrameKind::Json, &bytes).await.map_err(|e| e.to_string())
}

/// 探测采集区域尺寸：配置里写死就听配置，否则探测，再否则用安全的默认值。
pub async fn resolve_screen(capture: &CaptureInput, cfg: &HostConfig) -> Screen {
    if cfg.screen_width > 0 && cfg.screen_height > 0 {
        return Screen { w: cfg.screen_width, h: cfg.screen_height };
    }
    if let Some(disp) = capture.display() {
        if let Some((w, h)) = crate::capture::detect_x11_screen(&disp).await {
            // 变量名刻意不叫 `display`：tracing 里 `%display` 会被宏解析成
            // 它自己的 `display()` 辅助函数，而不是我们的局部变量。
            info!(w, h, display = %disp, "从 xdpyinfo 探测到屏幕尺寸");
            return Screen { w, h };
        }
        warn!(display = %disp, "屏幕尺寸探测失败（xdpyinfo 不可用？），回退 1920x1080；如不对请在配置里写死 screen_width/screen_height");
        return Screen { w: 1920, h: 1080 };
    }
    // 合成源：从 spec 里抠尺寸，抠不到就用 1280x720
    match capture {
        CaptureInput::Lavfi { spec, .. } => parse_size_from_spec(spec)
            .unwrap_or(Screen { w: 1280, h: 720 }),
        _ => Screen { w: 1920, h: 1080 },
    }
}

/// 从 lavfi 的 spec 里抠 `size=WxH`。
fn parse_size_from_spec(spec: &str) -> Option<Screen> {
    let idx = spec.find("size=")? + "size=".len();
    let rest = &spec[idx..];
    let end = rest.find(|c: char| !(c.is_ascii_digit() || c == 'x')).unwrap_or(rest.len());
    let token = &rest[..end];
    let (w, h) = token.split_once('x')?;
    Some(Screen { w: w.parse().ok()?, h: h.parse().ok()? })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_matches_normal_equality() {
        assert!(ct_eq("aB3dE5fG", "aB3dE5fG"));
        assert!(!ct_eq("aB3dE5fG", "aB3dE5fH"));
        assert!(!ct_eq("aB3dE5fG", "aB3dE5f"));
        assert!(!ct_eq("aB3dE5f", "aB3dE5fG"));
        assert!(ct_eq("", ""));
    }

    #[test]
    fn auth_locks_after_threshold_with_exponential_backoff() {
        let a = AuthState::new();
        let t0 = Instant::now();

        // 前 4 次只计数，不锁
        for i in 1..AUTH_MAX_FAILURES {
            let (n, lock) = a.record_failure(t0);
            assert_eq!(n, i);
            assert_eq!(lock, Duration::ZERO, "第 {n} 次不该锁");
            assert!(a.locked_for(t0).is_none());
        }

        // 第 5 次开始锁 60s
        let (n, lock) = a.record_failure(t0);
        assert_eq!(n, AUTH_MAX_FAILURES);
        assert_eq!(lock, Duration::from_secs(60));
        assert!(a.locked_for(t0).unwrap() >= Duration::from_secs(59));

        // 锁定到期后自动解开
        assert!(a.locked_for(t0 + Duration::from_secs(61)).is_none());

        // 再错一次翻倍
        let (_, lock) = a.record_failure(t0);
        assert_eq!(lock, Duration::from_secs(120));

        // 封顶
        for _ in 0..10 {
            let (_, lock) = a.record_failure(t0);
            assert!(lock <= AUTH_LOCK_CAP, "锁定时长 {lock:?} 超过封顶");
        }
    }

    #[test]
    fn successful_auth_resets_the_counter() {
        let a = AuthState::new();
        let t0 = Instant::now();
        for _ in 0..4 {
            a.record_failure(t0);
        }
        assert_eq!(a.failures(), 4);
        a.record_success();
        assert_eq!(a.failures(), 0);
        assert!(a.locked_for(t0).is_none());
        // 重置后再错 4 次仍不该锁
        for _ in 0..4 {
            let (_, lock) = a.record_failure(t0);
            assert_eq!(lock, Duration::ZERO);
        }
    }

    #[test]
    fn auth_state_is_shared_across_clones() {
        // 关键性质：只按单次会话计数的话，攻击者每次失败后重连就能重置计数。
        let a = AuthState::new();
        let b = a.clone();
        let t0 = Instant::now();
        for _ in 0..AUTH_MAX_FAILURES {
            b.record_failure(t0);
        }
        assert!(a.locked_for(t0).is_some(), "clone 必须共享同一份计数");
    }

    fn state_with(temp: Option<&str>) -> HostState {
        let mut cfg = HostConfig::default();
        cfg.password = "aB3dE5fG".into();
        cfg.temp_code = temp.map(|s| s.to_string());
        HostState::new(cfg, None)
    }

    #[test]
    fn permanent_password_does_not_consume_the_temp_code() {
        let s = state_with(Some("tE9mP2cO"));
        match s.authenticate("aB3dE5fG") {
            AuthResult::Ok { used_temp_code } => assert!(!used_temp_code),
            AuthResult::Bad => panic!("永久密码被拒"),
        }
        assert!(s.snapshot().temp_code.is_some(), "用永久密码登录不该消耗临时码");
        // 临时码仍然可用
        match s.authenticate("tE9mP2cO") {
            AuthResult::Ok { used_temp_code } => assert!(used_temp_code),
            AuthResult::Bad => panic!("临时码被拒"),
        }
    }

    #[test]
    fn temp_code_is_single_use() {
        let s = state_with(Some("tE9mP2cO"));
        assert!(matches!(s.authenticate("tE9mP2cO"), AuthResult::Ok { used_temp_code: true }));
        assert!(s.snapshot().temp_code.is_none(), "用过的临时码必须被清掉");
        // 第二次用同一个码必须失败
        assert!(matches!(s.authenticate("tE9mP2cO"), AuthResult::Bad));
        // 永久密码不受影响
        assert!(matches!(s.authenticate("aB3dE5fG"), AuthResult::Ok { .. }));
    }

    #[test]
    fn wrong_password_is_rejected_and_nothing_is_consumed() {
        let s = state_with(Some("tE9mP2cO"));
        assert!(matches!(s.authenticate("wrongPass9"), AuthResult::Bad));
        assert!(s.snapshot().temp_code.is_some());
    }

    #[test]
    fn no_temp_code_means_only_the_permanent_password_works() {
        let s = state_with(None);
        assert!(matches!(s.authenticate("aB3dE5fG"), AuthResult::Ok { .. }));
        assert!(matches!(s.authenticate("anything1"), AuthResult::Bad));
    }

    #[test]
    fn device_id_is_persisted_only_when_it_changes() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..").join("..").join("target").join("testtmp");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("state-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&p);

        let s = HostState::new(HostConfig::default(), Some(p.clone()));
        s.set_device_id("123456789");
        assert_eq!(s.device_id().as_deref(), Some("123456789"));
        let saved = HostConfig::load(&p).unwrap();
        assert_eq!(saved.device_id.as_deref(), Some("123456789"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn fps_meter_reports_the_completed_window() {
        let t0 = Instant::now();
        let mut m = FpsMeter::starting_at(t0, Duration::from_secs(1));
        assert_eq!(m.fps_at(t0), 0.0, "还没测出来时必须报 0，不能猜一个数");

        // 30 帧/秒：第 30 帧刚好把 1 秒窗口填满
        let mut closed = false;
        for i in 1..=30 {
            closed = m.tick_at(t0 + Duration::from_millis(i * 1000 / 30));
        }
        assert!(closed, "第 30 帧应当凑满窗口");
        let f = m.fps_at(t0 + Duration::from_secs(1));
        assert!((f - 30.0).abs() < 0.6, "测得 {f}");
    }

    #[test]
    fn fps_meter_zeroes_out_when_frames_stop() {
        let t0 = Instant::now();
        let mut m = FpsMeter::starting_at(t0, Duration::from_secs(1));
        for i in 1..=10 {
            m.tick_at(t0 + Duration::from_millis(i * 100));
        }
        let t1 = t0 + Duration::from_secs(1);
        assert!((m.fps_at(t1) - 10.0).abs() < 0.6, "测得 {}", m.fps_at(t1));
        // 停了四个窗口就必须归零 —— 一直报「10 fps」而画面已经冻住，比不显示更坏。
        assert_eq!(m.fps_at(t1 + Duration::from_secs(4)), 0.0);
    }

    #[test]
    fn settings_map_from_config() {
        let mut c = HostConfig::default();
        c.fps = 60;
        c.quality = Quality::Auto;
        let s = VideoSettings::from_config(&c);
        assert_eq!(s.fps, 60);
        assert_eq!(s.quality, Quality::Auto);
        assert_eq!(s.cq_and_maxrate(), (23, 0), "自动档应是不设上限的恒定质量");

        // fps=0 不该传下去（ffmpeg 会拒绝）
        c.fps = 0;
        assert_eq!(VideoSettings::from_config(&c).fps, 30);
    }

    #[test]
    fn quality_presets_convert_to_encoder_numbers() {
        let mk = |q: Quality, kbps: u32| VideoSettings {
            fps: 30,
            quality: q,
            max_bitrate_kbps: kbps,
            scale: 1.0,
        };

        // 档位 → 码率上限，全部来自 protocol 里那张权威表
        assert_eq!(mk(Quality::Auto, 0).cq_and_maxrate(), (23, 0));
        assert_eq!(mk(Quality::Fast, 0).cq_and_maxrate(), (32, 1_000));
        assert_eq!(mk(Quality::High, 0).cq_and_maxrate(), (28, 2_000));
        assert_eq!(mk(Quality::Hd, 0).cq_and_maxrate(), (23, 8_000));
        assert_eq!(mk(Quality::Bluray, 0).cq_and_maxrate(), (18, 20_000));

        // 自定义档才用配置里的数字
        assert_eq!(mk(Quality::Custom, 4_500).cq_and_maxrate(), (23, 4_500));
        // 自定义档但没给数字 → 0，也就是退回恒定质量
        assert_eq!(mk(Quality::Custom, 0).cq_and_maxrate(), (23, 0));
    }

    #[test]
    fn size_is_parsed_from_lavfi_spec() {
        assert_eq!(
            parse_size_from_spec("testsrc2=size=1280x720:rate=30"),
            Some(Screen { w: 1280, h: 720 })
        );
        assert_eq!(parse_size_from_spec("testsrc=size=320x240"), Some(Screen { w: 320, h: 240 }));
        assert_eq!(parse_size_from_spec("testsrc=rate=30"), None);
        assert_eq!(parse_size_from_spec(""), None);
    }

    #[test]
    fn capture_options_pick_up_settings() {
        let cap = CaptureInput::parse("lavfi:testsrc=size=640x480:rate=25").unwrap();
        let s = VideoSettings {
            fps: 25,
            quality: Quality::Bluray,
            max_bitrate_kbps: 0,
            scale: 0.5,
        };
        let o = make_capture_options(&cap, Screen { w: 640, h: 480 }, Encoder::X264, s);
        assert_eq!(o.fps, 25);
        assert_eq!(o.scale, 0.5);
        assert_eq!(o.width, 640);
        assert_eq!(o.encoder, Encoder::X264);
        // 原画档：CQ 18 + 20 Mbps 上限
        assert_eq!(o.cq, 18);
        assert_eq!(o.max_bitrate_kbps, 20_000);
    }

    #[tokio::test]
    async fn resolve_screen_prefers_explicit_config() {
        let cap = CaptureInput::parse("lavfi:testsrc=size=800x600").unwrap();
        let mut cfg = HostConfig::default();
        cfg.screen_width = 1600;
        cfg.screen_height = 900;
        let s = resolve_screen(&cap, &cfg).await;
        assert_eq!((s.w, s.h), (1600, 900));
    }

    #[tokio::test]
    async fn resolve_screen_reads_synthetic_spec() {
        let cap = CaptureInput::parse("lavfi:testsrc2=size=1024x768:rate=30").unwrap();
        let cfg = HostConfig::default();
        let s = resolve_screen(&cap, &cfg).await;
        assert_eq!((s.w, s.h), (1024, 768));
    }
}
