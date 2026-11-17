//! 麦克风转接的**接收侧**（M7 的最后一格）：audio 通道上行 Ogg → ffmpeg 解码 → 本机声卡。
//!
//! 控制端操作员对着他的麦克风说话，声音从**这台机器**的音箱放出来。
//! 与下行采集（`audio.rs`）合起来才是完整的「UU 远程」式音频：那条是「听见对面」，
//! 这条是「隔空说话」。解码 + 播放这一半与**控制端共用** `uulan_media::playback`
//! （2026-11-17 在真麒麟上逐条实测出来的纪律只留一份副本）。
//!
//! ## 「停」= 读半端的 EOF
//!
//! 控制端结束采集时放下上行写半端，TCP 写半边关闭，这里 `read_frame` 拿到错误/EOF
//! 就收尾 —— 所以协议里**没有** `MicStop`（见 `micin.rs` 模块头同一句话的另一半）。
//!
//! ## 回话纪律（与 `AudioState` 同款）
//!
//! 每一句 `MicStart` 必回一条 `MicState`；`running:true` 的口径是
//! 「本机解码器 + 播放后端**都**起来了」，不是「收到消息了」。起不来要带原因，
//! 否则控制端只能猜「没成功」还是「机器走了」。

use std::path::PathBuf;
use std::sync::Arc;

use tracing::{debug, info, warn};

use uulan_media::playback::{self, Counters, NullSink, PcmSink};
use uulan_protocol::crypto::SecReadHalf;
use uulan_protocol::frame::{read_frame, FrameKind};
use uulan_protocol::msg::H2C;

/// 本机把收到的声音送到哪儿。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicSink {
    /// 实时播放到系统默认输出设备（生产默认；麒麟上 = `aplay` → pulse 信宿）。
    Speaker,
    /// 解码后丢弃。**验收台专用**（没有声卡的机器 / Windows 本机全链路测试），
    /// 生产配置不要写 `null`。
    Null,
}

/// 解析配置里的写法。**拼错词必须让服务起不来**，不许静默退回默认 ——
/// 与 `power_allow` / `portmap_allow` 同一条纪律（「配了 null 却被当成 speaker」
/// 或反过来，现场都查不出来）。
pub fn parse_sink(s: &str) -> Result<MicSink, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "speaker" => Ok(MicSink::Speaker),
        "null" => Ok(MicSink::Null),
        other => Err(format!(
            "mic_sink 只能是 \"speaker\" 或 \"null\"（收到 {other:?}）"
        )),
    }
}

/// 一次上行音频的收尾统计（进 journal 的一行，验收脚本 grep 它）。
#[derive(Debug, Default, Clone)]
pub struct MicReport {
    pub bytes: u64,
    pub frames: u64,
    pub seconds: u64,
    /// 解出的 PCM 字节数。0 而 bytes>0 = 解码器一个字节都没吐（编码格式不对口）。
    pub pcm: u64,
    /// 播出去的字节数（`Null` 后端时也有：进了「播放」这一步）。
    pub played: u64,
    /// 峰值电平。bytes>0 而 peak=0 = 「有流全是静音」——与「没声音」分开记。
    pub peak: u64,
}

