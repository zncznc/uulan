//! 日志脱敏：把「拿到手就能用」的秘密换成能对日志、但拿不回原值的短标签。
//!
//! 为什么需要它：`journalctl --user -u uulan-host`、systemd 的 journal、
//! 被工单贴出来的日志文件，都是**能被第三方看到**的地方。会话票据（`gen_session`
//! 生成的 32 个十六进制字符）是三条 relay 连接的配对凭据 —— 它在 60 秒内是货真价实的
//! bearer token。把原值打进日志，等于把「能接管一次会话」的钥匙跟着日志一起传出去。
//!
//! 但完全不记又会失去排障能力：出了问题要能把「服务端说要开一个会话」和
//! 「被控端说有人要连进来」这两条日志对上。所以打成**不可逆的短标签**：
//!
//! ```text
//! 服务端：发起会话 session=t1f3c0a97
//! 被控端：有人要连进来 session=t1f3c0a97
//! ```
//!
//! 两边一致说明是同一次会话；标签本身是 SHA-256 的前 4 字节，反推不出票据。
//!
//! **不要**拿它给低熵的输入（口令）当「哈希存储」用 —— 口令校验走的是
//! `crypto.rs` 里的 HMAC 证明，这里只管日志。

use sha2::{Digest, Sha256};

/// 标签长度（含前缀）：`t` + 8 个十六进制字符。
pub const TAG_LEN: usize = 9;

/// 把一个秘密换成一个稳定但不可逆的短标签。
///
/// 同一个输入永远得到同一个标签（跨进程也一致 —— 用的是 SHA-256，没有随机盐），
/// 所以服务端与被控端各自打的日志能对上；但标签里不含原值的任何片段。
pub fn tag(secret: &str) -> String {
    let digest = Sha256::digest(secret.as_bytes());
    let mut out = String::with_capacity(TAG_LEN);
    out.push('t');
    out.push_str(&hex::encode(&digest[..4]));
    out
}

/// 只报告「有多少」，不报告内容。给「需要知道东西多大，但绝不该看到内容」的场合
/// （剪贴板/终端一类用户数据）统一口径用。
pub fn len_only(bytes: usize) -> String {
    format!("<{bytes} 字节已隐去>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_shape_and_stability() {
        let t = tag("0123456789abcdef0123456789abcdef");
        assert_eq!(t.len(), TAG_LEN);
        assert!(t.starts_with('t'));
        assert!(t[1..].chars().all(|c| c.is_ascii_hexdigit()));
        // 同一个票据在任何进程里都必须得到同一个标签，否则两端日志对不上。
        assert_eq!(t, tag("0123456789abcdef0123456789abcdef"));
    }

    #[test]
    fn tag_never_contains_the_secret() {
        for s in [
            "0123456789abcdef0123456789abcdef",
            "aB3dE5fG",              // 形态像访问密码
            "123456",                // 临时验证码
            "uulan",                 // 短到几乎不可能「恰好」出现在标签里
        ] {
            let t = tag(s);
            assert!(!t.contains(s), "标签 {t} 里出现了原值 {s}");
            // 也不该出现原值的任何一个长度 ≥ 4 的片段（防止有人只截一段打日志）。
            if s.len() >= 4 {
                for i in 0..=s.len() - 4 {
                    assert!(!t.contains(&s[i..i + 4]), "标签 {t} 里出现了原值片段");
                }
            }
        }
    }

    #[test]
    fn different_secrets_get_different_tags() {
        // 1000 个随机票据不该撞标签（32 位标签空间里 1000 个样本撞上的概率可忽略）。
        let mut seen = std::collections::HashSet::new();
        for i in 0..1000u32 {
            let s = format!("{i:032x}");
            assert!(seen.insert(tag(&s)), "两枚票据撞了同一个标签");
        }
        assert_eq!(seen.len(), 1000);
    }

    #[test]
    fn empty_and_unicode_do_not_panic() {
        assert_eq!(tag("").len(), TAG_LEN);
        assert_eq!(tag("中文口令").len(), TAG_LEN);
        assert_ne!(tag("中文口令"), tag("中文口今"));
    }

    #[test]
    fn len_only_hides_content() {
        assert_eq!(len_only(0), "<0 字节已隐去>");
        assert_eq!(len_only(192 * 1024), "<196608 字节已隐去>");
        assert!(!len_only(42).contains("hello"));
    }
}