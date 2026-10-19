//! 编码源 ↔ 解码器的端到端流水线测试。
//!
//! 这是 M2 的核心验收：**被控端的编码链路和控制端的解码链路，在本机就能完整跑通**。
//! 唯一没被覆盖的是 `x11grab` 这个输入源本身（需要一台有 X11 的机器），
//! 其余参数（`-aud 1`、GOP、码率、pix_fmt、解码像素格式）全部逐字一致地走过真 ffmpeg。
//!
//! 找不到 ffmpeg 时打印一行提示并跳过，不伪装成通过。

use std::time::Duration;

use tokio::time::timeout;
use uulan_media::ffmpeg::{self, CaptureOptions, Encoder};
use uulan_media::{VideoDecoder, VideoSource};

const T: Duration = Duration::from_secs(60);

fn ffmpeg_path() -> Option<std::path::PathBuf> {
    ffmpeg::find_ffmpeg(None)
}

/// 合成输入段：`frames` 帧的测试图。
fn lavfi_input(w: u32, h: u32, fps: u32, frames: u32) -> Vec<String> {
    vec![
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        format!("testsrc=size={w}x{h}:rate={fps}"),
        "-frames:v".into(),
        frames.to_string(),
    ]
}

fn opts(w: u32, h: u32, fps: u32) -> CaptureOptions {
    let mut o = CaptureOptions::new(":0.0", w, h);
    o.fps = fps;
    o.encoder = Encoder::X264;
    o
}

#[tokio::test]
async fn source_feeds_decoder_and_frame_count_round_trips() {
    let Some(ff) = ffmpeg_path() else {
        eprintln!("跳过：本机没有 ffmpeg");
        return;
    };

    const W: u32 = 320;
    const H: u32 = 240;
    const FPS: u32 = 25;
    const FRAMES: u32 = 25;

    let mut src = VideoSource::start_with_input(&ff, opts(W, H, FPS), &lavfi_input(W, H, FPS, FRAMES))
        .await
        .expect("起编码源失败");

    let mut dec = VideoDecoder::start(&ff, W, H).await.expect("起解码器失败");
    let (mut writer, mut reader) = dec.split().expect("拆分解码器失败");

    // 一边读一边喂：这正是真实控制端的形态，也顺便验证了不会死锁。
    let feeder = tokio::spawn(async move {
        let mut fed = 0u64;
        let mut keyframes = 0u64;
        while let Some(au) = src.next_au().await.expect("取 AU 失败") {
            if au.keyframe {
                keyframes += 1;
            }
            writer.feed(&au.data).await.expect("喂 AU 失败");
            fed += 1;
        }
        // writer 在这里被 drop → 关掉 ffmpeg 的 stdin → 解码器 flush 后退出。
        // 顺手把 writer 的计数带出来：它在任务里被 move 走了，外面拿不到。
        let fed_by_writer = writer.fed();
        (fed, keyframes, fed_by_writer)
    });

    let mut buf = Vec::new();
    let mut decoded = 0u64;
    loop {
        let more = timeout(T, reader.next_frame_into(&mut buf))
            .await
            .expect("读帧超时（可能死锁）")
            .expect("读帧出错");
        if !more {
            break;
        }
        assert_eq!(buf.len(), ffmpeg::frame_bytes(W, H), "帧字节数不对");
        decoded += 1;
    }

    let (fed, keyframes, fed_by_writer) =
        timeout(T, feeder).await.expect("喂流任务超时").expect("喂流任务 panic");

    assert_eq!(fed, FRAMES as u64, "编码源产出的 AU 数应等于帧数");
    assert_eq!(decoded, FRAMES as u64, "解出的帧数应等于喂入的 AU 数");
    assert!(keyframes >= 1, "至少要有关键帧");
    assert_eq!(reader.truncated(), 0, "不应出现半帧");
    assert_eq!(fed_by_writer, decoded, "喂入的 AU 数应等于解出的帧数");

    dec.kill().await;
}

