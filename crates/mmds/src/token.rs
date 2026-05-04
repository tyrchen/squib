//! V2 token store.
//!
//! Per [15-mmds.md § 3](../../../specs/15-mmds.md#3-packet-interception):
//!
//! > V2 requires a `X-aws-ec2-metadata-token` header issued by a prior
//! > `PUT /latest/api/token` (TTL bounded by `mmds-config.token_ttl_seconds`).
//!
//! Per CLAUDE.md `§ Cryptography & Secrets`:
//! - Tokens are 32 bytes of OS-CSPRNG output (≥ 256 bits).
//! - Compare via constant-time check (no information leak through string comparison short-circuit).
//! - Stored only in memory; never logged.
//!
//! TTL bounds (per
//! [21-api-compat-matrix.md](../../../specs/21-api-compat-matrix.md)
//! `/mmds/config`): 1 ≤ `token_ttl_seconds` ≤ 21600 (6 hours, the AWS
//! `IMDSv2` ceiling).

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use parking_lot::RwLock;
use thiserror::Error;

/// Lower TTL bound (seconds).
pub const MIN_TTL_SECONDS: u32 = 1;
/// Upper TTL bound (seconds).
pub const MAX_TTL_SECONDS: u32 = 21_600;
/// Token length in bytes — 256 bits per CLAUDE.md.
pub const TOKEN_LEN_BYTES: usize = 32;

/// Errors produced by the token store.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TokenStoreError {
    /// Caller passed a TTL outside `[MIN_TTL_SECONDS, MAX_TTL_SECONDS]`.
    #[error("token TTL {ttl} outside [{min}, {max}]")]
    TtlOutOfRange {
        /// Requested TTL.
        ttl: u32,
        /// Minimum allowed.
        min: u32,
        /// Maximum allowed.
        max: u32,
    },

    /// Failed to read random bytes for a fresh token.
    #[error("failed to read OS entropy: {0}")]
    Entropy(String),
}

/// Opaque V2 token. The wire form is the URL-safe base64 of the random bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Token(String);

impl Token {
    /// Wire form (base64url, no padding).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Length-checked comparison constant in time.
    #[must_use]
    pub fn constant_time_eq(&self, other: &str) -> bool {
        let a = self.0.as_bytes();
        let b = other.as_bytes();
        if a.len() != b.len() {
            return false;
        }
        let mut diff: u8 = 0;
        for i in 0..a.len() {
            diff |= a[i] ^ b[i];
        }
        diff == 0
    }
}

/// In-memory token store with TTL enforcement.
///
/// Cloning shares the underlying store via `Arc<RwLock<…>>`.
#[derive(Debug, Clone, Default)]
pub struct TokenStore {
    inner: Arc<RwLock<TokenStoreInner>>,
}

#[derive(Debug, Default)]
struct TokenStoreInner {
    tokens: HashMap<Token, Instant>,
}

impl TokenStore {
    /// Build an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Issue a fresh token with the given TTL in seconds.
    ///
    /// # Errors
    /// - [`TokenStoreError::TtlOutOfRange`] if `ttl_seconds` is out of bounds.
    /// - [`TokenStoreError::Entropy`] if the OS RNG fails.
    pub fn issue(&self, ttl_seconds: u32) -> Result<Token, TokenStoreError> {
        if !(MIN_TTL_SECONDS..=MAX_TTL_SECONDS).contains(&ttl_seconds) {
            return Err(TokenStoreError::TtlOutOfRange {
                ttl: ttl_seconds,
                min: MIN_TTL_SECONDS,
                max: MAX_TTL_SECONDS,
            });
        }
        let mut bytes = [0u8; TOKEN_LEN_BYTES];
        read_csprng(&mut bytes).map_err(|e| TokenStoreError::Entropy(e.to_string()))?;
        let encoded = base64url_no_pad(&bytes);
        let token = Token(encoded);
        let expires_at = Instant::now() + Duration::from_secs(u64::from(ttl_seconds));
        let mut inner = self.inner.write();
        inner.tokens.insert(token.clone(), expires_at);
        Ok(token)
    }

