//! 端到端加密（M6）：X25519 交换 + HKDF 派生 + ChaCha20-Poly1305 每记录认证加密。
//!
//! ## 为什么在这一层做
//!
//! 中继服务器是**看不懂内容的盲转发**，但「看不懂」在过去只是**约定**：它是明文转发，
//! 谁在 server 上抓一次包，就拿到了屏幕、键盘、剪贴板、文件。M6 要把它变成**事实**。
//!
//! 加密层刻意贴着 [`crate::frame`] 做，而不是改业务消息：
//! 业务代码照旧 `write_frame(kind, payload)` / `read_frame()`，只是对着一个
//! [`SecWriter`] / [`SecReader`]（实现了 `AsyncRead`/`AsyncWrite`）读写。
//! 于是 `uulan-bulk`、`uulan-fs`、终端、剪贴板一行都不用改。
//!
//! ## 握手放在哪
//!
//! 只在 **control 通道**上做一次，在 `Hello` 之前：
//!
//! ```text
//! 控制端                                  被控端
//!   |-- CryptoHello{eph_c, nonce_c} ----->|
//!   |<-- CryptoHello{eph_h, nonce_h} -----|
//!   |   K = HKDF(X25519(eph_c, eph_h))    |   （双方各自算，同一个值）
//!   |== 之后这条连接上每个帧都是密文 ==>
//! ```
//!
//! **video / bulk 不重复握手**：它们用 `HKDF(K, "uulan-ch-v1"||通道||方向)` 派生子密钥。
//! 理由有两条：视频要尽快出首帧，多一次往返就是多一帧的启动延迟；
//! 而三条连接本来就都经过同一个中继，各自握手并不会让它更安全。
//! 方向分开派生（c→h 与 h→c 用不同密钥）是为了让两个方向各自从 0 开始的计数器
//! **不可能**撞到同一个 (key, nonce) —— ChaCha20-Poly1305 下 nonce 重用等于把明文交出去。
//!
//! ## 口令证明（`Auth` 既不发明文口令，也不再是「可离线校验」的值）
//!
//! 握手（X25519）之后还有一轮 **PAKE**（[`crate::pake`]）：口令只用来生成 Ristretto255 的
//! 生成元，双方各自的点交换一次，得到共享点 `Kp`。会话密钥是 `HKDF(K ‖ Kp)` ——
//! 中继知道 `K`，但算不出 `Kp`，所以它连解密都做不到（旧版能解、只能靠证明发现自己被中间人）。
//!
//! 认证阶段发的是 `HMAC-SHA256(会话密钥, "uulan-auth-v2"||标签||转录哈希)`：
//! 控制端发 `uulan-auth-c2`，被控端发 `uulan-auth-h2`，**双向**都要验。
//!
//! - 被控端验：对面用的是某个候选凭据（否则拒绝，跟明文时代一样按次数锁定）；
//! - 控制端验：对面确实知道口令 —— 否则「中继冒充被控端」就能把用户的键盘、文件收走。
//!
//! 标签不同，所以两份证明不能互相冒用（不是反射攻击）。
//!
//! 一条连接上会同时出现 `n` 份控制端证明（`n` = 被控端的候选凭据数，通常 1 或 2）：
//! 控制端不知道对面有几个、是哪个，只能每个候选次序各算一把密钥、各给一份证明；
//! 被控端拿自己那一个共享点去核对，命中的序号就是它 `AuthOk` 里回的 `idx`。
//!
//! ## 这一层**没有**解决的问题（写在明处，别当成已经解决）
//!
//! 1. ~~主动 MITM 能拿口令证明离线撞字典~~ —— **已解决（v2 加密层）**：PAKE 之后
//!    证明里没有口令、密钥里有控制端算不出来的共享点，中继要么猜（必须在线连一次）、
//!    要么什么都读不到。剩下的只是「PAKE 的实现是本仓库自己写的、没做第三方审计」
//!    这一条，写进了 [`crate::pake`] 的模块注释。
//! 2. 记录长度仍以明文前缀发送。帧长在旧协议里本来就是明文的
//!    （`read_frame` 的长度前缀），TCP 层也能从分段看出来，所以不额外泄露新信息。
//!    代价是「屏幕上有没有大动作」这类流量特征仍可见。
//! 3. 没有前向保密之外的密钥轮换：一条会话一把密钥，重连即换（临时密钥对每次新生成）。

use std::pin::Pin;
use std::task::{ready, Context, Poll};

use bytes::Bytes;
use chacha20poly1305::aead::AeadInPlace;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce, Tag};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::frame::{read_frame, write_frame, FrameError, FrameKind, MAX_FRAME};
use crate::pake::{PakeInit, PakeResp};
use crate::rv::{Channel, Role};

/// 加密握手版本。与 [`crate::PROTO_VERSION`] 分开：加密层将来可能单独演进
/// （比如换 PAKE 时要递增它，而不必动整体协议）。
///
/// v2（M6 加固）：X25519 之后多一轮 PAKE（[`crate::pake`]），`Auth` 的证明不再含口令、
/// 改为「PAKE 密钥上的 HMAC」，会话密钥也由 PAKE 密钥混合而成。
pub const CRYPTO_VERSION: u32 = 2;

/// X25519 公钥字节数。
pub const EPH_LEN: usize = 32;
/// 握手里双方各带一个随机数，拼成 HKDF 的 salt。
pub const NONCE_LEN: usize = 16;
/// Poly1305 认证标签长度。
pub const TAG_LEN: usize = 16;

/// 记录（= 一个帧的密文）长度上限：明文上限 + 标签。
pub const MAX_RECORD: usize = MAX_FRAME + TAG_LEN + 16;

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("加密握手版本不支持：对端 v{0}，本端 v{CRYPTO_VERSION}")]
    BadVersion(u32),
    #[error("握手消息格式不对")]
    BadHello,
    #[error("X25519 交换结果为空（对端公钥是低阶点）")]
    WeakPeerKey,
    #[error("解密失败：密钥不对、内容被改过，或记录被重放")]
    Decrypt,
    #[error("记录长度非法：{0}")]
    BadRecordLen(usize),
    #[error("记录计数器已用尽")]
    CounterExhausted,
    #[error("明文缓冲区超过单帧上限（写方忘了 flush？）")]
    BufferOverflow,
    #[error("协议：{0}")]
    Protocol(String),
    /// 下层 IO 出错（含对端在记录中间断开）。
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("帧: {0}")]
    Frame(#[from] FrameError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<CryptoError> for std::io::Error {
    fn from(e: CryptoError) -> Self {
        match e {
            CryptoError::Io(io) => io,
            other => std::io::Error::new(std::io::ErrorKind::InvalidData, other.to_string()),
        }
    }
}

