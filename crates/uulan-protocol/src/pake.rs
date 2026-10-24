//! 口令认证密钥交换（CPace，M6 加固的最后一环）。
//!
//! ## 这一层解决的是什么问题
//!
//! 加密层的握手（[`crate::crypto`]）是不认证的 X25519：主动 MITM 的中继可以各连各的
//! 握手、同时拿到两份主密钥。旧办法是让双方各发一份 `HMAC(主密钥, …||口令)` 来证明
//! 「我知道口令」—— 但那正好给了中继一个**离线口令校验器**：它拿着抓到的证明，
//! 逐个猜口令算一遍 HMAC 就能撞字典（每猜一次不需要再连一次）。
//!
//! PAKE 把「猜口令」变回**必须在线**：口令不再是认证时用到的**输入**，而是
//! **生成元的来源**。攻击者不知道口令就不知道双方用的生成元，
//! 于是共享点 `G^(x·y)` 对它是 CDH 难题 —— 每猜一次都得跟被控端真实交互一轮。
//!
//! ## 为什么是 CPace（而不是 SPAKE2/OPAQUE）
//!
//! - **不引新依赖**：Ristretto255 的哈希到群映射由已经在依赖树里的 `curve25519-dalek`
//!   提供（`x25519-dalek` 的底层）。内网现场没有 crates.io，多一个 crate 就是多一份
//!   离线打包风险。
//! - **结构最短**：一轮消息（每端一个点）+ 一次 DH。SPAKE2 要两个固定盲化点并处理
//!   「M/N 的离散对数未知」这类约定；OPAQUE 要 OPRF（同一条理由：额外密码学组件）。
//!
//! ## 结构（balanced CPace）
//!
//! ```text
//! 控制端                                              被控端
//!   G = H2G(DSI‖sid‖口令)                               （每个候选口令各算一个 G_i）
//!   Ya = G^x          --PakeInit{ya}-->                  
//!                     <--PakeResp{yb_0..yb_n}--         Yb_i = G_i^y
//!   K_i = Yb_i^x                                        K = Ya^y
//! ```
//!
//! `K_i` 只在 `口令_i` 等于控制端口令时与 `K` 相等 —— 被控端不必知道是哪一个，
//! 它只需要拿自己的 `K` 去核对控制端发来的一组证明（见 `crypto::SessionKeys::client_proof`）。
//!
//! **`sid` 必须是加密握手算出来的转录哈希**：没有它，两次会话的点可以互相剪接。
//!
//! ## 已知边界（照实写）
//!
//! - 这份实现是**自己写的**（没有第三方审计）。用到的都是成熟原语：Ristretto255
//!   的均匀映射、常数标量乘、SHA-512/SHA-256 域分隔。审查重点应放在
//!   `generator()` 的域分隔与 `key_from()` 里把 `ya`/`ybs` 一起绑进去这两处。
//! - 只做**平衡式**（双方都知道口令）；不对抗「恶意被控端」这类更细的模型。
//! - 口令强度仍是根本：PAKE 只把「离线撞」变成「在线撞」，8 位随机码在
//!   [`crate::AUTH_MAX_FAILURES`] 这类锁定策略下才够用。

use curve25519_dalek::ristretto::{CompressedRistretto, RistrettoPoint};
use curve25519_dalek::scalar::Scalar;
use curve25519_dalek::traits::Identity;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};

use crate::crypto::{CryptoError, CRYPTO_VERSION};

/// 域分隔标签（改它 = 换协议）。
const DSI: &[u8] = b"uulan-cpace-v1";
/// Ristretto 压缩点长度。
pub const PAKE_PT_LEN: usize = 32;
/// 一次响应里最多几个候选口令（永久口令 + 临时码）。上限是为了不让对端用
/// 「一千个候选点」把我们拖进一次大规模的标量乘。
pub const PAKE_MAX_CANDIDATES: usize = 4;

/// 控制端 → 被控端：我的点。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PakeInit {
    pub v: u32,
    /// Ristretto 压缩点（hex，32 字节 = 64 个 hex 字符）。
    pub ya: String,
}

/// 被控端 → 控制端：**每个候选口令**各一个点。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PakeResp {
    pub v: u32,
    pub ybs: Vec<String>,
}

/// 口令 → 生成元。口令只在这里出现，之后不再参与任何计算。
///
/// 用 SHA-512 出 64 字节再交给 Ristretto 的均匀映射（`from_uniform_bytes`），
/// 这是 Ristretto255 官方规定的哈希到群方式。
fn generator(sid: &[u8; 32], password: &str) -> RistrettoPoint {
    let mut h = Sha512::new();
    h.update(DSI);
    h.update(sid);
    h.update((password.len() as u64).to_be_bytes());
    h.update(password.as_bytes());
    let wide: [u8; 64] = h.finalize().into();
    RistrettoPoint::from_uniform_bytes(&wide)
}

fn random_scalar() -> Scalar {
    let mut b = [0u8; 64];
    rand::thread_rng().fill_bytes(&mut b);
    Scalar::from_bytes_mod_order_wide(&b)
}

