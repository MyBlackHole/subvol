//! 配额管理 — bcachefs BTREE_ID_quotas 对齐
//!
//! ## 子模块
//!
//! - `types`：配额类型定义（BchQuota, BchQuotaCounter, 等）
//! - `ops`：配额 btree CRUD
//! - `account`：配额记账

pub mod account;
pub mod ops;
pub mod types;

#[cfg(test)]
use account::bch2_quota_account;
pub(crate) use account::bch2_quota_cur_get;
pub(crate) use ops::bch2_quota_check;
#[cfg(test)]
use ops::{bch2_quota_del, bch2_quota_get, bch2_quota_set};
pub use types::{
    qid_decode, qid_encode, BchQuota, BchQuotaCounter, BchQuotaCounters, BchQuotaType,
    BchSbQuotaCounter, BchSbQuotaType, QUOTA_VALUE_U64S,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bch_vol::BchVol;
    use crate::btree::key::{Bpos, BtreeEntry, KeyType};
    use crate::btree::BtreeId;

    /// 辅助函数：创建测试用 BchVol
    fn test_vol() -> BchVol {
        BchVol::test_trees()
    }

    /// 辅助函数：往 Subvolumes btree 插入子卷
    fn insert_subvol(vol: &BchVol, id: u32, cur_sectors: u64) {
        let sv = crate::subvol::BchSubvolume {
            flags: crate::subvol::BchSubvolumeFlags::empty(),
            snapshot: 0,
            inode: id as u64,
            creation_parent: 0,
            fs_path_parent: 0,
            otime_lo: 0,
            otime_hi: 0,
            size: 0,
            cur_sectors,
        };
        let pos = Bpos::new(id as u64, 0, 0);
        vol.insert_entry_raw(
            BtreeId::Subvolumes,
            BtreeEntry::raw(pos, KeyType::Normal, sv.to_bytes()),
            0,
        );
    }

    #[test]
    fn test_quota_set_and_get() {
        let vol = test_vol();

        // 设置配额：子卷 1，空间硬限制 10000 扇区，软限制 8000
        bch2_quota_set(
            &vol,
            BchQuotaType::Prj,
            1,
            BchQuotaCounters::Spc,
            10000,
            8000,
            0,
        );

        // 读取配额
        let quota = bch2_quota_get(&vol, BchQuotaType::Prj, 1);
        assert!(quota.is_some(), "quota should exist");
        let q = quota.unwrap();
        assert_eq!(q.c[BchQuotaCounters::Spc as usize].hardlimit, 10000);
        assert_eq!(q.c[BchQuotaCounters::Spc as usize].softlimit, 8000);
        assert_eq!(q.c[BchQuotaCounters::Ino as usize].hardlimit, 0); // 未设置
    }

    #[test]
    fn test_quota_get_nonexistent() {
        let vol = test_vol();
        let quota = bch2_quota_get(&vol, BchQuotaType::Prj, 999);
        assert!(quota.is_none(), "nonexistent quota should return None");
    }

    #[test]
    fn test_quota_del() {
        let vol = test_vol();

        // 设置后删除
        bch2_quota_set(
            &vol,
            BchQuotaType::Prj,
            2,
            BchQuotaCounters::Spc,
            5000,
            4000,
            0,
        );
        assert!(bch2_quota_get(&vol, BchQuotaType::Prj, 2).is_some());

        bch2_quota_del(&vol, BchQuotaType::Prj, 2, 0);
        assert!(bch2_quota_get(&vol, BchQuotaType::Prj, 2).is_none());
    }

    #[test]
    fn test_quota_check_under_limit() {
        let vol = test_vol();

        // 硬限制 1000 扇区，当前用量 500，需要 200 → OK
        bch2_quota_set(
            &vol,
            BchQuotaType::Prj,
            3,
            BchQuotaCounters::Spc,
            1000,
            800,
            0,
        );
        let result = bch2_quota_check(&vol, BchQuotaType::Prj, 3, BchQuotaCounters::Spc, 500, 200);
        assert!(result.is_ok(), "under limit should pass");
    }

    #[test]
    fn test_quota_check_exceeded() {
        let vol = test_vol();

        // 硬限制 1000 扇区，当前用量 900，需要 200 → 超限
        bch2_quota_set(
            &vol,
            BchQuotaType::Prj,
            4,
            BchQuotaCounters::Spc,
            1000,
            800,
            0,
        );
        let result = bch2_quota_check(&vol, BchQuotaType::Prj, 4, BchQuotaCounters::Spc, 900, 200);
        assert!(result.is_err(), "over limit should fail");
    }

    #[test]
    fn test_quota_check_no_limit() {
        let vol = test_vol();

        // hardlimit=0 表示不限制
        bch2_quota_set(&vol, BchQuotaType::Prj, 5, BchQuotaCounters::Spc, 0, 0, 0);
        let result = bch2_quota_check(
            &vol,
            BchQuotaType::Prj,
            5,
            BchQuotaCounters::Spc,
            u64::MAX - 1,
            1,
        );
        assert!(result.is_ok(), "hardlimit=0 should always pass");
    }

    #[test]
    fn test_quota_account_add() {
        let vol = test_vol();
        insert_subvol(&vol, 10, 100);

        assert_eq!(bch2_quota_cur_get(&vol, 10), 100);

        // +50 sector
        bch2_quota_account(&vol, 10, 50, 0);
        assert_eq!(bch2_quota_cur_get(&vol, 10), 150);
    }

    #[test]
    fn test_quota_account_sub() {
        let vol = test_vol();
        insert_subvol(&vol, 11, 200);

        assert_eq!(bch2_quota_cur_get(&vol, 11), 200);

        // -80 sectors
        bch2_quota_account(&vol, 11, -80, 0);
        assert_eq!(bch2_quota_cur_get(&vol, 11), 120);
    }

    #[test]
    fn test_quota_account_never_below_zero() {
        let vol = test_vol();
        insert_subvol(&vol, 12, 10);

        // -100 sectors (should floor at 0)
        bch2_quota_account(&vol, 12, -100, 0);
        assert_eq!(bch2_quota_cur_get(&vol, 12), 0);
    }

    #[test]
    fn test_qid_encode_decode() {
        // Prj type + id
        let encoded = qid_encode(BchQuotaType::Prj, 42);
        let (decoded_type, decoded_id) = qid_decode(encoded);
        assert_eq!(decoded_type, BchQuotaType::Prj);
        assert_eq!(decoded_id, 42);

        // Usr type
        let encoded = qid_encode(BchQuotaType::Usr, 1000);
        let (decoded_type, decoded_id) = qid_decode(encoded);
        assert_eq!(decoded_type, BchQuotaType::Usr);
        assert_eq!(decoded_id, 1000);
    }

    #[test]
    fn test_quota_btree_isolation() {
        let vol = test_vol();

        // 写入配额
        bch2_quota_set(
            &vol,
            BchQuotaType::Prj,
            99,
            BchQuotaCounters::Spc,
            9999,
            0,
            0,
        );

        // 验证只能通过 Quotas btree 访问
        let spc_pos = Bpos::new(
            qid_encode(BchQuotaType::Prj, 99),
            BchQuotaCounters::Spc as u64,
            0,
        );
        let in_quotas = vol.get_entry_raw(BtreeId::Quotas, spc_pos);
        assert!(in_quotas.is_some(), "entry must be in Quotas btree");

        // 不应在其他 btree 中
        let in_extents = vol.get_entry_raw(BtreeId::Extents, spc_pos);
        assert!(in_extents.is_none(), "entry must not leak to Extents btree");
    }

    #[test]
    fn test_quota_set_both_counters() {
        let vol = test_vol();

        // 同时设置空间和 inode 计数器
        bch2_quota_set(
            &vol,
            BchQuotaType::Prj,
            7,
            BchQuotaCounters::Spc,
            10000,
            8000,
            0,
        );
        bch2_quota_set(
            &vol,
            BchQuotaType::Prj,
            7,
            BchQuotaCounters::Ino,
            1000,
            500,
            0,
        );

        let quota = bch2_quota_get(&vol, BchQuotaType::Prj, 7).unwrap();
        assert_eq!(quota.c[BchQuotaCounters::Spc as usize].hardlimit, 10000);
        assert_eq!(quota.c[BchQuotaCounters::Spc as usize].softlimit, 8000);
        assert_eq!(quota.c[BchQuotaCounters::Ino as usize].hardlimit, 1000);
        assert_eq!(quota.c[BchQuotaCounters::Ino as usize].softlimit, 500);
    }

    #[test]
    fn test_quota_multiple_subvols_independent() {
        let vol = test_vol();

        // 两个子卷的配额互相独立
        bch2_quota_set(
            &vol,
            BchQuotaType::Prj,
            10,
            BchQuotaCounters::Spc,
            500,
            0,
            0,
        );
        bch2_quota_set(
            &vol,
            BchQuotaType::Prj,
            20,
            BchQuotaCounters::Spc,
            1000,
            0,
            0,
        );

        // 子卷 10 超限，20 正常
        assert!(
            bch2_quota_check(&vol, BchQuotaType::Prj, 10, BchQuotaCounters::Spc, 400, 200).is_err()
        );
        assert!(
            bch2_quota_check(&vol, BchQuotaType::Prj, 20, BchQuotaCounters::Spc, 400, 200).is_ok()
        );
    }
}