// ───────────────────────────── 握手 ─────────────────────────────

/// 握手消息（明文，走 control 通道的最前面两个帧）。
///
/// 临时公钥本身是公开信息，所以这一段不加密 —— 加密它没有意义，
/// 握手能防的是「事后解密」，不是「事后知道有过这次握手」。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CryptoHello {
    pub v: u32,
    /// X25519 临时公钥（hex，32 字节 = 64 个 hex 字符）。
    pub eph: String,
    /// 本端随机数（hex，16 字节）。
    pub nonce: String,
}

/// 一次会话的密钥材料。克隆是廉价的（64 字节），但**不要**把它写进日志。
#[derive(Clone)]
pub struct SessionKeys {
    master: [u8; 32],
    transcript: [u8; 32],
}

impl std::fmt::Debug for SessionKeys {
    /// 只报「有没有」，绝不打印密钥本身。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionKeys(<已派生>)")
    }
}

impl SessionKeys {
    /// 派生某个通道的收发密钥。
    pub fn channel(&self, ch: Channel) -> ChannelKeys {
        ChannelKeys {
            c2h: self.derive_channel(ch, 0x01),
            h2c: self.derive_channel(ch, 0x02),
        }
    }

    fn derive_channel(&self, ch: Channel, dir: u8) -> [u8; 32] {
        let hk = Hkdf::<Sha256>::from_prk(&self.master).expect("master 已是 32 字节，长度合法");
        let mut info = Vec::with_capacity(16);
        info.extend_from_slice(b"uulan-ch-v1");
        info.push(channel_code(ch));
        info.push(dir);
        let mut okm = [0u8; 32];
        hk.expand(&info, &mut okm).expect("32 字节在 HKDF-SHA256 上限内");
        okm
    }

    /// 握手转录哈希（同时也是 PAKE 的 `sid`）。
    pub fn transcript(&self) -> &[u8; 32] {
        &self.transcript
    }

    /// 把 PAKE 的口令密钥混进来，得到**真正的会话密钥**。
    ///
    /// 为什么必须混：不混的话主动 MITM 的中继自己也知道两端的主密钥，
    /// 加密只是「防被动抓包」。混进来以后它要解密就得先猜对出口令 —— 而 PAKE 保证
    /// 每猜一次都必须在线交互一轮。
    ///
    /// 对同一份握手密钥调用多次是**正常路径**：控制端并不知道被控端手上有几个候选凭据，
    /// 于是每个候选各算一把，最后按 `AuthOk.idx` 挑出真的那一把。
    pub fn with_pake(&self, pake_k: &[u8; 32]) -> SessionKeys {
        let mut ikm = Vec::with_capacity(64);
        ikm.extend_from_slice(&self.master);
        ikm.extend_from_slice(pake_k);
        let hk = Hkdf::<Sha256>::new(Some(&self.transcript), &ikm);
        let mut master = [0u8; 32];
        hk.expand(b"uulan-session-v2", &mut master)
            .expect("32 字节在 HKDF-SHA256 上限内");
        SessionKeys { master, transcript: self.transcript }
    }

    /// 控制端出示的证明。
    ///
    /// **不含口令**：口令已经在 PAKE 里进了密钥。正因为证明里没有口令、
    /// 又没法从抓到的证明反推 PAKE 密钥，中继拿它撞字典这条路就断了。
    pub fn client_proof(&self) -> String {
        self.proof(b"uulan-auth-c2")
    }

    /// 被控端出示的证明（标签不同，不能拿控制端的证明来冒用）。
    pub fn host_proof(&self) -> String {
        self.proof(b"uulan-auth-h2")
    }

    /// 校验「对方知道那份 PAKE 密钥」——即「对方用的是这个候选凭据」。
    pub fn matches_client_proof(&self, got: &str) -> bool {
        ct_eq_hex(&self.client_proof(), got)
    }

    pub fn matches_host_proof(&self, got: &str) -> bool {
        ct_eq_hex(&self.host_proof(), got)
    }

    fn proof(&self, label: &[u8]) -> String {
        let mut m = <Hmac<Sha256> as Mac>::new_from_slice(&self.master).expect("HMAC 接受任意长度密钥");
        m.update(b"uulan-auth-v2");
        m.update(label);
        m.update(&self.transcript);
        hex::encode(m.finalize().into_bytes())
    }

    /// 握手转录哈希（排障/日志用，可以公开）。
    pub fn transcript_hex(&self) -> String {
        hex::encode(self.transcript)
    }
}

/// 一个通道的收发密钥。方向不同 → 密钥不同 → 两边的计数器各自从 0 起也不会撞。
#[derive(Clone)]
pub struct ChannelKeys {
    pub c2h: [u8; 32],
    pub h2c: [u8; 32],
}

impl std::fmt::Debug for ChannelKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChannelKeys(<已派生>)")
    }
}

impl ChannelKeys {
    /// 本端「发」用的密钥。
    pub fn sending(&self, role: Role) -> [u8; 32] {
        match role {
            Role::Client => self.c2h,
            Role::Host => self.h2c,
        }
    }

    /// 本端「收」用的密钥。
    pub fn receiving(&self, role: Role) -> [u8; 32] {
        match role {
            Role::Client => self.h2c,
            Role::Host => self.c2h,
        }
    }
}

fn channel_code(ch: Channel) -> u8 {
    match ch {
        Channel::Control => 1,
        Channel::Video => 2,
        Channel::Bulk => 3,
        Channel::Audio => 4,
    }
}

fn channel_from_code(c: u8) -> Channel {
    match c {
        2 => Channel::Video,
        3 => Channel::Bulk,
        4 => Channel::Audio,
        _ => Channel::Control,
    }
}

