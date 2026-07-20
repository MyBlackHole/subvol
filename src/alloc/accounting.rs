use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::opts::Printbuf;
use crate::btree::bkey::bkey_val_u64s;
use crate::btree::types::*;
use crate::errcode::*;

pub fn u64s_neg(v: &mut [u64]) {
    for vi in v.iter_mut() {
        *vi = vi.wrapping_neg();
    }
}

pub fn accounting_counters(k: &Bkey) -> u32 {
    bkey_val_u64s(k) as u32 - (std::mem::size_of::<BchAccounting>() / 8) as u32 + BCH_ACCOUNTING_MAX_COUNTERS as u32
}

pub fn bpos_to_disk_accounting_pos(acc: &mut DiskAccountingPos, p: &Bpos) {
    let src = p as *const Bpos as *const u8;
    let dst = acc as *mut DiskAccountingPos as *mut u8;
    unsafe {
        std::ptr::copy_nonoverlapping(src, dst, std::mem::size_of::<Bpos>());
    }
}

pub fn fs_usage_data_type_to_base(fs_usage: &mut BchFsUsageBase, data_type: BchDataType, sectors: i64) {
    match data_type {
        BchDataType::Btree => fs_usage.btree = (fs_usage.btree as i64 + sectors) as u64,
        BchDataType::User | BchDataType::Parity => {
            fs_usage.data = (fs_usage.data as i64 + sectors) as u64;
        }
        BchDataType::Cached => fs_usage.cached = (fs_usage.cached as i64 + sectors) as u64,
        _ => {}
    }
}

pub fn accounting_mem_read(
    _c: &BchFs,
    _p: Bpos,
    _v: &mut [u64],
    _nr: u32,
) {
}

pub fn __bch2_accounting_maybe_kill(_c: &BchFs, _pos: Bpos) {
}

pub fn bch2_disk_accounting_mod(
    _trans: &mut BtreeTrans,
    _pos: &DiskAccountingPos,
    _v: &[i64],
    _nr: u32,
    _gc: bool,
) -> BchResult<()> {
    Ok(())
}

pub fn bch2_mod_dev_cached_sectors(
    _trans: &mut BtreeTrans,
    _dev: u32,
    _sectors: i64,
    _gc: bool,
) -> BchResult<()> {
    Ok(())
}

pub fn bch2_accounting_validate(
    _c: &BchFs,
    _k: BkeySC,
    _ctx: &BkeyValidateContext,
) -> BchResult<()> {
    Ok(())
}

pub fn bch2_accounting_to_text(
    _buf: &mut Printbuf,
    _c: &BchFs,
    _k: BkeySC,
) {
}

pub fn bch2_accounting_swab(_c: &BchFs, _k: BkeyS) {
}

pub fn bch2_accounting_update_sb(_trans: &mut BtreeTrans) -> BchResult<()> {
    Ok(())
}

pub fn bch2_accounting_mem_insert(
    _c: &BchFs,
    _a: BkeySCAccounting,
    _mode: BchAccountingMode,
) -> BchResult<()> {
    Ok(())
}

pub fn bch2_accounting_mem_insert_locked(
    _c: &BchFs,
    _a: BkeySCAccounting,
    _mode: BchAccountingMode,
) -> BchResult<()> {
    Ok(())
}

pub fn bch2_accounting_mem_gc(_c: &BchFs) {
}

pub fn bch2_fs_replicas_usage_read(_c: &BchFs, _buf: &mut Vec<u8>) -> BchResult<()> {
    Ok(())
}

pub fn bch2_fs_accounting_read(
    _c: &BchFs,
    _buf: &mut Vec<u8>,
    _flags: u32,
) -> BchResult<()> {
    Ok(())
}

pub fn bch2_fs_accounting_read_key(
    _trans: &mut BtreeTrans,
    _pos: &DiskAccountingPos,
    _v: &mut [u64],
    _nr: u32,
) -> BchResult<()> {
    Ok(())
}

pub fn bch2_gc_accounting_start(_c: &BchFs) -> BchResult<()> {
    Ok(())
}

pub fn bch2_gc_accounting_done(_c: &BchFs) -> BchResult<()> {
    Ok(())
}

pub fn bch2_accounting_read(_c: &BchFs) -> BchResult<()> {
    Ok(())
}

pub fn bch2_dev_usage_remove(_c: &BchFs, _ca: &BchDev) -> BchResult<()> {
    Ok(())
}

pub fn bch2_dev_usage_init(_ca: &mut BchDev, _rw: bool) -> BchResult<()> {
    Ok(())
}

pub fn bch2_verify_accounting_clean(_c: &BchFs) {
}

pub fn bch2_accounting_gc_free(_c: &BchFs) {
}

pub fn bch2_fs_accounting_exit(_c: &BchFs) {
}

/* Type aliases for readability */
pub type BkeySC = (); 
pub type BkeyS = ();
pub type BkeySCAccounting = ();
pub type BkeyValidateContext = ();
pub type BchAccountingMode = u8;

pub const BCH_ACCOUNTING_NORMAL: u8 = 0;
pub const BCH_ACCOUNTING_GC: u8 = 1;
pub const BCH_ACCOUNTING_READ: u8 = 2;
