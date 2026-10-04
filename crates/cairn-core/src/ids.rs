//! ULID-based identifiers (SPEC §3.4, §4.1, §7.3).
//!
//! Format: 26 characters, Crockford base32, 128 bits = 48-bit millisecond
//! timestamp + 80 bits of randomness. Lexicographic order == creation order.
//! Generation is monotonic within a millisecond.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const RANDOM_MASK_80: u128 = (1u128 << 80) - 1;
const TIME_MASK_48: u64 = (1u64 << 48) - 1;

/// A 26-char ULID string.
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct Ulid(String);

#[derive(Default)]
struct MonotonicState {
    last_ms: u64,
    last_random: u128,
}

static STATE: Mutex<MonotonicState> = Mutex::new(MonotonicState {
    last_ms: 0,
    last_random: 0,
});

// `as_millis` is a `u128`; truncating to `u64` is safe until the year
// ~584,000,000 CE, long after this process is gone.
#[expect(
    clippy::cast_possible_truncation,
    reason = "millis since the epoch cannot overflow u64 in any real process"
)]
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_millis() as u64
}

/// 80 bits of OS entropy.
fn entropy80() -> u128 {
    let mut buf = [0u8; 10];
    getrandom::getrandom(&mut buf).expect("OS entropy unavailable");
    let mut v: u128 = 0;
    for b in buf {
        v = (v << 8) | u128::from(b);
    }
    v & RANDOM_MASK_80
}

impl Ulid {
    /// Generate a new ULID using the system clock and OS entropy.
    #[must_use]
    pub fn new() -> Self {
        Self::from_parts(now_ms())
    }

    /// Construct from an explicit millisecond timestamp. Same `ms` in the same
    /// process yields strictly increasing ULIDs (monotonic rule). Deterministic
    /// given the process state, which makes it usable in tests.
    #[must_use]
    pub fn from_parts(ms: u64) -> Self {
        let time = ms & TIME_MASK_48;
        let random = {
            let mut state = STATE.lock().expect("ulid state poisoned");
            if state.last_ms == ms && ms != 0 {
                state.last_random = (state.last_random + 1) & RANDOM_MASK_80;
            } else {
                state.last_random = entropy80();
            }
            state.last_ms = ms;
            state.last_random
        };
        Self(encode(time, random))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Encode 48-bit time + 80-bit random as 26 Crockford-base32 characters.
fn encode(time: u64, random: u128) -> String {
    let t = time & TIME_MASK_48;
    let mut out = String::with_capacity(26);
    // 10 chars × 5 bits = 50 bits for a 48-bit value (top char uses 3 bits).
    for shift in [45u32, 40, 35, 30, 25, 20, 15, 10, 5, 0] {
        out.push(char::from(CROCKFORD[((t >> shift) & 0x1F) as usize]));
    }
    // 16 chars × 5 bits = 80 bits exactly.
    for i in (0..16).rev() {
        let bits = (random >> (i * 5)) & 0x1F;
        out.push(char::from(CROCKFORD[bits as usize]));
    }
    out
}

impl Default for Ulid {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for Ulid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

macro_rules! id_newtype {
    ($name:ident, $doc:expr) => {
        #[doc = $doc]
        #[derive(
            Clone,
            Debug,
            Default,
            PartialEq,
            Eq,
            Hash,
            PartialOrd,
            Ord,
            Serialize,
            Deserialize,
            schemars::JsonSchema,
        )]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            #[must_use]
            pub fn new() -> Self {
                Self(Ulid::new().to_string())
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_newtype!(SessionId, "Session identifier (SPEC §11.7).");
id_newtype!(MessageId, "Message identifier (SPEC §4.1).");
id_newtype!(CheckpointId, "Checkpoint identifier (SPEC §9.8).");
id_newtype!(JobId, "Background job identifier (SPEC §6.2.9).");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ulid_is_26_chars_crockford() {
        let id = Ulid::new();
        assert_eq!(id.as_str().len(), 26, "{id}");
        for c in id.as_str().chars() {
            assert!(CROCKFORD.contains(&(c as u8)), "invalid char {c} in {id}");
        }
    }

    #[test]
    fn ulid_is_monotonic_within_same_ms() {
        let ms = 1_700_000_000_042;
        let a = Ulid::from_parts(ms);
        let b = Ulid::from_parts(ms);
        let c = Ulid::from_parts(ms);
        assert!(a.as_str() < b.as_str(), "{a} !< {b}");
        assert!(b.as_str() < c.as_str(), "{b} !< {c}");
    }

    #[test]
    fn ulid_orders_by_timestamp_across_days() {
        // Interleaving with other tests must not break ordering: the time
        // characters are the high part of the string.
        let early = Ulid::from_parts(1_600_000_000_000);
        let late = Ulid::from_parts(1_900_000_000_000);
        assert!(early.as_str() < late.as_str(), "{early} !< {late}");
        assert_ne!(&early.as_str()[..10], &late.as_str()[..10]);
        assert_eq!(encode(1_600_000_000_000, 0).len(), 26);
    }

    #[test]
    fn encode_is_pure() {
        assert_eq!(encode(0, 0), "00000000000000000000000000");
        assert_eq!(encode(1, 0), "00000000010000000000000000");
    }

    #[test]
    fn ids_roundtrip_json() {
        let s = SessionId::new();
        let json = serde_json::to_string(&s).unwrap();
        let back: SessionId = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
        assert_eq!(s.as_str().len(), 26);
    }
}