struct LocalHandshake {
    secret: StaticSecret,
    hello: CryptoHello,
}

fn new_local() -> LocalHandshake {
    let mut b = [0u8; EPH_LEN];
    rand::thread_rng().fill_bytes(&mut b);
    let secret = StaticSecret::from(b);
    let public = PublicKey::from(&secret);
    let mut n = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut n);
    LocalHandshake {
        secret,
        hello: CryptoHello {
            v: CRYPTO_VERSION,
            eph: hex::encode(public.as_bytes()),
            nonce: hex::encode(n),
        },
    }
}

/// 按角色把两份 hello 拼成密钥。**两端算出来的必须是同一份**，所以顺序在这里钉死：
/// 客户端公钥在前，客户端随机数在前。
fn finish(local: &LocalHandshake, peer: &CryptoHello, we_are_client: bool) -> Result<SessionKeys, CryptoError> {
    if peer.v != CRYPTO_VERSION {
        return Err(CryptoError::BadVersion(peer.v));
    }
    let peer_eph = decode_fixed::<EPH_LEN>(&peer.eph).ok_or(CryptoError::BadHello)?;
    let peer_nonce = decode_fixed::<NONCE_LEN>(&peer.nonce).ok_or(CryptoError::BadHello)?;
    let our_nonce = decode_fixed::<NONCE_LEN>(&local.hello.nonce).ok_or(CryptoError::BadHello)?;

    let shared = local.secret.diffie_hellman(&PublicKey::from(peer_eph));
    if !shared.was_contributory() {
        // 全零共享密钥意味着对端给了低阶点：这时候算出来的 K 是可预测的。
        return Err(CryptoError::WeakPeerKey);
    }

    let (client_eph, host_eph, client_nonce, host_nonce) = if we_are_client {
        (&local.hello.eph, &peer.eph, &our_nonce, &peer_nonce)
    } else {
        (&peer.eph, &local.hello.eph, &peer_nonce, &our_nonce)
    };

    let mut salt = Vec::with_capacity(NONCE_LEN * 2);
    salt.extend_from_slice(client_nonce);
    salt.extend_from_slice(host_nonce);

    let hk = Hkdf::<Sha256>::new(Some(&salt), shared.as_bytes());
    let mut master = [0u8; 32];
    hk.expand(b"uulan-session-v1", &mut master)
        .expect("32 字节在 HKDF-SHA256 上限内");

    // 转录哈希：把两个公钥和两个随机数都绑进来。
    // 没有它，一次握手可以被「剪接」到另一次上（两边的公钥各换一半）。
    let mut h = Sha256::new();
    h.update(b"uulan-hs-v1");
    h.update(client_eph.as_bytes());
    h.update(host_eph.as_bytes());
    h.update(client_nonce);
    h.update(host_nonce);
    let transcript: [u8; 32] = h.finalize().into();

    Ok(SessionKeys { master, transcript })
}

fn decode_fixed<const N: usize>(s: &str) -> Option<[u8; N]> {
    let v = hex::decode(s).ok()?;
    if v.len() != N {
        return None;
    }
    let mut out = [0u8; N];
    out.copy_from_slice(&v);
    Some(out)
}

async fn send_hello<S>(s: &mut S, hello: &CryptoHello) -> Result<(), CryptoError>
where
    S: AsyncWrite + Unpin,
{
    let bytes = serde_json::to_vec(hello)?;
    write_frame(s, FrameKind::Json, &bytes).await?;
    Ok(())
}

async fn recv_hello<S>(s: &mut S) -> Result<CryptoHello, CryptoError>
where
    S: AsyncRead + Unpin,
{
    let f = read_frame(s).await?;
    if f.kind != FrameKind::Json {
        return Err(CryptoError::Protocol(format!("握手时期望 JSON，收到 {:?}", f.kind)));
    }
    Ok(f.decode_json::<CryptoHello>()?)
}

// ───────────────────────────── PAKE 的两帧 ─────────────────────────────
//
// 与 CryptoHello 一样是**明文**帧（一次性的公开点），排在 CryptoHello 之后、
// 加密层之前。它们本身不保密；保密的是「只有知道口令才能算出同一个共享点」。

/// 控制端发自己的 PAKE 点。
pub async fn send_pake_init<S>(s: &mut S, init: &PakeInit) -> Result<(), CryptoError>
where
    S: AsyncWrite + Unpin,
{
    write_frame(s, FrameKind::Json, &serde_json::to_vec(init)?).await?;
    Ok(())
}

/// 被控端收控制端的 PAKE 点。
pub async fn recv_pake_init<S>(s: &mut S) -> Result<PakeInit, CryptoError>
where
    S: AsyncRead + Unpin,
{
    let f = read_frame(s).await?;
    if f.kind != FrameKind::Json {
        return Err(CryptoError::Protocol(format!("PAKE 时期望 JSON，收到 {:?}", f.kind)));
    }
    Ok(f.decode_json::<PakeInit>()?)
}

/// 被控端回自己的 PAKE 点（**每个候选凭据一个**）。
pub async fn send_pake_resp<S>(s: &mut S, resp: &PakeResp) -> Result<(), CryptoError>
where
    S: AsyncWrite + Unpin,
{
    write_frame(s, FrameKind::Json, &serde_json::to_vec(resp)?).await?;
    Ok(())
}

/// 控制端收被控端的 PAKE 点。
pub async fn recv_pake_resp<S>(s: &mut S) -> Result<PakeResp, CryptoError>
where
    S: AsyncRead + Unpin,
{
    let f = read_frame(s).await?;
    if f.kind != FrameKind::Json {
        return Err(CryptoError::Protocol(format!("PAKE 时期望 JSON，收到 {:?}", f.kind)));
    }
    Ok(f.decode_json::<PakeResp>()?)
}

/// 控制端侧握手：发自己的 hello → 收被控端的 hello → 算密钥。
pub async fn client_handshake<S>(s: &mut S) -> Result<SessionKeys, CryptoError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let local = new_local();
    send_hello(s, &local.hello).await?;
    let peer = recv_hello(s).await?;
    finish(&local, &peer, true)
}

