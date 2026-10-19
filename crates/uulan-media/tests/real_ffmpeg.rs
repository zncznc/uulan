//! 用**真的 FFmpeg**验证 AU 切分器。
//!
//! 单元测试用的是手工拼的字节流，证明不了「真实编码器产出的码流」也能切对。
//! 这里真的起一个 ffmpeg，让它编码确定的帧数，再断言切出来的 AU 数与之相等。
//! 找不到 ffmpeg 时整体跳过（打印一行提示），不伪装成通过。

use std::path::PathBuf;
use std::process::{Command, Stdio};

use uulan_media::annexb::{nal, split_all, AuSplitter};
use uulan_media::ffmpeg::{self, CaptureOptions, Encoder};

/// 找 ffmpeg；找不到就返回 None，调用方跳过。
fn ffmpeg() -> Option<PathBuf> {
    ffmpeg::find_ffmpeg(None)
}

/// 用 lavfi 生成 `frames` 帧、指定尺寸的 H.264 裸流。
/// `aud` 决定是否打开访问单元分隔符。
fn encode_test_stream(frames: u32, fps: u32, w: u32, h: u32, aud: bool) -> Vec<u8> {
    let ff = ffmpeg().expect("调用方已确认 ffmpeg 存在");
    let mut cmd = Command::new(ff);
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-f",
        "lavfi",
        "-i",
        &format!("testsrc=size={w}x{h}:rate={fps}"),
        "-frames:v",
        &frames.to_string(),
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-tune",
        "zerolatency",
        "-g",
        &uulan_media::ffmpeg::gop_for(fps).to_string(),
        "-pix_fmt",
        "yuv420p",
    ]);
    if aud {
        cmd.args(["-aud", "1"]);
    }
    cmd.args(["-f", "h264", "pipe:1"]);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = cmd.output().expect("起 ffmpeg 失败");
    assert!(out.status.success(), "ffmpeg 编码失败: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

/// 生成 H.264 到文件（给需要文件输入的测试用）。
fn encode_to_file(args_extra: &[&str], path: &std::path::Path) {
    let ff = ffmpeg().expect("调用方已确认 ffmpeg 存在");
    let mut cmd = Command::new(ff);
    cmd.args(["-y", "-hide_banner", "-loglevel", "error", "-nostdin"]);
    cmd.args(args_extra);
    cmd.arg(path);
    cmd.stdout(Stdio::null()).stderr(Stdio::piped());
    let out = cmd.output().expect("起 ffmpeg 失败");
    assert!(out.status.success(), "ffmpeg 失败: {}", String::from_utf8_lossy(&out.stderr));
}

fn scratch(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("testtmp");
    std::fs::create_dir_all(&p).expect("创建测试临时目录失败");
    p.push(name);
    p
}

#[test]
fn real_stream_with_aud_splits_into_exactly_one_au_per_frame() {
    let Some(_) = ffmpeg() else {
        eprintln!("跳过：本机没有 ffmpeg");
        return;
    };

    const FRAMES: u32 = 40;
    const FPS: u32 = 20;
    let stream = encode_test_stream(FRAMES, FPS, 320, 240, true);

    let aus = split_all(&stream);
    assert_eq!(
        aus.len() as u32,
        FRAMES,
        "开了 -aud 1 后 AU 数必须等于帧数（实际 {} 个 AU / {} 帧）",
        aus.len(),
        FRAMES
    );

    // 每个 AU 必须以 AUD 开头，且至少含一条 VCL NAL。
    //
    // 注意「至少一条」而不是「恰好一条」：`-tune zerolatency` 会打开 x264 的
    // slice-based threading，于是**每一帧被切成多条 slice**（线程数决定条数）。
    // 这正是 `first_mb_in_slice` 那条兜底判据必须存在的原因 —— 没有 AUD 时，
    // 只有靠 slice header 才能把同一帧的多条 slice 正确归到一个 AU 里。
    let mut total_vcl = 0usize;
    let mut max_slices = 0usize;
    for (i, au) in aus.iter().enumerate() {
        let types = uulan_media::annexb::nal_types(au);
        assert_eq!(types.first(), Some(&nal::AUD), "第 {i} 个 AU 不是以 AUD 开头: {types:?}");
        let vcl = types.iter().filter(|t| nal::is_vcl(**t)).count();
        assert!(vcl >= 1, "第 {i} 个 AU 里没有 VCL: {types:?}");
        total_vcl += vcl;
        max_slices = max_slices.max(vcl);
    }

    // 全流里的 slice NAL 总数必须等于各 AU 内的 slice 数之和（不多切也不少切）
    let stream_vcl = uulan_media::annexb::nal_types(&stream)
        .iter()
        .filter(|t| nal::is_vcl(**t))
        .count();
    assert_eq!(total_vcl, stream_vcl, "AU 切分时弄丢了 slice");
    eprintln!("提示：单帧最多 {max_slices} 条 slice（sliced-threads 的副作用）");

    // 第一帧必须是 IDR
    let first = uulan_media::annexb::nal_types(&aus[0]);
    assert!(first.contains(&nal::IDR_SLICE), "第一帧不是 IDR: {first:?}");
    assert!(first.contains(&nal::SPS) && first.contains(&nal::PPS), "第一帧缺参数集: {first:?}");

    // 逐字节拼回去必须等于原流
    assert_eq!(aus.concat(), stream, "切分后拼回原流不一致（丢字节或重复）");
}

#[test]
fn real_stream_without_aud_still_splits_per_frame() {
    let Some(_) = ffmpeg() else {
        eprintln!("跳过：本机没有 ffmpeg");
        return;
    };

    const FRAMES: u32 = 40;
    let stream = encode_test_stream(FRAMES, 20, 320, 240, false);
    assert_eq!(
        uulan_media::annexb::nal_types(&stream).iter().filter(|t| **t == nal::AUD).count(),
        0,
        "这个流本来就不该有 AUD"
    );

    // 兜底启发式：靠 first_mb_in_slice == 0
    let aus = split_all(&stream);
    assert_eq!(
        aus.len() as u32,
        FRAMES,
        "没有 AUD 时也要靠 slice header 切对（实际 {} 个 AU / {} 帧）",
        aus.len(),
        FRAMES
    );
    assert_eq!(aus.concat(), stream);
}

#[test]
fn real_stream_survives_awkward_chunking() {
    let Some(_) = ffmpeg() else {
        eprintln!("跳过：本机没有 ffmpeg");
        return;
    };

    const FRAMES: u32 = 30;
    let stream = encode_test_stream(FRAMES, 15, 320, 240, true);

    // 真实场景：从 ffmpeg stdout 读到的块长毫无规律。用质数块长模拟。
    for chunk in [1usize, 7, 61, 1024, 8191] {
        let mut s = AuSplitter::new();
        let mut got: Vec<Vec<u8>> = Vec::new();
        for c in stream.chunks(chunk) {
            got.extend(s.push(c));
        }
        if let Some(last) = s.flush() {
            got.push(last);
        }
        assert_eq!(got.len() as u32, FRAMES, "块长 {chunk} 时 AU 数不对");
        assert_eq!(got.concat(), stream, "块长 {chunk} 时字节流被破坏");
    }
}

/// 被控端真正要跑的那条命令行，用真 ffmpeg 验证「参数合法且能产出可切分的流」。
///
/// 输入源用 `lavfi` 替换掉 `x11grab`（测试环境未必有 X11），
/// 其余参数完全取自 `encode_args`，这样参数写错（比如 `-aud` 拼错）会被这个测试拦住。
#[test]
fn encode_args_produce_a_split_table_stream() {
    let Some(ff) = ffmpeg() else {
        eprintln!("跳过：本机没有 ffmpeg");
        return;
    };

    let o = CaptureOptions {
        display: ":0.0".into(),
        width: 320,
        height: 240,
        fps: 20,
        cq: ffmpeg::DEFAULT_CQ,
        max_bitrate_kbps: 0,
        scale: 1.0,
        encoder: Encoder::X264,
        draw_mouse: false,
        vaapi_device: "/dev/dri/renderD128".into(),
    };

    let args = ffmpeg::encode_args(&o);
    // 把 x11grab 输入段换成 lavfi 合成源，其余原样保留
    let mut cmd = Command::new(&ff);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
    cmd.args(["-f", "lavfi", "-i", "testsrc=size=320x240:rate=20"]);
    cmd.args(["-frames:v", "25"]);

    // 跳过原 args 里从 "-f x11grab" 到输入地址 "-i :0.0+0,0" 这一段。
    // 注意要定位到**输入地址**而不是第一个 "-i"：encode_args 里的 "-i" 就是它，
    // 取错位置会把输入地址当成输出文件名丢给 ffmpeg。
    let in_pos = args
        .iter()
        .position(|x| x.ends_with("+0,0"))
        .expect("encode_args 里必须有 x11grab 输入地址");
    cmd.args(&args[in_pos + 1..]);

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = cmd.output().expect("起 ffmpeg 失败");
    assert!(
        out.status.success(),
        "encode_args 产出的参数被 ffmpeg 拒绝: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let aus = split_all(&out.stdout);
    assert_eq!(aus.len(), 25, "encode_args 产出的流应当能切出 25 帧，实际 {}", aus.len());
}

/// 解码方向：用 `decode_args` 真的解一段流，验证能拿到正确大小的 BGRA 帧。
#[test]
fn decode_args_roundtrip_produces_bgra_frames() {
    let Some(ff) = ffmpeg() else {
        eprintln!("跳过：本机没有 ffmpeg");
        return;
    };

    const W: u32 = 320;
    const H: u32 = 240;
    const FRAMES: u32 = 10;
    let stream = encode_test_stream(FRAMES, 10, W, H, true);

    let mut cmd = Command::new(&ff);
    cmd.args(ffmpeg::decode_args(W, H));
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("起 ffmpeg 解码失败");
    {
        use std::io::Write;
        let mut si = child.stdin.take().unwrap();
        si.write_all(&stream).expect("写码流失败");
        // 关掉 stdin，让 ffmpeg 知道流结束
    }
    let out = child.wait_with_output().expect("等 ffmpeg 结束");
    assert!(out.status.success(), "解码失败: {}", String::from_utf8_lossy(&out.stderr));

    let fsz = ffmpeg::frame_bytes(W, H);
    assert_eq!(
        out.stdout.len() % fsz,
        0,
        "输出字节数 {} 不是整帧（每帧 {}）",
        out.stdout.len(),
        fsz
    );
    assert_eq!(out.stdout.len() / fsz, FRAMES as usize, "解出的帧数不对");
}

/// 真实文件路径（不走管道）也应当能切分 —— 覆盖「读文件回放」的场景。
#[test]
fn split_stream_read_from_file() {
    let Some(_) = ffmpeg() else {
        eprintln!("跳过：本机没有 ffmpeg");
        return;
    };

    let p = scratch("file_input.h264");
    encode_to_file(
        &[
            "-f", "lavfi", "-i", "testsrc=size=320x240:rate=25", "-frames:v", "20",
            "-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency", "-aud", "1",
            "-pix_fmt", "yuv420p", "-f", "h264",
        ],
        &p,
    );
    let bytes = std::fs::read(&p).expect("读回码流失败");
    let aus = split_all(&bytes);
    assert_eq!(aus.len(), 20);
    assert_eq!(aus.concat(), bytes);
    let _ = std::fs::remove_file(&p);
}
