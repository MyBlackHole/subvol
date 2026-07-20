use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::opts::Printbuf;
use crate::alloc::buckets::*;
use crate::btree::types::*;
use crate::errcode::*;

pub fn replicas_entry_sort(_e: &mut BchReplicasEntryV1) {
}

pub fn replicas_entry_to_text(_buf: &mut Printbuf, _e: &BchReplicasEntryV1) {
}

pub fn replicas_entry_validate(
    _e: &BchReplicasEntryV1,
    _c: &BchFs,
    _err: &mut Printbuf,
) -> BchResult<()> {
    Ok(())
}

pub fn cpu_replicas_to_text(_buf: &mut Printbuf, _cpu: &BchReplicasCpu) {
}

pub fn devlist_to_replicas(
    _e: &mut BchReplicasEntryV1,
    _data_type: BchDataType,
    _devs: &[u8],
) {
}

pub fn replicas_marked_locked(
    _c: &BchFs,
    _e: &BchReplicasEntryV1,
) -> bool {
    false
}

pub fn replicas_marked(
    _c: &BchFs,
    _e: &BchReplicasEntryV1,
) -> bool {
    false
}

pub fn mark_replicas(
    _c: &BchFs,
    _e: &BchReplicasEntryV1,
) -> BchResult<()> {
    Ok(())
}

pub fn bkey_to_replicas(_c: &BchFs, _e: &mut BchReplicasEntryV1, _k: ()) {
}

pub fn replicas_entry_cached(e: &mut BchReplicasEntryV1, dev: u32) {
    e.data_type = BchDataType::Cached as u8;
    e.nr_devs = 1;
    e.nr_required = 1;
    e.devs[0] = dev as u8;
}

pub fn can_read_replicas_with_devs(
    _c: &BchFs,
    _devs: &BchDevsMask,
    _e: &BchReplicasEntryV1,
    _nr_required: u32,
    _err: &mut Printbuf,
) -> bool {
    false
}

pub fn can_read_fs_with_devs(
    _c: &BchFs,
    _devs: &BchDevsMask,
    _nr_required: u32,
    _err: &mut Printbuf,
) -> bool {
    false
}

pub fn can_write_fs_with_devs(
    _c: &BchFs,
    _devs: BchDevsMask,
    _nr_required: u32,
    _err: &mut Printbuf,
) -> bool {
    false
}

pub fn sb_has_journal(_sb: &BchSb) -> bool {
    false
}

pub fn sb_dev_has_data(_sb: &BchSb, _dev: u32) -> u32 {
    0
}

pub fn dev_has_data(_c: &BchFs, _ca: &BchDev) -> u32 {
    0
}

pub fn replicas_entry_put_many(_c: &BchFs, _e: &BchReplicasEntryV1, _nr: u32) {
}

pub fn replicas_entry_put(c: &BchFs, e: &BchReplicasEntryV1) {
    replicas_entry_put_many(c, e, 1);
}

pub fn replicas_entry_get(
    _c: &BchFs,
    _e: &BchReplicasEntryV1,
) -> BchResult<()> {
    Ok(())
}

pub fn replicas_entry_kill(_c: &BchFs, _e: &BchReplicasEntryV1) {
}

pub fn replicas_gc_reffed(_c: &BchFs) -> BchResult<()> {
    Ok(())
}

pub fn replicas_gc_accounted(_c: &BchFs) -> BchResult<()> {
    Ok(())
}

pub fn replicas_entry_has_dev(r: &BchReplicasEntryV1, dev: u32) -> bool {
    for i in 0..r.nr_devs as usize {
        if r.devs[i] == dev as u8 {
            return true;
        }
    }
    false
}

pub fn replicas_entry_eq(l: &BchReplicasEntryV1, r: &BchReplicasEntryV1) -> bool {
    if l.nr_devs != r.nr_devs {
        return false;
    }
    for i in 0..l.nr_devs as usize {
        if l.devs[i] != r.devs[i] {
            return false;
        }
    }
    l.data_type == r.data_type
}

pub fn sb_replicas_to_cpu_replicas(_c: &BchFs) -> BchResult<()> {
    Ok(())
}

pub fn verify_replicas_refs_clean(_c: &BchFs) {
}

pub fn fs_replicas_exit(_c: &BchFs) {
}
