//! 2B1T dual-rail wire format.
//!
//! Each trit occupies two bits; four trits pack LSB-first into one byte,
//! sixteen trits into one little-endian `u32` word (exactly one PIO OSR fill):
//!
//! | trit | bits | P rail (bit 0) | N rail (bit 1) |
//! |------|------|----------------|----------------|
//! |  0   | 0b00 | low            | low            |
//! | +1   | 0b01 | high           | low            |
//! | -1   | 0b10 | low            | high           |
//! | rsv  | 0b11 | high           | high           |  (tile sync word `0xFFFF` only)
//!
//! The firmware is a transparent bridge: bytes received over TCP are emitted
//! unchanged, so framing (sync word, tile header, CRC-16) stays the job of the
//! host-side `trich-codec` and the FPGA.

/// Trits carried by one 32-bit PIO word.
pub const TRITS_PER_WORD: usize = 16;
/// Bytes per PIO word.
pub const BYTES_PER_WORD: usize = 4;

/// Logical value of a 2-bit slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Zero,
    Pos,
    Neg,
    Reserved,
}

/// Rail levels `(p, n)` driven for trit index `i` (0 = first on the wire) of `word`.
///
/// This is the reference model of what the PIO `out pins, 2` instruction does
/// with a right-shifting OSR: OUT base pin = P rail, base + 1 = N rail.
pub const fn rails(word: u32, i: usize) -> (bool, bool) {
    let bits = (word >> (2 * i)) & 0b11;
    (bits & 0b01 != 0, bits & 0b10 != 0)
}

/// Decode slot `i` of `word`.
pub const fn slot(word: u32, i: usize) -> Slot {
    match rails(word, i) {
        (false, false) => Slot::Zero,
        (true, false) => Slot::Pos,
        (false, true) => Slot::Neg,
        (true, true) => Slot::Reserved,
    }
}

/// Largest word-aligned prefix of `filled` bytes.
pub const fn aligned_len(filled: usize) -> usize {
    filled & !(BYTES_PER_WORD - 1)
}

/// Zero-pad `buf[..filled]` up to the next word boundary (trit `0` = `0b00`,
/// zero switching power). Returns the padded length.
pub fn pad_to_word(buf: &mut [u8], filled: usize) -> usize {
    let padded = aligned_len(filled + BYTES_PER_WORD - 1);
    buf[filled..padded].fill(0);
    padded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rails_follow_trich_codec_mapping() {
        // byte 0 = [+1, -1, 0, rsv] LSB-first -> 0b11_00_10_01
        let w = u32::from_le_bytes([0b1100_1001, 0, 0, 0]);
        assert_eq!(rails(w, 0), (true, false));
        assert_eq!(rails(w, 1), (false, true));
        assert_eq!(rails(w, 2), (false, false));
        assert_eq!(rails(w, 3), (true, true));
        assert_eq!(slot(w, 0), Slot::Pos);
        assert_eq!(slot(w, 1), Slot::Neg);
        assert_eq!(slot(w, 2), Slot::Zero);
        assert_eq!(slot(w, 3), Slot::Reserved);
    }

    #[test]
    fn byte_order_is_little_endian_lsb_first() {
        // trit 4 is the first slot of byte 1; trit 15 the last slot of byte 3
        let w = u32::from_le_bytes([0, 0b01, 0, 0b10 << 6]);
        assert_eq!(slot(w, 4), Slot::Pos);
        assert_eq!(slot(w, 15), Slot::Neg);
        for i in (0..TRITS_PER_WORD).filter(|i| *i != 4 && *i != 15) {
            assert_eq!(slot(w, i), Slot::Zero, "slot {i}");
        }
    }

    #[test]
    fn sync_word_is_all_reserved() {
        let w = u32::from_le_bytes([0xFF, 0xFF, 0, 0]);
        assert!((0..8).all(|i| slot(w, i) == Slot::Reserved));
    }

    #[test]
    fn aligned_len_rounds_down() {
        assert_eq!(aligned_len(0), 0);
        assert_eq!(aligned_len(3), 0);
        assert_eq!(aligned_len(4), 4);
        assert_eq!(aligned_len(1023), 1020);
    }

    #[test]
    fn pad_to_word_zero_fills_tail() {
        let mut buf = [0xAAu8; 8];
        assert_eq!(pad_to_word(&mut buf, 5), 8);
        assert_eq!(&buf[5..8], &[0, 0, 0]);
        assert_eq!(buf[4], 0xAA);
        assert_eq!(pad_to_word(&mut buf, 4), 4);
        assert_eq!(pad_to_word(&mut buf, 0), 0);
    }
}
