use arrow::buffer::{BooleanBuffer, Buffer};
use std::ops::Range;

/// An arrow-compatible mutable bitset implementation
///
/// Note: This currently operates on individual bytes at a time
/// it could be optimised to instead operate on usize blocks
#[derive(Debug, Default, Clone)]
pub struct BitSet {
    /// The underlying data
    ///
    /// Data is stored in the least significant bit of a byte first
    buffer: Vec<u8>,

    /// The length of this mask in bits
    len: usize,
}

impl BitSet {
    /// Creates a new BitSet
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a new BitSet with `count` unset bits.
    pub fn with_size(count: usize) -> Self {
        let mut bitset = Self::default();
        bitset.append_unset(count);
        bitset
    }

    /// Reserve space for `count` further bits
    pub fn reserve(&mut self, count: usize) {
        let new_buf_len = (self.len + count + 7) >> 3;
        self.buffer.reserve(new_buf_len);
    }

    /// Appends `count` unset bits
    pub fn append_unset(&mut self, count: usize) {
        self.len += count;
        let new_buf_len = (self.len + 7) >> 3;
        self.buffer.resize(new_buf_len, 0);
    }

    /// Appends `count` set bits
    pub fn append_set(&mut self, count: usize) {
        let new_len = self.len + count;
        let new_buf_len = (new_len + 7) >> 3;

        let skew = self.len & 7;
        if skew != 0 {
            *self.buffer.last_mut().unwrap() |= 0xFF << skew;
        }

        self.buffer.resize(new_buf_len, 0xFF);

        let rem = new_len & 7;
        if rem != 0 {
            *self.buffer.last_mut().unwrap() &= (1 << rem) - 1;
        }

        self.len = new_len;
    }

    /// Truncates the bitset to the provided length
    pub fn truncate(&mut self, len: usize) {
        let new_buf_len = (len + 7) >> 3;
        self.buffer.truncate(new_buf_len);
        let overrun = len & 7;
        if overrun > 0 {
            *self.buffer.last_mut().unwrap() &= (1 << overrun) - 1;
        }
        self.len = len;
    }

    /// Extends this [`BitSet`] by the context of `other`
    pub fn extend_from(&mut self, other: &BitSet) {
        self.append_bits(other.len, &other.buffer)
    }

    /// Extends this [`BitSet`] by `range` elements in `other`
    pub fn extend_from_range(&mut self, other: &BitSet, range: Range<usize>) {
        let count = range.end - range.start;
        if count == 0 {
            return;
        }

        let start_byte = range.start >> 3;
        let end_byte = (range.end + 7) >> 3;
        let skew = range.start & 7;

        // `append_bits` requires the provided `to_set` to be byte aligned, therefore
        // if the range being copied is not byte aligned we must first append
        // the leading bits to reach a byte boundary
        if skew == 0 {
            // No skew can simply append bytes directly
            self.append_bits(count, &other.buffer[start_byte..end_byte])
        } else if start_byte + 1 == end_byte {
            // Append bits from single byte
            self.append_bits(count, &[other.buffer[start_byte] >> skew])
        } else {
            // Append trailing bits from first byte to reach byte boundary, then append
            // bits from the remaining byte-aligned mask
            let offset = 8 - skew;
            self.append_bits(offset, &[other.buffer[start_byte] >> skew]);
            self.append_bits(count - offset, &other.buffer[(start_byte + 1)..end_byte]);
        }
    }

    /// Appends `count` boolean values from the slice of packed bits
    pub fn append_bits(&mut self, count: usize, to_set: &[u8]) {
        assert_eq!((count + 7) >> 3, to_set.len());

        let new_len = self.len + count;
        let new_buf_len = (new_len + 7) >> 3;
        self.buffer.reserve(new_buf_len - self.buffer.len());

        let whole_bytes = count >> 3;
        let overrun = count & 7;

        let skew = self.len & 7;
        if skew == 0 {
            self.buffer.extend_from_slice(&to_set[..whole_bytes]);
            if overrun > 0 {
                let masked = to_set[whole_bytes] & ((1 << overrun) - 1);
                self.buffer.push(masked)
            }

            self.len = new_len;
            debug_assert_eq!(self.buffer.len(), new_buf_len);
            return;
        }

        for to_set_byte in &to_set[..whole_bytes] {
            let low = *to_set_byte << skew;
            let high = *to_set_byte >> (8 - skew);

            *self.buffer.last_mut().unwrap() |= low;
            self.buffer.push(high);
        }

        if overrun > 0 {
            let masked = to_set[whole_bytes] & ((1 << overrun) - 1);
            let low = masked << skew;
            *self.buffer.last_mut().unwrap() |= low;

            if overrun > 8 - skew {
                let high = masked >> (8 - skew);
                self.buffer.push(high)
            }
        }

        self.len = new_len;
        debug_assert_eq!(self.buffer.len(), new_buf_len);
    }