/// 被控端侧握手：收控制端的 hello → 回自己的 → 算密钥。
///
/// 顺序是「先收后发」：这样被控端在握手卡住时可以先超时收尾，
/// 而不是先把字节发出去再等一个可能永远不来的 hello。
pub async fn host_handshake<S>(s: &mut S) -> Result<SessionKeys, CryptoError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let local = new_local();
    let peer = recv_hello(s).await?;
    send_hello(s, &local.hello).await?;
    finish(&local, &peer, false)
}

/// 十六进制字符串的常数时间比较（长度不同也要走完整轮）。
fn ct_eq_hex(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = (a.len() ^ b.len()) as u8;
    let n = a.len().max(b.len());
    for i in 0..n {
        let x = *a.get(i).unwrap_or(&0);
        let y = *b.get(i).unwrap_or(&0);
        diff |= x ^ y;
    }
    diff == 0
}

// ───────────────────────────── 记录层 ─────────────────────────────

/// 记录层 AAD 前缀 + 通道号：把「这是哪条通道的第几条记录」绑进认证标签，
/// 于是把 video 的记录塞进 control 通道会直接失败。
const REC_AAD_PREFIX: &[u8; 12] = b"uulan-rec-v1";

/// 一条记录（= 一个帧）的密封状态。
struct AeadState {
    cipher: ChaCha20Poly1305,
    ctr: u64,
    ch: u8,
}

impl AeadState {
    fn new(key: [u8; 32], ch: Channel) -> Self {
        Self {
            cipher: ChaCha20Poly1305::new((&key).into()),
            ctr: 0,
            ch: channel_code(ch),
        }
    }

    fn next_nonce(&self) -> Result<[u8; 12], CryptoError> {
        if self.ctr == u64::MAX {
            return Err(CryptoError::CounterExhausted);
        }
        let mut n = [0u8; 12];
        n[4..].copy_from_slice(&self.ctr.to_be_bytes());
        Ok(n)
    }

    fn aad(&self) -> [u8; 13] {
            let mut a = [0u8; 13];
            a[..12].copy_from_slice(REC_AAD_PREFIX);
            a[12] = self.ch;
            a
        }

        /// 换成另一把密钥，计数器归零。
        ///
        /// 为什么归零是安全的：nonce 是「密钥 + 计数器」的组合，密钥换了以后
        /// 同一个计数器值不会产生重复的 (key, nonce)。反过来说，**同一把密钥**
        /// 重新包一遍（`SecConn::new` 再来一次）才是灾难 —— 那才是 nonce 重用。
        fn rekey(&mut self, key: [u8; 32]) {
            let ch = self.ch;
            *self = AeadState::new(key, channel_from_code(ch));
        }

    /// 加密一条记录，返回「密文 || 16 字节标签」。计数器只在这里前进 ——
    /// **密封成功才算用过这个 nonce**，失败时计数器不动（宁可重试也不跳号）。
    fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let nonce = self.next_nonce()?;
        let mut buf = Vec::with_capacity(plaintext.len() + TAG_LEN);
        buf.extend_from_slice(plaintext);
        let tag = self
            .cipher
            .encrypt_in_place_detached(Nonce::from_slice(&nonce), &self.aad(), &mut buf)
            .map_err(|_| CryptoError::Decrypt)?;
        buf.extend_from_slice(tag.as_slice());
        self.ctr += 1;
        Ok(buf)
    }

    /// 解密一条记录，就地改成明文。
    fn open(&mut self, ct: &mut Vec<u8>) -> Result<(), CryptoError> {
        if ct.len() < TAG_LEN {
            return Err(CryptoError::Decrypt);
        }
        let nonce = self.next_nonce()?;
        let body_len = ct.len() - TAG_LEN;
        let (body, tag) = ct.split_at_mut(body_len);
        self.cipher
            .decrypt_in_place_detached(
                Nonce::from_slice(&nonce),
                &self.aad(),
                body,
                Tag::from_slice(tag),
            )
            .map_err(|_| CryptoError::Decrypt)?;
        ct.truncate(ct.len() - TAG_LEN);
        self.ctr += 1;
        Ok(())
    }
}

/// 加密写：把字节缓冲成「一条记录」，`flush()` 时密封并写下 `u32 长度 || 密文`。
///
/// 为什么在 `flush` 处切记录：现有的 [`crate::write_frame`] 每帧写完必然 flush，
/// 所以「一次 flush = 一帧 = 一条记录」。记录边界其实不影响正确性
/// （读侧是把记录拼回字节流再解析帧），但「一帧一记录」让丢包/截断的定位容易得多。
pub struct SecWriter<W> {
    inner: W,
    aead: AeadState,
    /// 还没封进记录的明文字节。
    buf: Vec<u8>,
    /// 已封好、还没写完的记录（`u32 长度 || 密文`）。
    pending: Vec<u8>,
    pending_off: usize,
}

impl<W> SecWriter<W> {
    /// 用「本端发送方向」的密钥构造。
    pub fn new(inner: W, keys: &ChannelKeys, role: Role, ch: Channel) -> Self {
        Self {
            inner,
            aead: AeadState::new(keys.sending(role), ch),
            buf: Vec::new(),
            pending: Vec::new(),
            pending_off: 0,
        }
    }

    /// 还没写出去的明文字节数（测试/排障用）。
    pub fn buffered(&self) -> usize {
        self.buf.len() + self.pending.len() - self.pending_off
    }

    /// 换成会话密钥（鉴权通过之后调用一次）。
    ///
    /// 调用点必须保证「此刻没有半条记录在飞」：加密层是按 flush 切记录的，
    /// 换密钥时缓冲里若还有明文，就会被封在新密钥下、而对端还在用旧密钥读。
    pub fn rekey(&mut self, key: [u8; 32]) {
        debug_assert!(
            self.buf.is_empty() && self.pending.is_empty(),
            "换密钥时还有未写完的记录"
        );
        self.buf.clear();
        self.pending.clear();
        self.pending_off = 0;
        self.aead.rekey(key);
    }

    fn start_record(&mut self) -> Result<(), CryptoError> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let sealed = self.aead.seal(&self.buf)?;
        self.buf.clear();
        self.pending.clear();
        self.pending_off = 0;
        self.pending.extend_from_slice(&(sealed.len() as u32).to_be_bytes());
        self.pending.extend_from_slice(&sealed);
        Ok(())
    }
}

