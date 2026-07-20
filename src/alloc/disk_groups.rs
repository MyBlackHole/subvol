use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::opts::Printbuf;
use crate::alloc::buckets::*;
use crate::btree::types::*;
use crate::errcode::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetType {
    Null,
    Dev,
    Group,
}

pub struct Target {
    pub type_: TargetType,
    pub dev: u32,
    pub group: u32,
}

pub const TARGET_DEV_START: u16 = 1;
pub const TARGET_GROUP_START: u16 = 256 + TARGET_DEV_START;

pub fn dev_to_target(dev: u32) -> u16 {
    TARGET_DEV_START + dev as u16
}

pub fn group_to_target(group: u32) -> u16 {
    TARGET_GROUP_START + group as u16
}

pub fn target_decode(target: u32) -> Target {
    if target >= TARGET_GROUP_START as u32 {
        Target {
            type_: TargetType::Group,
            dev: 0,
            group: target - TARGET_GROUP_START as u32,
        }
    } else if target >= TARGET_DEV_START as u32 {
        Target {
            type_: TargetType::Dev,
            dev: target - TARGET_DEV_START as u32,
            group: 0,
        }
    } else {
        Target {
            type_: TargetType::Null,
            dev: 0,
            group: 0,
        }
    }
}

pub fn target_to_mask(_c: &BchFs, _target: u32) -> Option<BchDevsMask> {
    Some(BchDevsMask::new())
}

pub fn target_rw_devs(c: &BchFs, data_type: BchDataType, target: u16) -> BchDevsMask {
    let mut devs = BchDevsMask::new();
    if let Some(t) = target_to_mask(c, target as u32) {
        devs.d[0] = c.allocator.rw_devs[data_type as usize].d[0] & t.d[0];
    }
    devs
}

pub fn target_accepts_data(c: &BchFs, data_type: BchDataType, target: u16) -> bool {
    let rw_devs = target_rw_devs(c, data_type, target);
    rw_devs.d[0] != 0
}

pub fn dev_in_target_rcu(_c: &BchFs, _dev: u32, _target: u32) -> bool {
    false
}

pub fn dev_in_target(c: &BchFs, dev: u32, target: u32) -> bool {
    dev_in_target_rcu(c, dev, target)
}

pub fn disk_path_find(_sb: &mut BchSb, _path: &str) -> BchResult<i32> {
    Ok(-1)
}

pub fn disk_path_find_or_create(_sb: &mut BchSb, _path: &str) -> BchResult<i32> {
    Ok(-1)
}

pub fn disk_path_to_text(_buf: &mut Printbuf, _c: &BchFs, _idx: u32) {
}

pub fn disk_path_to_text_sb(_buf: &mut Printbuf, _sb: &BchSb, _idx: u32) {
}

pub fn target_to_text(_out: &mut Printbuf, _c: &BchFs, _target: u32) {
}

pub fn opt_target_parse(
    _c: &BchFs,
    _s: &str,
    _v: &mut u64,
    _err: &mut Printbuf,
) -> BchResult<i32> {
    Ok(0)
}

pub fn opt_target_to_text(_buf: &mut Printbuf, _c: &BchFs, _sb: &BchSb, _v: u64) {
}

pub fn sb_disk_groups_to_cpu(_c: &BchFs) -> BchResult<()> {
    Ok(())
}

pub fn __bch2_dev_group_set(
    _c: &BchFs,
    _ca: &BchDev,
    _name: &str,
) -> BchResult<()> {
    Ok(())
}

pub fn bch2_dev_group_set(
    _c: &BchFs,
    _ca: &BchDev,
    _name: &str,
) -> BchResult<()> {
    Ok(())
}

pub fn sb_validate_disk_groups(
    _sb: &BchSb,
    _field: &BchSbField,
) -> &'static str {
    ""
}

pub fn disk_groups_to_text(_buf: &mut Printbuf, _c: &BchFs) {
}

pub fn disk_groups_nr(groups: &BchSbFieldDiskGroups) -> u32 {
    let entries_ptr = &groups.entries as *const _ as *const u8;
    let field_end = unsafe {
        entries_ptr.add(std::mem::size_of::<BchDiskGroup>())
    };
    let bytes = field_end as usize - entries_ptr as usize;
    (bytes / std::mem::size_of::<BchDiskGroup>()) as u32
}
