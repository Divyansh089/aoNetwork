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
