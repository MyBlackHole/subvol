use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::btree::types::*;
use crate::errcode::BchResult;

pub fn bch2_trigger_stripe(
    _trans: *mut BtreeTrans,
    _btree_id: BtreeId,
    _old: (),
    _new: (),
    _flags: u64,
) -> BchResult<i32> {
    todo!()
}

pub fn bch2_trigger_extent(
    _trans: *mut BtreeTrans,
    _btree_id: BtreeId,
    _old: (),
    _new: (),
    _flags: u64,
) -> BchResult<i32> {
    todo!()
}

pub fn bch2_trigger_reservation(
    _trans: *mut BtreeTrans,
    _btree_id: BtreeId,
    _old: (),
    _new: (),
    _flags: u64,
) -> BchResult<i32> {
    todo!()
}
