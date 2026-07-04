use crate::alloc::{alloc_freespace_bucket_idx, AllocRequest, BchAllocator};
use crate::block_device::BchDev;
use crate::btree::{Bpos, BtreeId, BtreeKey, BtreeTrans, KeyType};
use crate::types::StorageError;
use crate::BchVol;

/// 每个 btree_bitmap 的 freespace 遍历位置（对应 bcachefs `dev_alloc_cursor`）。
///
/// bcachefs `foreground.c:376`:
/// ```c
/// struct dev_alloc_cursor {
///     unsigned per_btree_bitmap[BTREE_BITMAP_NR];
/// };
/// ```
/// BTREE_BITMAP_NR = 3（ANY / YES / NO）。
/// 每次分配后 cursor 前移，下次继续扫描，避免反复从起始位置遍历。
#[derive(Debug, Default)]
pub struct AllocCursor {
    /// per-btree-bitmap cursor offsets
    pub per_btree_bitmap: [u64; 3],
}

impl AllocCursor {
    pub fn new() -> Self {
        Self {
            per_btree_bitmap: [0; 3],
        }
    }

    pub(crate) fn bitmap_idx(filter: &crate::alloc::BtreeBitmapFilter) -> usize {
        match filter {
            crate::alloc::BtreeBitmapFilter::Any => 0,
            crate::alloc::BtreeBitmapFilter::Yes => 1,
            crate::alloc::BtreeBitmapFilter::No => 2,
        }
    }
}

/// 从 freespace btree 扫描并分配一个空闲 bucket。
///
/// 对应 bcachefs `bch2_bucket_alloc_freelist()`（`foreground.c:438-508`）。
/// 从当前 cursor 位置开始遍历 Freespace btree，
/// 对每个 KEY_TYPE_Normal 的 key（代表空闲 extent）检查内部 bucket 是否可分配，
/// 返回第一个满足 `may_alloc_bucket_journal_seq` 的 bucket。
///
/// 返回 `(group_id, local_bucket_idx)` 或 `None`（无可用 bucket）。
///
/// `preferred_group`：如果指定，只返回属于该 group 的 bucket。
/// 在 bcachefs 中，group hint 通过 cursor + 遍历顺序隐式实现，
/// subvol 通过显式 group 过滤来保持与 hint 逻辑的兼容。
pub fn bch2_bucket_alloc_freelist(
    vol: &BchVol,
    alloc: &BchAllocator,
    ca: &BchDev,
    req: &AllocRequest,
    cursor: &mut AllocCursor,
    preferred_group: Option<u32>,
) -> Result<Option<(u32, u32)>, StorageError> {
    let freespace = vol.btree(BtreeId::Freespace);
    let bitmap = AllocCursor::bitmap_idx(&req.btree_bitmap);
    let start_offset = cursor.per_btree_bitmap[bitmap];
    let flushed_seq = vol
        .journal_ref()
        .last_seq_ondisk
        .load(std::sync::atomic::Ordering::Acquire);

    // 从 cursor 位置开始顺序扫描 freespace btree
    let target = BtreeKey::from_bpos(
        Bpos::new(ca.dev_idx as u64, start_offset, 0),
        KeyType::Normal,
    );
    // 使用 Btree 内部方法获取 root 引用和 cache 引用
    let (root_ref, _) = freespace.root_and_cache();
    let mut trans = BtreeTrans::new_ro(vol);
    let iter = trans.bch2_trans_get_iter(root_ref, &target, false, BtreeId::Freespace);

    let mut scanned = 0u64;
    loop {
        let entry = match iter.peek_entry() {
            Some(e) => e,
            None => {
                cursor.per_btree_bitmap[bitmap] = 0;
                return Ok(None);
            }
        };

        if entry.pos.inode > ca.dev_idx as u64 {
            cursor.per_btree_bitmap[bitmap] = 0;
            return Ok(None);
        }

        if entry.key_type == KeyType::Set && entry.pos.inode == ca.dev_idx as u64 {
            let bucket_idx = alloc_freespace_bucket_idx(entry.pos);
            if let Some((grp_id, local_idx)) =
                alloc.try_alloc_freespace_bucket(ca, bucket_idx, flushed_seq)
            {
                if preferred_group.is_none() || preferred_group == Some(grp_id) {
                    cursor.per_btree_bitmap[bitmap] = bucket_idx + 1;
                    return Ok(Some((grp_id, local_idx)));
                }
            }
        }

        if !iter.advance() {
            cursor.per_btree_bitmap[bitmap] = 0;
            return Ok(None);
        }

        scanned += 1;
        if scanned > 1_000_000 {
            cursor.per_btree_bitmap[bitmap] = 0;
            return Ok(None);
        }
    }
}