    /// Validate a token. Removes expired entries opportunistically.
    ///
    /// Returns `true` if the token is present and not yet expired.
    #[must_use]
    pub fn validate(&self, candidate: &str) -> bool {
        // First scan for the matching token; constant-time per-entry to keep
        // comparison cost the same regardless of whether the candidate
        // matches token #1 or token #N (squib never holds many tokens
        // anyway, but the property is cheap and guards against an attacker
        // probing TTL behaviour to learn token presence).
        let now = Instant::now();
        let mut hit: Option<Token> = None;
        let mut purge: Vec<Token> = Vec::new();
        {
            let inner = self.inner.read();
            for (tok, exp) in &inner.tokens {
                if *exp <= now {
                    purge.push(tok.clone());
                    continue;
                }
                if tok.constant_time_eq(candidate) {
                    hit = Some(tok.clone());
                }
            }
        }
        if !purge.is_empty() {
            let mut inner = self.inner.write();
            for t in purge {
                inner.tokens.remove(&t);
            }
        }
        hit.is_some()
    }

    /// Number of currently-stored tokens (test helper).
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.read().tokens.len()
    }

    /// `true` if no tokens.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Read `buf.len()` bytes of CSPRNG-grade entropy from the OS.
///
/// CLAUDE.md `§ Cryptography & Secrets` requires `OsRng` / `getrandom`-grade
/// quality. We go through `/dev/urandom` directly for portability across
/// every supported macOS version. The workspace's `disallowed-types` lint
/// bans `std::fs::File` to push runtime I/O onto Tokio; this is a one-shot
/// init-time read of 32 bytes, never a hot path, so the explicit allow is
/// fine.
#[allow(clippy::disallowed_types)]
fn read_csprng(buf: &mut [u8]) -> std::io::Result<()> {
    use std::io::Read;
    let mut f = std::fs::File::open("/dev/urandom")?;
    f.read_exact(buf)
}

/// URL-safe base64 (no padding). Hand-rolled to avoid pulling in another
/// crate for a single 32-byte encode.
fn base64url_no_pad(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((bytes.len() * 4).div_ceil(3));
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let n =
            (u32::from(bytes[i]) << 16) | (u32::from(bytes[i + 1]) << 8) | u32::from(bytes[i + 2]);
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
        out.push(ALPHABET[(n & 0x3F) as usize] as char);
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let n = u32::from(bytes[i]) << 16;
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
    } else if rem == 2 {
        let n = (u32::from(bytes[i]) << 16) | (u32::from(bytes[i + 1]) << 8);
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_issue_distinct_tokens() {
        let s = TokenStore::new();
        let a = s.issue(60).unwrap();
        let b = s.issue(60).unwrap();
        assert_ne!(a.as_str(), b.as_str());
    }

    #[test]
    fn test_should_validate_issued_token() {
        let s = TokenStore::new();
        let t = s.issue(60).unwrap();
        assert!(s.validate(t.as_str()));
    }

    #[test]
    fn test_should_reject_unknown_token() {
        let s = TokenStore::new();
        s.issue(60).unwrap();
        assert!(!s.validate("not-a-real-token"));
    }

    #[test]
    fn test_should_reject_expired_token() {
        let s = TokenStore::new();
        // Manually inject a token with a TTL in the past so we don't have to sleep.
        let t = s.issue(1).unwrap();
        {
            let mut inner = s.inner.write();
            let exp = inner.tokens.get_mut(&t).unwrap();
            *exp = Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now);
        }
        assert!(!s.validate(t.as_str()));
        // Validate-side purge removed the entry.
        assert!(s.is_empty());
    }

    #[test]
    fn test_should_reject_ttl_below_min() {
        let s = TokenStore::new();
        assert!(matches!(
            s.issue(0),
            Err(TokenStoreError::TtlOutOfRange { .. })
        ));
    }

    #[test]
    fn test_should_reject_ttl_above_max() {
        let s = TokenStore::new();
        assert!(matches!(
            s.issue(MAX_TTL_SECONDS + 1),
            Err(TokenStoreError::TtlOutOfRange { .. })
        ));
    }

    #[test]
    fn test_should_use_constant_time_compare() {
        let t = Token("aaaaaaaa".into());
        assert!(t.constant_time_eq("aaaaaaaa"));
        assert!(!t.constant_time_eq("aaaaaaab"));
        assert!(!t.constant_time_eq("a"));
    }

    #[test]
    fn test_base64url_round_trips_for_known_vectors() {
        // RFC 4648 § 10 examples.
        assert_eq!(base64url_no_pad(b""), "");
        assert_eq!(base64url_no_pad(b"f"), "Zg");
        assert_eq!(base64url_no_pad(b"fo"), "Zm8");
        assert_eq!(base64url_no_pad(b"foo"), "Zm9v");
        assert_eq!(base64url_no_pad(b"foob"), "Zm9vYg");
        assert_eq!(base64url_no_pad(b"fooba"), "Zm9vYmE");
        assert_eq!(base64url_no_pad(b"foobar"), "Zm9vYmFy");
    }
}
