//! Proof-of-work that SIMIT's public lookup asks of every client ("wehatecaptcha").
//!
//! The SIMIT web page does this silently, with no user input: it fetches a random `question`
//! from the captcha service, then finds `difficulty` prime nonces such that
//! `sha256('{"question":"…","time":T,"nonce":N}')` starts with `"0000"`. The solutions are sent,
//! JSON-encoded, in the lookup body as `reCaptchaDTO.response`. This mirrors the page's
//! `captcha-worker.js` step by step so the server sees the same thing a browser sends.
//! It is a cost-per-request check, not a human check; SAPPA still keeps its own rate limits.

use serde::Serialize;
use sha2::{Digest, Sha256};

/// One solved challenge, serialized exactly like the browser's `JSON.stringify` (key order and
/// no whitespace matter: the hash is computed over this text).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Solution {
    pub question: String,
    pub time: i64,
    pub nonce: u64,
}

fn leading_zero_hex4(digest: &[u8]) -> bool {
    // Hex "0000" = the first two bytes are zero.
    digest[0] == 0 && digest[1] == 0
}

fn encode(question: &str, time: i64, nonce: u64) -> String {
    serde_json::to_string(&Solution {
        question: question.to_string(),
        time,
        nonce,
    })
    .expect("plain struct")
}

fn itoa_buf() -> [u8; 20] {
    [0; 20]
}

/// Decimal digits of `n` without allocating.
fn format_u64(buf: &mut [u8; 20], mut n: u64) -> &[u8] {
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            return &buf[i..];
        }
    }
}

pub fn is_prime(n: u64) -> bool {
    if n < 2 {
        return false;
    }
    if n < 4 {
        return true;
    }
    if n.is_multiple_of(2) {
        return false;
    }
    let mut i = 3;
    while i * i <= n {
        if n.is_multiple_of(i) {
            return false;
        }
        i += 2;
    }
    true
}

/// True when `s` is a valid answer for its question and time.
pub fn verify(s: &Solution) -> bool {
    is_prime(s.nonce) && leading_zero_hex4(&Sha256::digest(encode(&s.question, s.time, s.nonce)))
}

/// Finds `difficulty` consecutive solutions, as the browser worker does: the first search starts
/// at nonce 2, each next one right after the previous answer.
///
/// Up to ~1M hashes per solution, so the fixed JSON prefix is hashed once and only the nonce
/// digits are added per try. Same text as [`encode`]: `{"question":"…","time":T,"nonce":N}`.
pub fn solve(question: &str, time: i64, difficulty: u32) -> Vec<Solution> {
    let full = encode(question, time, 0);
    let prefix = &full[..full.len() - "0}".len()];
    let mut base = Sha256::new();
    base.update(prefix.as_bytes());

    let mut out = Vec::with_capacity(difficulty as usize);
    let mut nonce: u64 = 1;
    let mut digits = itoa_buf();
    for _ in 0..difficulty.max(1) {
        loop {
            nonce += 1;
            let mut h = base.clone();
            h.update(format_u64(&mut digits, nonce));
            h.update(b"}");
            // Hash first: it is the rare condition, and the primality test is the slow one.
            if leading_zero_hex4(&h.finalize()) && is_prime(nonce) {
                break;
            }
        }
        out.push(Solution {
            question: question.to_string(),
            time,
            nonce,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_matches_browser_json_stringify() {
        assert_eq!(
            encode("912e47b917f75615b88daa4b145f6fb9", 1795060908, 522409),
            r#"{"question":"912e47b917f75615b88daa4b145f6fb9","time":1795060908,"nonce":522409}"#
        );
    }

    /// A solution the real SIMIT page produced (captured from browser traffic).
    #[test]
    fn verifies_a_browser_solution() {
        let s = Solution {
            question: "912e47b917f75615b88daa4b145f6fb9".into(),
            time: 1795060908,
            nonce: 522409,
        };
        assert!(verify(&s));
        assert!(!verify(&Solution { nonce: 522411, ..s }));
    }

    #[test]
    fn solves_like_the_browser_worker() {
        // The browser's answer is the first valid nonce, so our search must find exactly it.
        let got = solve("912e47b917f75615b88daa4b145f6fb9", 1795060908, 1);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].nonce, 522409);
    }

    #[test]
    fn several_solutions_are_increasing_and_valid() {
        let got = solve("51105c0550b068e4620513eea19d07a3", 1_795_000_000, 2);
        assert_eq!(got.len(), 2);
        assert!(got[0].nonce < got[1].nonce);
        assert!(got.iter().all(verify));
    }

    #[test]
    fn digits() {
        let mut b = itoa_buf();
        for n in [0u64, 7, 10, 522409, u64::MAX] {
            assert_eq!(format_u64(&mut b, n), n.to_string().as_bytes());
        }
    }

    #[test]
    fn primes() {
        let p: Vec<u64> = (0..30).filter(|n| is_prime(*n)).collect();
        assert_eq!(p, vec![2, 3, 5, 7, 11, 13, 17, 19, 23, 29]);
    }
}
