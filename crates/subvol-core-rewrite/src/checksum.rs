pub const BCH_CSUM_none: u32 = 0;
pub const BCH_CSUM_crc32c_nonzero: u32 = 1;
pub const BCH_CSUM_crc64_nonzero: u32 = 2;
pub const BCH_CSUM_chacha20_poly1305_80: u32 = 3;
pub const BCH_CSUM_chacha20_poly1305_128: u32 = 4;
pub const BCH_CSUM_crc32c: u32 = 5;
pub const BCH_CSUM_crc64: u32 = 6;
pub const BCH_CSUM_xxhash: u32 = 7;
pub const BCH_CSUM_NR: u32 = 8;

/// Matches bcachefs `bch2_checksum_mergeable()`.
pub const fn bch2_checksum_mergeable(type_: u32) -> bool {
    matches!(type_, BCH_CSUM_none | BCH_CSUM_crc32c | BCH_CSUM_crc64)
}

pub fn bch2_checksum_merge(
    type_: u32,
    mut a: crate::btree::bset::bch_csum,
    b: crate::btree::bset::bch_csum,
    mut b_len: usize,
) -> crate::btree::bset::bch_csum {
    assert!(bch2_checksum_mergeable(type_));
    let mut seed = a.lo;
    let zeroes = [0u8; 4096];
    while b_len != 0 {
        let len = b_len.min(zeroes.len());
        seed = match type_ {
            BCH_CSUM_none => 0,
            BCH_CSUM_crc32c => crc32c(seed as u32, &zeroes[..len]) as u64,
            BCH_CSUM_crc64 => crc64_be(seed, &zeroes[..len]),
            _ => unreachable!(),
        };
        b_len -= len;
    }
    a.lo = seed ^ b.lo;
    a.hi ^= b.hi;
    a
}