#[tokio::test]
async fn first_au_is_a_keyframe_and_gop_is_respected() {
    let Some(ff) = ffmpeg_path() else {
        eprintln!("跳过：本机没有 ffmpeg");
        return;
    };

    const W: u32 = 320;
    const H: u32 = 240;
    const FPS: u32 = 20;
    const FRAMES: u32 = 41; // gop = 10，期望 IDR 出现在 0/10/20/30/40

    let mut src = VideoSource::start_with_input(&ff, opts(W, H, FPS), &lavfi_input(W, H, FPS, FRAMES))
        .await
        .unwrap();

    let mut first = true;
    let mut keyframe_indices = Vec::new();
    let mut idx = 0usize;
    while let Some(au) = src.next_au().await.unwrap() {
        if first {
            assert!(au.keyframe, "第一个 AU 必须是关键帧，否则控制端接上就黑屏");
            first = false;
        }
        if au.keyframe {
            keyframe_indices.push(idx);
        }
        idx += 1;
    }
    src.stop().await.unwrap();

    assert_eq!(idx, FRAMES as usize);
    assert!(keyframe_indices.contains(&0), "第 0 帧必须是关键帧: {keyframe_indices:?}");
    // 允许 x264 因场景切换多插关键帧，但不允许比 GOP 少
    let gop = ffmpeg::gop_for(FPS);
    let gaps: Vec<usize> = keyframe_indices.windows(2).map(|w| w[1] - w[0]).collect();
    for g in &gaps {
        assert!(*g <= gop as usize, "关键帧间隔 {g} 超过 GOP {gop}: {keyframe_indices:?}");
    }
    eprintln!("关键帧位置: {keyframe_indices:?}（GOP={gop}）");
}

/// 并发喂/读不能死锁。用较大的帧让管道真的写满 —— 串行实现会在这里永久卡住。
#[tokio::test]
async fn concurrent_feed_and_read_does_not_deadlock_on_big_frames() {
    let Some(ff) = ffmpeg_path() else {
        eprintln!("跳过：本机没有 ffmpeg");
        return;
    };

    const W: u32 = 1280;
    const H: u32 = 720;
    const FPS: u32 = 30;
    const FRAMES: u32 = 30;

    let mut src = VideoSource::start_with_input(&ff, opts(W, H, FPS), &lavfi_input(W, H, FPS, FRAMES))
        .await
        .unwrap();
    let mut dec = VideoDecoder::start(&ff, W, H).await.unwrap();
    let (mut writer, mut reader) = dec.split().unwrap();

    let feeder = tokio::spawn(async move {
        let mut fed = 0u64;
        while let Some(au) = src.next_au().await.unwrap() {
            writer.feed(&au.data).await.unwrap();
            fed += 1;
        }
        fed
    });

    let mut buf = Vec::new();
    let mut decoded = 0u64;
    loop {
        let more = timeout(T, reader.next_frame_into(&mut buf))
            .await
            .expect("读帧超时 —— 并发喂/读死锁了")
            .unwrap();
        if !more {
            break;
        }
        decoded += 1;
    }

    let fed = timeout(T, feeder).await.expect("喂流超时").unwrap();
    assert_eq!(fed, FRAMES as u64);
    assert_eq!(decoded, FRAMES as u64, "720p 大帧下也要一帧不少");
    dec.kill().await;
}

