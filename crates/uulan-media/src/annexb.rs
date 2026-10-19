//! H.264 Annex-B 码流解析：NAL 扫描 + 访问单元（Access Unit）切分。
//!
//! # 为什么要自己切 AU
//!
//! 被控端把屏幕交给 FFmpeg 编码后，从它的 stdout 拿到的是一串**裸 Annex-B 字节**，
//! 没有任何「帧」的边界信息。而我们要把「一帧」作为一条 [`crate::annexb`] 消息
//! 带上时间戳发出去，所以必须自己找回 AU 边界。
//!
//! # 用什么判据
//!
//! Annex-B 里 NAL 靠起始码 `00 00 01`（或 4 字节 `00 00 00 01`）分隔。但
//! 「一条 NAL」不等于「一帧」：一帧可能由 SEI + IDR 多条 NAL 组成，也可能被切成多个 slice。
//!
//! 切分策略，从可靠到兜底：
//!
//! 1. **AUD（NAL 类型 9）**。我们在编码器上显式打开 `-aud 1`，于是每个 AU 都以一个
//!    AUD 开头，边界是**精确**的，不需要任何猜测。libx264 / h264_nvenc / h264_vaapi
//!    都有这个开关（注意 ffmpeg 9 下 `-flags +aud` 会失败，只有 `-aud 1` 和
//!    `h264_metadata=aud=insert` 有效）。
//! 2. **兜底启发式**（流里没有 AUD 时）：当前 AU 已经出现过 VCL NAL 之后，
//!    - 遇到 SPS / PPS / SEI → 开启新 AU；
//!    - 遇到 VCL NAL 且其 slice header 的 `first_mb_in_slice == 0` → 开启新 AU。
//!    `first_mb_in_slice != 0` 说明这是同一帧的后续 slice，必须归到同一个 AU 里，
//!    否则解码器会拿到半张图。
//!
//! 参考项目正是在 AUD 这个点上栽过跟头（HEVC + `hevc_vaapi` 只在 IRAP 处写 AUD，
//! 读取端按 AUD 切分就把整个 GOP 当成一帧，表现为「连接停在正在传输画面」）。
//! 我们显式要求编码器每个 AU 都写 AUD，把这个坑绕开。

/// H.264 NAL 单元类型（ITU-T H.264 表 7-1）。
pub mod nal {
    pub const NON_IDR_SLICE: u8 = 1;
    pub const PARTITION_A: u8 = 2;
    pub const PARTITION_B: u8 = 3;
    pub const PARTITION_C: u8 = 4;
    pub const IDR_SLICE: u8 = 5;
    pub const SEI: u8 = 6;
    pub const SPS: u8 = 7;
    pub const PPS: u8 = 8;
    pub const AUD: u8 = 9;
    pub const END_SEQUENCE: u8 = 10;
    pub const END_STREAM: u8 = 11;
    pub const FILLER: u8 = 12;

    /// 是否是 VCL（视频编码层）NAL —— 也就是真正携带图像数据的那些。
    pub fn is_vcl(t: u8) -> bool {
        (1..=5).contains(&t)
    }

    /// AU 开头常见的「非 VCL 前缀」：参数集与补充增强信息。
    pub fn is_prefix_non_vcl(t: u8) -> bool {
        matches!(t, SPS | PPS | SEI)
    }

    /// 是否关键帧（IDR）。控制端可用它决定能否立刻出图。
    pub fn is_keyframe(t: u8) -> bool {
        t == IDR_SLICE
    }

    pub fn name(t: u8) -> &'static str {
        match t {
            NON_IDR_SLICE => "non-IDR slice",
            PARTITION_A => "slice partition A",
            PARTITION_B => "slice partition B",
            PARTITION_C => "slice partition C",
            IDR_SLICE => "IDR slice",
            SEI => "SEI",
            SPS => "SPS",
            PPS => "PPS",
            AUD => "AUD",
            END_SEQUENCE => "end of sequence",
            END_STREAM => "end of stream",
            FILLER => "filler",
            _ => "unknown",
        }
    }
}

/// 码流里已定位的一条 NAL。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nal {
    /// 起始码在缓冲区里的偏移（含 4 字节起始码的额外前导 0）。
    pub sc_offset: usize,
    /// NAL 头字节的偏移。payload 从 `hdr_offset + 1` 开始。
    pub hdr_offset: usize,
    /// NAL 类型（头字节低 5 位）。
    pub typ: u8,
}