/// 跑一路麦克风转接，直到对端停（写半端 EOF）/ 本机起不来 / 任务被 abort。
///
/// 返回收尾报告；`notify` 负责每一阶段的 `MicState`（含失败原因）。
pub async fn run(
    ffmpeg: PathBuf,
    sink: MicSink,
    rd: SecReadHalf,
    notify: Box<dyn Fn(H2C) + Send + Sync>,
) -> MicReport {
    // 播放后端先挑 —— 它是这台机器上最可能没有的东西，先失败先说，别拉了解码器才塌。
    let player: Box<dyn PcmSink> = match sink {
        MicSink::Null => Box::new(NullSink),
        MicSink::Speaker => match playback::pipe_speaker() {
            Ok(p) => p,
            Err(e) => {
                // 控制端必须看见这句：否则它的统计是「被控端正在推流」而屋里一声不响。
                warn!(error = %e, "麦克风转接起不来：本机没有可用的播放后端");
                notify(H2C::MicState { running: false, codec: String::new(), detail: e });
                return MicReport::default();
            }
        },
    };

    // 有界通道：播放跟不上就反压到 socket（发送端是实时的麦克风，积压没有意义，
    // 但比丢字节好 —— Opus 断流的表现比「延迟几百毫秒」难查）。
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(16);
    let stats: Arc<Counters> = Arc::new(Counters::default());

    let st_in = stats.clone();
    let mut rd = rd;
    let reader = tokio::spawn(async move {
        loop {
            let f = match read_frame(&mut rd).await {
                Ok(f) => f,
                Err(e) => {
                    debug!(error = %e, "麦克风上行通道结束");
                    break;
                }
            };
            match f.kind {
                FrameKind::Blob => {
                    let n = f.payload.len();
                    if n == 0 {
                        continue;
                    }
                    st_in.add_frame(n);
                    if tx.send(f.payload.to_vec()).is_err() {
                        break;
                    }
                }
                FrameKind::Bye => break,
                other => debug!(?other, "麦克风上行收到意外帧，已忽略"),
            }
        }
    });

    let st_pump = stats.clone();
    // running:true 的时机由 pump 的 on_ready 回调经 channel 转达：pump 在阻塞线程里，
    // notify 是异步侧的闭包，跨界只能走 channel —— 这正是
    // 「口径 = 解码器 + 播放后端都起来了」的实现（不是「收到 MicStart 了」）。
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
    // 收尾的**回话与账目都放进 on_end**（在阻塞线程里执行到最后）：
    // 控制端整个进程先退出是常态 —— 那时会话收尾会 abort 本任务，留在异步侧的
    // 「麦克风转接收尾」一行永远落不了盘（2026-11-18 真机测试台暴露：播放器释放了、
    // 账没落）。spawn_blocking 杀不到，这段就 guaranteed 说得出口。
    let notify = std::sync::Arc::new(notify);
    let notify_end = notify.clone();
    let on_end: Box<dyn FnOnce(&playback::PumpEnd, &Arc<Counters>) + Send> = Box::new(move |end, st| {
        use std::sync::atomic::Ordering::Relaxed;
        let bytes = st.ogg_bytes.load(Relaxed);
        let detail = match end {
            playback::PumpEnd::Ended => format!("麦克风流结束（本机收到 {bytes} 字节）"),
            playback::PumpEnd::DecoderGone(w) => format!("本机解码器起不来：{w}"),
            playback::PumpEnd::SinkGone(w) => format!("本机播放中断：{w}"),
        };
        if !matches!(end, playback::PumpEnd::Ended) {
            warn!(detail = %detail, "麦克风转接非正常结束");
        }
        notify_end(H2C::MicState { running: false, codec: String::new(), detail });
        info!(
            bytes = bytes,
            frames = st.frames.load(Relaxed),
            pcm = st.pcm_bytes.load(Relaxed),
            played = st.played_bytes.load(Relaxed),
            peak = st.peak.load(Relaxed),
            "麦克风转接收尾"
        );
    });
    let pump = tokio::task::spawn_blocking(move || {
        playback::pump(
            &ffmpeg,
            rx,
            player,
            &st_pump,
            Box::new(move || {
                let _ = ready_tx.send(());
            }),
            on_end,
        )
    });

    // 解码器真的起来了才算 running:true（pump 在 spawn 成功后发这一格）。
    // 三种结局：收到信号 = 起来了；发送端断了 = pump 以 DecoderGone 提前收场（原因在
    // pump 的返回值里，那里再回话）；超时 = 异常慢，先不算起来，等 pump 的最终结局。
    // `recv_timeout` 是 std 的阻塞调用 —— 放 spawn_blocking，别占 tokio worker 5 秒。
    let decoder_up = match tokio::task::spawn_blocking(move || {
        ready_rx.recv_timeout(std::time::Duration::from_secs(5))
    })
    .await
    {
        Ok(Ok(())) => true,
        _ => false,
    };
    if decoder_up {
        info!("麦克风转接已接上（解码器 + 本机播放后端就绪）");
        notify(H2C::MicState {
            running: true,
            codec: format!("opus/ogg ← 控制端麦克风（sink={:?}）", sink),
            detail: String::new(),
        });
    }

    // 正常路径下这里能 await 到；被 abort 也无妨 —— 回话与账目已由 on_end
    // 在阻塞线程里落完（那才是这套收尾的真正责任人）。
    let t0 = std::time::Instant::now();
    let end = match pump.await {
        Ok(e) => e,
        Err(e) => {
            warn!(error = %e, "麦克风转接的播放任务崩了");
            reader.abort();
            return MicReport::default();
        }
    };
    let _ = reader.await;
    use std::sync::atomic::Ordering::Relaxed;
    let report = MicReport {
        bytes: stats.ogg_bytes.load(Relaxed),
        frames: stats.frames.load(Relaxed),
        seconds: t0.elapsed().as_secs_f64().round() as u64,
        pcm: stats.pcm_bytes.load(Relaxed),
        played: stats.played_bytes.load(Relaxed),
        peak: stats.peak.load(Relaxed),
    };
    debug!(?end, "麦克风转接 run 收尾");
    report
}

/// 自检用：这台机器能不能**播**（麦克风转接落地的前提）。不拉解码器、不碰声卡。
pub fn probe(sink: MicSink) -> Result<String, String> {
    if sink == MicSink::Null {
        return Ok("null（验收台：解码后丢弃）".into());
    }
    // pipe_speaker 会真的起一个 aplay —— 自检不能碰声卡（它可能正被占用，
    // 而且「自检顺手播了一下」很难看）。改为问「候选在不在 PATH」。
    let dirs: Vec<std::path::PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let cand = playback::player_candidates()
        .into_iter()
        .find(|c| playback::find_in_path(c.program, &dirs).is_some())
        .ok_or_else(|| {
            "这台机器上没有 PATH 里可找的播放命令（候选只有实测过的 aplay）".to_string()
        })?;
    Ok(format!("{}（在 PATH）", cand.program))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sink_is_strict_like_power_allow() {
        assert_eq!(parse_sink("speaker").unwrap(), MicSink::Speaker);
        assert_eq!(parse_sink(" Null ").unwrap(), MicSink::Null);
        // 拼错词必须报错且把两个合法值写进消息（现场照着改，不用翻文档）。
        let e = parse_sink("loud").unwrap_err();
        assert!(e.contains("speaker") && e.contains("null"), "{e}");
        assert!(parse_sink("").is_err(), "空串不是 speaker 的同义词");
    }

    #[test]
    fn probe_null_is_the_test_rig_answer() {
        assert!(probe(MicSink::Null).unwrap().contains("null"));
    }

    /// 反例钉：`mic_sink` 拼错词绝不能被当成「没配」。
    #[test]
    fn typo_never_falls_back_to_default() {
        assert!(parse_sink("sepeaker").is_err());
        assert!(parse_sink(" speakers").is_err(), "带错拼的名词 trim 之后也得拒绝");
    }
}
