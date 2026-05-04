//! `UUIDv7` construction from `OsRng` and a wall-clock timestamp.
//!
//! The `uuid` crate's built-in `v7` feature transitively pulls a different
//! `getrandom` major than `ed25519-dalek` does, which would violate the
//! workspace's no-duplicate-versions ban. We use `uuid::Builder` (which is
//! always available, takes random bytes from the caller, and does no RNG of
//! its own) and feed it from `rand_core::OsRng` so the entire crate stays on
//! one RNG stack.

use rand_core::{OsRng, RngCore};
use time::OffsetDateTime;
use uuid::Uuid;

/// Mint a `UUIDv7` from the current wall clock and `OsRng`.
///
/// The system clock is expected to be set to a sane post-1970 value. A
/// pre-epoch clock is a host-configuration bug; this function returns an
/// error rather than coercing the timestamp.
pub(crate) fn now_v7() -> Result<Uuid, UuidV7Error> {
    let nanos = OffsetDateTime::now_utc().unix_timestamp_nanos();
    let millis_i128 = nanos / 1_000_000;
    let unix_ms = u64::try_from(millis_i128).map_err(|_| UuidV7Error::ClockBeforeEpoch)?;
    let mut random = [0_u8; 10];
    OsRng.fill_bytes(&mut random);
    Ok(build(unix_ms, random))
}

fn build(unix_ms: u64, random: [u8; 10]) -> Uuid {
    uuid::Builder::from_unix_timestamp_millis(unix_ms, &random).into_uuid()
}

/// Errors that can occur when constructing a `UUIDv7` from the wall clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UuidV7Error {
    /// The system clock returned a timestamp before the Unix epoch.
    /// `UUIDv7` cannot encode pre-1970 instants.
    ClockBeforeEpoch,
}

impl std::fmt::Display for UuidV7Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClockBeforeEpoch => f.write_str("system clock is before the Unix epoch"),
        }
    }
}

impl std::error::Error for UuidV7Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_sets_version_seven() {
        let uuid = build(1_700_000_000_000, [0xFF; 10]);
        assert_eq!(uuid.get_version_num(), 7);
    }

    #[test]
    fn build_sets_rfc4122_variant() {
        let uuid = build(1_700_000_000_000, [0xFF; 10]);
        assert_eq!(uuid.get_variant(), uuid::Variant::RFC4122);
    }

    #[test]
    fn build_round_trips_timestamp_high_bytes() {
        let unix_ms: u64 = 0x0000_0123_4567_89AB;
        let uuid = build(unix_ms, [0; 10]);
        let bytes = uuid.as_bytes();
        assert_eq!(bytes[0], 0x01);
        assert_eq!(bytes[1], 0x23);
        assert_eq!(bytes[2], 0x45);
        assert_eq!(bytes[3], 0x67);
        assert_eq!(bytes[4], 0x89);
        assert_eq!(bytes[5], 0xAB);
    }

    #[test]
    fn now_v7_is_version_seven_and_rfc4122() {
        let uuid = now_v7().unwrap();
        assert_eq!(uuid.get_version_num(), 7);
        assert_eq!(uuid.get_variant(), uuid::Variant::RFC4122);
    }

    #[test]
    fn now_v7_yields_distinct_ids_on_consecutive_calls() {
        let a = now_v7().unwrap();
        let b = now_v7().unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn build_with_zero_timestamp_still_valid_v7() {
        let uuid = build(0, [0; 10]);
        assert_eq!(uuid.get_version_num(), 7);
        assert_eq!(uuid.get_variant(), uuid::Variant::RFC4122);
    }

    #[test]
    fn distinct_random_input_at_same_millisecond_yields_distinct_uuids() {
        let unix_ms: u64 = 1_700_000_000_000;
        let mut a_random = [0_u8; 10];
        let mut b_random = [0_u8; 10];
        OsRng.fill_bytes(&mut a_random);
        OsRng.fill_bytes(&mut b_random);
        assert_ne!(
            a_random, b_random,
            "OsRng drew identical 10-byte arrays — vanishingly unlikely"
        );
        let a = build(unix_ms, a_random);
        let b = build(unix_ms, b_random);
        assert_ne!(a, b);
    }
}