/// 在 `buf` 中从 `from` 起找下一个起始码。
///
/// 返回 `(起始码偏移, 起始码长度, NAL 头偏移)`。起始码偏移**含** 4 字节形式的
/// 额外前导零 —— 这样相邻 NAL 的字节区间首尾相接、不重不漏。
fn find_start_code(buf: &[u8], from: usize) -> Option<(usize, usize, usize)> {
    let mut i = from;
    while i + 2 < buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            let sc_len = if i > 0 && buf[i - 1] == 0 { 4 } else { 3 };
            let sc = i + 3 - sc_len;
            return Some((sc, sc_len, i + 3));
        }
        i += 1;
    }
    None
}

/// 去掉防竞争字节（`00 00 03` → `00 00`）后取第一个 bit。
///
/// slice header 的第一个语法元素是 `first_mb_in_slice`（ue(v)）。ue(v) 的编码规则是
/// 「N 个 0 + 一个 1 + N 位信息」，所以**首 bit 为 1 等价于该值为 0**。
/// 我们只需要这一个 bit，不需要完整解析 EXP-Golomb。
fn first_bit_after_unescape(body: &[u8]) -> Option<bool> {
    let mut out = [0u8; 8];
    let mut n = 0usize;
    let mut zeros = 0usize;
    let mut i = 0usize;
    while i < body.len() && n < out.len() {
        let b = body[i];
        if zeros >= 2 && b == 3 {
            // 防竞争字节，丢弃
            zeros = 0;
            i += 1;
            continue;
        }
        out[n] = b;
        n += 1;
        zeros = if b == 0 { zeros + 1 } else { 0 };
        i += 1;
    }
    if n == 0 {
        return None;
    }
    Some(out[0] & 0x80 != 0)
}

/// 流式 AU 切分器。
///
/// 用法：反复 `push(chunk)`，把返回的每个 `Vec<u8>` 当作一个完整的 AU 发出去；
/// 流结束时调 [`AuSplitter::flush`] 取出尾部残留。
#[derive(Debug, Default)]
pub struct AuSplitter {
    /// 尚未交付的字节。已交付的 AU 会从头部裁掉，避免无限增长。
    buf: Vec<u8>,
    /// `buf` 内已定位的 NAL，偏移严格递增。
    nals: Vec<Nal>,
    /// 下次扫描的起点。
    scan_from: usize,
    /// 当前 AU 的首个 NAL 在 `nals` 中的下标。
    au_start_idx: usize,
    /// 已交付的 AU 数（诊断用）。
    emitted: u64,
}

