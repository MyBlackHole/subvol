//! Quota btree 内部 CRUD 辅助。
//!
//! Key 编码：Bpos { inode: qid_encode(type, id), offset: counter_type, snapshot: 0 }
//! Value: BchQuota (2 × BchQuotaCounter)

use crate::alloc::quota::types::{
    qid_encode, BchQuota, BchQuotaCounter, BchQuotaCounters, BchQuotaType,
};
use crate::bch_vol::BchVol;
use crate::btree::key::{Bpos, BtreeEntry, KeyType, KeyValue};
use crate::btree::BtreeId;
use crate::types::AllocError;

/// 解析 key 返回 quota 值（序列化 BchQuota → 反序列化）
fn read_quota(vol: &BchVol, pos: Bpos) -> Option<BchQuota> {
    let raw = vol.get_entry_raw(BtreeId::Quotas, pos)?;
    match &raw.value {
        KeyValue::Raw(bytes) => bincode::deserialize(bytes).ok(),
        _ => None,
    }
}

/// 写入 quota 值（序列化 BchQuota → Raw bytes）
fn write_quota(vol: &BchVol, pos: Bpos, quota: &BchQuota, journal_seq: u64) {
    let bytes = bincode::serialize(quota).unwrap_or_default();
    let entry = BtreeEntry::raw(pos, KeyType::Normal, bytes);
    vol.insert_entry_raw(BtreeId::Quotas, entry, journal_seq);
}

/// 创建或更新配额的 subvol 内部辅助。
///
/// 本地 bcachefs 没有同名公开入口；设置路径由静态 `bch2_set_quota()` 和
/// `bch2_set_quota_trans()` 实现。
///
/// `qtype`: 配额类型（Usr/Grp/Prj）
/// `qid`: 用户/组/项目 ID
/// `ctype`: 计数器类型（Spc/Ino）
/// `hardlimit`: 硬限制（0 = 不限制）
/// `softlimit`: 软限制（0 = 不限制）
pub(crate) fn bch2_quota_set(
    vol: &BchVol,
    qtype: BchQuotaType,
    qid: u64,
    ctype: BchQuotaCounters,
    hardlimit: u64,
    softlimit: u64,
    journal_seq: u64,
) {
    let pos = Bpos::new(qid_encode(qtype, qid), ctype as u64, 0);
    let mut quota = read_quota(vol, pos).unwrap_or_default();
    quota.c[ctype as usize] = BchQuotaCounter {
        hardlimit,
        softlimit,
    };
    write_quota(vol, pos, &quota, journal_seq);
}

/// 读取完整配额的 subvol 内部辅助。
///
/// 本地 bcachefs 没有同名公开入口；读取路径由静态 `bch2_get_quota()` 实现。
///
/// 返回 `Some(BchQuota)` 当配额条目存在，否则 `None`。
pub(crate) fn bch2_quota_get(vol: &BchVol, qtype: BchQuotaType, qid: u64) -> Option<BchQuota> {
    // 空间计数器和 inode 计数器分别查询，合并为完整 BchQuota
    let spc_pos = Bpos::new(qid_encode(qtype, qid), BchQuotaCounters::Spc as u64, 0);
    let ino_pos = Bpos::new(qid_encode(qtype, qid), BchQuotaCounters::Ino as u64, 0);

    let spc = read_quota(vol, spc_pos).map(|q| q.c[BchQuotaCounters::Spc as usize]);
    let ino = read_quota(vol, ino_pos).map(|q| q.c[BchQuotaCounters::Ino as usize]);

    if spc.is_none() && ino.is_none() {
        return None;
    }

    Some(BchQuota {
        c: [spc.unwrap_or_default(), ino.unwrap_or_default()],
    })
}

/// 删除配额的 subvol 内部辅助；本地 bcachefs 没有同名公开入口。
///
/// 注意：Btree::delete() 使用 BtreeKey（丢失 inode 字段），
/// 因此配额删除使用 whiteout 插入代替 delete()。
pub(crate) fn bch2_quota_del(vol: &BchVol, qtype: BchQuotaType, qid: u64, journal_seq: u64) {
    let spc_pos = Bpos::new(qid_encode(qtype, qid), BchQuotaCounters::Spc as u64, 0);
    let ino_pos = Bpos::new(qid_encode(qtype, qid), BchQuotaCounters::Ino as u64, 0);
    vol.insert_entry_raw(
        BtreeId::Quotas,
        BtreeEntry::new(spc_pos, KeyType::Whiteout, KeyValue::Raw(vec![])),
        journal_seq,
    );
    vol.insert_entry_raw(
        BtreeId::Quotas,
        BtreeEntry::new(ino_pos, KeyType::Whiteout, KeyValue::Raw(vec![])),
        journal_seq,
    );
}

/// 检查配额是否允许分配的 subvol 内部辅助。
///
/// 本地 bcachefs 没有同名公开入口；限制检查位于 `bch2_quota_acct()` 的内部路径。
///
/// 检查 `(cur_sectors + sectors_needed) <= hardlimit`。
/// 返回 `Ok(())` 通过或 `Err(AllocError::QuotaExceeded)` 超限。
/// 当前仅实现空间计数器检查。
pub(crate) fn bch2_quota_check(
    vol: &BchVol,
    qtype: BchQuotaType,
    qid: u64,
    ctype: BchQuotaCounters,
    cur_sectors: u64,
    sectors_needed: u64,
) -> Result<(), AllocError> {
    let pos = Bpos::new(qid_encode(qtype, qid), ctype as u64, 0);
    let quota = read_quota(vol, pos).ok_or(AllocError::QuotaExceeded("no quota entry".into()))?;

    let counter = quota.c[ctype as usize];
    if counter.hardlimit == 0 {
        return Ok(());
    }

    let new_usage = cur_sectors.saturating_add(sectors_needed);
    if new_usage > counter.hardlimit {
        return Err(AllocError::QuotaExceeded(format!(
            "quota exceeded: {} + {} > {}",
            cur_sectors, sectors_needed, counter.hardlimit
        )));
    }
    Ok(())
}
