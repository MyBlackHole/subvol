//! Alloc btree — `bch_alloc_v4` persistent values.

use crate::alloc::bucket::{BchDataType, BUCKET_GC_GEN_MAX};
use crate::types::StorageError;

pub const BCH_ALLOC_V4_U64S_V0: usize = 6;
pub const BCH_ALLOC_V4_U64S: usize = 8;
const BCH_BACKPOINTER_U64S: usize = 5;
const LRU_TIME_MAX: u64 = (1_u64 << 48) - 1;

/// Local bcachefs `fs/alloc/format.h:82-100`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C, align(8))]
pub struct BchAllocV4 {
    pub journal_seq_nonempty: u64,
    pub flags: u32,
    pub gen: u8,
    pub oldest_gen: u8,
    pub data_type: u8,
    pub stripe_redundancy_obsolete: u8,
    pub dirty_sectors: u32,
    pub cached_sectors: u32,
    pub io_time: [u64; 2],
    pub stripe_refcount: u32,
    pub nr_external_backpointers: u32,
    pub journal_seq_empty: u64,
    pub stripe_sectors: u32,
    pub pad: u32,
}

/// Existing Rust alloc-btree boundary name; the represented value is exactly v4.
pub type BchAllocEntry = BchAllocV4;

pub(crate) const BCH_ALLOC_V4_ZERO: BchAllocV4 = BchAllocV4 {
    journal_seq_nonempty: 0,
    flags: 0,
    gen: 0,
    oldest_gen: 0,
    data_type: BchDataType::Free as u8,
    stripe_redundancy_obsolete: 0,
    dirty_sectors: 0,
    cached_sectors: 0,
    io_time: [0; 2],
    stripe_refcount: 0,
    nr_external_backpointers: 0,
    journal_seq_empty: 0,
    stripe_sectors: 0,
    pad: 0,
};

const fn backpointers_start(flags: u32) -> usize {
    ((flags >> 2) & 0x3f) as usize
}

const fn nr_backpointers(flags: u32) -> usize {
    ((flags >> 8) & 0x3f) as usize
}

const fn set_backpointers_start(flags: u32, value: u32) -> u32 {
    (flags & !(0x3f << 2)) | ((value & 0x3f) << 2)
}

const fn set_nr_backpointers(flags: u32, value: u32) -> u32 {
    (flags & !(0x3f << 8)) | ((value & 0x3f) << 8)
}

/// Local bcachefs `alloc_v4_u64s_noerror()` (`fs/alloc/background.h:209`).
const fn alloc_v4_u64s_noerror(a: &BchAllocV4) -> usize {
    let start = backpointers_start(a.flags);
    (if start != 0 {
        start
    } else {
        BCH_ALLOC_V4_U64S_V0
    }) + nr_backpointers(a.flags) * BCH_BACKPOINTER_U64S
}