impl AuSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 已交付的 AU 数量。
    pub fn emitted(&self) -> u64 {
        self.emitted
    }

    /// 喂入一段字节，返回其中已经确定完整的 AU。
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        if !chunk.is_empty() {
            self.buf.extend_from_slice(chunk);
        }
        self.scan();
        self.drain_ready()
    }

    /// 码流结束：把尾部残留当成最后一个 AU 交出去。
    ///
    /// 只在 `push` 过字节之后调用。返回 `None` 表示没有残留。
    pub fn flush(&mut self) -> Option<Vec<u8>> {
        if self.au_start_idx < self.nals.len() {
            let start = self.nals[self.au_start_idx].sc_offset;
            if start < self.buf.len() {
                let au = self.buf[start..].to_vec();
                self.emitted += 1;
                self.nals.clear();
                self.au_start_idx = 0;
                self.buf.clear();
                self.scan_from = 0;
                return Some(au);
            }
        }
        None
    }

    fn scan(&mut self) {
        let mut pos = self.scan_from;
        loop {
            match find_start_code(&self.buf, pos) {
                None => {
                    // 缓冲区里再没有完整起始码。留 3 字节退路：
                    // 一条 4 字节起始码可能横跨两次 push，尾部那几个 0 要留着重扫。
                    self.scan_from = self.buf.len().saturating_sub(3);
                    return;
                }
                Some((sc, _sc_len, hdr)) => {
                    if hdr >= self.buf.len() {
                        // 起始码齐了，但 NAL 头字节还没到 —— 下次从这条起始码重扫。
                        self.scan_from = sc;
                        return;
                    }
                    // 重扫时要防止把同一条 NAL 记两遍（偏移严格递增即可判定）。
                    if self.nals.last().map_or(true, |n| n.sc_offset < sc) {
                        let typ = self.buf[hdr] & 0x1f;
                        self.nals.push(Nal { sc_offset: sc, hdr_offset: hdr, typ });
                    }
                    pos = hdr + 1;
                }
            }
        }
    }

    /// 把已经能确定边界的 AU 交出去。
    ///
    /// 一个 AU 只有在**看到下一个 AU 的开头**时才算完整 —— 最后那个 AU 要等
    /// [`AuSplitter::flush`]。这是流式解析的固有代价，换来的是零延迟（不必等整帧收完）。
    fn drain_ready(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            let mut next = None;
            for k in (self.au_start_idx + 1)..self.nals.len() {
                let has_vcl = self.nals[self.au_start_idx..k].iter().any(|n| nal::is_vcl(n.typ));
                if self.starts_new_au(k, has_vcl) {
                    next = Some(k);
                    break;
                }
            }
            let Some(k) = next else { break };

            let start = self.nals[self.au_start_idx].sc_offset;
            let end = self.nals[k].sc_offset;
            if end > start {
                out.push(self.buf[start..end].to_vec());
                self.emitted += 1;
            }
            self.au_start_idx = k;
        }
        self.compact();
        out
    }

    /// `nals[k]` 是不是一个新 AU 的开头。`has_vcl` 表示当前 AU 里已经有图像数据了。
    fn starts_new_au(&self, k: usize, has_vcl: bool) -> bool {
        let t = self.nals[k].typ;

        // AUD 永远是 AU 的开头（我们让编码器每帧都写一个）。注意要放在
        // `has_vcl` 判断之前：AUD 出现就意味着上一帧到此为止。
        if t == nal::AUD {
            return true;
        }
        // 当前 AU 还没有任何图像数据 —— 说明这一帧还没真正开始，不能切。
        if !has_vcl {
            return false;
        }
        // 当前 AU 已经有图像数据了，再看到参数集/SEI 就是下一帧的开头。
        if nal::is_prefix_non_vcl(t) {
            return true;
        }
        // 再看到 VCL：只有 slice header 的 first_mb_in_slice == 0 才是新的一帧；
        // 否则是本帧的后续 slice，必须留在同一个 AU 里。
        if nal::is_vcl(t) {
            return self.first_mb_is_zero(k);
        }
        false
    }

    fn first_mb_is_zero(&self, k: usize) -> bool {
        let n = self.nals[k];
        let body_start = n.hdr_offset + 1;
        if body_start >= self.buf.len() {
            // 拿不到 slice header：保守地认为不是新 AU（宁可多并一帧，
            // 也不要把一帧切成两半 —— 后者会让解码器画出半张图）。
            return false;
        }
        first_bit_after_unescape(&self.buf[body_start..]).unwrap_or(false)
    }

    /// 把已交付的字节从头部裁掉，并同步调整所有偏移。
    fn compact(&mut self) {
        if self.au_start_idx == 0 {
            return;
        }
        let cut = self.nals[self.au_start_idx].sc_offset;
        if cut == 0 {
            return;
        }
        self.buf.drain(..cut);
        self.nals.drain(..self.au_start_idx);
        for n in &mut self.nals {
            n.sc_offset -= cut;
            n.hdr_offset -= cut;
        }
        self.au_start_idx = 0;
        self.scan_from = self.scan_from.saturating_sub(cut);
    }
}

/// 一次性切分整段码流（测试与离线工具用）。
pub fn split_all(buf: &[u8]) -> Vec<Vec<u8>> {
    let mut s = AuSplitter::new();
    let mut out = s.push(buf);
    if let Some(last) = s.flush() {
        out.push(last);
    }
    out
}

/// 列出码流里所有 NAL 的类型（诊断用）。
pub fn nal_types(buf: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    let mut pos = 0usize;
    while let Some((sc, sc_len, hdr)) = find_start_code(buf, pos) {
        if hdr >= buf.len() {
            break;
        }
        v.push(buf[hdr] & 0x1f);
        pos = hdr + 1;
        let _ = (sc, sc_len);
    }
    v
}