impl<W: AsyncWrite + Unpin> SecWriter<W> {
    fn poll_drain(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        while this.pending_off < this.pending.len() {
            match Pin::new(&mut this.inner).poll_write(cx, &this.pending[this.pending_off..]) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(std::io::Error::from(std::io::ErrorKind::WriteZero)))
                }
                Poll::Ready(Ok(n)) => this.pending_off += n,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
        this.pending.clear();
        this.pending_off = 0;
        Poll::Ready(Ok(()))
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for SecWriter<W> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        // 先把上一条记录写完再收新字节：内层 socket 的缓冲有限，不能一直往里塞。
        ready!(self.as_mut().poll_drain(cx))?;
        if self.buf.len() + buf.len() > MAX_RECORD {
            return Poll::Ready(Err(std::io::Error::from(CryptoError::BufferOverflow)));
        }
        self.buf.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        // 上一条记录可能还没写完（上次 flush 走到一半被 Pending 打断）。
        ready!(self.as_mut().poll_drain(cx))?;
        if !self.buf.is_empty() {
            self.start_record()?;
            ready!(self.as_mut().poll_drain(cx))?;
        }
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        ready!(self.as_mut().poll_flush(cx))?;
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// 解密读：每次从内层读够一整条记录 → 解密 → 当作字节流吐出去。
///
/// 状态机是拆开的（帧头 4 字节 / 记录体），所以 `poll_read` 被取消（`select!` 里另一支赢了）
/// 不会丢字节 —— 这与 [`crate::read_frame`] 的「不可取消」是两回事：
/// 前者是字节流的 poll 状态机，后者是两段 `read_exact` 的组合。
pub struct SecReader<R> {
    inner: R,
    aead: AeadState,
    /// 已解密、还没交出去的明文。
    plain: Vec<u8>,
    pos: usize,
    hdr: [u8; 4],
    hdr_have: usize,
    body: Vec<u8>,
    body_have: usize,
    body_len: usize,
    eof: bool,
}

impl<R> SecReader<R> {
    /// 用「本端接收方向」的密钥构造。
    pub fn new(inner: R, keys: &ChannelKeys, role: Role, ch: Channel) -> Self {
        Self {
            inner,
            aead: AeadState::new(keys.receiving(role), ch),
            plain: Vec::new(),
            pos: 0,
            hdr: [0u8; 4],
            hdr_have: 0,
            body: Vec::new(),
            body_have: 0,
            body_len: 0,
            eof: false,
        }
    }

    /// 换成会话密钥（鉴权通过之后调用一次）。已经解出来的明文不受影响 ——
    /// 换的是「往后每条记录用哪把密钥」，缓冲区里的字节早就解完了。
    pub fn rekey(&mut self, key: [u8; 32]) {
        self.aead.rekey(key);
    }
}

impl<R: AsyncRead + Unpin> SecReader<R> {
    /// 读一条完整记录并解密进 `plain`。
    ///
    /// 返回 `Ok(true)` = 有新明文；`Ok(false)` = 对端正常结束；
    /// `Err(..)` = 断在记录中间（这是**必须报错**的：说明这一帧不完整）。
    fn poll_refill(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<bool>> {
        let this = self.get_mut();

        while this.hdr_have < 4 {
            let (done, n) = {
                let mut rb = ReadBuf::new(&mut this.hdr[this.hdr_have..]);
                match Pin::new(&mut this.inner).poll_read(cx, &mut rb) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Ready(Ok(())) => (true, rb.filled().len()),
                }
            };
            if done && n == 0 {
                return Poll::Ready(if this.hdr_have == 0 {
                    this.eof = true;
                    Ok(false)
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "加密记录的长度前缀只收到一半",
                    ))
                });
            }
            this.hdr_have += n;
        }

        if this.body_len == 0 {
            let len = u32::from_be_bytes(this.hdr) as usize;
            if len < TAG_LEN || len > MAX_RECORD {
                return Poll::Ready(Err(std::io::Error::from(CryptoError::BadRecordLen(len))));
            }
            this.body_len = len;
            this.body.clear();
            this.body.resize(len, 0);
            this.body_have = 0;
        }

        while this.body_have < this.body_len {
            let (done, n) = {
                let mut rb = ReadBuf::new(&mut this.body[this.body_have..]);
                match Pin::new(&mut this.inner).poll_read(cx, &mut rb) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Ready(Ok(())) => (true, rb.filled().len()),
                }
            };
            if done && n == 0 {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "加密记录只收到一半",
                )));
            }
            this.body_have += n;
        }

        let mut body = std::mem::take(&mut this.body);
        if let Err(e) = this.aead.open(&mut body) {
            return Poll::Ready(Err(e.into()));
        }
        this.plain = body;
        this.pos = 0;
        this.hdr_have = 0;
        this.body_len = 0;
        Poll::Ready(Ok(true))
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for SecReader<R> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        loop {
            if self.pos < self.plain.len() {
                let n = buf.remaining().min(self.plain.len() - self.pos);
                buf.put_slice(&self.plain[self.pos..self.pos + n]);
                self.pos += n;
                if self.pos >= self.plain.len() {
                    self.plain.clear();
                    self.pos = 0;
                }
                return Poll::Ready(Ok(()));
            }
            match ready!(self.as_mut().poll_refill(cx)) {
                Ok(true) => continue,
                Ok(false) => return Poll::Ready(Ok(())),
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
    }
}

/// 加密连接的读半端（`TcpStream::into_split()` 的读半 + 解密）。
pub type SecReadHalf = SecReader<OwnedReadHalf>;
/// 加密连接的写半端（`TcpStream::into_split()` 的写半 + 加密）。
pub type SecWriteHalf = SecWriter<OwnedWriteHalf>;

/// 已经握过手（且计数器已经前进过）的控制通道。
///
/// 为什么不能当整连接再包一次：加密层的计数器在 Hello/Auth 期间已经走过几步，
/// 重新构造 [`SecConn`] 会让计数器回到 0 —— 那是 nonce 重用，等于把明文交出去。
/// 所以这里只交出「两半」本身。
pub struct SecControl {
    pub rd: SecReadHalf,
    pub wr: SecWriteHalf,
}

