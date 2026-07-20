use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::opts::Printbuf;
use crate::alloc::buckets::*;
use crate::btree::types::*;
use crate::errcode::*;

pub fn swab40(x: u64) -> u64 {
    (((x & 0x00000000ff) << 32)
        | ((x & 0x000000ff00) << 16)
        | ((x & 0x0000ff0000) << 0)
        | ((x & 0x00ff000000) >> 16)
        | ((x & 0xff00000000) >> 32))
}

pub fn bp_pos_to_bucket(ca: &BchDev, bp_pos: Bpos) -> Bpos {
    let bucket_sector = bp_pos.offset >> ca.fs().sb.extent_bp_shift;
    Bpos::pos(bp_pos.inode, sector_to_bucket(ca, bucket_sector))
}

pub fn bp_pos_to_bucket_and_offset(
    ca: &BchDev,
    bp_pos: Bpos,
    bucket_offset: &mut u32,
) -> Bpos {
    let bucket_sector = bp_pos.offset >> ca.fs().sb.extent_bp_shift;
    Bpos::pos(bp_pos.inode, sector_to_bucket_and_offset(ca, bucket_sector, bucket_offset))
}

pub fn bp_pos_to_bucket_nodev_noerror(c: &BchFs, bp_pos: Bpos, bucket: &mut Bpos) -> bool {
    if let Some(ca) = c.devs.get(bp_pos.inode as usize).and_then(|d| d.as_ref()) {
        *bucket = bp_pos_to_bucket(ca, bp_pos);
        true
    } else {
        false
    }
}

pub fn bucket_pos_to_bp_noerror(ca: &BchDev, bucket: Bpos, bucket_offset: u64) -> Bpos {
    Bpos::pos(
        bucket.inode,
        (bucket_to_sector(ca, bucket.offset) << ca.fs().sb.extent_bp_shift) + bucket_offset,
    )
}

pub fn bucket_pos_to_bp(ca: &BchDev, bucket: Bpos, bucket_offset: u64) -> Bpos {
    bucket_pos_to_bp_noerror(ca, bucket, bucket_offset)
}

pub fn bucket_pos_to_bp_start(ca: &BchDev, bucket: Bpos) -> Bpos {
    bucket_pos_to_bp(ca, bucket, 0)
}

pub fn bucket_pos_to_bp_end(ca: &BchDev, bucket: Bpos) -> Bpos {
    let successor = Bpos::pos(bucket.inode, bucket.offset + 1);
    let end = bucket_pos_to_bp(ca, successor, 0);
    let mut prev = end;
    if prev.offset > 0 {
        prev.offset -= 1;
    }
    prev
}

pub fn backpointer_btree(bp: &BchBackpointer) -> BtreeId {
    if bp.btree_id & 0x80 != 0 {
        BtreeId::StripeBackpointers
    } else {
        BtreeId::Backpointers
    }
}

pub fn bch2_backpointer_validate(
    _c: &BchFs,
    _k: (),
    _ctx: &(),
) -> BchResult<()> {
    Ok(())
}

pub fn bch2_backpointer_to_text(_buf: &mut Printbuf, _c: &BchFs, _k: ()) {
}

pub fn bch2_backpointer_swab(_c: &BchFs, _k: ()) {
}

pub fn bch2_bucket_backpointer_mod_nowritebuffer(
    _trans: &mut BtreeTrans,
    _orig_k: (),
    _bp_i: &BkeyI,
    _insert: bool,
) -> BchResult<()> {
    Ok(())
}

pub fn bch2_bucket_backpointer_mod(
    _trans: &mut BtreeTrans,
    _orig_k: (),
    _bp_i: &BkeyI,
    _insert: bool,
) -> BchResult<()> {
    Ok(())
}

pub fn bch2_bkey_ptr_data_type(
    _k: (),
    _p: (),
    _entry: (),
) -> BchDataType {
    BchDataType::User
}

pub fn bch2_extent_ptr_to_bp_pos(_c: &BchFs, _k: (), _p: ()) -> Bpos {
    Bpos::ZERO
}

pub fn bch2_extent_ptr_to_bp(
    _c: &BchFs,
    _btree_id: u8,
    _level: u8,
    _k: (),
    _p: (),
    _entry: (),
    _bp: &mut BkeyI,
) {
}

pub fn bch2_backpointer_get_key(
    _trans: &mut BtreeTrans,
    _bp: (),
    _iter: &mut BtreeIter,
    _level: u32,
    _flush: &mut (),
) -> () {
}

pub fn bch2_backpointer_get_node(
    _trans: &mut BtreeTrans,
    _bp: (),
    _iter: &mut BtreeIter,
    _flush: &mut (),
) {
}

pub fn bch2_check_bucket_backpointer_mismatch(
    _trans: &mut BtreeTrans,
    _ca: &BchDev,
    _bucket: u64,
    _invalidate: bool,
    _flush: &mut (),
) -> BchResult<()> {
    Ok(())
}

pub fn bch2_check_btree_backpointers(_c: &BchFs) -> BchResult<()> {
    Ok(())
}

pub fn bch2_check_extents_to_backpointers(_c: &BchFs) -> BchResult<()> {
    Ok(())
}

pub fn bch2_check_backpointers_to_extents(_c: &BchFs) -> BchResult<()> {
    Ok(())
}

pub fn bch2_bucket_bitmap_resize(
    _ca: &mut BchDev,
    _bitmap: &mut (),
    _old_nbuckets: u64,
    _new_nbuckets: u64,
) -> BchResult<()> {
    Ok(())
}

pub fn bch2_bucket_bitmap_free(_bitmap: &mut ()) {
}
