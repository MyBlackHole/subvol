//! 配额记账内部辅助。
//!
//! 每次分配/释放 block 时更新子卷的 cur_sectors 计数。

use crate::bch_vol::BchVol;
use crate::btree::key::{Bpos, BtreeEntry, KeyType, KeyValue};
use crate::btree::BtreeId;

/// 更新子卷的 cur_sectors（扇区 ±delta）
///
/// 读 Subvolumes btree → 更新 cur_sectors → 写回。
/// 本地 bcachefs 没有 `bch2_quota_account()`；其公开记账入口是
/// `bch2_quota_acct()`，因此该兼容辅助仅限 crate 内部使用。
pub(crate) fn bch2_quota_account(vol: &BchVol, subvol_id: u32, delta: i64, journal_seq: u64) {
    let pos = Bpos::new(subvol_id as u64, 0, 0);
    let raw = vol.get_entry_raw(BtreeId::Subvolumes, pos);

    let (sv, _new_cur) = match raw {
        Some(entry) => {
            if let KeyValue::Raw(bytes) = &entry.value {
                if let Ok(mut sv) = crate::subvol::BchSubvolume::from_bytes(bytes) {
                    let new_cur = (sv.cur_sectors as i64).saturating_add(delta).max(0) as u64;
                    sv.cur_sectors = new_cur;
                    let cur = sv.cur_sectors;
                    (sv, cur)
                } else {
                    return;
                }
            } else {
                return;
            }
        }
        None => return,
    };

    let bytes = sv.to_bytes();
    vol.insert_entry_raw(
        BtreeId::Subvolumes,
        BtreeEntry::raw(pos, KeyType::Normal, bytes),
        journal_seq,
    );
}

/// 获取子卷的 cur_sectors（从 Subvolumes btree 读取）
pub(crate) fn bch2_quota_cur_get(vol: &BchVol, subvol_id: u32) -> u64 {
    let pos = Bpos::new(subvol_id as u64, 0, 0);
    let raw = vol.get_entry_raw(BtreeId::Subvolumes, pos);
    match raw {
        Some(entry) => {
            if let KeyValue::Raw(bytes) = &entry.value {
                if let Ok(sv) = crate::subvol::BchSubvolume::from_bytes(bytes) {
                    return sv.cur_sectors;
                }
            }
            0
        }
        None => 0,
    }
}