    /// Sets a given bit
    pub fn set(&mut self, idx: usize) {
        assert!(idx <= self.len);

        let byte_idx = idx >> 3;
        let bit_idx = idx & 7;
        self.buffer[byte_idx] |= 1 << bit_idx;
    }

    /// Returns if the given index is set
    pub fn get(&self, idx: usize) -> bool {
        assert!(idx <= self.len);

        let byte_idx = idx >> 3;
        let bit_idx = idx & 7;
        (self.buffer[byte_idx] >> bit_idx) & 1 != 0
    }

    /// Converts this BitSet to a buffer compatible with arrows boolean encoding
    pub fn to_arrow(&self) -> BooleanBuffer {
        let offset = 0;
        BooleanBuffer::new(Buffer::from(&self.buffer), offset, self.len)
    }

    /// Returns the number of values stored in the bitset
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns if this bitset is empty
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns the number of bytes used by this bitset
    pub fn byte_len(&self) -> usize {
        self.buffer.len()
    }

    /// Return the raw packed bytes used by this bitset
    pub fn bytes(&self) -> &[u8] {
        &self.buffer
    }

    /// Return `true` if all bits in the [`BitSet`] are currently set.
    pub fn is_all_set(&self) -> bool {
        // An empty bitmap has no set bits.
        if self.len == 0 {
            return false;
        }

        // Check all the bytes in the bitmap that have all their bits considered
        // part of the bit set.
        let full_blocks = (self.len / 8).saturating_sub(1);
        if !self.buffer.iter().take(full_blocks).all(|&v| v == u8::MAX) {
            return false;
        }

        // Check the last byte of the bitmap that may only be partially part of
        // the bit set, and therefore need masking to check only the relevant
        // bits.
        let mask = match self.len % 8 {
            1..=8 => !(0xFF << (self.len % 8)), // LSB mask
            0 => 0xFF,
            _ => unreachable!(),
        };
        *self.buffer.last().unwrap() == mask
    }

    /// Return `true` if all bits in the [`BitSet`] are currently unset.
    pub fn is_all_unset(&self) -> bool {
        self.buffer.iter().all(|&v| v == 0)
    }
}

/// Returns an iterator over set bit positions in increasing order
pub fn iter_set_positions(bytes: &[u8]) -> impl Iterator<Item = usize> + '_ {
    iter_set_positions_with_offset(bytes, 0)
}