const CRC32C_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < table.len() {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0x82f6_3b78
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

const CRC64_TABLE: [u64; 256] = {
    let mut table = [0u64; 256];
    let mut i = 0;
    while i < table.len() {
        let mut crc = (i as u64) << 56;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & (1 << 63) != 0 {
                (crc << 1) ^ 0x42f0_e1eb_a9ea_3693
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

pub fn crc32c(mut crc: u32, data: &[u8]) -> u32 {
    for byte in data {
        crc = CRC32C_TABLE[((crc ^ *byte as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    crc
}

pub fn crc64_be(mut crc: u64, data: &[u8]) -> u64 {
    for byte in data {
        let table = ((crc >> 56) ^ *byte as u64) & 0xff;
        crc = CRC64_TABLE[table as usize] ^ (crc << 8);
    }
    crc
}

const PRIME64_1: u64 = 11_400_714_785_074_694_791;
const PRIME64_2: u64 = 14_029_467_366_897_019_727;
const PRIME64_3: u64 = 1_609_587_929_392_839_161;
const PRIME64_4: u64 = 9_650_029_242_287_828_579;
const PRIME64_5: u64 = 2_870_177_450_012_600_261;

fn xxh64_round(mut acc: u64, input: u64) -> u64 {
    acc = acc.wrapping_add(input.wrapping_mul(PRIME64_2));
    acc = acc.rotate_left(31);
    acc.wrapping_mul(PRIME64_1)
}

fn xxh64_merge_round(mut acc: u64, mut val: u64) -> u64 {
    val = xxh64_round(0, val);
    acc ^= val;
    acc.wrapping_mul(PRIME64_1).wrapping_add(PRIME64_4)
}

pub fn xxh64(input: &[u8], seed: u64) -> u64 {
    let mut offset = 0usize;
    let mut hash;

    if input.len() >= 32 {
        let limit = input.len() - 32;
        let mut v1 = seed.wrapping_add(PRIME64_1).wrapping_add(PRIME64_2);
        let mut v2 = seed.wrapping_add(PRIME64_2);
        let mut v3 = seed;
        let mut v4 = seed.wrapping_sub(PRIME64_1);
        loop {
            v1 = xxh64_round(
                v1,
                u64::from_le_bytes(input[offset..offset + 8].try_into().unwrap()),
            );
            offset += 8;
            v2 = xxh64_round(
                v2,
                u64::from_le_bytes(input[offset..offset + 8].try_into().unwrap()),
            );
            offset += 8;
            v3 = xxh64_round(
                v3,
                u64::from_le_bytes(input[offset..offset + 8].try_into().unwrap()),
            );
            offset += 8;
            v4 = xxh64_round(
                v4,
                u64::from_le_bytes(input[offset..offset + 8].try_into().unwrap()),
            );
            offset += 8;
            if offset > limit {
                break;
            }
        }
        hash = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
        hash = xxh64_merge_round(hash, v1);
        hash = xxh64_merge_round(hash, v2);
        hash = xxh64_merge_round(hash, v3);
        hash = xxh64_merge_round(hash, v4);
    } else {
        hash = seed.wrapping_add(PRIME64_5);
    }

    hash = hash.wrapping_add(input.len() as u64);
    while offset + 8 <= input.len() {
        let word = u64::from_le_bytes(input[offset..offset + 8].try_into().unwrap());
        hash ^= xxh64_round(0, word);
        hash = hash
            .rotate_left(27)
            .wrapping_mul(PRIME64_1)
            .wrapping_add(PRIME64_4);
        offset += 8;
    }
    if offset + 4 <= input.len() {
        let word = u32::from_le_bytes(input[offset..offset + 4].try_into().unwrap());
        hash ^= (word as u64).wrapping_mul(PRIME64_1);
        hash = hash
            .rotate_left(23)
            .wrapping_mul(PRIME64_2)
            .wrapping_add(PRIME64_3);
        offset += 4;
    }
    while offset < input.len() {
        hash ^= (input[offset] as u64).wrapping_mul(PRIME64_5);
        hash = hash.rotate_left(11).wrapping_mul(PRIME64_1);
        offset += 1;
    }

    hash ^= hash >> 33;
    hash = hash.wrapping_mul(PRIME64_2);
    hash ^= hash >> 29;
    hash = hash.wrapping_mul(PRIME64_3);
    hash ^ (hash >> 32)
}

pub fn bch2_checksum(type_: u32, data: &[u8]) -> crate::btree::bset::bch_csum {
    let value = match type_ {
        BCH_CSUM_none => 0,
        BCH_CSUM_crc32c_nonzero => crc32c(u32::MAX, data) as u64 ^ u32::MAX as u64,
        BCH_CSUM_crc64_nonzero => crc64_be(u64::MAX, data) ^ u64::MAX,
        BCH_CSUM_crc32c => crc32c(0, data) as u64,
        BCH_CSUM_crc64 => crc64_be(0, data),
        BCH_CSUM_xxhash => xxh64(data, 0),
        _ => 0,
    };
    crate::btree::bset::bch_csum { lo: value, hi: 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_mergeability_matches_bcachefs() {
        assert!(bch2_checksum_mergeable(BCH_CSUM_none));
        assert!(bch2_checksum_mergeable(BCH_CSUM_crc32c));
        assert!(bch2_checksum_mergeable(BCH_CSUM_crc64));
        assert!(!bch2_checksum_mergeable(BCH_CSUM_crc32c_nonzero));
        assert!(!bch2_checksum_mergeable(BCH_CSUM_crc64_nonzero));
        assert!(!bch2_checksum_mergeable(BCH_CSUM_xxhash));
        assert!(!bch2_checksum_mergeable(u32::MAX));
    }

    #[test]
    fn checksum_merge_matches_local_zero_fill_rule() {
        let a = crate::btree::bset::bch_csum { lo: 7, hi: 11 };
        let b = crate::btree::bset::bch_csum { lo: 13, hi: 17 };
        assert_eq!(bch2_checksum_merge(BCH_CSUM_none, a, b, 8193).lo, 13);
        assert_eq!(bch2_checksum_merge(BCH_CSUM_none, a, b, 8193).hi, 26);
        let expected = crc32c(7, &[0u8; 4]) as u64 ^ 13;
        assert_eq!(bch2_checksum_merge(BCH_CSUM_crc32c, a, b, 4).lo, expected);
    }

    #[test]
    fn local_checksum_vectors() {
        let data = b"123456789";
        assert_eq!(crc32c(0, data), 0x58e3_fa20);
        assert_eq!(crc32c(u32::MAX, data) ^ u32::MAX, 0xe306_9283);
        assert_eq!(crc64_be(0, data), 0x6c40_df5f_0b49_7347);
        assert_eq!(xxh64(b"", 0), 0xef46_db37_51d8_e999);
        assert_eq!(xxh64(data, 0), 0x8cb8_41db_40e6_ae83);
    }

    #[test]
    fn bch2_checksum_uses_local_seed_and_final_xor_rules() {
        let data = b"123456789";
        assert_eq!(bch2_checksum(BCH_CSUM_none, data).lo, 0);
        assert_eq!(bch2_checksum(BCH_CSUM_crc32c_nonzero, data).lo, 0xe306_9283);
        assert_eq!(
            bch2_checksum(BCH_CSUM_crc64, data).lo,
            0x6c40_df5f_0b49_7347
        );
        assert_eq!(
            bch2_checksum(BCH_CSUM_xxhash, data).lo,
            0x8cb8_41db_40e6_ae83
        );
    }
}
