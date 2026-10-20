//! M2.5 验收：**被控端换分辨率之后，控制端真的重建了解码器**。
//!
//! 链路是真的：真 TCP 连接、真协议帧、真 ffmpeg 解码进程。
//! 唯一替身是「被控端」—— 这里用一个假发流器直接按协议往 socket 里写
//! `VideoCfg` + `VideoAu`，因为要验的是控制端对分辨率变化的反应，
//! 而不是被控端怎么发现分辨率变了（那是 `resize_host` 那个测试的事）。
//!
//! 两段码流用 ffmpeg 现场编码（`testsrc2` 两个尺寸），所以不需要往仓库里塞二进制。

use std::process::Command;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

use uulan_client::watch::VideoWatcher;
use uulan_media::ffmpeg;
use uulan_protocol::frame::{write_frame, FrameKind};
use uulan_protocol::msg::{H2C, HostVideoCfg};

const FRAMES: u32 = 6;

fn ffmpeg_or_skip() -> Option<std::path::PathBuf> {
    let p = ffmpeg::find_ffmpeg(None);
    if p.is_none() {
        eprintln!("跳过：本机没有 ffmpeg");
    }
    p
}

/// 用 ffmpeg 现场编一段 H.264（Annex-B）码流。
fn encode(w: u32, h: u32, ff: &std::path::Path) -> Vec<u8> {
    let out = Command::new(ff)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc2=size={w}x{h}:rate=15"),
            "-frames:v",
            &FRAMES.to_string(),
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-x264-params",
            "aud=1",
            "-f",
            "h264",
            "-",
        ])
        .output()
        .expect("跑 ffmpeg 编码失败");
    assert!(
        out.status.success(),
        "ffmpeg 编码 {w}x{h} 失败：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!out.stdout.is_empty(), "ffmpeg 没吐出码流");
    out.stdout
}

fn cfg(w: u32, h: u32, fps: f32) -> HostVideoCfg {
    HostVideoCfg {
        w,
        h,
        codec: "h264".into(),
        fps: 15,
        encoder: "libx264".into(),
        chroma: "420".into(),
        actual_fps: fps,
    }
}

async fn send_cfg(s: &mut TcpStream, c: HostVideoCfg) {
    let bytes = serde_json::to_vec(&H2C::VideoCfg(c)).unwrap();
    write_frame(s, FrameKind::Json, &bytes).await.unwrap();
}

async fn send_au(s: &mut TcpStream, ts: u64, data: &[u8]) {
    let packed = uulan_protocol::frame::video::pack_au(ts, data);
    write_frame(s, FrameKind::VideoAu, &packed).await.unwrap();
}

#[tokio::test]
async fn decoder_is_rebuilt_when_host_resolution_changes() {
    let Some(ff) = ffmpeg_or_skip() else { return };
    let small = encode(320, 240, &ff);
    let big = encode(480, 360, &ff);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // 假被控端：320x240 → 只报帧率的 VideoCfg（不该触发重建）→ 480x360
    let server = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        send_cfg(&mut s, cfg(320, 240, 0.0)).await;
        send_au(&mut s, 1_000, &small).await;
        // 给控制端时间把这一代解出来，否则重建会把还没解完的帧一起丢掉
        tokio::time::sleep(Duration::from_millis(1200)).await;
        // 只报帧率的常规上报：分辨率没变，绝不能重建解码器
        send_cfg(&mut s, cfg(320, 240, 14.7)).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        send_cfg(&mut s, cfg(480, 360, 0.0)).await;
        send_au(&mut s, 2_000, &big).await;
        tokio::time::sleep(Duration::from_millis(2500)).await;
        let _ = s.shutdown().await;
    });

    let stream = TcpStream::connect(addr).await.unwrap();
    let mut watcher = VideoWatcher::start(stream, Some(ff.clone()))
        .await
        .expect("启动解码失败");

    assert_eq!((watcher.info().w, watcher.info().h), (320, 240));

    let mut small_frames = 0u32;
    let mut big_frames = 0u32;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(f)) = timeout(Duration::from_secs(5), watcher.next_frame()).await else {
            break;
        };
        match (f.width, f.height) {
            (320, 240) => {
                assert_eq!(
                    f.pixels.len(),
                    ffmpeg::frame_bytes(320, 240),
                    "320x240 这一代的帧字节数不对"
                );
                small_frames += 1;
            }
            (480, 360) => {
                assert_eq!(
                    f.pixels.len(),
                    ffmpeg::frame_bytes(480, 360),
                    "480x360 这一代的帧字节数不对"
                );
                big_frames += 1;
            }
            (w, h) => panic!("出现了第三种画幅 {w}x{h}"),
        }
        if big_frames >= 1 {
            // 有了一帧新尺寸的就够证明重建成功，不必等它把 6 帧全解完
            break;
        }
    }

    let s = watcher.snapshot();
    eprintln!(
        "帧：320x240 {small_frames} 帧 / 480x360 {big_frames} 帧；重建 {} 次；解出 {} 帧 / 喂入 {} AU / 丢帧 {}",
        s.resizes, s.decoded, s.fed, s.dropped
    );

    assert!(small_frames >= 1, "换分辨率之前的帧一帧都没解出来");
    assert!(big_frames >= 1, "换分辨率之后没有解出新尺寸的帧（解码器没重建？）");
    // 三条 VideoCfg 里只有一条真带新分辨率 —— 帧率上报绝不能触发重建
    assert_eq!(s.resizes, 1, "重建次数不对：只该在真换分辨率时重建一次");
    assert_eq!((s.cfg_w, s.cfg_h), (480, 360), "统计里的当前画幅没跟着更新");
    assert_eq!(watcher.stats().size(), (480, 360));

    watcher.shutdown();
    let _ = timeout(Duration::from_secs(5), server).await;
}