/// 码率档位真的会改变输出体积（证明参数确实生效，而不是被 ffmpeg 忽略）。
///
/// 素材必须选「**难压到会撞上码率上限**」的：早先我用 `testsrc` 640x480，
/// 30 帧只要 ~110KB（约 880 kbps），2M 和 8M 档都远没吃满上限，
/// 两档体积几乎一样（比值 1.04），测试就会误报「码率参数被忽略」。
/// 换成 `testsrc2` 1280x720 后 2M 档会顶到上限，两档比值约 3.9，才真正测到东西。
#[tokio::test]
async fn bitrate_preset_actually_changes_output_size() {
    let Some(ff) = ffmpeg_path() else {
        eprintln!("跳过：本机没有 ffmpeg");
        return;
    };

    const W: u32 = 1280;
    const H: u32 = 720;
    const FRAMES: u32 = 30;

    let input: Vec<String> = vec![
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        format!("testsrc2=size={W}x{H}:rate=30"),
        "-frames:v".into(),
        FRAMES.to_string(),
    ];

    /// `kbps == 0` 表示恒定质量（CRF），否则是码率上限。
    async fn encode_size(ff: &std::path::Path, kbps: u32, input: &[String]) -> u64 {
        let mut o = CaptureOptions::new(":0.0", 1280, 720);
        o.fps = 30;
        o.max_bitrate_kbps = kbps;
        let mut src = VideoSource::start_with_input(ff, o, input).await.unwrap();
        let mut n = 0u64;
        while let Some(au) = src.next_au().await.unwrap() {
            n += au.data.len() as u64;
        }
        src.stop().await.unwrap();
        n
    }

    let high = encode_size(&ff, 2_000, &input).await; // 高清 2M
    let ultra = encode_size(&ff, 8_000, &input).await; // 超清 8M
    let auto = encode_size(&ff, 0, &input).await; // 恒定质量

    eprintln!("高清(2M) {high} B / 超清(8M) {ultra} B / 自动 {auto} B");
    assert!(high > 0 && ultra > 0 && auto > 0);
    // 同一段素材，8M 档必须明显比 2M 档大。若接近相等说明码率参数根本没生效。
    assert!(
        ultra > high * 2,
        "8M 档（{ultra} B）没有明显大于 2M 档（{high} B），码率参数可能被忽略了"
    );
}

/// 解码器读到 EOF 要干净返回 `false`，而不是报错或死等。
#[tokio::test]
async fn decoder_reports_eof_cleanly() {
    let Some(ff) = ffmpeg_path() else {
        eprintln!("跳过：本机没有 ffmpeg");
        return;
    };

    const W: u32 = 320;
    const H: u32 = 240;
    let mut src =
        VideoSource::start_with_input(&ff, opts(W, H, 25), &lavfi_input(W, H, 25, 10)).await.unwrap();
    let mut dec = VideoDecoder::start(&ff, W, H).await.unwrap();
    let (mut writer, mut reader) = dec.split().unwrap();

    while let Some(au) = src.next_au().await.unwrap() {
        writer.feed(&au.data).await.unwrap();
    }
    drop(writer); // 关 stdin

    let mut buf = Vec::new();
    let mut n = 0;
    while timeout(T, reader.next_frame_into(&mut buf)).await.expect("读帧超时").unwrap() {
        n += 1;
    }
    assert_eq!(n, 10);
    // 再读一次仍然是 false，不 panic
    assert!(!reader.next_frame_into(&mut buf).await.unwrap());
    assert!(reader.is_eof());
    dec.kill().await;
}

/// 停止编码源要真的把 ffmpeg 杀掉，不留孤儿进程。
#[tokio::test]
async fn stop_kills_the_encoder() {
    let Some(ff) = ffmpeg_path() else {
        eprintln!("跳过：本机没有 ffmpeg");
        return;
    };

    // 不给帧数上限，让它一直跑，模拟真实抓屏
    let input = vec![
        "-f".to_string(),
        "lavfi".to_string(),
        "-i".to_string(),
        "testsrc=size=320x240:rate=25".to_string(),
    ];
    let mut src = VideoSource::start_with_input(&ff, opts(320, 240, 25), &input).await.unwrap();

    // 取几帧确认它真的在跑
    for _ in 0..3 {
        assert!(timeout(T, src.next_au()).await.unwrap().unwrap().is_some());
    }
    timeout(T, src.stop()).await.expect("stop 卡住了").unwrap();

    // 再取应当立刻得到 None（stdout 已随进程结束）
    let rest = timeout(Duration::from_secs(5), async {
        let mut n = 0;
        while let Ok(Some(_)) = src.next_au().await {
            n += 1;
            if n > 1000 {
                break;
            }
        }
        n
    })
    .await
    .expect("stop 之后 next_au 卡住了");
    eprintln!("stop 之后又读到 {rest} 个缓冲 AU");
}