/// 这个 AU 是不是关键帧。
///
/// 判据是「第一条 VCL NAL 是 IDR」。用第一条而不是「含 IDR」：
/// 正常码流里 IDR 只出现在帧首，但万一遇到非规范编码，按首条判定更接近解码器行为。
pub fn is_keyframe_au(au: &[u8]) -> bool {
    let mut pos = 0usize;
    while let Some((_sc, _sc_len, hdr)) = find_start_code(au, pos) {
        if hdr >= au.len() {
            break;
        }
        let t = au[hdr] & 0x1f;
        if nal::is_vcl(t) {
            return t == nal::IDR_SLICE;
        }
        pos = hdr + 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 拼一条 NAL：起始码 + 头字节 + 载荷。
    fn nal_bytes(typ: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, typ & 0x1f];
        v.extend_from_slice(payload);
        v
    }

    /// slice NAL：第一个 bit 为 1 表示 first_mb_in_slice == 0（本帧首 slice）。
    fn slice_bytes(typ: u8, first_mb_zero: bool, payload: &[u8]) -> Vec<u8> {
        let first = if first_mb_zero { 0b1000_0000 } else { 0b0100_0000 };
        let mut v = vec![0, 0, 0, 1, typ & 0x1f, first];
        v.extend_from_slice(payload);
        v
    }

    fn aud() -> Vec<u8> {
        nal_bytes(nal::AUD, &[0x10])
    }

    #[test]
    fn find_start_code_handles_3_and_4_byte_forms() {
        let b = [0xAA, 0, 0, 1, 0x67, 0xBB, 0, 0, 0, 1, 0x68];
        let (sc, len, hdr) = find_start_code(&b, 0).unwrap();
        assert_eq!((sc, len, hdr), (1, 3, 4));
        let (sc, len, hdr) = find_start_code(&b, 5).unwrap();
        assert_eq!((sc, len, hdr), (6, 4, 10));
    }

    #[test]
    fn split_with_aud_is_exact() {
        // 三帧，每帧 AUD + SPS/PPS/SEI + slice；参数集只出现在第一帧
        let mut stream = Vec::new();
        stream.extend(aud());
        stream.extend(nal_bytes(nal::SPS, b"sps"));
        stream.extend(nal_bytes(nal::PPS, b"pps"));
        stream.extend(nal_bytes(nal::SEI, b"sei"));
        stream.extend(slice_bytes(nal::IDR_SLICE, true, b"idr"));
        stream.extend(aud());
        stream.extend(slice_bytes(nal::NON_IDR_SLICE, true, b"p1"));
        stream.extend(aud());
        stream.extend(slice_bytes(nal::NON_IDR_SLICE, true, b"p2"));

        let aus = split_all(&stream);
        assert_eq!(aus.len(), 3, "应该切成 3 个 AU");
        assert_eq!(nal_types(&aus[0]), vec![9, 7, 8, 6, 5]);
        assert_eq!(nal_types(&aus[1]), vec![9, 1]);
        assert_eq!(nal_types(&aus[2]), vec![9, 1]);
        // 逐字节拼回去必须等于原流：不丢字节、不重复
        let joined: Vec<u8> = aus.concat();
        assert_eq!(joined, stream);
    }

    #[test]
    fn split_without_aud_falls_back_to_slice_heuristic() {
        let mut stream = Vec::new();
        stream.extend(nal_bytes(nal::SPS, b"sps"));
        stream.extend(nal_bytes(nal::PPS, b"pps"));
        stream.extend(slice_bytes(nal::IDR_SLICE, true, b"idr"));
        stream.extend(slice_bytes(nal::NON_IDR_SLICE, true, b"p1"));
        stream.extend(slice_bytes(nal::NON_IDR_SLICE, true, b"p2"));

        let aus = split_all(&stream);
        assert_eq!(aus.len(), 3);
        assert_eq!(nal_types(&aus[0]), vec![7, 8, 5]);
        assert_eq!(nal_types(&aus[1]), vec![1]);
        assert_eq!(nal_types(&aus[2]), vec![1]);
        assert_eq!(aus.concat(), stream);
    }

    #[test]
    fn multi_slice_frame_stays_in_one_au() {
        // 同一帧被切成 3 个 slice：只有第一个 first_mb_in_slice==0
        let mut stream = Vec::new();
        stream.extend(nal_bytes(nal::SPS, b"sps"));
        stream.extend(nal_bytes(nal::PPS, b"pps"));
        stream.extend(slice_bytes(nal::IDR_SLICE, true, b"slice0"));
        stream.extend(slice_bytes(nal::IDR_SLICE, false, b"slice1"));
        stream.extend(slice_bytes(nal::IDR_SLICE, false, b"slice2"));
        // 第二帧
        stream.extend(slice_bytes(nal::NON_IDR_SLICE, true, b"next"));
        stream.extend(slice_bytes(nal::NON_IDR_SLICE, false, b"next2"));

        let aus = split_all(&stream);
        assert_eq!(aus.len(), 2, "多 slice 的一帧不能被切开");
        assert_eq!(nal_types(&aus[0]), vec![7, 8, 5, 5, 5]);
        assert_eq!(nal_types(&aus[1]), vec![1, 1]);
    }

    #[test]
    fn parameter_sets_after_vcl_start_a_new_au() {
        let mut stream = Vec::new();
        stream.extend(nal_bytes(nal::SPS, b"sps"));
        stream.extend(slice_bytes(nal::IDR_SLICE, true, b"idr"));
        stream.extend(nal_bytes(nal::SEI, b"sei"));
        stream.extend(slice_bytes(nal::NON_IDR_SLICE, true, b"p"));

        let aus = split_all(&stream);
        assert_eq!(aus.len(), 2);
        assert_eq!(nal_types(&aus[0]), vec![7, 5]);
        assert_eq!(nal_types(&aus[1]), vec![6, 1]);
    }

    #[test]
    fn streaming_one_byte_at_a_time_gives_same_result() {
        let mut stream = Vec::new();
        for i in 0..5u8 {
            stream.extend(aud());
            stream.extend(slice_bytes(if i == 0 { nal::IDR_SLICE } else { nal::NON_IDR_SLICE }, true, &[i]));
        }

        let mut s = AuSplitter::new();
        let mut got = Vec::new();
        for b in &stream {
            got.extend(s.push(std::slice::from_ref(b)));
        }
        if let Some(last) = s.flush() {
            got.push(last);
        }
        assert_eq!(got.len(), 5, "逐字节喂入也必须得到 5 个 AU");
        assert_eq!(got.concat(), stream, "起始码跨 chunk 不能丢字节");
    }

    #[test]
    fn streaming_odd_chunk_sizes_gives_same_result() {
        let mut stream = Vec::new();
        for i in 0..20u8 {
            stream.extend(aud());
            stream.extend(slice_bytes(nal::NON_IDR_SLICE, true, &vec![i; 97]));
        }

        // 故意用与起始码边界错开的块长（3、5、7、11 循环）
        for chunk in [3usize, 5, 7, 11, 13] {
            let mut s = AuSplitter::new();
            let mut got = Vec::new();
            for c in stream.chunks(chunk) {
                got.extend(s.push(c));
            }
            if let Some(last) = s.flush() {
                got.push(last);
            }
            assert_eq!(got.len(), 20, "块长 {chunk} 时 AU 数不对");
            assert_eq!(got.concat(), stream, "块长 {chunk} 时字节流被破坏");
        }
    }

    #[test]
    fn last_au_needs_flush() {
        let mut s = AuSplitter::new();
        let mut stream = aud();
        stream.extend(slice_bytes(nal::IDR_SLICE, true, b"x"));
        // 只有一个 AU：push 阶段看不到「下一个 AU 的开头」，交不出来
        assert!(s.push(&stream).is_empty());
        let last = s.flush().expect("flush 应交出尾部 AU");
        assert_eq!(nal_types(&last), vec![9, 5]);
        assert_eq!(last, stream);
    }

    #[test]
    fn buffer_does_not_grow_without_bound() {
        // 连续喂 200 帧，内部缓冲区不应累积 —— 已交付的字节要被裁掉。
        let mut s = AuSplitter::new();
        let mut frame = aud();
        frame.extend(slice_bytes(nal::NON_IDR_SLICE, true, &vec![0xAB; 4096]));
        for _ in 0..200 {
            let _ = s.push(&frame);
        }
        assert_eq!(s.emitted, 199, "200 帧里有 1 帧还在缓冲区里等 flush");
        assert!(
            s.buf.len() <= frame.len() * 2,
            "缓冲区膨胀到 {} 字节（单帧 {} 字节），compact 没生效",
            s.buf.len(),
            frame.len()
        );
    }

    #[test]
    fn empty_input_is_harmless() {
        let mut s = AuSplitter::new();
        assert!(s.push(&[]).is_empty());
        assert!(s.flush().is_none());
    }

    #[test]
    fn nal_names_are_not_empty() {
        for t in 0..=12u8 {
            assert!(!nal::name(t).is_empty());
        }
    }
}