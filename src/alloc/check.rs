use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::btree::types::*;
use crate::errcode::*;

pub fn bch2_check_alloc_info(_c: &BchFs) -> BchResult<()> {
    Ok(())
}

pub fn bch2_check_alloc_to_lru_refs(_c: &BchFs) -> BchResult<()> {
    Ok(())
}

pub fn bch2_dev_freespace_init(_c: &BchFs, _ca: &BchDev, _start: u64, _end: u64) -> BchResult<()> {
    Ok(())
}

pub fn bch2_fs_freespace_init(_c: &BchFs) -> BchResult<()> {
    Ok(())
}