/// Returns an iterator over set bit positions in increasing order starting
/// at the provided bit offset
pub fn iter_set_positions_with_offset(
    bytes: &[u8],
    offset: usize,
) -> impl Iterator<Item = usize> + '_ {
    let mut byte_idx = offset >> 3;
    let mut in_progress = bytes.get(byte_idx).cloned().unwrap_or(0);

    let skew = offset & 7;
    in_progress &= 0xFF << skew;

    std::iter::from_fn(move || loop {
        if in_progress != 0 {
            let bit_pos = in_progress.trailing_zeros();
            in_progress ^= 1 << bit_pos;
            return Some((byte_idx << 3) + (bit_pos as usize));
        }
        byte_idx += 1;
        in_progress = *bytes.get(byte_idx)?;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::BooleanBufferBuilder;
    use rand::prelude::*;
    use rand::rngs::OsRng;

    /// Computes a compacted representation of a given bool array
    fn compact_bools(bools: &[bool]) -> Vec<u8> {
        bools
            .chunks(8)
            .map(|x| {
                let mut collect = 0_u8;
                for (idx, set) in x.iter().enumerate() {
                    if *set {
                        collect |= 1 << idx
                    }
                }
                collect
            })
            .collect()
    }

    fn iter_set_bools(bools: &[bool]) -> impl Iterator<Item = usize> + '_ {
        bools.iter().enumerate().filter_map(|(x, y)| y.then(|| x))
    }

    #[test]
    fn test_compact_bools() {
        let bools = &[
            false, false, true, true, false, false, true, false, true, false,
        ];
        let collected = compact_bools(bools);
        let indexes: Vec<_> = iter_set_bools(bools).collect();
        assert_eq!(collected.as_slice(), &[0b01001100, 0b00000001]);
        assert_eq!(indexes.as_slice(), &[2, 3, 6, 8])
    }

    #[test]
    fn test_bit_mask() {
        let mut mask = BitSet::new();

        mask.append_bits(8, &[0b11111111]);
        let d1 = mask.buffer.clone();

        mask.append_bits(3, &[0b01010010]);
        let d2 = mask.buffer.clone();

        mask.append_bits(5, &[0b00010100]);
        let d3 = mask.buffer.clone();

        mask.append_bits(2, &[0b11110010]);
        let d4 = mask.buffer.clone();

        mask.append_bits(15, &[0b11011010, 0b01010101]);
        let d5 = mask.buffer.clone();

        assert_eq!(d1.as_slice(), &[0b11111111]);
        assert_eq!(d2.as_slice(), &[0b11111111, 0b00000010]);
        assert_eq!(d3.as_slice(), &[0b11111111, 0b10100010]);
        assert_eq!(d4.as_slice(), &[0b11111111, 0b10100010, 0b00000010]);
        assert_eq!(
            d5.as_slice(),
            &[0b11111111, 0b10100010, 0b01101010, 0b01010111, 0b00000001]
        );

        assert!(mask.get(0));
        assert!(!mask.get(8));
        assert!(mask.get(9));
        assert!(mask.get(19));
    }

    fn make_rng() -> StdRng {
        let seed = OsRng.next_u64();
        println!("Seed: {seed}");
        StdRng::seed_from_u64(seed)
    }

    #[test]
    fn test_bit_mask_all_set() {
        let mut mask = BitSet::new();
        let mut all_bools = vec![];
        let mut rng = make_rng();

        for _ in 0..100 {
            let mask_length = (rng.next_u32() % 50) as usize;
            let bools: Vec<_> = std::iter::repeat(true).take(mask_length).collect();

            let collected = compact_bools(&bools);
            mask.append_bits(mask_length, &collected);
            all_bools.extend_from_slice(&bools);
        }

        let collected = compact_bools(&all_bools);
        assert_eq!(mask.buffer, collected);

        let expected_indexes: Vec<_> = iter_set_bools(&all_bools).collect();
        let actual_indexes: Vec<_> = iter_set_positions(&mask.buffer).collect();
        assert_eq!(expected_indexes, actual_indexes);
    }

    #[test]
    fn test_bit_mask_fuzz() {
        let mut mask = BitSet::new();
        let mut all_bools = vec![];
        let mut rng = make_rng();

        for _ in 0..100 {
            let mask_length = (rng.next_u32() % 50) as usize;
            let bools: Vec<_> = std::iter::from_fn(|| Some(rng.next_u32() & 1 == 0))
                .take(mask_length)
                .collect();

            let collected = compact_bools(&bools);
            mask.append_bits(mask_length, &collected);
            all_bools.extend_from_slice(&bools);
        }

        let collected = compact_bools(&all_bools);
        assert_eq!(mask.buffer, collected);

        let expected_indexes: Vec<_> = iter_set_bools(&all_bools).collect();
        let actual_indexes: Vec<_> = iter_set_positions(&mask.buffer).collect();
        assert_eq!(expected_indexes, actual_indexes);

        if !all_bools.is_empty() {
            for _ in 0..10 {
                let offset = rng.next_u32() as usize % all_bools.len();

                let expected_indexes: Vec<_> = iter_set_bools(&all_bools[offset..])
                    .map(|x| x + offset)
                    .collect();

                let actual_indexes: Vec<_> =
                    iter_set_positions_with_offset(&mask.buffer, offset).collect();

                assert_eq!(expected_indexes, actual_indexes);
            }
        }

        for index in actual_indexes {
            assert!(mask.get(index));
        }
    }

    #[test]
    fn test_append_fuzz() {
        let mut mask = BitSet::new();
        let mut all_bools = vec![];
        let mut rng = make_rng();

        for _ in 0..100 {
            let len = (rng.next_u32() % 32) as usize;
            let set = rng.next_u32() & 1 == 0;

            match set {
                true => mask.append_set(len),
                false => mask.append_unset(len),
            }

            all_bools.extend(std::iter::repeat(set).take(len));

            let collected = compact_bools(&all_bools);
            assert_eq!(mask.buffer, collected);
        }
    }

    #[test]
    fn test_truncate_fuzz() {
        let mut mask = BitSet::new();
        let mut all_bools = vec![];
        let mut rng = make_rng();

        for _ in 0..100 {
            let mask_length = (rng.next_u32() % 32) as usize;
            let bools: Vec<_> = std::iter::from_fn(|| Some(rng.next_u32() & 1 == 0))
                .take(mask_length)
                .collect();

            let collected = compact_bools(&bools);
            mask.append_bits(mask_length, &collected);
            all_bools.extend_from_slice(&bools);

            if !all_bools.is_empty() {
                let truncate = rng.next_u32() as usize % all_bools.len();
                mask.truncate(truncate);
                all_bools.truncate(truncate);
            }

            let collected = compact_bools(&all_bools);
            assert_eq!(mask.buffer, collected);
        }
    }

    #[test]
    fn test_extend_range_fuzz() {
        let mut rng = make_rng();
        let src_len = 32;
        let src_bools: Vec<_> = std::iter::from_fn(|| Some(rng.next_u32() & 1 == 0))
            .take(src_len)
            .collect();

        let mut src_mask = BitSet::new();
        src_mask.append_bits(src_len, &compact_bools(&src_bools));

        let mut dst_bools = Vec::new();
        let mut dst_mask = BitSet::new();

        for _ in 0..100 {
            let a = rng.next_u32() as usize % src_len;
            let b = rng.next_u32() as usize % src_len;

            let start = a.min(b);
            let end = a.max(b);

            dst_bools.extend_from_slice(&src_bools[start..end]);
            dst_mask.extend_from_range(&src_mask, start..end);

            let collected = compact_bools(&dst_bools);
            assert_eq!(dst_mask.buffer, collected);
        }
    }

    #[test]
    fn test_arrow_compat() {
        let bools = &[
            false, false, true, true, false, false, true, false, true, false, false, true,
        ];

        let mut builder = BooleanBufferBuilder::new(bools.len());
        builder.append_slice(bools);
        let buffer = builder.finish();

        let collected = compact_bools(bools);
        let mut mask = BitSet::new();
        mask.append_bits(bools.len(), &collected);
        let mask_buffer = mask.to_arrow();

        assert_eq!(collected.as_slice(), buffer.values());
        assert_eq!(buffer.values(), mask_buffer.into_inner().as_slice());
    }

    #[test]
    #[should_panic = "idx <= self.len"]
    fn test_bitset_set_get_out_of_bounds() {
        let mut v = BitSet::with_size(4);

        // The bitset is of length 4, which is backed by a single byte with 8
        // bits of storage capacity.
        //
        // Accessing bits past the 4 the bitset "contains" should not succeed.

        v.get(5);
        v.set(5);
    }

    #[test]
    fn test_all_set_unset() {
        for i in 1..100 {
            let mut v = BitSet::new();
            v.append_set(i);
            assert!(v.is_all_set());
            assert!(!v.is_all_unset());
        }
    }

    #[test]
    fn test_all_set_unset_multi_byte() {
        let mut v = BitSet::new();

        // Bitmap is composed of entirely set bits.
        v.append_set(100);
        assert!(v.is_all_set());
        assert!(!v.is_all_unset());

        // Now the bitmap is neither composed of entirely set, nor entirely
        // unset bits.
        v.append_unset(1);
        assert!(!v.is_all_set());
        assert!(!v.is_all_unset());

        let mut v = BitSet::new();

        // Bitmap is composed of entirely unset bits.
        v.append_unset(100);
        assert!(!v.is_all_set());
        assert!(v.is_all_unset());

        // And once again, it is neither all set, nor all unset.
        v.append_set(1);
        assert!(!v.is_all_set());
        assert!(!v.is_all_unset());
    }

    #[test]
    fn test_all_set_unset_single_byte() {
        let mut v = BitSet::new();

        // Bitmap is composed of entirely set bits.
        v.append_set(2);
        assert!(v.is_all_set());
        assert!(!v.is_all_unset());

        // Now the bitmap is neither composed of entirely set, nor entirely
        // unset bits.
        v.append_unset(1);
        assert!(!v.is_all_set());
        assert!(!v.is_all_unset());

        let mut v = BitSet::new();

        // Bitmap is composed of entirely unset bits.
        v.append_unset(2);
        assert!(!v.is_all_set());
        assert!(v.is_all_unset());

        // And once again, it is neither all set, nor all unset.
        v.append_set(1);
        assert!(!v.is_all_set());
        assert!(!v.is_all_unset());
    }

    #[test]
    fn test_all_set_unset_empty() {
        let v = BitSet::new();
        assert!(!v.is_all_set());
        assert!(v.is_all_unset());
    }
}
