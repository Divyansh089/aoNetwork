//! Canonical unsigned LEB128 for length fields.
//!
//! Exactly one encoding is accepted per value: overlong forms (a trailing
//! zero group) and values above `u64::MAX` are rejected, so a decoded frame
//! re-encodes to the same bytes.

use crate::FrameError;

/// Longest encoding of a `u64`: ceil(64 / 7) groups.
const MAX_LEN: usize = 10;

/// Appends the canonical encoding of `value` to `out`.
pub(crate) fn encode(mut value: u64, out: &mut Vec<u8>) {
    loop {
        // Truncation is intended: only the low 7 bits are kept.
        let group = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            out.push(group);
            return;
        }
        out.push(group | 0x80);
    }
}

/// Number of bytes [`encode`] writes for `value`.
pub(crate) fn encoded_len(value: u64) -> usize {
    let bits = (u64::BITS - value.leading_zeros()) as usize;
    bits.div_ceil(7).max(1)
}

/// Decodes one value from the front of `bytes`.
///
/// Returns the value and the number of bytes consumed.
///
/// # Errors
///
/// [`FrameError::Truncated`] if `bytes` ends inside the value;
/// [`FrameError::InvalidVarint`] if the encoding is overlong or overflows.
pub(crate) fn decode(bytes: &[u8]) -> Result<(u64, usize), FrameError> {
    let mut value = 0u64;
    for (index, &byte) in bytes.iter().take(MAX_LEN).enumerate() {
        let group = u64::from(byte & 0x7F);
        // The tenth group holds bit 63 only; anything more overflows.
        if index == MAX_LEN - 1 && (group > 1 || byte & 0x80 != 0) {
            return Err(FrameError::InvalidVarint);
        }
        value |= group << (7 * index);
        if byte & 0x80 == 0 {
            // A final zero group after the first byte is an overlong form.
            if byte == 0 && index > 0 {
                return Err(FrameError::InvalidVarint);
            }
            return Ok((value, index + 1));
        }
    }
    Err(FrameError::Truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use proptest::test_runner::{Config, MapFailurePersistence, RngSeed};

    fn config() -> Config {
        let mut config = Config::with_cases(256);
        config.rng_seed = RngSeed::Fixed(0xA004_0001);
        // In-memory persistence: Miri forbids the getcwd that file
        // persistence needs. The fixed seed replays every run.
        config.failure_persistence = Some(Box::new(MapFailurePersistence::default()));
        config
    }

    fn encoded(value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        encode(value, &mut out);
        out
    }

    #[test]
    fn known_vectors() {
        let cases: [(u64, &[u8]); 6] = [
            (0, &[0x00]),
            (1, &[0x01]),
            (127, &[0x7F]),
            (128, &[0x80, 0x01]),
            (300, &[0xAC, 0x02]),
            (
                u64::MAX,
                &[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01],
            ),
        ];
        for (value, bytes) in cases {
            assert_eq!(encoded(value), bytes, "encode {value}");
            assert_eq!(encoded_len(value), bytes.len(), "len {value}");
            assert_eq!(decode(bytes), Ok((value, bytes.len())), "decode {value}");
        }
    }

    #[test]
    fn decode_stops_at_the_last_group() {
        assert_eq!(decode(&[0x80, 0x01, 0xAA, 0xBB]), Ok((128, 2)));
    }

    #[test]
    fn empty_and_unterminated_input_is_truncated() {
        assert_eq!(decode(&[]), Err(FrameError::Truncated));
        assert_eq!(decode(&[0x80]), Err(FrameError::Truncated));
        assert_eq!(decode(&[0xFF; 9]), Err(FrameError::Truncated));
    }

    #[test]
    fn overlong_encodings_are_rejected() {
        assert_eq!(decode(&[0x80, 0x00]), Err(FrameError::InvalidVarint));
        assert_eq!(decode(&[0xFF, 0x80, 0x00]), Err(FrameError::InvalidVarint));
    }

    #[test]
    fn overflow_is_rejected() {
        let mut too_big = [0xFF; 10];
        too_big[9] = 0x02;
        assert_eq!(decode(&too_big), Err(FrameError::InvalidVarint));
        // Continuation bit on the tenth group would need an eleventh.
        let mut eleven = [0xFF; 11];
        eleven[10] = 0x01;
        assert_eq!(decode(&eleven), Err(FrameError::InvalidVarint));
    }

    proptest! {
        #![proptest_config(config())]

        #[test]
        fn round_trips(value in any::<u64>()) {
            let bytes = encoded(value);
            prop_assert_eq!(bytes.len(), encoded_len(value));
            prop_assert_eq!(decode(&bytes), Ok((value, bytes.len())));
        }

        #[test]
        fn arbitrary_bytes_never_panic_and_accepted_forms_are_canonical(
            input in prop::collection::vec(any::<u8>(), 0..16),
        ) {
            if let Ok((value, used)) = decode(&input) {
                prop_assert_eq!(&encoded(value)[..], &input[..used]);
            }
        }
    }
}