impl std::fmt::Debug for SecControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecControl(<已加密>)")
    }
}

impl SecControl {
    pub fn new(stream: TcpStream, keys: &ChannelKeys, role: Role) -> Self {
        let (rd, wr) = stream.into_split();
        Self {
            rd: SecReader::new(rd, keys, role, Channel::Control),
            wr: SecWriter::new(wr, keys, role, Channel::Control),
        }
    }

    /// 交给下一层（控制循环 / 文件 / 终端 / 转发都用同一对半端）。
    pub fn into_split(self) -> (SecReadHalf, SecWriteHalf) {
        (self.rd, self.wr)
    }
}

/// 一条已加密的会话连接（生产路径用：底下的东西就是 `TcpStream`）。
///
/// `into_split()` 的签名刻意跟 `TcpStream::into_split()` 一样，
/// 业务代码那边的改法就是「类型换了，调用一个字没变」。
pub struct SecConn {
    stream: TcpStream,
    keys: ChannelKeys,
    role: Role,
    ch: Channel,
}

impl std::fmt::Debug for SecConn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecConn").field("channel", &self.ch).finish()
    }
}

impl SecConn {
    pub fn new(stream: TcpStream, keys: &ChannelKeys, role: Role, ch: Channel) -> Self {
        Self { stream, keys: keys.clone(), role, ch }
    }

    /// 只要写半端（视频通道在被控端是单向的：只推流，不读）。
    ///
    /// 丢掉读半端不会关掉连接 —— 写半端还持有底层 socket。
    pub fn into_write_half(self) -> SecWriter<OwnedWriteHalf> {
        self.into_split().1
    }

    /// 拆成加密的两半（读、写各自一个方向密钥，互不干扰）。
    pub fn into_split(self) -> (SecReader<OwnedReadHalf>, SecWriter<OwnedWriteHalf>) {
        let (rd, wr) = self.stream.into_split();
        (
            SecReader::new(rd, &self.keys, self.role, self.ch),
            SecWriter::new(wr, &self.keys, self.role, self.ch),
        )
    }
}

/// 把一帧的明文拼出来（kind 字节 + payload），读侧再拆回去。
///
/// 单独提供这两个函数是为了让「加密层到底怎么解释字节」在一个地方可读、可测：
/// 它必须和 [`crate::frame`] 的线上格式**逐字节一致**，否则两端对同一条记录的理解会错位。
pub fn frame_to_plain(kind: FrameKind, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(payload.len() + 5);
    v.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    v.push(kind as u8);
    v.extend_from_slice(payload);
    v
}

