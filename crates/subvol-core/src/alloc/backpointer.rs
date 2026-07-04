// Backpointer btree — bcachefs 对齐

use serde::{Deserialize, Serialize};

use crate::alloc::BchDataType;
use crate::alloc::SECTORS_PER_BLOCK;
use crate::btree::key::ExtentPtr;
use crate::btree::key::{Bpos, BtreeKey, KeyType};
use crate::btree::{BtreeId, BtreeTrans};
use crate::types::StorageError;

// ── backpointer flags BITMASK (bcachefs_format.h:596-598) ──
/// BACKPOINTER_RECONCILE_PHYS(bp) — 物理 reconcile work ID (bit 0-1)
pub const fn backpointer_reconcile_phys(bp: &BchBackpointer) -> u32 {
    (bp.flags >> 0) & 0x3
}
pub fn set_backpointer_reconcile_phys(bp: &mut BchBackpointer, v: u32) {
    bp.flags = (bp.flags & !0x3) | ((v & 0x3) << 0);
}

/// BACKPOINTER_ERASURE_CODED(bp) — 是否 EC 编码 (bit 2)
pub const fn backpointer_erasure_coded(bp: &BchBackpointer) -> u32 {
    (bp.flags >> 2) & 0x1
}
pub fn set_backpointer_erasure_coded(bp: &mut BchBackpointer, v: bool) {
    bp.flags = (bp.flags & !(1 << 2)) | ((v as u32) << 2);
}

/// BACKPOINTER_STRIPE_PTR(bp) — 是否 stripe 指针 (bit 3)
pub const fn backpointer_stripe_ptr(bp: &BchBackpointer) -> u32 {
    (bp.flags >> 3) & 0x1
}
pub fn set_backpointer_stripe_ptr(bp: &mut BchBackpointer, v: bool) {
    bp.flags = (bp.flags & !(1 << 3)) | ((v as u32) << 3);
}

/// bcachefs 对齐: 根据 backpointer flags 选择 backpointer btree
///
/// stripe ptr → BTREE_ID_stripe_backpointers，subvol 暂未实现 → fallback
pub fn backpointer_btree(bp: &BchBackpointer) -> BtreeId {
    if backpointer_stripe_ptr(bp) != 0 {
        BtreeId::Backpointers // 简化: subvol 无 EC/stripe
    } else {
        BtreeId::Backpointers
    }
}

/// bcachefs struct bch_backpointer (bcachefs_format.h:568-577)
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[repr(C, align(8))]
pub struct BchBackpointer {
    pub btree_id: u8,
    pub level: u8,
    pub data_type: u8,
    pub bucket_gen: u8,
    pub flags: u32,
    pub bucket_len: u32,
    pub pos: Bpos,
}

impl BchBackpointer {
    pub const fn default() -> Self {
        BchBackpointer {
            btree_id: 0,
            level: 0,
            data_type: 0,
            bucket_gen: 0,
            flags: 0,
            bucket_len: 0,
            pos: Bpos::MIN,
        }
    }
}

/// bcachefs 对齐: bch2_bucket_backpointer_mod (backpointers.c)
///
/// insert=true → 写入 Normal entry（创建 backpointer）
/// insert=false → 写入 Deleted entry（删除 backpointer）
#[allow(dead_code)]
pub fn bch2_bucket_backpointer_mod(
    trans: &mut BtreeTrans<'_>,
    btree_id: BtreeId,
    level: u8,
    extent_key: &BtreeKey,
    ptr: &ExtentPtr,
    sectors: u32,
    insert: bool,
) -> Result<(), StorageError> {
    // bcachefs (backpointers.h:166-175) stores the physical pointer offset
    // in 512-byte sectors. ExtentPtr uses block units in subvol, so convert
    // exactly once at the btree key boundary.
    let bp_pos = Bpos::new(ptr.dev as u64, ptr.offset * SECTORS_PER_BLOCK, 0);

    let mut bp_value = BchBackpointer {
        btree_id: btree_id as u8,
        level,
        data_type: if ptr.cached {
            BchDataType::Cached as u8
        } else {
            BchDataType::User as u8
        },
        bucket_gen: ptr.gen,
        flags: 0,
        bucket_len: sectors,
        pos: extent_key.to_bpos(),
    };
    set_backpointer_erasure_coded(&mut bp_value, false);
    set_backpointer_stripe_ptr(&mut bp_value, false);
    // subvol 无 reconcilation → BACKPOINTER_RECONCILE_PHYS 保持 0

    let bp_btree = backpointer_btree(&bp_value);

    let bytes = bincode::serialize(&bp_value)
        .map_err(|e| StorageError::InvalidData(format!("backpointer serialize: {}", e)))?;

    let key = BtreeKey::from_bpos(
        bp_pos,
        if insert {
            KeyType::Normal
        } else {
            KeyType::Deleted
        },
    );
    if insert {
        trans.bch2_trans_update_raw(bp_btree, 0, false, key, bytes, 0);
    } else {
        trans.bch2_trans_delete(bp_btree, 0, false, key, 0);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backpointer_size() {
        // Bpos(24B) + flags(4B) + bucket_len(4B) + 4*u8(4B) + align padding = 40
        assert_eq!(std::mem::size_of::<BchBackpointer>(), 40);
    }

    #[test]
    fn test_backpointer_serialize_roundtrip() {
        let bp = BchBackpointer {
            btree_id: 0,
            level: 1,
            data_type: 2,
            bucket_gen: 5,
            flags: 1,
            bucket_len: 8,
            pos: Bpos::new(100, 200, 3),
        };
        let bytes = bincode::serialize(&bp).unwrap();
        let bp2: BchBackpointer = bincode::deserialize(&bytes).unwrap();
        assert_eq!(bp.btree_id, bp2.btree_id);
        assert_eq!(bp.level, bp2.level);
        assert_eq!(bp.data_type, bp2.data_type);
        assert_eq!(bp.bucket_gen, bp2.bucket_gen);
        assert_eq!(bp.flags, bp2.flags);
        assert_eq!(bp.bucket_len, bp2.bucket_len);
        assert_eq!(bp.pos.inode, bp2.pos.inode);
        assert_eq!(bp.pos.offset, bp2.pos.offset);
        assert_eq!(bp.pos.snapshot, bp2.pos.snapshot);
    }
}