fn decode_point(s: &str) -> Result<RistrettoPoint, CryptoError> {
    let v = hex::decode(s).map_err(|_| CryptoError::BadHello)?;
    if v.len() != PAKE_PT_LEN {
        return Err(CryptoError::BadHello);
    }
    let arr: [u8; PAKE_PT_LEN] = v.try_into().map_err(|_| CryptoError::BadHello)?;
    let p = CompressedRistretto(arr).decompress().ok_or(CryptoError::WeakPeerKey)?;
    if p == RistrettoPoint::identity() {
        // 身份元 = 「生成元被抵消」，这时候共享点是可预测的。
        return Err(CryptoError::WeakPeerKey);
    }
    Ok(p)
}

fn encode_point(p: &RistrettoPoint) -> String {
    hex::encode(p.compress().to_bytes())
}

/// 把 DH 结果压成 32 字节密钥。**必须把 `ya` 与全部 `ybs` 一并绑进来**：
/// 否则「换掉响应里第 i 个点」不会改变密钥，中间人就能挑一个更弱的候选序号。
fn key_from(sid: &[u8; 32], ya: &str, ybs: &[String], dh: &RistrettoPoint) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"uulan-pake-k1");
    h.update(sid);
    h.update((ya.len() as u64).to_be_bytes());
    h.update(ya.as_bytes());
    for y in ybs {
        h.update((y.len() as u64).to_be_bytes());
        h.update(y.as_bytes());
    }
    h.update(dh.compress().to_bytes());
    h.finalize().into()
}

/// 控制端侧：只用**自己那一个**口令。
pub struct PakeClient {
    x: Scalar,
    ya: String,
    sid: [u8; 32],
}

impl PakeClient {
    /// 发 `ya` 之前先建好状态。
    pub fn start(sid: &[u8; 32], password: &str) -> (Self, PakeInit) {
        let g = generator(sid, password);
        let x = random_scalar();
        let ya = encode_point(&(g * x));
        (
            Self { x, ya: ya.clone(), sid: *sid },
            PakeInit { v: CRYPTO_VERSION, ya },
        )
    }

    /// 收到 `PakeResp` 后算出**每个候选次序各一把**密钥；只有「被控端恰好有这个口令」
    /// 的那一个是真的，其余都是垃圾（控制端本来也不知道是哪一个，只能全试一遍）。
    pub fn finish(&self, resp: &PakeResp) -> Result<Vec<[u8; 32]>, CryptoError> {
        if resp.v != CRYPTO_VERSION {
            return Err(CryptoError::BadVersion(resp.v));
        }
        if resp.ybs.is_empty() || resp.ybs.len() > PAKE_MAX_CANDIDATES {
            return Err(CryptoError::Protocol(format!(
                "PAKE 候选数{}不合理",
                resp.ybs.len()
            )));
        }
        let mut out = Vec::with_capacity(resp.ybs.len());
        for y in &resp.ybs {
            let p = decode_point(y)?;
            let dh = p * self.x;
            out.push(key_from(&self.sid, &self.ya, &resp.ybs, &dh));
        }
        Ok(out)
    }
}

/// 被控端侧：**每个候选凭据各算一个点**，自己只留一个共享点。
pub struct PakeHost {
    y: Scalar,
    sid: [u8; 32],
    /// 已经发出去的点列表：算密钥时要原样再绑一次（控制端用的是它收到的那份）。
    ybs: Vec<String>,
}

impl PakeHost {
    /// 按候选口令列表生成响应。顺序即候选次序（控制端的证明按同一序号回）。
    pub fn respond(sid: &[u8; 32], candidates: &[String]) -> Result<(Self, PakeResp), CryptoError> {
        if candidates.is_empty() || candidates.len() > PAKE_MAX_CANDIDATES {
            return Err(CryptoError::Protocol(format!(
                "PAKE 候选数{}不合理",
                candidates.len()
            )));
        }
        let y = random_scalar();
        let mut ybs = Vec::with_capacity(candidates.len());
        for pw in candidates {
            let g = generator(sid, pw);
            ybs.push(encode_point(&(g * y)));
        }
        let resp = PakeResp { v: CRYPTO_VERSION, ybs: ybs.clone() };
        Ok((Self { y, sid: *sid, ybs }, resp))
    }

    /// 收到控制端的 `ya` 后算出共享点。**与候选无关**：只要控制端用的口令是候选之一，
    /// 它的 `K_i` 就等于这里的值。
    pub fn finish(&self, init: &PakeInit) -> Result<[u8; 32], CryptoError> {
        if init.v != CRYPTO_VERSION {
            return Err(CryptoError::BadVersion(init.v));
        }
        let ya = decode_point(&init.ya)?;
        let dh = ya * self.y;
        Ok(key_from(&self.sid, &init.ya, &self.ybs, &dh))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(tag: u8) -> [u8; 32] {
        let mut s = [0u8; 32];
        s[0] = tag;
        s
    }

    #[test]
    fn generator_depends_on_password_and_sid() {
        let s = sid(1);
        assert_ne!(generator(&s, "abc").compress(), generator(&s, "abd").compress());
        assert_ne!(generator(&sid(1), "abc").compress(), generator(&sid(2), "abc").compress());
    }

    #[test]
    fn points_reject_garbage_and_identity() {
        assert!(decode_point("zz").is_err());
        assert!(decode_point(&"00".repeat(32)).is_err(), "非法的压缩点必须拒绝");
        assert!(decode_point(&hex::encode(RistrettoPoint::identity().compress().to_bytes())).is_err());
    }
}