/// Local bcachefs `alloc_data_type()` (`fs/alloc/background.h:124`).
const fn alloc_data_type(a: &BchAllocV4, data_type: u8) -> u8 {
    if a.stripe_refcount != 0 {
        return if data_type == BchDataType::Parity as u8 {
            data_type
        } else {
            BchDataType::Stripe as u8
        };
    }
    if a.stripe_sectors.wrapping_add(a.dirty_sectors) != 0 {
        return if data_type == BchDataType::Cached as u8 || data_type == BchDataType::Stripe as u8 {
            BchDataType::User as u8
        } else {
            data_type
        };
    }
    if a.cached_sectors != 0 {
        return BchDataType::Cached as u8;
    }
    if data_type == BchDataType::NeedDiscard as u8 {
        return BchDataType::NeedDiscard as u8;
    }
    if a.gen.wrapping_sub(a.oldest_gen) >= BUCKET_GC_GEN_MAX {
        BchDataType::NeedGcGens as u8
    } else {
        BchDataType::Free as u8
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn decode_fixed(bytes: &[u8; BCH_ALLOC_V4_U64S * 8]) -> BchAllocV4 {
    BchAllocV4 {
        journal_seq_nonempty: read_u64(bytes, 0),
        flags: read_u32(bytes, 8),
        gen: bytes[12],
        oldest_gen: bytes[13],
        data_type: bytes[14],
        stripe_redundancy_obsolete: bytes[15],
        dirty_sectors: read_u32(bytes, 16),
        cached_sectors: read_u32(bytes, 20),
        io_time: [read_u64(bytes, 24), read_u64(bytes, 32)],
        stripe_refcount: read_u32(bytes, 40),
        nr_external_backpointers: read_u32(bytes, 44),
        journal_seq_empty: read_u64(bytes, 48),
        stripe_sectors: read_u32(bytes, 56),
        pad: read_u32(bytes, 60),
    }
}

fn encode_fixed(a: &BchAllocV4) -> [u8; BCH_ALLOC_V4_U64S * 8] {
    let mut bytes = [0_u8; BCH_ALLOC_V4_U64S * 8];
    bytes[0..8].copy_from_slice(&a.journal_seq_nonempty.to_le_bytes());
    bytes[8..12].copy_from_slice(&a.flags.to_le_bytes());
    bytes[12] = a.gen;
    bytes[13] = a.oldest_gen;
    bytes[14] = a.data_type;
    bytes[15] = a.stripe_redundancy_obsolete;
    bytes[16..20].copy_from_slice(&a.dirty_sectors.to_le_bytes());
    bytes[20..24].copy_from_slice(&a.cached_sectors.to_le_bytes());
    bytes[24..32].copy_from_slice(&a.io_time[0].to_le_bytes());
    bytes[32..40].copy_from_slice(&a.io_time[1].to_le_bytes());
    bytes[40..44].copy_from_slice(&a.stripe_refcount.to_le_bytes());
    bytes[44..48].copy_from_slice(&a.nr_external_backpointers.to_le_bytes());
    bytes[48..56].copy_from_slice(&a.journal_seq_empty.to_le_bytes());
    bytes[56..60].copy_from_slice(&a.stripe_sectors.to_le_bytes());
    bytes[60..64].copy_from_slice(&a.pad.to_le_bytes());
    bytes
}

/// Local bcachefs `bch2_alloc_v4_validate()` (`fs/alloc/background.c:698`).
fn validate_alloc_v4(a: &BchAllocV4, value_u64s: usize) -> Result<(), StorageError> {
    if alloc_v4_u64s_noerror(a) > value_u64s {
        return Err(StorageError::InvalidData(format!(
            "alloc_v4_val_size_bad: {} > {}",
            alloc_v4_u64s_noerror(a),
            value_u64s
        )));
    }

    if backpointers_start(a.flags) == 0 && nr_backpointers(a.flags) != 0 {
        return Err(StorageError::InvalidData(
            "alloc_v4_backpointers_start_bad".into(),
        ));
    }

    if alloc_data_type(a, a.data_type) != a.data_type {
        return Err(StorageError::InvalidData(format!(
            "alloc_key_data_type_bad: got {} should be {}",
            a.data_type,
            alloc_data_type(a, a.data_type)
        )));
    }

    for i in 0..2 {
        if a.io_time[i] > LRU_TIME_MAX {
            return Err(StorageError::InvalidData(format!(
                "alloc_key_io_time_bad: io_time[{}] {} > {}",
                i, a.io_time[i], LRU_TIME_MAX
            )));
        }
    }

    let stripe_sectors =
        if backpointers_start(a.flags) * 8 > std::mem::offset_of!(BchAllocV4, stripe_sectors) {
            a.stripe_sectors
        } else {
            0
        };

    match a.data_type {
        x if x == BchDataType::Free as u8
            || x == BchDataType::NeedGcGens as u8
            || x == BchDataType::NeedDiscard as u8 =>
        {
            if stripe_sectors != 0
                || a.dirty_sectors != 0
                || a.cached_sectors != 0
                || a.stripe_refcount != 0
            {
                return Err(StorageError::InvalidData(
                    "alloc_key_empty_but_have_data".into(),
                ));
            }
        }
        x if x == BchDataType::Sb as u8
            || x == BchDataType::Journal as u8
            || x == BchDataType::Btree as u8
            || x == BchDataType::User as u8
            || x == BchDataType::Parity as u8 =>
        {
            if a.dirty_sectors == 0 && stripe_sectors == 0 {
                return Err(StorageError::InvalidData(
                    "alloc_key_dirty_sectors_0".into(),
                ));
            }
        }
        x if x == BchDataType::Cached as u8 => {
            if a.cached_sectors == 0
                || a.dirty_sectors != 0
                || stripe_sectors != 0
                || a.stripe_refcount != 0
            {
                return Err(StorageError::InvalidData(
                    "alloc_key_cached_inconsistency".into(),
                ));
            }
        }
        x if x == BchDataType::Stripe as u8 => {}
        _ => {}
    }

    Ok(())
}

/// Encode a newly written alloc value in current v4 form.
pub fn serialize_alloc_entry(entry: &BchAllocV4) -> Vec<u8> {
    let mut entry = *entry;
    entry.flags = set_backpointers_start(entry.flags, BCH_ALLOC_V4_U64S as u32);
    entry.flags = set_nr_backpointers(entry.flags, 0);
    encode_fixed(&entry).to_vec()
}

/// Decode and make mutable using the local bcachefs v4-only conversion path.
pub fn deserialize_alloc_entry(bytes: &[u8]) -> Result<BchAllocV4, StorageError> {
    if bytes.len() < BCH_ALLOC_V4_U64S_V0 * 8 || bytes.len() % 8 != 0 {
        return Err(StorageError::InvalidData(format!(
            "alloc_v4_val_size_bad: {} bytes",
            bytes.len()
        )));
    }

    let mut fixed = [0_u8; BCH_ALLOC_V4_U64S * 8];
    let copy = bytes.len().min(fixed.len());
    fixed[..copy].copy_from_slice(&bytes[..copy]);
    let a = decode_fixed(&fixed);
    validate_alloc_v4(&a, bytes.len() / 8)?;

    // Local bcachefs `__bch2_alloc_to_v4_mut()` v4 branch, in exact order.
    let src = {
        let start = backpointers_start(a.flags);
        if start != 0 {
            start
        } else {
            BCH_ALLOC_V4_U64S_V0
        }
    };
    let flags = set_backpointers_start(read_u32(&fixed, 8), BCH_ALLOC_V4_U64S as u32);
    fixed[8..12].copy_from_slice(&flags.to_le_bytes());
    if src < BCH_ALLOC_V4_U64S {
        fixed[src * 8..BCH_ALLOC_V4_U64S * 8].fill(0);
    }
    let flags = set_nr_backpointers(read_u32(&fixed, 8), 0);
    fixed[8..12].copy_from_slice(&flags.to_le_bytes());

    Ok(decode_fixed(&fixed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_v4_layout_matches_local_bcachefs() {
        assert_eq!(std::mem::size_of::<BchAllocV4>(), 64);
        assert_eq!(std::mem::align_of::<BchAllocV4>(), 8);
        assert_eq!(std::mem::offset_of!(BchAllocV4, journal_seq_nonempty), 0);
        assert_eq!(std::mem::offset_of!(BchAllocV4, flags), 8);
        assert_eq!(std::mem::offset_of!(BchAllocV4, gen), 12);
        assert_eq!(std::mem::offset_of!(BchAllocV4, oldest_gen), 13);
        assert_eq!(std::mem::offset_of!(BchAllocV4, data_type), 14);
        assert_eq!(
            std::mem::offset_of!(BchAllocV4, stripe_redundancy_obsolete),
            15
        );
        assert_eq!(std::mem::offset_of!(BchAllocV4, dirty_sectors), 16);
        assert_eq!(std::mem::offset_of!(BchAllocV4, cached_sectors), 20);
        assert_eq!(std::mem::offset_of!(BchAllocV4, io_time), 24);
        assert_eq!(std::mem::offset_of!(BchAllocV4, stripe_refcount), 40);
        assert_eq!(
            std::mem::offset_of!(BchAllocV4, nr_external_backpointers),
            44
        );
        assert_eq!(std::mem::offset_of!(BchAllocV4, journal_seq_empty), 48);
        assert_eq!(std::mem::offset_of!(BchAllocV4, stripe_sectors), 56);
        assert_eq!(std::mem::offset_of!(BchAllocV4, pad), 60);
    }

    #[test]
    fn alloc_v4_current_roundtrip() {
        let mut a = BCH_ALLOC_V4_ZERO;
        a.data_type = BchDataType::Stripe as u8;
        a.dirty_sectors = 8;
        a.io_time = [11, 12];
        a.stripe_refcount = u16::MAX as u32 + 1;
        let bytes = serialize_alloc_entry(&a);
        assert_eq!(bytes.len(), 64);
        let restored = deserialize_alloc_entry(&bytes).unwrap();
        assert_eq!(restored.dirty_sectors, 8);
        assert_eq!(restored.io_time, [11, 12]);
        assert_eq!(restored.stripe_refcount, u16::MAX as u32 + 1);
        assert_eq!(backpointers_start(restored.flags), 8);
        assert_eq!(nr_backpointers(restored.flags), 0);
    }

    #[test]
    fn alloc_v4_v0_short_value_is_zero_padded() {
        let mut a = BCH_ALLOC_V4_ZERO;
        a.flags = set_backpointers_start(a.flags, 6);
        a.gen = 7;
        let bytes = encode_fixed(&a);
        let restored = deserialize_alloc_entry(&bytes[..48]).unwrap();
        assert_eq!(restored.gen, 7);
        assert_eq!(restored.journal_seq_empty, 0);
        assert_eq!(restored.stripe_sectors, 0);
        assert_eq!(backpointers_start(restored.flags), 8);
    }

    #[test]
    fn alloc_v4_seven_u64_value_preserves_journal_seq_empty() {
        let mut a = BCH_ALLOC_V4_ZERO;
        a.flags = set_backpointers_start(a.flags, 7);
        a.journal_seq_empty = 91;
        a.stripe_sectors = 123;
        let bytes = encode_fixed(&a);
        let restored = deserialize_alloc_entry(&bytes[..56]).unwrap();
        assert_eq!(restored.journal_seq_empty, 91);
        assert_eq!(restored.stripe_sectors, 0);
        assert_eq!(backpointers_start(restored.flags), 8);
    }

    #[test]
    fn alloc_v4_inline_backpointer_is_accepted_then_removed() {
        let mut a = BCH_ALLOC_V4_ZERO;
        a.flags = set_backpointers_start(a.flags, 8);
        a.flags = set_nr_backpointers(a.flags, 1);
        let mut bytes = encode_fixed(&a).to_vec();
        bytes.extend_from_slice(&[0x5a; 40]);
        let restored = deserialize_alloc_entry(&bytes).unwrap();
        assert_eq!(backpointers_start(restored.flags), 8);
        assert_eq!(nr_backpointers(restored.flags), 0);
        assert_eq!(serialize_alloc_entry(&restored).len(), 64);
    }

    #[test]
    fn alloc_v4_rejects_old_bincode_payload() {
        let bytes = bincode::serialize(&(1_u64, 2_u32)).unwrap();
        assert!(deserialize_alloc_entry(&bytes).is_err());
    }

    #[test]
    fn alloc_v4_validation_keeps_local_error_order() {
        let mut a = BCH_ALLOC_V4_ZERO;
        a.flags = set_nr_backpointers(a.flags, 1);
        let bytes = encode_fixed(&a);
        let err = deserialize_alloc_entry(&bytes).unwrap_err().to_string();
        assert!(err.contains("alloc_v4_val_size_bad"));

        let mut bytes = bytes.to_vec();
        bytes.extend_from_slice(&[0; 24]);
        let err = deserialize_alloc_entry(&bytes).unwrap_err().to_string();
        assert!(err.contains("alloc_v4_backpointers_start_bad"));
    }

    #[test]
    fn alloc_v4_validation_rejects_data_type_then_io_time() {
        let mut a = BCH_ALLOC_V4_ZERO;
        a.flags = set_backpointers_start(a.flags, 8);
        a.data_type = BchDataType::User as u8;
        let err = deserialize_alloc_entry(&encode_fixed(&a))
            .unwrap_err()
            .to_string();
        assert!(err.contains("alloc_key_data_type_bad"));

        a.data_type = BchDataType::Cached as u8;
        a.cached_sectors = 1;
        a.io_time[0] = LRU_TIME_MAX + 1;
        let err = deserialize_alloc_entry(&encode_fixed(&a))
            .unwrap_err()
            .to_string();
        assert!(err.contains("alloc_key_io_time_bad"));
    }

    #[test]
    fn alloc_v4_validation_rejects_empty_type_with_data() {
        let mut a = BCH_ALLOC_V4_ZERO;
        a.flags = set_backpointers_start(a.flags, 8);
        a.dirty_sectors = 1;
        let err = deserialize_alloc_entry(&encode_fixed(&a))
            .unwrap_err()
            .to_string();
        assert!(err.contains("alloc_key_empty_but_have_data"));
    }
}