/// [`frame_to_plain`] 的逆。坏数据返回 `None`（不 panic）。
pub fn plain_to_frame(p: &[u8]) -> Option<(FrameKind, Bytes)> {
    if p.len() < 5 {
        return None;
    }
    let len = u32::from_be_bytes(p[..4].try_into().ok()?) as usize;
    if p.len() != len + 5 {
        return None;
    }
    let kind = FrameKind::from_u8(p[4])?;
    Some((kind, Bytes::copy_from_slice(&p[5..])))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

    async fn pair() -> (SessionKeys, SessionKeys) {
        let (mut a, mut b) = tokio::io::duplex(1 << 20);
        let (c, h) = tokio::join!(client_handshake(&mut a), host_handshake(&mut b));
        (c.unwrap(), h.unwrap())
    }

    #[tokio::test]
    async fn handshake_derives_same_keys_on_both_sides() {
        let (c, h) = pair().await;
        for ch in [Channel::Control, Channel::Video, Channel::Bulk] {
            let ck = c.channel(ch);
            let hk = h.channel(ch);
            assert_eq!(ck.c2h, hk.c2h, "c→h 密钥两端必须一致");
            assert_eq!(ck.h2c, hk.h2c);
        }
        assert_eq!(c.transcript_hex(), h.transcript_hex());
    }

    #[tokio::test]
    async fn channels_and_directions_have_distinct_keys() {
        let (c, _) = pair().await;
        let ctl = c.channel(Channel::Control);
        let vid = c.channel(Channel::Video);
        assert_ne!(ctl.c2h, vid.c2h, "不同通道必须不同密钥");
        assert_ne!(ctl.c2h, ctl.h2c, "同一通道两个方向必须不同密钥");
    }

    #[tokio::test]
    async fn different_sessions_get_different_keys() {
        let (a, _) = pair().await;
        let (b, _) = pair().await;
        assert_ne!(a.channel(Channel::Control).c2h, b.channel(Channel::Control).c2h);
        assert_ne!(a.transcript_hex(), b.transcript_hex());
    }

    /// 在内存里跑一遍「PAKE + 混合」：握手 → 口令 → 两端各自的会话密钥。
    /// 生产路径的点是真的走线的，这里只是省掉 socket。
    fn pake_pair(
        c: &SessionKeys,
        h: &SessionKeys,
        client_pw: &str,
        host_cands: &[&str],
    ) -> (SessionKeys, SessionKeys) {
        let sid = *c.transcript();
        let (cl, init) = crate::pake::PakeClient::start(&sid, client_pw);
        let cands: Vec<String> = host_cands.iter().map(|s| s.to_string()).collect();
        let (host, resp) = crate::pake::PakeHost::respond(&sid, &cands).unwrap();
        let ck = cl.finish(&resp).unwrap();
        let hk = host.finish(&init).unwrap();
        let idx = host_cands.iter().position(|p| *p == client_pw).unwrap_or(0);
        (c.with_pake(&ck[idx]), h.with_pake(&hk))
    }

    #[tokio::test]
    async fn proofs_are_directional_and_do_not_cross() {
        let (c, h) = pair().await;
        let (cw, hw) = pake_pair(&c, &h, "aB3dE5fG", &["aB3dE5fG"]);
        let cp = cw.client_proof();
        assert!(hw.matches_client_proof(&cp), "被控端应认这份控制端证明");
        // 方向之间不能互相冒用
        assert!(!hw.matches_host_proof(&cp), "控制端证明不能当被控端证明用");
        let hp = hw.host_proof();
        assert!(cw.matches_host_proof(&hp));
        assert!(!cw.matches_client_proof(&hp));
    }

    #[tokio::test]
    async fn proofs_differ_across_sessions() {
        let (c1, h1) = pair().await;
        let (c2, h2) = pair().await;
        let (cw1, _) = pake_pair(&c1, &h1, "pw", &["pw"]);
        let (cw2, _) = pake_pair(&c2, &h2, "pw", &["pw"]);
        assert_ne!(cw1.client_proof(), cw2.client_proof());
    }

    /// 猜错口令时两端算出来的**密钥本身**就不一样 —— 这正是「中继没法拿抓到的证明
    /// 离线撞字典」的根：它连「这个口令对应的密钥」都算不出来（那需要 PAKE 共享点）。
    #[tokio::test]
    async fn wrong_password_yields_unrelated_keys_and_unverifiable_proofs() {
        let (c, h) = pair().await;
        let sid = *c.transcript();
        // 控制端用错口令，被控端只有正确口令
        let (cl, init) = crate::pake::PakeClient::start(&sid, "wrongPass9");
        let (host, resp) =
            crate::pake::PakeHost::respond(&sid, &["aB3dE5fG".to_string()]).unwrap();
        let ck = cl.finish(&resp).unwrap()[0];
        let hk = host.finish(&init).unwrap();
        assert_ne!(ck, hk, "口令不同，PAKE 共享点必须不同");
        let cw = c.with_pake(&ck);
        let hw = h.with_pake(&hk);
        assert!(!hw.matches_client_proof(&cw.client_proof()), "错口令的证明必须验不过");
        assert_ne!(
            cw.channel(Channel::Control).c2h,
            hw.channel(Channel::Control).c2h,
            "错口令的会话密钥必须不同（中继无法解密）"
        );
    }

    /// 候选清单里第二个命中也能用 —— 临时码就是走这条（凭据顺序不能写死成「只有第一个」）。
    #[tokio::test]
    async fn proof_matches_the_right_candidate_slot() {
        let (c, h) = pair().await;
        let (cw, hw) = pake_pair(&c, &h, "tE9mP2cO", &["aB3dE5fG", "tE9mP2cO"]);
        assert!(hw.matches_client_proof(&cw.client_proof()), "第二个候选必须能命中");
    }

    /// PAKE 密钥参与会话密钥派生：同一份握手、同一口令之外的任何差异都必须变密钥。
    #[tokio::test]
    async fn session_keys_mix_in_the_pake_secret() {
        let (c, _) = pair().await;
        let a = c.with_pake(&[1u8; 32]);
        let b = c.with_pake(&[2u8; 32]);
        assert_ne!(a.channel(Channel::Control).c2h, b.channel(Channel::Control).c2h);
        assert_ne!(
            a.channel(Channel::Control).c2h,
            c.channel(Channel::Control).c2h,
            "混了 PAKE 之后必须和只做 DH 的密钥不同"
        );
    }

    #[tokio::test]
    async fn handshake_rejects_wrong_version() {
        let local = new_local();
        let mut bad = local.hello.clone();
        bad.v = 99;
        let e = finish(&local, &bad, true).unwrap_err();
        assert!(matches!(e, CryptoError::BadVersion(99)), "{e}");
    }

    #[tokio::test]
    async fn handshake_rejects_junk_public_key() {
        let local = new_local();
        let mut bad = local.hello.clone();
        bad.eph = "00".repeat(EPH_LEN);
        let e = finish(&local, &bad, true).unwrap_err();
        assert!(matches!(e, CryptoError::WeakPeerKey), "{e}");
        let mut bad2 = local.hello.clone();
        bad2.eph = "zz".into();
        assert!(matches!(finish(&local, &bad2, true), Err(CryptoError::BadHello)));
    }

    #[tokio::test]
    async fn record_roundtrip_preserves_bytes() {
        let (c, h) = pair().await;
        let (mut w, mut r) = tokio::io::duplex(1 << 20);
        let keys_c = c.channel(Channel::Control);
        let keys_h = h.channel(Channel::Control);
        let mut sender = SecWriter::new(&mut w, &keys_c, Role::Client, Channel::Control);
        let mut recver = SecReader::new(&mut r, &keys_h, Role::Host, Channel::Control);

        let json = "{\"type\":\"hello\",\"name\":\"测试\"}";
        let plain = frame_to_plain(FrameKind::Json, json.as_bytes());
        sender.write_all(&plain).await.unwrap();
        sender.flush().await.unwrap();

        let mut got = vec![0u8; plain.len()];
        recver.read_exact(&mut got).await.unwrap();
        assert_eq!(got, plain);
        let (k, p) = plain_to_frame(&got).unwrap();
        assert_eq!(k, FrameKind::Json);
        assert_eq!(&p[..], json.as_bytes());
    }

    #[tokio::test]
    async fn plaintext_is_not_on_the_wire() {
        let (c, h) = pair().await;
        let (w, mut r) = tokio::io::duplex(1 << 20);
        let keys_c = c.channel(Channel::Control);
        let mut sender = SecWriter::new(w, &keys_c, Role::Client, Channel::Control);
        let secret = "口令-SECRET-1234";
        let plain = frame_to_plain(FrameKind::Json, secret.as_bytes());
        sender.write_all(&plain).await.unwrap();
        sender.flush().await.unwrap();
        drop(sender);

        let mut wire = Vec::new();
        r.read_to_end(&mut wire).await.unwrap();
        assert!(!wire.is_empty());
        let needle = secret.as_bytes();
        assert!(
            !wire.windows(needle.len()).any(|win| win == needle),
            "密文里不该出现明文口令"
        );
        let _ = h;
    }

    #[tokio::test]
    async fn tampered_ciphertext_is_rejected() {
        let (c, h) = pair().await;
        let (w, mut r) = tokio::io::duplex(1 << 20);
        let mut sender = SecWriter::new(w, &c.channel(Channel::Control), Role::Client, Channel::Control);
        sender.write_all(b"something important").await.unwrap();
        sender.flush().await.unwrap();
        drop(sender);

        let mut wire = Vec::new();
        r.read_to_end(&mut wire).await.unwrap();
        // 翻掉密文里的一个 bit（长度前缀后的第一个字节）
        wire[4] ^= 0x01;

        let mut reader = SecReader::new(&wire[..], &h.channel(Channel::Control), Role::Host, Channel::Control);
        let mut out = [0u8; 32];
        let e = reader.read(&mut out).await.unwrap_err();
        assert!(e.to_string().contains("解密失败"), "{e}");
    }

    #[tokio::test]
    async fn replayed_record_is_rejected() {
        let (c, h) = pair().await;
        let (w, mut r) = tokio::io::duplex(1 << 20);
        let mut sender = SecWriter::new(w, &c.channel(Channel::Control), Role::Client, Channel::Control);
        sender.write_all(b"first").await.unwrap();
        sender.flush().await.unwrap();
        sender.write_all(b"second").await.unwrap();
        sender.flush().await.unwrap();
        drop(sender);
        let mut wire = Vec::new();
        r.read_to_end(&mut wire).await.unwrap();

        // 把第一条记录原样再放一遍（计数器已经前进，nonce 对不上）
        let first_len = 4 + u32::from_be_bytes(wire[..4].try_into().unwrap()) as usize;
        let mut replayed = wire.clone();
        replayed.extend_from_slice(&wire[..first_len]);

        let mut reader = SecReader::new(&replayed[..], &h.channel(Channel::Control), Role::Host, Channel::Control);
        let mut buf = Vec::new();
        let e = reader.read_to_end(&mut buf).await.unwrap_err();
        assert!(e.to_string().contains("解密失败"), "{e}");
    }

    #[tokio::test]
    async fn wrong_direction_key_cannot_read() {
        let (c, h) = pair().await;
        let (w, mut r) = tokio::io::duplex(1 << 20);
        // 故意用控制端「接收」方向的密钥去加密 → 对端按「接收」解，必然失败
        let mut sender = SecWriter::new(w, &c.channel(Channel::Video), Role::Client, Channel::Video);
        sender.write_all(b"video frame").await.unwrap();
        sender.flush().await.unwrap();
        drop(sender);
        let mut wire = Vec::new();
        r.read_to_end(&mut wire).await.unwrap();
        let mut reader = SecReader::new(&wire[..], &h.channel(Channel::Bulk), Role::Host, Channel::Bulk);
        let mut out = [0u8; 32];
        assert!(reader.read(&mut out).await.is_err());
    }

    #[tokio::test]
    async fn truncated_record_mid_body_errors() {
        let (c, h) = pair().await;
        let (w, mut r) = tokio::io::duplex(1 << 20);
        let mut sender = SecWriter::new(w, &c.channel(Channel::Control), Role::Client, Channel::Control);
        sender.write_all(&vec![7u8; 4096]).await.unwrap();
        sender.flush().await.unwrap();
        drop(sender);
        let mut wire = Vec::new();
        r.read_to_end(&mut wire).await.unwrap();
        wire.truncate(wire.len() - 100); // 砍掉尾巴 → 记录不完整

        let mut reader = SecReader::new(&wire[..], &h.channel(Channel::Control), Role::Host, Channel::Control);
        let mut out = vec![0u8; 4096];
        let e = reader.read_exact(&mut out).await.unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn large_frame_roundtrip() {
        let (c, h) = pair().await;
        let (mut w, mut r) = tokio::io::duplex(16 << 20);
        let mut sender = SecWriter::new(&mut w, &c.channel(Channel::Video), Role::Client, Channel::Video);
        let payload: Vec<u8> = (0..(2 << 20)).map(|i| (i % 251) as u8).collect();
        let plain = frame_to_plain(FrameKind::VideoAu, &payload);
        sender.write_all(&plain).await.unwrap();
        sender.flush().await.unwrap();

        let mut reader = SecReader::new(&mut r, &h.channel(Channel::Video), Role::Host, Channel::Video);
        let mut got = vec![0u8; plain.len()];
        reader.read_exact(&mut got).await.unwrap();
        assert_eq!(got.len(), plain.len());
        let (k, p) = plain_to_frame(&got).unwrap();
        assert_eq!(k, FrameKind::VideoAu);
        assert_eq!(&p[..], &payload[..]);
    }

    #[tokio::test]
    async fn many_small_records_stream_fine() {
        let (c, h) = pair().await;
        let (mut w, mut r) = tokio::io::duplex(1 << 20);
        let keys_c = c.channel(Channel::Control);
        let keys_h = h.channel(Channel::Control);
        let mut sender = SecWriter::new(&mut w, &keys_c, Role::Client, Channel::Control);
        let mut reader = SecReader::new(&mut r, &keys_h, Role::Host, Channel::Control);

        for i in 0..200u32 {
            let plain = frame_to_plain(FrameKind::Json, format!("{{\"seq\":{i}}}").as_bytes());
            sender.write_all(&plain).await.unwrap();
            sender.flush().await.unwrap();
        }
        for i in 0..200u32 {
            let mut hdr = [0u8; 5];
            reader.read_exact(&mut hdr).await.unwrap();
            let len = u32::from_be_bytes(hdr[..4].try_into().unwrap()) as usize;
            assert_eq!(hdr[4], FrameKind::Json as u8);
            let mut body = vec![0u8; len];
            reader.read_exact(&mut body).await.unwrap();
            assert_eq!(String::from_utf8(body).unwrap(), format!("{{\"seq\":{i}}}"));
        }
    }

    #[tokio::test]
    async fn frame_helpers_roundtrip_and_reject_junk() {
        let p = frame_to_plain(FrameKind::Blob, b"abc");
        let (k, b) = plain_to_frame(&p).unwrap();
        assert_eq!(k, FrameKind::Blob);
        assert_eq!(&b[..], b"abc");
        assert!(plain_to_frame(&[]).is_none());
        assert!(plain_to_frame(&[0, 0, 0, 9, 1, 2]).is_none()); // 长度与实到不符
        assert!(plain_to_frame(&[0, 0, 0, 0, 0x7f]).is_none()); // 未知帧型
    }

    // 让 `DuplexStream` 的两个半端在测试里能有具体类型名（可读性用）。
    #[allow(dead_code)]
    fn _assert_duplex(_: DuplexStream) {}
}