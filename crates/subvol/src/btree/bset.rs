use super::bkey::{
    bch_val, bkey, bkey_format, bkey_i, bkey_s, bkey_s_c, bkey_start_offset, bkey_start_pos,
    bkey_val_bytes, bkey_val_u64s, bpos, bpos_ge, bpos_gt, bpos_le, bpos_lt, set_bkey_val_u64s,
};

pub const BTREE_MAX_DEPTH: u8 = 4;

#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_csum {
    pub lo: u64,
    pub hi: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_devs_list {
    pub nr: u8,
    pub data: [u8; super::types::BCH_BKEY_PTRS_MAX],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_devs_mask {
    pub d: [usize; 4],
}

pub const fn dev_mask_nr(devs: &bch_devs_mask) -> u32 {
    let mut ret = 0;
    let mut i = 0;
    while i < devs.d.len() {
        ret += devs.d[i].count_ones();
        i += 1;
    }
    ret
}

pub unsafe fn bch2_dev_idx_is_online(c: *const super::types::bch_fs, dev: u32) -> bool {
    let word = (dev as usize) / usize::BITS as usize;
    let bit = (dev as usize) % usize::BITS as usize;
    ((*c).devs_online.d[word] & (1usize << bit)) != 0
}

pub const fn bch2_dev_list_has_dev(devs: bch_devs_list, dev: u8) -> bool {
    let mut i = 0;
    while i < devs.nr as usize {
        if devs.data[i] == dev {
            return true;
        }
        i += 1;
    }
    false
}

pub fn bch2_dev_list_drop_dev(devs: &mut bch_devs_list, dev: u8) {
    let mut i = 0;
    while i < devs.nr as usize {
        if devs.data[i] == dev {
            let mut j = i + 1;
            while j < devs.nr as usize {
                devs.data[j - 1] = devs.data[j];
                j += 1;
            }
            devs.nr -= 1;
            return;
        }
        i += 1;
    }
}

pub fn bch2_dev_list_add_dev(devs: &mut bch_devs_list, dev: u8) {
    if !bch2_dev_list_has_dev(*devs, dev) {
        assert!((devs.nr as usize) < devs.data.len());
        devs.data[devs.nr as usize] = dev;
        devs.nr += 1;
    }
}

pub const fn bch2_dev_list_single(dev: u8) -> bch_devs_list {
    bch_devs_list {
        nr: 1,
        data: {
            let mut data = [0; super::types::BCH_BKEY_PTRS_MAX];
            data[0] = dev;
            data
        },
    }
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_extent_ptr {
    pub v: u64,
}

pub const fn BCH_EXTENT_PTR_TYPE(ptr: &bch_extent_ptr) -> u64 {
    ptr.v & 1
}

pub const fn SET_BCH_EXTENT_PTR_TYPE(ptr: &mut bch_extent_ptr, value: u64) {
    ptr.v = (ptr.v & !1) | (value & 1);
}

pub const fn BCH_EXTENT_PTR_CACHED(ptr: &bch_extent_ptr) -> u64 {
    (ptr.v >> 1) & 1
}

pub const fn SET_BCH_EXTENT_PTR_CACHED(ptr: &mut bch_extent_ptr, value: u64) {
    ptr.v = (ptr.v & !(1 << 1)) | ((value & 1) << 1);
}

pub const fn BCH_EXTENT_PTR_UNUSED(ptr: &bch_extent_ptr) -> u64 {
    (ptr.v >> 2) & 1
}

pub const fn SET_BCH_EXTENT_PTR_UNUSED(ptr: &mut bch_extent_ptr, value: u64) {
    ptr.v = (ptr.v & !(1 << 2)) | ((value & 1) << 2);
}

pub const fn BCH_EXTENT_PTR_UNWRITTEN(ptr: &bch_extent_ptr) -> u64 {
    (ptr.v >> 3) & 1
}

pub const fn SET_BCH_EXTENT_PTR_UNWRITTEN(ptr: &mut bch_extent_ptr, value: u64) {
    ptr.v = (ptr.v & !(1 << 3)) | ((value & 1) << 3);
}

pub const fn BCH_EXTENT_PTR_OFFSET(ptr: &bch_extent_ptr) -> u64 {
    (ptr.v >> 4) & ((1u64 << 44) - 1)
}

pub const fn SET_BCH_EXTENT_PTR_OFFSET(ptr: &mut bch_extent_ptr, value: u64) {
    const MASK: u64 = ((1u64 << 44) - 1) << 4;
    ptr.v = (ptr.v & !MASK) | ((value << 4) & MASK);
}

pub const fn BCH_EXTENT_PTR_DEV(ptr: &bch_extent_ptr) -> u64 {
    (ptr.v >> 48) & 0xff
}

pub const fn SET_BCH_EXTENT_PTR_DEV(ptr: &mut bch_extent_ptr, value: u64) {
    ptr.v = (ptr.v & !(0xff << 48)) | ((value & 0xff) << 48);
}

pub const fn BCH_EXTENT_PTR_GEN(ptr: &bch_extent_ptr) -> u64 {
    ptr.v >> 56
}

pub const fn SET_BCH_EXTENT_PTR_GEN(ptr: &mut bch_extent_ptr, value: u64) {
    ptr.v = (ptr.v & !(0xff << 56)) | ((value & 0xff) << 56);
}

pub const BCH_EXTENT_ENTRY_ptr: u8 = 0;
pub const BCH_EXTENT_ENTRY_crc32: u8 = 1;
pub const BCH_EXTENT_ENTRY_crc64: u8 = 2;
pub const BCH_EXTENT_ENTRY_crc128: u8 = 3;
pub const BCH_EXTENT_ENTRY_stripe_ptr: u8 = 4;
pub const BCH_EXTENT_ENTRY_rebalance_v1: u8 = 5;
pub const BCH_EXTENT_ENTRY_flags: u8 = 6;
pub const BCH_EXTENT_ENTRY_reconcile: u8 = 7;
pub const BCH_EXTENT_ENTRY_reconcile_bp: u8 = 8;
pub const BCH_EXTENT_ENTRY_MAX: u8 = 9;
pub const BCH_REPLICAS_MAX: u32 = 4;
pub const BKEY_EXTENT_PTR_U64S_MAX: u32 = ((core::mem::size_of::<bch_extent_crc128>()
    + core::mem::size_of::<bch_extent_ptr>())
    / 8) as u32;
pub const BKEY_EXTENT_VAL_U64S_MAX: u32 = 5 + BKEY_EXTENT_PTR_U64S_MAX * (BCH_REPLICAS_MAX * 2 + 1);

pub static bch_crc_bytes: [u8; 8] = [0, 4, 8, 10, 16, 4, 8, 8];

pub const fn extent_entry_u64s_known(type_: u8) -> u32 {
    match type_ {
        BCH_EXTENT_ENTRY_ptr => (core::mem::size_of::<bch_extent_ptr>() / 8) as u32,
        BCH_EXTENT_ENTRY_crc32 => (core::mem::size_of::<bch_extent_crc32>() / 8) as u32,
        BCH_EXTENT_ENTRY_crc64 => (core::mem::size_of::<bch_extent_crc64>() / 8) as u32,
        BCH_EXTENT_ENTRY_crc128 => (core::mem::size_of::<bch_extent_crc128>() / 8) as u32,
        BCH_EXTENT_ENTRY_stripe_ptr => (core::mem::size_of::<bch_extent_stripe_ptr>() / 8) as u32,
        BCH_EXTENT_ENTRY_rebalance_v1 => 1,
        BCH_EXTENT_ENTRY_flags => (core::mem::size_of::<bch_extent_flags>() / 8) as u32,
        BCH_EXTENT_ENTRY_reconcile => (core::mem::size_of::<bch_extent_reconcile>() / 8) as u32,
        BCH_EXTENT_ENTRY_reconcile_bp => {
            (core::mem::size_of::<bch_extent_reconcile_bp>() / 8) as u32
        }
        _ => 0,
    }
}

/// Matches bcachefs `extent_entry_u64s()` against the single supported extent
/// entry table in this rewrite.
pub unsafe fn extent_entry_u64s(
    _c: *const super::types::bch_fs,
    entry: *const bch_extent_entry,
) -> usize {
    let type_ = extent_entry_type(entry);
    assert!(type_ < BCH_EXTENT_ENTRY_MAX as u32);
    extent_entry_u64s_known(type_ as u8) as usize
}

/// Matches bcachefs `extent_entry_bytes()`.
pub unsafe fn extent_entry_bytes(
    c: *const super::types::bch_fs,
    entry: *const bch_extent_entry,
) -> usize {
    extent_entry_u64s(c, entry) * core::mem::size_of::<u64>()
}

/// Matches bcachefs `extent_entry_next()`.
pub unsafe fn extent_entry_next(
    c: *const super::types::bch_fs,
    entry: *const bch_extent_entry,
) -> *const bch_extent_entry {
    (entry.cast::<u8>())
        .add(extent_entry_bytes(c, entry))
        .cast()
}

/// Matches bcachefs `extent_entry_next_safe()`.
pub unsafe fn extent_entry_next_safe(
    c: *const super::types::bch_fs,
    entry: *const bch_extent_entry,
    end: *const bch_extent_entry,
) -> *const bch_extent_entry {
    if extent_entry_type(entry) < BCH_EXTENT_ENTRY_MAX as u32 {
        extent_entry_next(c, entry)
    } else {
        end
    }
}

/// Matches bcachefs `__extent_entry_insert()` for the supported packed value
/// layout.
pub unsafe fn __extent_entry_insert(
    c: *const super::types::bch_fs,
    k: *mut bkey_i,
    dst: *mut bch_extent_entry,
    new: *const bch_extent_entry,
) {
    let new_u64s = extent_entry_u64s(c, new) as isize;
    let value = core::ptr::addr_of_mut!((*k).v);
    let end = value.cast::<u8>().add(bkey_val_bytes(&(*k).k));
    let dst_u64 = dst.cast::<u64>();
    let end_u64 = end.cast::<u64>();
    core::ptr::copy(
        dst_u64,
        dst_u64.offset(new_u64s),
        end_u64.offset_from(dst_u64) as usize,
    );
    (*k).k.u64s = (*k).k.u64s.wrapping_add(new_u64s as u8);
    core::ptr::copy_nonoverlapping(new.cast::<u64>(), dst_u64, new_u64s as usize);
}

/// Matches bcachefs `extent_entry_drop()`.
pub unsafe fn extent_entry_drop(
    c: *const super::types::bch_fs,
    k: bkey_s,
    entry: *mut bch_extent_entry,
) {
    let next = extent_entry_next(c, entry);
    assert!((*k.k).type_ != KEY_TYPE_stripe);
    let end = k.v.cast::<u8>().add(bkey_val_bytes(&(*k.k))).cast::<u64>();
    let entry_u64 = entry.cast::<u64>();
    let next_u64 = next.cast::<u64>();
    core::ptr::copy(next_u64, entry_u64, end.offset_from(next_u64) as usize);
    (*k.k).u64s = (*k.k)
        .u64s
        .wrapping_sub(next_u64.offset_from(entry_u64) as u8);
}

pub unsafe fn bch2_bkey_extent_entry_drop_s(
    c: *const super::types::bch_fs,
    k: bkey_s,
    entry: *mut bch_extent_entry,
) {
    let end = k.v.cast::<u8>().add(bkey_val_bytes(&(*k.k)));
    let next = extent_entry_next(c, entry);
    core::ptr::copy(
        next.cast::<u64>(),
        entry.cast::<u64>(),
        end.cast::<u64>().offset_from(next.cast::<u64>()) as usize,
    );
    (*k.k).u64s = (*k.k).u64s.wrapping_sub(extent_entry_u64s(c, entry) as u8);
}

pub unsafe fn bch2_bkey_extent_entry_drop(
    c: *const super::types::bch_fs,
    k: *mut bkey_i,
    entry: *mut bch_extent_entry,
) {
    let end = core::ptr::addr_of_mut!((*k).v)
        .cast::<u8>()
        .add(bkey_val_bytes(&(*k).k));
    let next = extent_entry_next(c, entry);
    core::ptr::copy(
        next.cast::<u64>(),
        entry.cast::<u64>(),
        end.cast::<u64>().offset_from(next.cast::<u64>()) as usize,
    );
    (*k).k.u64s = (*k).k.u64s.wrapping_sub(extent_entry_u64s(c, entry) as u8);
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_extent_crc32 {
    pub word0: u32,
    pub csum: u32,
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_extent_crc64 {
    pub word0: u64,
    pub csum_lo: u64,
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_extent_crc128 {
    pub word0: u64,
    pub csum: bch_csum,
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_extent_stripe_ptr {
    pub v: u64,
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_extent_rebalance_v1 {
    pub v: u64,
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_extent_flags {
    pub v: u64,
}

pub const BCH_EXTENT_FLAG_poisoned: u8 = 0;

#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_extent_reconcile {
    pub v: u64,
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_extent_reconcile_bp {
    pub v: u64,
}

#[repr(C, packed(1))]
#[derive(Clone, Copy, Default)]
pub struct bch_stripe {
    pub v: bch_val,
    pub sectors: u16,
    pub algorithm: u8,
    pub nr_blocks: u8,
    pub nr_redundant: u8,
    pub csum_granularity_bits: u8,
    pub csum_type: u8,
    pub disk_label: u8,
    pub ptrs: [bch_extent_ptr; 0],
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_extent_crc_unpacked {
    pub compressed_size: u32,
    pub uncompressed_size: u32,
    pub live_size: u32,
    pub csum_type: u8,
    pub compression_type: u8,
    pub offset: u16,
    pub nonce: u16,
    pub csum: bch_csum,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct extent_ptr_decoded {
    pub has_ec: bool,
    pub do_ec_reconstruct: bool,
    pub crc_retry_nr: u8,
    pub crc: bch_extent_crc_unpacked,
    pub ptr: bch_extent_ptr,
    pub ec: bch_extent_stripe_ptr,
}

pub const BCH_COMPRESSION_TYPE_none: u8 = 0;
pub const BCH_COMPRESSION_TYPE_incompressible: u8 = 5;
pub const CRC32_SIZE_MAX: u32 = 1 << 7;
pub const CRC64_SIZE_MAX: u32 = 1 << 9;
pub const CRC128_SIZE_MAX: u32 = 1 << 13;
pub const CRC32_NONCE_MAX: u16 = 0;
pub const CRC64_NONCE_MAX: u16 = (1 << 10) - 1;
pub const CRC128_NONCE_MAX: u16 = (1 << 13) - 1;

pub const fn crc_is_compressed(crc: bch_extent_crc_unpacked) -> bool {
    crc.compression_type != BCH_COMPRESSION_TYPE_none
        && crc.compression_type != BCH_COMPRESSION_TYPE_incompressible
}

pub const fn crc_is_encoded(crc: bch_extent_crc_unpacked) -> bool {
    crc.csum_type != crate::checksum::BCH_CSUM_none as u8 || crc_is_compressed(crc)
}

pub static bch2_crc_field_size_max: [u32; BCH_EXTENT_ENTRY_MAX as usize] = [
    0,
    CRC32_SIZE_MAX,
    CRC64_SIZE_MAX,
    CRC128_SIZE_MAX,
    0,
    0,
    0,
    0,
    0,
];

#[repr(C)]
#[derive(Clone, Copy)]
pub union bch_extent_crc {
    pub type_: u8,
    pub crc32: bch_extent_crc32,
    pub crc64: bch_extent_crc64,
    pub crc128: bch_extent_crc128,
}

pub unsafe fn bch2_extent_crc_unpack(
    k: *const bkey,
    crc: *const bch_extent_crc,
) -> bch_extent_crc_unpacked {
    assert!(!k.is_null());
    if crc.is_null() {
        return bch_extent_crc_unpacked {
            compressed_size: (*k).size,
            uncompressed_size: (*k).size,
            live_size: (*k).size,
            ..Default::default()
        };
    }
    let live_size = (*k).size;
    match extent_entry_type(crc.cast::<bch_extent_entry>()) as u8 {
        BCH_EXTENT_ENTRY_crc32 => {
            let value = (*crc).crc32;
            bch_extent_crc_unpacked {
                compressed_size: ((value.word0 >> 2) & 0x7f) as u32 + 1,
                uncompressed_size: ((value.word0 >> 9) & 0x7f) as u32 + 1,
                live_size,
                offset: ((value.word0 >> 16) & 0x7f) as u16,
                csum_type: ((value.word0 >> 24) & 0xf) as u8,
                compression_type: ((value.word0 >> 28) & 0xf) as u8,
                csum: bch_csum {
                    lo: value.csum as u64,
                    hi: 0,
                },
                ..Default::default()
            }
        }
        BCH_EXTENT_ENTRY_crc64 => {
            let value = (*crc).crc64;
            bch_extent_crc_unpacked {
                compressed_size: ((value.word0 >> 3) & 0x1ff) as u32 + 1,
                uncompressed_size: ((value.word0 >> 12) & 0x1ff) as u32 + 1,
                live_size,
                offset: ((value.word0 >> 21) & 0x1ff) as u16,
                nonce: ((value.word0 >> 30) & 0x3ff) as u16,
                csum_type: ((value.word0 >> 40) & 0xf) as u8,
                compression_type: ((value.word0 >> 44) & 0xf) as u8,
                csum: bch_csum {
                    lo: value.csum_lo,
                    hi: (value.word0 >> 48) & 0xffff,
                },
            }
        }
        BCH_EXTENT_ENTRY_crc128 => {
            let value = (*crc).crc128;
            bch_extent_crc_unpacked {
                compressed_size: ((value.word0 >> 4) & 0x1fff) as u32 + 1,
                uncompressed_size: ((value.word0 >> 17) & 0x1fff) as u32 + 1,
                live_size,
                offset: ((value.word0 >> 30) & 0x1fff) as u16,
                nonce: ((value.word0 >> 43) & 0x1fff) as u16,
                csum_type: ((value.word0 >> 56) & 0xf) as u8,
                compression_type: ((value.word0 >> 60) & 0xf) as u8,
                csum: value.csum,
            }
        }
        _ => panic!("invalid extent crc entry type"),
    }
}

pub unsafe fn bch2_extent_crc_pack(
    dst: *mut bch_extent_crc,
    src: bch_extent_crc_unpacked,
    type_: u8,
) {
    assert!(!dst.is_null());
    assert!(src.compressed_size != 0 && src.uncompressed_size != 0);
    match type_ {
        BCH_EXTENT_ENTRY_crc32 => {
            (*dst).crc32 = bch_extent_crc32 {
                word0: (1 << 1)
                    | (((src.compressed_size - 1) as u32 & 0x7f) << 2)
                    | (((src.uncompressed_size - 1) as u32 & 0x7f) << 9)
                    | ((src.offset as u32 & 0x7f) << 16)
                    | ((src.csum_type as u32 & 0xf) << 24)
                    | ((src.compression_type as u32 & 0xf) << 28),
                csum: src.csum.lo as u32,
            };
        }
        BCH_EXTENT_ENTRY_crc64 => {
            (*dst).crc64 = bch_extent_crc64 {
                word0: (1 << 2)
                    | (((src.compressed_size - 1) as u64 & 0x1ff) << 3)
                    | (((src.uncompressed_size - 1) as u64 & 0x1ff) << 12)
                    | ((src.offset as u64 & 0x1ff) << 21)
                    | ((src.nonce as u64 & 0x3ff) << 30)
                    | ((src.csum_type as u64 & 0xf) << 40)
                    | ((src.compression_type as u64 & 0xf) << 44)
                    | ((src.csum.hi & 0xffff) << 48),
                csum_lo: src.csum.lo,
            };
        }
        BCH_EXTENT_ENTRY_crc128 => {
            (*dst).crc128 = bch_extent_crc128 {
                word0: (1 << 3)
                    | (((src.compressed_size - 1) as u64 & 0x1fff) << 4)
                    | (((src.uncompressed_size - 1) as u64 & 0x1fff) << 17)
                    | ((src.offset as u64 & 0x1fff) << 30)
                    | ((src.nonce as u64 & 0x1fff) << 43)
                    | ((src.csum_type as u64 & 0xf) << 56)
                    | ((src.compression_type as u64 & 0xf) << 60),
                csum: src.csum,
            };
        }
        _ => panic!("invalid extent crc entry type"),
    }
}

pub unsafe fn bch2_extent_crc_append(
    c: *const super::types::bch_fs,
    k: *mut bkey_i,
    new: bch_extent_crc_unpacked,
) {
    let ptrs = bch2_bkey_ptrs(bkey_s {
        k: &mut (*k).k,
        v: &mut (*k).v,
    });
    let bytes = bch_crc_bytes[new.csum_type as usize] as u32;
    let type_ = if bytes <= 4
        && new.uncompressed_size <= CRC32_SIZE_MAX
        && new.nonce <= CRC32_NONCE_MAX
    {
        BCH_EXTENT_ENTRY_crc32
    } else if bytes <= 10 && new.uncompressed_size <= CRC64_SIZE_MAX && new.nonce <= CRC64_NONCE_MAX
    {
        BCH_EXTENT_ENTRY_crc64
    } else if bytes <= 16
        && new.uncompressed_size <= CRC128_SIZE_MAX
        && new.nonce <= CRC128_NONCE_MAX
    {
        BCH_EXTENT_ENTRY_crc128
    } else {
        panic!("invalid extent crc size or nonce");
    };

    bch2_extent_crc_pack(ptrs.end.cast::<bch_extent_crc>(), new, type_);
    (*k).k.u64s = (*k)
        .k
        .u64s
        .wrapping_add(extent_entry_u64s(c, ptrs.end) as u8);
    assert!(bkey_val_u64s(&(*k).k) <= BKEY_EXTENT_VAL_U64S_MAX);
}

fn bch2_crc_unpacked_cmp(l: bch_extent_crc_unpacked, r: bch_extent_crc_unpacked) -> bool {
    l.csum_type != r.csum_type
        || l.compression_type != r.compression_type
        || l.compressed_size != r.compressed_size
        || l.uncompressed_size != r.uncompressed_size
        || l.offset != r.offset
        || l.live_size != r.live_size
        || l.nonce != r.nonce
        || ((l.csum.lo ^ r.csum.lo) | (l.csum.hi ^ r.csum.hi)) != 0
}

unsafe fn bkey_find_crc(
    c: *const super::types::bch_fs,
    k: bkey_s,
    crc: bch_extent_crc_unpacked,
) -> *mut bch_extent_entry {
    let ptrs = bch2_bkey_ptrs(k);
    let mut entry = ptrs.start;
    while !entry.is_null() && (entry as usize) < (ptrs.end as usize) {
        if extent_entry_is_crc(entry)
            && !bch2_crc_unpacked_cmp(crc, bch2_extent_crc_unpack(k.k, entry.cast()))
        {
            return entry;
        }
        entry = extent_entry_next_safe(c, entry, ptrs.end).cast_mut();
    }
    core::ptr::null_mut()
}

pub unsafe fn bch2_bkey_narrow_crc(
    c: *const super::types::bch_fs,
    k: *mut bkey_i,
    old: bch_extent_crc_unpacked,
    new: bch_extent_crc_unpacked,
) -> bool {
    assert!(!crc_is_compressed(new));
    assert_eq!(new.offset, 0);
    assert_eq!(new.uncompressed_size, new.live_size);

    let key = bkey_s {
        k: core::ptr::addr_of_mut!((*k).k),
        v: core::ptr::addr_of_mut!((*k).v),
    };
    let old_e = bkey_find_crc(c, key, old);
    if old_e.is_null() {
        return false;
    }

    let ptrs = bch2_bkey_ptrs(key);
    let mut entry = extent_entry_next(c, old_e).cast_mut();
    while !entry.is_null() && (entry as usize) < (ptrs.end as usize) {
        if extent_entry_is_crc(entry) {
            break;
        }
        if extent_entry_is_ptr(entry) {
            let ptr = &mut (*entry).ptr;
            SET_BCH_EXTENT_PTR_OFFSET(ptr, BCH_EXTENT_PTR_OFFSET(ptr) + old.offset as u64);
        }
        entry = extent_entry_next_safe(c, entry, ptrs.end).cast_mut();
    }

    bch2_extent_crc_pack(
        old_e.cast::<bch_extent_crc>(),
        new,
        extent_entry_type(old_e) as u8,
    );
    true
}

pub unsafe fn bch2_reservation_merge(l: bkey_s, r: bkey_s_c) -> bool {
    let l_v = &mut *l.v.cast::<bch_reservation>();
    let r_v = &*r.v.cast::<bch_reservation>();
    if l_v.generation != r_v.generation || l_v.nr_replicas != r_v.nr_replicas {
        return false;
    }
    crate::btree::bkey::bch2_key_resize(
        &mut *l.k,
        ((*l.k).size as u32).wrapping_add((*r.k).size as u32),
    );
    true
}

pub unsafe fn bch2_extent_ptr_decoded_append(
    c: *const super::types::bch_fs,
    k: *mut bkey_i,
    p: *mut extent_ptr_decoded,
) {
    let crc = bch2_extent_crc_unpack(core::ptr::addr_of!((*k).k), core::ptr::null());
    let mut pos;
    if !bch2_crc_unpacked_cmp(crc, (*p).crc) {
        pos = bch2_bkey_ptrs(bkey_s {
            k: core::ptr::addr_of_mut!((*k).k),
            v: core::ptr::addr_of_mut!((*k).v),
        })
        .start;
    } else {
        pos = bkey_find_crc(
            c,
            bkey_s {
                k: core::ptr::addr_of_mut!((*k).k),
                v: core::ptr::addr_of_mut!((*k).v),
            },
            (*p).crc,
        );
        if !pos.is_null() {
            pos = extent_entry_next(c, pos).cast_mut();
        } else {
            bch2_extent_crc_append(c, k, (*p).crc);
            pos = core::ptr::addr_of_mut!((*k).v)
                .cast::<u8>()
                .add(bkey_val_bytes(&(*k).k))
                .cast();
        }
    }

    SET_BCH_EXTENT_PTR_TYPE(&mut (*p).ptr, 1 << BCH_EXTENT_ENTRY_ptr);
    __extent_entry_insert(c, k, pos, core::ptr::addr_of!((*p).ptr).cast());
    if (*p).has_ec {
        (*p).ec.v = 1 << BCH_EXTENT_ENTRY_stripe_ptr;
        __extent_entry_insert(c, k, pos, core::ptr::addr_of!((*p).ec).cast());
    }
}

unsafe fn extent_entry_prev(
    c: *const super::types::bch_fs,
    ptrs: bkey_ptrs,
    entry: *mut bch_extent_entry,
) -> *mut bch_extent_entry {
    let mut i = ptrs.start;
    if i == entry {
        return core::ptr::null_mut();
    }
    while extent_entry_next(c, i) != entry {
        i = extent_entry_next(c, i).cast_mut();
    }
    i
}

pub unsafe fn bch2_bkey_drop_ptr_noerror(
    c: *const super::types::bch_fs,
    k: bkey_s,
    ptr: *mut bch_extent_ptr,
) {
    let ptrs = bch2_bkey_ptrs(k);
    let entry = ptr.cast::<bch_extent_entry>();
    if (*k.k).type_ == KEY_TYPE_stripe {
        SET_BCH_EXTENT_PTR_DEV(&mut *ptr, crate::sb::BCH_SB_MEMBER_INVALID as u64);
        return;
    }
    assert!(ptr >= ptrs.start.cast::<bch_extent_ptr>());
    assert!(ptr < ptrs.end.cast::<bch_extent_ptr>());
    assert_eq!(BCH_EXTENT_PTR_TYPE(&*ptr), 1 << BCH_EXTENT_ENTRY_ptr);

    let mut next = extent_entry_next(c, entry).cast_mut();
    let mut drop_crc = true;
    while next != ptrs.end {
        if extent_entry_is_crc(next) {
            break;
        } else if extent_entry_is_ptr(next) {
            drop_crc = false;
            break;
        }
        next = extent_entry_next(c, next).cast_mut();
    }

    extent_entry_drop(c, k, entry);
    let mut entry = extent_entry_prev(c, ptrs, entry);
    while !entry.is_null() {
        if extent_entry_is_ptr(entry) {
            break;
        }
        if (extent_entry_is_crc(entry) && drop_crc) || extent_entry_is_stripe_ptr(entry) {
            extent_entry_drop(c, k, entry);
        }
        entry = extent_entry_prev(c, ptrs, entry);
    }
}

pub unsafe fn bch2_bkey_drop_ptr(
    c: *const super::types::bch_fs,
    k: bkey_s,
    ptr: *mut bch_extent_ptr,
) {
    if (*k.k).type_ != KEY_TYPE_stripe {
        let mut decoded = extent_ptr_decoded::default();
        if bch2_bkey_has_device_decode(
            c,
            bkey_s_c { k: k.k, v: k.v },
            BCH_EXTENT_PTR_DEV(&*ptr) as u32,
            &mut decoded,
        ) && decoded.has_ec
        {
            SET_BCH_EXTENT_PTR_DEV(&mut *ptr, crate::sb::BCH_SB_MEMBER_INVALID as u64);
            return;
        }
    }
    bch2_bkey_drop_ptr_noerror(c, k, ptr);
}

pub unsafe fn bch2_bkey_drop_ptrs_mask(
    c: *const super::types::bch_fs,
    k: *mut bkey_i,
    mut ptrs: u32,
) {
    while ptrs != 0 {
        let drop = 31 - ptrs.leading_zeros();
        let key = bkey_s {
            k: core::ptr::addr_of_mut!((*k).k),
            v: core::ptr::addr_of_mut!((*k).v),
        };
        let ranges = bch2_bkey_ptrs(key);
        let mut entry = ranges.start;
        let mut index = 0u32;
        while !entry.is_null() && (entry as usize) < (ranges.end as usize) {
            if extent_entry_is_ptr(entry) {
                if index == drop {
                    bch2_bkey_drop_ptr_noerror(c, key, core::ptr::addr_of_mut!((*entry).ptr));
                    break;
                }
                index += 1;
            }
            entry = extent_entry_next_safe(c, entry, ranges.end).cast_mut();
        }
        ptrs ^= 1 << drop;
    }
}

pub unsafe fn bch2_bkey_drop_device_noerror(c: *const super::types::bch_fs, k: bkey_s, dev: u32) {
    loop {
        let ranges = bch2_bkey_ptrs(k);
        let mut entry = ranges.start;
        let mut dropped = false;
        while !entry.is_null() && (entry as usize) < (ranges.end as usize) {
            if extent_entry_is_ptr(entry) && BCH_EXTENT_PTR_DEV(&(*entry).ptr) as u32 == dev {
                bch2_bkey_drop_ptr_noerror(c, k, core::ptr::addr_of_mut!((*entry).ptr));
                dropped = true;
                break;
            }
            entry = extent_entry_next_safe(c, entry, ranges.end).cast_mut();
        }
        if !dropped {
            break;
        }
    }
}

pub unsafe fn bch2_bkey_drop_device(c: *const super::types::bch_fs, k: bkey_s, dev: u32) {
    loop {
        let ranges = bch2_bkey_ptrs(k);
        let mut entry = ranges.start;
        let mut dropped = false;
        while !entry.is_null() && (entry as usize) < (ranges.end as usize) {
            if extent_entry_is_ptr(entry) && BCH_EXTENT_PTR_DEV(&(*entry).ptr) as u32 == dev {
                bch2_bkey_drop_ptr(c, k, core::ptr::addr_of_mut!((*entry).ptr));
                dropped = true;
                break;
            }
            entry = extent_entry_next_safe(c, entry, ranges.end).cast_mut();
        }
        if !dropped {
            break;
        }
    }
}

unsafe fn bch2_bkey_drop_ec(c: *const super::types::bch_fs, k: *mut bkey_i, dev: u32) {
    let key = bkey_s {
        k: core::ptr::addr_of_mut!((*k).k),
        v: core::ptr::addr_of_mut!((*k).v),
    };
    let ptrs = bch2_bkey_ptrs(key);
    let mut entry = ptrs.start;
    let mut ec = core::ptr::null_mut();
    while !entry.is_null() && (entry as usize) < (ptrs.end as usize) {
        if extent_entry_is_stripe_ptr(entry) {
            ec = entry;
        } else if extent_entry_is_ptr(entry) && BCH_EXTENT_PTR_DEV(&(*entry).ptr) as u32 == dev {
            if !ec.is_null() {
                bch2_bkey_extent_entry_drop(c, k, ec);
            }
            return;
        }
        entry = extent_entry_next_safe(c, entry, ptrs.end).cast_mut();
    }
}

pub unsafe fn bch2_bkey_drop_ec_mask(
    c: *const super::types::bch_fs,
    k: *mut bkey_i,
    mut mask: u32,
) {
    while mask != 0 {
        let ptrs = bch2_bkey_ptrs(bkey_s {
            k: core::ptr::addr_of_mut!((*k).k),
            v: core::ptr::addr_of_mut!((*k).v),
        });
        let mut entry = ptrs.start;
        let mut ptr_bit = 1u32;
        while !entry.is_null() && (entry as usize) < (ptrs.end as usize) {
            if extent_entry_is_ptr(entry) {
                if (mask & ptr_bit) != 0 {
                    bch2_bkey_drop_ec(c, k, BCH_EXTENT_PTR_DEV(&(*entry).ptr) as u32);
                    mask &= !ptr_bit;
                    break;
                }
                ptr_bit <<= 1;
            }
            entry = extent_entry_next_safe(c, entry, ptrs.end).cast_mut();
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union bch_extent_entry {
    pub type_: usize,
    pub ptr: bch_extent_ptr,
    pub crc32: bch_extent_crc32,
    pub crc64: bch_extent_crc64,
    pub crc128: bch_extent_crc128,
    pub stripe_ptr: bch_extent_stripe_ptr,
    pub rebalance_v1: bch_extent_rebalance_v1,
    pub flags: bch_extent_flags,
    pub reconcile: bch_extent_reconcile,
    pub reconcile_bp: bch_extent_reconcile_bp,
}

pub unsafe fn extent_entry_type(entry: *const bch_extent_entry) -> u32 {
    if entry.is_null() {
        return u32::MAX;
    }
    let type_ = (*entry).type_ as u64;
    if type_ == 0 {
        u32::MAX
    } else {
        type_.trailing_zeros()
    }
}

pub unsafe fn extent_entry_is_ptr(entry: *const bch_extent_entry) -> bool {
    extent_entry_type(entry) == BCH_EXTENT_ENTRY_ptr as u32
}

pub unsafe fn extent_entry_is_stripe_ptr(entry: *const bch_extent_entry) -> bool {
    extent_entry_type(entry) == BCH_EXTENT_ENTRY_stripe_ptr as u32
}

pub unsafe fn extent_entry_is_crc(entry: *const bch_extent_entry) -> bool {
    matches!(
        extent_entry_type(entry),
        x if x == BCH_EXTENT_ENTRY_crc32 as u32
            || x == BCH_EXTENT_ENTRY_crc64 as u32
            || x == BCH_EXTENT_ENTRY_crc128 as u32
    )
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct bkey_ptrs_c {
    pub start: *const bch_extent_entry,
    pub end: *const bch_extent_entry,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct bkey_ptrs {
    pub start: *mut bch_extent_entry,
    pub end: *mut bch_extent_entry,
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bset {
    pub seq: u64,
    pub journal_seq: u64,
    pub flags: u32,
    pub version: u16,
    pub u64s: u16,
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct btree_node {
    pub csum: bch_csum,
    pub magic: u64,
    pub flags: u64,
    pub min_key: bpos,
    pub max_key: bpos,
    pub _ptr: bch_extent_ptr,
    pub format: bkey_format,
    pub keys: bset,
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct btree_node_entry {
    pub csum: bch_csum,
    pub keys: bset,
}

pub const BFLOAT_FAILED_UNPACKED: u8 = u8::MAX;
pub const BFLOAT_FAILED: u8 = u8::MAX;
pub const BKEY_MANTISSA_BITS: u32 = 16;
pub const BKEY_TYPE_strict_btree_checks: u32 = 1 << 0;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bkey_float {
    pub exponent: u8,
    pub key_offset: u8,
    pub mantissa: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct rw_aux_tree {
    pub offset: u16,
    pub k: bpos,
}

pub const KEY_TYPE_deleted: u8 = 0;
pub const KEY_TYPE_whiteout: u8 = 1;
pub const KEY_TYPE_error: u8 = 2;
pub const KEY_TYPE_cookie: u8 = 3;
pub const KEY_TYPE_hash_whiteout: u8 = 4;
pub const KEY_TYPE_btree_ptr: u8 = 5;
pub const KEY_TYPE_extent: u8 = 6;
pub const KEY_TYPE_reservation: u8 = 7;
pub const KEY_TYPE_inode: u8 = 8;
pub const KEY_TYPE_inode_generation: u8 = 9;
pub const KEY_TYPE_dirent: u8 = 10;
pub const KEY_TYPE_xattr: u8 = 11;
pub const KEY_TYPE_alloc: u8 = 12;
pub const KEY_TYPE_quota: u8 = 13;
pub const KEY_TYPE_stripe: u8 = 14;
pub const KEY_TYPE_reflink_p: u8 = 15;
pub const KEY_TYPE_reflink_v: u8 = 16;
pub const KEY_TYPE_inline_data: u8 = 17;
pub const KEY_TYPE_btree_ptr_v2: u8 = 18;
pub const KEY_TYPE_indirect_inline_data: u8 = 19;
pub const KEY_TYPE_alloc_v2: u8 = 20;
pub const KEY_TYPE_subvolume: u8 = 21;
pub const KEY_TYPE_snapshot: u8 = 22;
pub const KEY_TYPE_inode_v2: u8 = 23;
pub const KEY_TYPE_alloc_v3: u8 = 24;
pub const KEY_TYPE_set: u8 = 25;
pub const KEY_TYPE_lru: u8 = 26;
pub const KEY_TYPE_alloc_v4: u8 = 27;
pub const KEY_TYPE_backpointer: u8 = 28;
pub const KEY_TYPE_inode_v3: u8 = 29;
pub const KEY_TYPE_bucket_gens: u8 = 30;
pub const KEY_TYPE_snapshot_tree: u8 = 31;
pub const KEY_TYPE_logged_op_truncate: u8 = 32;
pub const KEY_TYPE_logged_op_finsert: u8 = 33;
pub const KEY_TYPE_accounting: u8 = 34;
pub const KEY_TYPE_inode_alloc_cursor: u8 = 35;
pub const KEY_TYPE_extent_whiteout: u8 = 36;
pub const KEY_TYPE_logged_op_stripe_update: u8 = 37;
pub const KEY_TYPE_MAX: u8 = 38;

/* bcachefs alloc/format.h:bch_alloc_v4 and bcachefs_format.h:bch_backpointer.
 * These are the single-format on-disk values used by the derived trees. */
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_alloc_v4 {
    pub v: bch_val,
    pub journal_seq_nonempty: u64,
    pub flags: u32,
    pub gen: u8,
    pub oldest_gen: u8,
    pub data_type: u8,
    pub stripe_redundancy_obsolete: u8,
    pub dirty_sectors: u32,
    pub cached_sectors: u32,
    pub io_time: [u64; 2],
    pub stripe_refcount: u32,
    pub nr_external_backpointers: u32,
    pub journal_seq_empty: u64,
    pub stripe_sectors: u32,
    pub pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_backpointer {
    pub v: bch_val,
    pub btree_id: u8,
    pub level: u8,
    pub data_type: u8,
    pub bucket_gen: u8,
    pub flags: u32,
    pub bucket_len: u32,
    pub pos: bpos,
}

/// Matches bcachefs `bch2_bkey_type_flags[]`.
#[allow(non_upper_case_globals)]
pub static bch2_bkey_type_flags: [u32; KEY_TYPE_MAX as usize] = [
    0,
    0,
    0,
    0,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    0,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
    BKEY_TYPE_strict_btree_checks,
];

/// Matches bcachefs `bkey_whiteout()`.
pub const fn bkey_whiteout(k: &super::bkey::bkey_packed) -> bool {
    k.type_ == KEY_TYPE_deleted || k.type_ == KEY_TYPE_whiteout
}

/// Matches bcachefs `bkey_extent_whiteout()`.
pub const fn bkey_extent_whiteout(k: &super::bkey::bkey_packed) -> bool {
    k.type_ == KEY_TYPE_deleted
        || k.type_ == KEY_TYPE_whiteout
        || k.type_ == KEY_TYPE_extent_whiteout
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_btree_ptr {
    pub v: bch_val,
    pub _data: [u64; 0],
    pub start: [bch_extent_ptr; 0],
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_btree_ptr_v2 {
    pub v: bch_val,
    pub mem_ptr: u64,
    pub seq: u64,
    pub sectors_written: u16,
    pub flags: u16,
    pub min_key: bpos,
    pub _data: [u64; 0],
    pub start: [bch_extent_ptr; 0],
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default)]
pub struct bch_extent {
    pub v: bch_val,
    pub _data: [u64; 0],
    pub start: [bch_extent_entry; 0],
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_reservation {
    pub v: bch_val,
    pub generation: u32,
    pub nr_replicas: u8,
    pub pad: [u8; 3],
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default)]
pub struct bch_inline_data {
    pub v: bch_val,
    pub data: [u8; 0],
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default)]
pub struct bch_indirect_inline_data {
    pub v: bch_val,
    pub refcount: u64,
    pub data: [u8; 0],
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default)]
pub struct bch_reflink_v {
    pub v: bch_val,
    pub refcount: u64,
    pub start: [bch_extent_entry; 0],
    pub _data: [u64; 0],
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bkey_i_btree_ptr_v2 {
    pub k: bkey,
    pub v: bch_btree_ptr_v2,
}

pub unsafe fn bkey_i_to_btree_ptr_v2(k: *mut super::bkey::bkey_i) -> *mut bkey_i_btree_ptr_v2 {
    k.cast()
}

pub unsafe fn btree_node_mem_ptr(k: *const super::bkey::bkey_i) -> *mut super::types::btree {
    if (*k).k.type_ == KEY_TYPE_btree_ptr_v2 {
        (*(k as *const bkey_i_btree_ptr_v2)).v.mem_ptr as usize as *mut super::types::btree
    } else {
        core::ptr::null_mut()
    }
}

pub unsafe fn bch2_bkey_ptrs_c(k: bkey_s_c) -> bkey_ptrs_c {
    if k.k.is_null() || k.v.is_null() {
        return bkey_ptrs_c {
            start: core::ptr::null(),
            end: core::ptr::null(),
        };
    }
    let start = match (*k.k).type_ {
        KEY_TYPE_btree_ptr_v2 => {
            k.v.cast::<u8>()
                .add(core::mem::size_of::<bch_btree_ptr_v2>())
        }
        KEY_TYPE_stripe => {
            let stripe = k.v.cast::<bch_stripe>();
            core::ptr::addr_of!((*stripe).ptrs)
                .cast::<bch_extent_ptr>()
                .cast::<u8>()
        }
        KEY_TYPE_btree_ptr | KEY_TYPE_extent | KEY_TYPE_reflink_v => k.v.cast::<u8>(),
        _ => {
            return bkey_ptrs_c {
                start: core::ptr::null(),
                end: core::ptr::null(),
            }
        }
    }
    .cast::<bch_extent_entry>();
    let end = if (*k.k).type_ == KEY_TYPE_stripe {
        let stripe = k.v.cast::<bch_stripe>();
        core::ptr::addr_of!((*stripe).ptrs)
            .cast::<bch_extent_ptr>()
            .add((*stripe).nr_blocks as usize)
            .cast::<bch_extent_entry>()
    } else {
        k.v.cast::<u8>()
            .add(bkey_val_bytes(&*k.k))
            .cast::<bch_extent_entry>()
    };
    bkey_ptrs_c { start, end }
}

pub unsafe fn bch2_bkey_ptrs(k: bkey_s) -> bkey_ptrs {
    let ptrs = bch2_bkey_ptrs_c(bkey_s_c { k: k.k, v: k.v });
    bkey_ptrs {
        start: ptrs.start.cast_mut(),
        end: ptrs.end.cast_mut(),
    }
}

pub unsafe fn bch2_bkey_extent_ptrs_flags(ptrs: bkey_ptrs_c) -> u64 {
    if ptrs.start != ptrs.end
        && !ptrs.start.is_null()
        && extent_entry_type(ptrs.start) == BCH_EXTENT_ENTRY_flags as u32
    {
        (*ptrs.start).flags.v >> 7
    } else {
        0
    }
}

pub unsafe fn bch2_bkey_extent_flags(k: bkey_s_c) -> u64 {
    bch2_bkey_extent_ptrs_flags(bch2_bkey_ptrs_c(k))
}

pub unsafe fn bch2_ptr_swab(c: *const super::types::bch_fs, k: bkey_s) {
    let ptrs = bch2_bkey_ptrs(k);
    let mut d = ptrs.start.cast::<u64>();
    let end = ptrs.end.cast::<u64>();
    while d != end {
        *d = (*d).swap_bytes();
        d = d.add(1);
    }

    let mut entry = ptrs.start;
    while !entry.is_null() && (entry as usize) < (ptrs.end as usize) {
        match extent_entry_type(entry) {
            x if x == BCH_EXTENT_ENTRY_ptr as u32 => {}
            x if x == BCH_EXTENT_ENTRY_crc32 as u32 => {
                (*entry).crc32.csum = (*entry).crc32.csum.swap_bytes();
            }
            x if x == BCH_EXTENT_ENTRY_crc64 as u32 => {
                let word0 = (*entry).crc64.word0;
                let csum_hi = ((word0 >> 48) as u16).swap_bytes() as u64;
                (*entry).crc64.word0 = (word0 & !(0xffffu64 << 48)) | (csum_hi << 48);
                (*entry).crc64.csum_lo = (*entry).crc64.csum_lo.swap_bytes();
            }
            x if x == BCH_EXTENT_ENTRY_crc128 as u32 => {
                (*entry).crc128.csum.hi = (*entry).crc128.csum.hi.swap_bytes();
                (*entry).crc128.csum.lo = (*entry).crc128.csum.lo.swap_bytes();
            }
            x if x == BCH_EXTENT_ENTRY_stripe_ptr as u32 => {}
            x if x == BCH_EXTENT_ENTRY_rebalance_v1 as u32
                || x == BCH_EXTENT_ENTRY_reconcile as u32 => {}
            _ => return,
        }
        entry = extent_entry_next(c, entry).cast_mut();
    }
}

pub unsafe fn bch2_bkey_has_device_c(
    c: *const super::types::bch_fs,
    k: bkey_s_c,
    dev: u32,
) -> *const bch_extent_ptr {
    let ptrs = bch2_bkey_ptrs_c(k);
    let mut entry = ptrs.start;
    while !entry.is_null() && (entry as usize) < (ptrs.end as usize) {
        if extent_entry_is_ptr(entry) && BCH_EXTENT_PTR_DEV(&(*entry).ptr) as u32 == dev {
            return core::ptr::addr_of!((*entry).ptr);
        }
        entry = extent_entry_next_safe(c, entry, ptrs.end);
    }
    core::ptr::null()
}

/// Matches the inline `bch2_bkey_has_device()` wrapper in local
/// `fs/data/extents.h`: retain the mutable key view while delegating the
/// lookup and pointer traversal to the const implementation.
pub unsafe fn bch2_bkey_has_device(
    c: *const super::types::bch_fs,
    k: super::bkey::bkey_s,
    dev: u32,
) -> *mut bch_extent_ptr {
    bch2_bkey_has_device_c(c, bkey_s_c { k: k.k, v: k.v }, dev) as *mut bch_extent_ptr
}

pub unsafe fn bch2_bkey_has_device_decode(
    c: *const super::types::bch_fs,
    k: bkey_s_c,
    dev: u32,
    ret: *mut extent_ptr_decoded,
) -> bool {
    let ptrs = bch2_bkey_ptrs_c(k);
    let mut entry = ptrs.start;
    let mut crc = bch2_extent_crc_unpack(k.k, core::ptr::null());
    while !entry.is_null() && (entry as usize) < (ptrs.end as usize) {
        let mut decoded = extent_ptr_decoded {
            crc,
            ..Default::default()
        };
        let mut found = false;
        while (entry as usize) < (ptrs.end as usize) {
            match extent_entry_type(entry) {
                x if x == BCH_EXTENT_ENTRY_ptr as u32 => {
                    decoded.ptr = (*entry).ptr;
                    found = true;
                    break;
                }
                x if x == BCH_EXTENT_ENTRY_crc32 as u32
                    || x == BCH_EXTENT_ENTRY_crc64 as u32
                    || x == BCH_EXTENT_ENTRY_crc128 as u32 =>
                {
                    decoded.crc = bch2_extent_crc_unpack(k.k, entry.cast());
                    crc = decoded.crc;
                }
                x if x == BCH_EXTENT_ENTRY_stripe_ptr as u32 => {
                    decoded.ec = (*entry).stripe_ptr;
                    decoded.has_ec = true;
                }
                _ => {}
            }
            entry = extent_entry_next_safe(c, entry, ptrs.end);
        }
        if !found {
            break;
        }
        if BCH_EXTENT_PTR_DEV(&decoded.ptr) as u32 == dev {
            *ret = decoded;
            return true;
        }
        entry = extent_entry_next_safe(c, entry, ptrs.end);
    }
    false
}

pub unsafe fn bch2_bkey_dev_ptr_bit(c: *const super::types::bch_fs, k: bkey_s_c, dev: u32) -> u32 {
    let ptrs = bch2_bkey_ptrs_c(k);
    let mut entry = ptrs.start;
    let mut ptr_bit = 1u32;
    while !entry.is_null() && (entry as usize) < (ptrs.end as usize) {
        if extent_entry_is_ptr(entry) {
            if BCH_EXTENT_PTR_DEV(&(*entry).ptr) as u32 == dev {
                return ptr_bit;
            }
            ptr_bit <<= 1;
        }
        entry = extent_entry_next_safe(c, entry, ptrs.end);
    }
    0
}

pub unsafe fn bch2_bkey_devs(c: *const super::types::bch_fs, k: bkey_s_c) -> bch_devs_list {
    let mut ret = bch_devs_list::default();
    let ptrs = bch2_bkey_ptrs_c(k);
    let mut entry = ptrs.start;
    while !entry.is_null() && (entry as usize) < (ptrs.end as usize) {
        if extent_entry_is_ptr(entry)
            && BCH_EXTENT_PTR_DEV(&(*entry).ptr) != crate::sb::BCH_SB_MEMBER_INVALID as u64
        {
            ret.data[ret.nr as usize] = BCH_EXTENT_PTR_DEV(&(*entry).ptr) as u8;
            ret.nr += 1;
        }
        entry = extent_entry_next_safe(c, entry, ptrs.end);
    }
    ret
}

pub unsafe fn bch2_bkey_ptrs_match(
    k1: bkey_s_c,
    p1: extent_ptr_decoded,
    k2: bkey_s_c,
    p2: extent_ptr_decoded,
) -> bool {
    let p1_ec_idx = (p1.ec.v >> 17) & ((1u64 << 47) - 1);
    let p2_ec_idx = (p2.ec.v >> 17) & ((1u64 << 47) - 1);
    let p1_ec_block = (p1.ec.v >> 5) & 0xff;
    let p2_ec_block = (p2.ec.v >> 5) & 0xff;
    let same_device_or_ec = BCH_EXTENT_PTR_DEV(&p1.ptr) == BCH_EXTENT_PTR_DEV(&p2.ptr)
        || (p1.has_ec && p2.has_ec && p1_ec_idx == p2_ec_idx && p1_ec_block == p2_ec_block);
    if !same_device_or_ec || BCH_EXTENT_PTR_GEN(&p1.ptr) != BCH_EXTENT_PTR_GEN(&p2.ptr) {
        return false;
    }
    let p1_disk_offset = BCH_EXTENT_PTR_OFFSET(&p1.ptr) as i128 + p1.crc.offset as i128
        - bkey_start_offset(&*k1.k) as i128;
    let p2_disk_offset = BCH_EXTENT_PTR_OFFSET(&p2.ptr) as i128 + p2.crc.offset as i128
        - bkey_start_offset(&*k2.k) as i128;
    if p1_disk_offset != p2_disk_offset {
        return false;
    }
    (BCH_EXTENT_PTR_OFFSET(&p1.ptr) >= BCH_EXTENT_PTR_OFFSET(&p2.ptr)
        && BCH_EXTENT_PTR_OFFSET(&p1.ptr)
            < BCH_EXTENT_PTR_OFFSET(&p2.ptr) + p2.crc.compressed_size as u64)
        || (BCH_EXTENT_PTR_OFFSET(&p2.ptr) >= BCH_EXTENT_PTR_OFFSET(&p1.ptr)
            && BCH_EXTENT_PTR_OFFSET(&p2.ptr)
                < BCH_EXTENT_PTR_OFFSET(&p1.ptr) + p1.crc.compressed_size as u64)
}

pub unsafe fn bch2_extents_match(
    c: *const super::types::bch_fs,
    k1: bkey_s_c,
    k2: bkey_s_c,
) -> bool {
    if (*k1.k).type_ != (*k2.k).type_ {
        return false;
    }
    if bkey_extent_is_direct_data(&*k1.k) {
        let ptrs1 = bch2_bkey_ptrs_c(k1);
        let ptrs2 = bch2_bkey_ptrs_c(k2);
        if bkey_extent_is_unwritten(c, k1) != bkey_extent_is_unwritten(c, k2) {
            return false;
        }

        let mut entry1 = ptrs1.start;
        let mut crc1 = bch2_extent_crc_unpack(k1.k, core::ptr::null());
        while !entry1.is_null() && (entry1 as usize) < (ptrs1.end as usize) {
            let mut p1 = extent_ptr_decoded {
                crc: crc1,
                ..Default::default()
            };
            let mut found1 = false;
            while (entry1 as usize) < (ptrs1.end as usize) {
                match extent_entry_type(entry1) {
                    x if x == BCH_EXTENT_ENTRY_ptr as u32 => {
                        p1.ptr = (*entry1).ptr;
                        found1 = true;
                        break;
                    }
                    x if x == BCH_EXTENT_ENTRY_crc32 as u32
                        || x == BCH_EXTENT_ENTRY_crc64 as u32
                        || x == BCH_EXTENT_ENTRY_crc128 as u32 =>
                    {
                        p1.crc = bch2_extent_crc_unpack(k1.k, entry1.cast());
                        crc1 = p1.crc;
                    }
                    x if x == BCH_EXTENT_ENTRY_stripe_ptr as u32 => {
                        p1.ec = (*entry1).stripe_ptr;
                        p1.has_ec = true;
                    }
                    _ => {}
                }
                entry1 = extent_entry_next_safe(c, entry1, ptrs1.end);
            }
            if !found1 {
                break;
            }

            let mut entry2 = ptrs2.start;
            let mut crc2 = bch2_extent_crc_unpack(k2.k, core::ptr::null());
            while !entry2.is_null() && (entry2 as usize) < (ptrs2.end as usize) {
                let mut p2 = extent_ptr_decoded {
                    crc: crc2,
                    ..Default::default()
                };
                let mut found2 = false;
                while (entry2 as usize) < (ptrs2.end as usize) {
                    match extent_entry_type(entry2) {
                        x if x == BCH_EXTENT_ENTRY_ptr as u32 => {
                            p2.ptr = (*entry2).ptr;
                            found2 = true;
                            break;
                        }
                        x if x == BCH_EXTENT_ENTRY_crc32 as u32
                            || x == BCH_EXTENT_ENTRY_crc64 as u32
                            || x == BCH_EXTENT_ENTRY_crc128 as u32 =>
                        {
                            p2.crc = bch2_extent_crc_unpack(k2.k, entry2.cast());
                            crc2 = p2.crc;
                        }
                        x if x == BCH_EXTENT_ENTRY_stripe_ptr as u32 => {
                            p2.ec = (*entry2).stripe_ptr;
                            p2.has_ec = true;
                        }
                        _ => {}
                    }
                    entry2 = extent_entry_next_safe(c, entry2, ptrs2.end);
                }
                if !found2 {
                    break;
                }
                if bch2_bkey_ptrs_match(k1, p1, k2, p2) {
                    return true;
                }
                entry2 = extent_entry_next_safe(c, entry2, ptrs2.end);
            }
            entry1 = extent_entry_next_safe(c, entry1, ptrs1.end);
        }
        false
    } else {
        true
    }
}

pub unsafe fn bch2_extent_has_ptr(
    c: *const super::types::bch_fs,
    k1: bkey_s_c,
    p1: extent_ptr_decoded,
    k2: bkey_s,
) -> *mut bch_extent_ptr {
    let ptrs2 = bch2_bkey_ptrs(k2);
    let mut entry2 = ptrs2.start;
    let mut crc2 = bch2_extent_crc_unpack(k2.k, core::ptr::null());
    while !entry2.is_null() && (entry2 as usize) < (ptrs2.end as usize) {
        let mut p2 = extent_ptr_decoded {
            crc: crc2,
            ..Default::default()
        };
        let mut found2 = false;
        while (entry2 as usize) < (ptrs2.end as usize) {
            match extent_entry_type(entry2) {
                x if x == BCH_EXTENT_ENTRY_ptr as u32 => {
                    p2.ptr = (*entry2).ptr;
                    found2 = true;
                    break;
                }
                x if x == BCH_EXTENT_ENTRY_crc32 as u32
                    || x == BCH_EXTENT_ENTRY_crc64 as u32
                    || x == BCH_EXTENT_ENTRY_crc128 as u32 =>
                {
                    p2.crc = bch2_extent_crc_unpack(k2.k, entry2.cast());
                    crc2 = p2.crc;
                }
                x if x == BCH_EXTENT_ENTRY_stripe_ptr as u32 => {
                    p2.ec = (*entry2).stripe_ptr;
                    p2.has_ec = true;
                }
                _ => {}
            }
            entry2 = extent_entry_next_safe(c, entry2, ptrs2.end).cast_mut();
        }
        if !found2 {
            break;
        }
        let p1_disk_offset = BCH_EXTENT_PTR_OFFSET(&p1.ptr) as i128 + p1.crc.offset as i128
            - bkey_start_offset(&*k1.k) as i128;
        let p2_disk_offset = BCH_EXTENT_PTR_OFFSET(&p2.ptr) as i128 + p2.crc.offset as i128
            - bkey_start_offset(&*k2.k) as i128;
        if BCH_EXTENT_PTR_DEV(&p1.ptr) == BCH_EXTENT_PTR_DEV(&p2.ptr)
            && BCH_EXTENT_PTR_GEN(&p1.ptr) == BCH_EXTENT_PTR_GEN(&p2.ptr)
            && p1_disk_offset == p2_disk_offset
        {
            return core::ptr::addr_of_mut!((*entry2).ptr);
        }
        entry2 = extent_entry_next_safe(c, entry2, ptrs2.end).cast_mut();
    }
    core::ptr::null_mut()
}

pub unsafe fn bch2_bkey_matches_ptr(
    c: *const super::types::bch_fs,
    k: bkey_s_c,
    m: bch_extent_ptr,
    offset: u64,
) -> bool {
    let ptrs = bch2_bkey_ptrs_c(k);
    let mut entry = ptrs.start;
    let mut crc = bch2_extent_crc_unpack(k.k, core::ptr::null());
    while !entry.is_null() && (entry as usize) < (ptrs.end as usize) {
        let mut p = extent_ptr_decoded {
            crc,
            ..Default::default()
        };
        let mut found = false;
        while (entry as usize) < (ptrs.end as usize) {
            match extent_entry_type(entry) {
                x if x == BCH_EXTENT_ENTRY_ptr as u32 => {
                    p.ptr = (*entry).ptr;
                    found = true;
                    break;
                }
                x if x == BCH_EXTENT_ENTRY_crc32 as u32
                    || x == BCH_EXTENT_ENTRY_crc64 as u32
                    || x == BCH_EXTENT_ENTRY_crc128 as u32 =>
                {
                    p.crc = bch2_extent_crc_unpack(k.k, entry.cast());
                    crc = p.crc;
                }
                x if x == BCH_EXTENT_ENTRY_stripe_ptr as u32 => {
                    p.ec = (*entry).stripe_ptr;
                    p.has_ec = true;
                }
                _ => {}
            }
            entry = extent_entry_next_safe(c, entry, ptrs.end);
        }
        if !found {
            break;
        }
        let disk_offset = BCH_EXTENT_PTR_OFFSET(&p.ptr) as i128 + p.crc.offset as i128
            - bkey_start_offset(&*k.k) as i128;
        if BCH_EXTENT_PTR_DEV(&p.ptr) == BCH_EXTENT_PTR_DEV(&m)
            && BCH_EXTENT_PTR_GEN(&p.ptr) == BCH_EXTENT_PTR_GEN(&m)
            && disk_offset == BCH_EXTENT_PTR_OFFSET(&m) as i128 - offset as i128
        {
            return true;
        }
        entry = extent_entry_next_safe(c, entry, ptrs.end);
    }
    false
}

pub unsafe fn bch2_bkey_replicas(c: *mut super::types::bch_fs, k: bkey_s_c) -> u32 {
    let ptrs = bch2_bkey_ptrs_c(k);
    let mut entry = ptrs.start;
    let mut crc = bch2_extent_crc_unpack(k.k, core::ptr::null());
    let mut replicas = 0u32;
    while !entry.is_null() && (entry as usize) < (ptrs.end as usize) {
        let mut p = extent_ptr_decoded {
            crc,
            ..Default::default()
        };
        let mut found = false;
        while (entry as usize) < (ptrs.end as usize) {
            match extent_entry_type(entry) {
                x if x == BCH_EXTENT_ENTRY_ptr as u32 => {
                    p.ptr = (*entry).ptr;
                    found = true;
                    break;
                }
                x if x == BCH_EXTENT_ENTRY_crc32 as u32
                    || x == BCH_EXTENT_ENTRY_crc64 as u32
                    || x == BCH_EXTENT_ENTRY_crc128 as u32 =>
                {
                    p.crc = bch2_extent_crc_unpack(k.k, entry.cast());
                    crc = p.crc;
                }
                x if x == BCH_EXTENT_ENTRY_stripe_ptr as u32 => {
                    p.ec = (*entry).stripe_ptr;
                    p.has_ec = true;
                }
                _ => {}
            }
            entry = extent_entry_next_safe(c, entry, ptrs.end);
        }
        if !found {
            break;
        }
        if BCH_EXTENT_PTR_CACHED(&p.ptr) == 0 {
            if p.has_ec {
                replicas += (p.ec.v >> 13) as u32 & 0xf;
            }
            replicas += 1;
        }
        entry = extent_entry_next_safe(c, entry, ptrs.end);
    }
    replicas
}

pub unsafe fn bch2_bkey_sectors_compressed(c: *const super::types::bch_fs, k: bkey_s_c) -> u32 {
    let ptrs = bch2_bkey_ptrs_c(k);
    if ptrs.start.is_null() || ptrs.end.is_null() {
        return 0;
    }

    let mut crc = bch2_extent_crc_unpack(k.k, core::ptr::null());
    let mut entry = ptrs.start;
    let mut ret: u32 = 0;
    while (entry as usize) < (ptrs.end as usize) {
        match extent_entry_type(entry) {
            x if x == BCH_EXTENT_ENTRY_crc32 as u32
                || x == BCH_EXTENT_ENTRY_crc64 as u32
                || x == BCH_EXTENT_ENTRY_crc128 as u32 =>
            {
                crc = bch2_extent_crc_unpack(k.k, entry.cast::<bch_extent_crc>());
            }
            x if x == BCH_EXTENT_ENTRY_ptr as u32 => {
                if BCH_EXTENT_PTR_CACHED(&(*entry).ptr) == 0 && crc_is_compressed(crc) {
                    ret = ret.wrapping_add(crc.compressed_size);
                }
            }
            _ => {}
        }
        entry = extent_entry_next_safe(c, entry, ptrs.end);
    }
    ret
}

pub unsafe fn bch2_bkey_nr_dirty_ptrs(c: *const super::types::bch_fs, k: bkey_s_c) -> u32 {
    let ptrs = bch2_bkey_ptrs_c(k);
    if ptrs.start.is_null() || ptrs.end.is_null() {
        return 0;
    }
    let mut entry = ptrs.start;
    let mut ret = 0;
    while (entry as usize) < (ptrs.end as usize) {
        if extent_entry_type(entry) == BCH_EXTENT_ENTRY_ptr as u32 {
            let ptr = &(*entry).ptr;
            ret += (BCH_EXTENT_PTR_CACHED(ptr) == 0
                && BCH_EXTENT_PTR_DEV(ptr) != crate::sb::BCH_SB_MEMBER_INVALID as u64)
                as u32;
        }
        entry = extent_entry_next_safe(c, entry, ptrs.end);
    }
    ret
}

pub unsafe fn bch2_bkey_nr_ptrs_allocated(c: *const super::types::bch_fs, k: bkey_s_c) -> u32 {
    if k.k.is_null() || k.v.is_null() {
        return 0;
    }
    if (*k.k).type_ == KEY_TYPE_reservation {
        return (*k.v.cast::<bch_reservation>()).nr_replicas as u32;
    }

    let ptrs = bch2_bkey_ptrs_c(k);
    if ptrs.start.is_null() || ptrs.end.is_null() {
        return 0;
    }
    let mut entry = ptrs.start;
    let mut ret = 0;
    while (entry as usize) < (ptrs.end as usize) {
        if extent_entry_type(entry) == BCH_EXTENT_ENTRY_ptr as u32 {
            ret += (BCH_EXTENT_PTR_CACHED(&(*entry).ptr) == 0) as u32;
        }
        entry = extent_entry_next_safe(c, entry, ptrs.end);
    }
    ret
}

pub unsafe fn bch2_bkey_nr_ptrs_fully_allocated(
    c: *const super::types::bch_fs,
    k: bkey_s_c,
) -> u32 {
    if k.k.is_null() || k.v.is_null() {
        return 0;
    }
    if (*k.k).type_ == KEY_TYPE_reservation {
        return (*k.v.cast::<bch_reservation>()).nr_replicas as u32;
    }

    let ptrs = bch2_bkey_ptrs_c(k);
    if ptrs.start.is_null() || ptrs.end.is_null() {
        return 0;
    }
    let mut crc = bch2_extent_crc_unpack(k.k, core::ptr::null());
    let mut entry = ptrs.start;
    let mut ret = 0;
    while (entry as usize) < (ptrs.end as usize) {
        match extent_entry_type(entry) {
            x if x == BCH_EXTENT_ENTRY_crc32 as u32
                || x == BCH_EXTENT_ENTRY_crc64 as u32
                || x == BCH_EXTENT_ENTRY_crc128 as u32 =>
            {
                crc = bch2_extent_crc_unpack(k.k, entry.cast::<bch_extent_crc>());
            }
            x if x == BCH_EXTENT_ENTRY_ptr as u32 => {
                if BCH_EXTENT_PTR_CACHED(&(*entry).ptr) == 0 && !crc_is_compressed(crc) {
                    ret += 1;
                }
            }
            _ => {}
        }
        entry = extent_entry_next_safe(c, entry, ptrs.end);
    }
    ret
}

pub unsafe fn bkey_extent_is_unwritten(c: *const super::types::bch_fs, k: bkey_s_c) -> bool {
    let ptrs = bch2_bkey_ptrs_c(k);
    if ptrs.start.is_null() || ptrs.end.is_null() {
        return false;
    }
    let mut entry = ptrs.start;
    while (entry as usize) < (ptrs.end as usize) {
        if extent_entry_type(entry) == BCH_EXTENT_ENTRY_ptr as u32
            && BCH_EXTENT_PTR_UNWRITTEN(&(*entry).ptr) != 0
        {
            return true;
        }
        entry = extent_entry_next_safe(c, entry, ptrs.end);
    }
    false
}

pub const fn bkey_extent_is_direct_data(k: &bkey) -> bool {
    matches!(
        k.type_,
        KEY_TYPE_btree_ptr | KEY_TYPE_btree_ptr_v2 | KEY_TYPE_extent | KEY_TYPE_reflink_v
    )
}

pub const fn bch2_extent_ptr_eq(ptr1: bch_extent_ptr, ptr2: bch_extent_ptr) -> bool {
    BCH_EXTENT_PTR_CACHED(&ptr1) == BCH_EXTENT_PTR_CACHED(&ptr2)
        && BCH_EXTENT_PTR_UNWRITTEN(&ptr1) == BCH_EXTENT_PTR_UNWRITTEN(&ptr2)
        && BCH_EXTENT_PTR_OFFSET(&ptr1) == BCH_EXTENT_PTR_OFFSET(&ptr2)
        && BCH_EXTENT_PTR_DEV(&ptr1) == BCH_EXTENT_PTR_DEV(&ptr2)
        && BCH_EXTENT_PTR_GEN(&ptr1) == BCH_EXTENT_PTR_GEN(&ptr2)
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum bch_extent_overlap {
    BCH_EXTENT_OVERLAP_ALL = 0,
    BCH_EXTENT_OVERLAP_BACK = 1,
    BCH_EXTENT_OVERLAP_FRONT = 2,
    BCH_EXTENT_OVERLAP_MIDDLE = 3,
}

pub const fn bch2_extent_overlap(k: &bkey, m: &bkey) -> bch_extent_overlap {
    let cmp1 = bpos_lt(k.p, m.p) as u8;
    let cmp2 = bpos_gt(bkey_start_pos(k), bkey_start_pos(m)) as u8;
    match (cmp1 << 1) | cmp2 {
        0 => bch_extent_overlap::BCH_EXTENT_OVERLAP_ALL,
        1 => bch_extent_overlap::BCH_EXTENT_OVERLAP_BACK,
        2 => bch_extent_overlap::BCH_EXTENT_OVERLAP_FRONT,
        _ => bch_extent_overlap::BCH_EXTENT_OVERLAP_MIDDLE,
    }
}

pub const fn bkey_is_btree_ptr(k: &bkey) -> bool {
    matches!(k.type_, KEY_TYPE_btree_ptr | KEY_TYPE_btree_ptr_v2)
}

pub const fn bkey_is_user_data(k: &bkey) -> bool {
    matches!(
        k.type_,
        KEY_TYPE_extent | KEY_TYPE_inline_data | KEY_TYPE_reservation
    )
}

pub const fn bkey_extent_is_inline_data(k: &bkey) -> bool {
    k.type_ == KEY_TYPE_inline_data || k.type_ == KEY_TYPE_indirect_inline_data
}

pub unsafe fn bkey_inline_data_offset(k: *const bkey) -> usize {
    match (*k).type_ {
        KEY_TYPE_inline_data => core::mem::size_of::<bch_inline_data>(),
        KEY_TYPE_indirect_inline_data => core::mem::size_of::<bch_indirect_inline_data>(),
        _ => panic!("invalid inline data key type"),
    }
}

pub unsafe fn bkey_inline_data_bytes(k: *const bkey) -> usize {
    bkey_val_bytes(&*k) as usize - bkey_inline_data_offset(k)
}

pub const fn bkey_extent_is_data(k: &bkey) -> bool {
    bkey_extent_is_direct_data(k) || bkey_extent_is_inline_data(k) || k.type_ == KEY_TYPE_reflink_p
}

pub const fn bkey_extent_is_allocation(k: &bkey) -> bool {
    matches!(
        k.type_,
        KEY_TYPE_extent
            | KEY_TYPE_reservation
            | KEY_TYPE_reflink_p
            | KEY_TYPE_reflink_v
            | KEY_TYPE_inline_data
            | KEY_TYPE_indirect_inline_data
            | KEY_TYPE_error
    )
}

pub unsafe fn bkey_extent_is_reservation(c: *const super::types::bch_fs, k: bkey_s_c) -> bool {
    !k.k.is_null() && ((*k.k).type_ == KEY_TYPE_reservation || bkey_extent_is_unwritten(c, k))
}

pub unsafe fn bch2_bkey_is_incompressible(c: *const super::types::bch_fs, k: bkey_s_c) -> bool {
    let ptrs = bch2_bkey_ptrs_c(k);
    if ptrs.start.is_null() || ptrs.end.is_null() {
        return false;
    }
    let mut entry = ptrs.start;
    while (entry as usize) < (ptrs.end as usize) {
        let type_ = extent_entry_type(entry);
        if type_ == BCH_EXTENT_ENTRY_crc32 as u32
            || type_ == BCH_EXTENT_ENTRY_crc64 as u32
            || type_ == BCH_EXTENT_ENTRY_crc128 as u32
        {
            let crc = bch2_extent_crc_unpack(k.k, entry.cast::<bch_extent_crc>());
            if crc.compression_type == BCH_COMPRESSION_TYPE_incompressible {
                return true;
            }
        }
        entry = extent_entry_next_safe(c, entry, ptrs.end);
    }
    false
}

pub unsafe fn bch2_bkey_can_read(c: *const super::types::bch_fs, k: bkey_s_c) -> bool {
    let ptrs = bch2_bkey_ptrs_c(k);
    let mut entry = ptrs.start;
    let mut crc = bch2_extent_crc_unpack(k.k, core::ptr::null());
    while !entry.is_null() && (entry as usize) < (ptrs.end as usize) {
        let mut p = extent_ptr_decoded {
            crc,
            ..Default::default()
        };
        let mut found = false;
        while (entry as usize) < (ptrs.end as usize) {
            match extent_entry_type(entry) {
                x if x == BCH_EXTENT_ENTRY_ptr as u32 => {
                    p.ptr = (*entry).ptr;
                    found = true;
                    break;
                }
                x if x == BCH_EXTENT_ENTRY_crc32 as u32
                    || x == BCH_EXTENT_ENTRY_crc64 as u32
                    || x == BCH_EXTENT_ENTRY_crc128 as u32 =>
                {
                    p.crc = bch2_extent_crc_unpack(k.k, entry.cast());
                    crc = p.crc;
                }
                x if x == BCH_EXTENT_ENTRY_stripe_ptr as u32 => {
                    p.ec = (*entry).stripe_ptr;
                    p.has_ec = true;
                }
                _ => {}
            }
            entry = extent_entry_next_safe(c, entry, ptrs.end);
        }
        if !found {
            break;
        }
        if BCH_EXTENT_PTR_CACHED(&p.ptr) == 0
            && (BCH_EXTENT_PTR_DEV(&p.ptr) as u32 != crate::sb::BCH_SB_MEMBER_INVALID as u32
                || p.has_ec)
        {
            return true;
        }
        entry = extent_entry_next_safe(c, entry, ptrs.end);
    }
    false
}

pub unsafe fn bch2_bkey_propagate_incompressible(
    c: *const super::types::bch_fs,
    dst: *mut bkey_i,
    src: bkey_s_c,
) {
    if dst.is_null() || !bch2_bkey_is_incompressible(c, src) {
        return;
    }
    let ptrs = bch2_bkey_ptrs(bkey_s {
        k: &mut (*dst).k,
        v: &mut (*dst).v,
    });
    if ptrs.start.is_null() || ptrs.end.is_null() {
        return;
    }
    let mut entry = ptrs.start;
    while (entry as usize) < (ptrs.end as usize) {
        let type_ = extent_entry_type(entry);
        if type_ == BCH_EXTENT_ENTRY_crc32 as u32
            || type_ == BCH_EXTENT_ENTRY_crc64 as u32
            || type_ == BCH_EXTENT_ENTRY_crc128 as u32
        {
            let mut crc = bch2_extent_crc_unpack(&(*dst).k, entry.cast::<bch_extent_crc>());
            if crc.compression_type == BCH_COMPRESSION_TYPE_none {
                crc.compression_type = BCH_COMPRESSION_TYPE_incompressible;
                bch2_extent_crc_pack(entry.cast::<bch_extent_crc>(), crc, type_ as u8);
            }
        }
        entry = extent_entry_next_safe(c, entry, ptrs.end).cast_mut();
    }
}

pub unsafe fn bch2_bkey_append_ptr(
    _c: *const super::types::bch_fs,
    k: *mut bkey_i,
    mut ptr: bch_extent_ptr,
) {
    assert_eq!((*k).k.type_, KEY_TYPE_btree_ptr_v2);
    SET_BCH_EXTENT_PTR_TYPE(&mut ptr, 1);
    let dest = (&mut (*k).v as *mut bch_val)
        .cast::<u8>()
        .add(bkey_val_bytes(&(*k).k))
        .cast::<bch_extent_ptr>();
    *dest = ptr;
    (*k).k.u64s += 1;
}

pub unsafe fn bch2_cut_front_s(_c: *const super::types::bch_fs, where_: bpos, k: bkey_s) -> i32 {
    if k.k.is_null() {
        return -22;
    }
    let start = bkey_start_pos(&*k.k);
    if bpos_le(where_, start) {
        return 0;
    }
    assert!(!bpos_gt(where_, (*k.k).p));

    let sub = where_.offset - start.offset;
    let mut new_val_u64s = bkey_val_u64s(&*k.k);
    (*k.k).size -= sub as u32;
    if (*k.k).size == 0 {
        (*k.k).type_ = KEY_TYPE_deleted;
        new_val_u64s = 0;
    }

    match (*k.k).type_ {
        KEY_TYPE_extent | KEY_TYPE_reflink_v => {
            let ptrs = bch2_bkey_ptrs(k);
            let mut entry = ptrs.start;
            let mut seen_crc = false;
            while !entry.is_null() && (entry as usize) < (ptrs.end as usize) {
                match extent_entry_type(entry) {
                    x if x == BCH_EXTENT_ENTRY_ptr as u32 => {
                        if !seen_crc {
                            let ptr = &mut (*entry).ptr;
                            SET_BCH_EXTENT_PTR_OFFSET(
                                ptr,
                                BCH_EXTENT_PTR_OFFSET(ptr).wrapping_add(sub),
                            );
                        }
                    }
                    x if x == BCH_EXTENT_ENTRY_crc32 as u32 => {
                        let crc = &mut (*entry).crc32;
                        crc.word0 = (crc.word0 & !(0x7f << 16))
                            | (((((crc.word0 >> 16) & 0x7f).wrapping_add(sub as u32)) & 0x7f)
                                << 16);
                        seen_crc = true;
                    }
                    x if x == BCH_EXTENT_ENTRY_crc64 as u32 => {
                        let crc = &mut (*entry).crc64;
                        crc.word0 = (crc.word0 & !(0x1ff << 21))
                            | (((((crc.word0 >> 21) & 0x1ff).wrapping_add(sub)) & 0x1ff) << 21);
                        seen_crc = true;
                    }
                    x if x == BCH_EXTENT_ENTRY_crc128 as u32 => {
                        let crc = &mut (*entry).crc128;
                        crc.word0 = (crc.word0 & !(0x1fff << 30))
                            | (((((crc.word0 >> 30) & 0x1fff).wrapping_add(sub)) & 0x1fff) << 30);
                        seen_crc = true;
                    }
                    _ => {}
                }
                let type_ = extent_entry_type(entry);
                let u64s = extent_entry_u64s_known(type_ as u8);
                if u64s == 0 {
                    break;
                }
                entry = (entry.cast::<u8>()).add((u64s * 8) as usize).cast();
            }
        }
        _ => {}
    }

    let val_u64s_delta = bkey_val_u64s(&*k.k) - new_val_u64s;
    set_bkey_val_u64s(&mut *k.k, new_val_u64s);
    if val_u64s_delta != 0 && !k.v.is_null() {
        core::ptr::write_bytes(
            k.v.cast::<u8>().add((new_val_u64s * 8) as usize),
            0,
            (val_u64s_delta * 8) as usize,
        );
    }
    -(val_u64s_delta as i32)
}

pub unsafe fn bch2_cut_back_s(where_: bpos, k: bkey_s) -> i32 {
    if k.k.is_null() {
        return -22;
    }
    if bpos_ge(where_, (*k.k).p) {
        return 0;
    }
    assert!(bpos_ge(where_, bkey_start_pos(&*k.k)));

    let len = where_.offset - bkey_start_pos(&*k.k).offset;
    let new_val_u64s = if len == 0 { 0 } else { bkey_val_u64s(&*k.k) };
    (*k.k).p.offset = where_.offset;
    (*k.k).size = len as u32;
    if len == 0 {
        (*k.k).type_ = KEY_TYPE_deleted;
    }
    let val_u64s_delta = bkey_val_u64s(&*k.k) - new_val_u64s;
    set_bkey_val_u64s(&mut *k.k, new_val_u64s);
    if val_u64s_delta != 0 && !k.v.is_null() {
        core::ptr::write_bytes(
            k.v.cast::<u8>().add((new_val_u64s * 8) as usize),
            0,
            (val_u64s_delta * 8) as usize,
        );
    }
    -(val_u64s_delta as i32)
}

pub unsafe fn bch2_cut_front(c: *const super::types::bch_fs, where_: bpos, k: *mut bkey_i) {
    let _ = bch2_cut_front_s(
        c,
        where_,
        bkey_s {
            k: &mut (*k).k,
            v: &mut (*k).v,
        },
    );
}

pub unsafe fn bch2_cut_back(where_: bpos, k: *mut bkey_i) {
    let _ = bch2_cut_back_s(
        where_,
        bkey_s {
            k: &mut (*k).k,
            v: &mut (*k).v,
        },
    );
}

pub const fn BSET_CSUM_TYPE(i: &bset) -> u32 {
    i.flags & 0xf
}

pub const fn SET_BSET_CSUM_TYPE(i: &mut bset, v: u32) {
    i.flags = (i.flags & !0xf) | (v & 0xf);
}

pub const fn BSET_BIG_ENDIAN(i: &bset) -> u32 {
    (i.flags >> 4) & 1
}

pub const fn SET_BSET_BIG_ENDIAN(i: &mut bset, v: u32) {
    i.flags = (i.flags & !(1 << 4)) | ((v & 1) << 4);
}

pub const fn BSET_SEPARATE_WHITEOUTS(i: &bset) -> u32 {
    (i.flags >> 5) & 1
}

pub const fn SET_BSET_SEPARATE_WHITEOUTS(i: &mut bset, v: u32) {
    i.flags = (i.flags & !(1 << 5)) | ((v & 1) << 5);
}

pub const fn BSET_OFFSET(i: &bset) -> u32 {
    i.flags >> 16
}

pub const fn SET_BSET_OFFSET(i: &mut bset, v: u32) {
    i.flags = (i.flags & 0xffff) | ((v & 0xffff) << 16);
}

pub const fn BTREE_NODE_ID_LO(n: &btree_node) -> u64 {
    n.flags & 0xf
}

pub const fn SET_BTREE_NODE_ID_LO(n: &mut btree_node, v: u64) {
    n.flags = (n.flags & !0xf) | (v & 0xf);
}

pub const fn BTREE_NODE_LEVEL(n: &btree_node) -> u64 {
    (n.flags >> 4) & 0xf
}

pub const fn SET_BTREE_NODE_LEVEL(n: &mut btree_node, v: u64) {
    n.flags = (n.flags & !(0xf << 4)) | ((v & 0xf) << 4);
}

pub const fn BTREE_NODE_NEW_EXTENT_OVERWRITE(n: &btree_node) -> u64 {
    (n.flags >> 8) & 1
}

pub const fn SET_BTREE_NODE_NEW_EXTENT_OVERWRITE(n: &mut btree_node, v: u64) {
    n.flags = (n.flags & !(1 << 8)) | ((v & 1) << 8);
}

pub const fn BTREE_NODE_ID_HI(n: &btree_node) -> u64 {
    (n.flags >> 9) & 0xffff
}

pub const fn SET_BTREE_NODE_ID_HI(n: &mut btree_node, v: u64) {
    n.flags = (n.flags & !(0xffff << 9)) | ((v & 0xffff) << 9);
}

pub const fn BTREE_NODE_SEQ(n: &btree_node) -> u64 {
    n.flags >> 32
}

pub const fn SET_BTREE_NODE_SEQ(n: &mut btree_node, v: u64) {
    n.flags = (n.flags & u32::MAX as u64) | ((v & u32::MAX as u64) << 32);
}

pub const fn BTREE_NODE_ID(n: &btree_node) -> u64 {
    BTREE_NODE_ID_LO(n) | (BTREE_NODE_ID_HI(n) << 4)
}

pub const fn SET_BTREE_NODE_ID(n: &mut btree_node, v: u64) {
    SET_BTREE_NODE_ID_LO(n, v);
    SET_BTREE_NODE_ID_HI(n, v >> 4);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bcachefs_dev_idx_is_online_reads_local_device_mask() {
        let mut c = super::super::types::bch_fs::default();
        c.devs_online.d[0] = (1usize << 3) | (1usize << 7);
        unsafe {
            assert!(bch2_dev_idx_is_online(&c, 3));
            assert!(bch2_dev_idx_is_online(&c, 7));
            assert!(!bch2_dev_idx_is_online(&c, 4));
        }
    }

    #[test]
    fn bcachefs_bset_and_node_layout() {
        assert_eq!(core::mem::size_of::<bch_csum>(), 16);
        assert_eq!(core::mem::size_of::<bch_extent_ptr>(), 8);
        assert_eq!(core::mem::size_of::<bset>(), 24);
        assert_eq!(core::mem::align_of::<bset>(), 8);
        assert_eq!(core::mem::offset_of!(btree_node, min_key), 32);
        assert_eq!(core::mem::offset_of!(btree_node, max_key), 52);
        assert_eq!(core::mem::offset_of!(btree_node, _ptr), 72);
        assert_eq!(core::mem::offset_of!(btree_node, format), 80);
        assert_eq!(core::mem::offset_of!(btree_node, keys), 136);
        assert_eq!(core::mem::size_of::<btree_node>(), 160);
        assert_eq!(core::mem::align_of::<btree_node>(), 8);
        assert_eq!(core::mem::size_of::<btree_node_entry>(), 40);
        assert_eq!(core::mem::size_of::<bkey_float>(), 4);
        assert_eq!(core::mem::size_of::<rw_aux_tree>(), 24);
        assert_eq!(core::mem::size_of::<bch_btree_ptr_v2>(), 40);
        assert_eq!(core::mem::size_of::<bkey_i_btree_ptr_v2>(), 80);
    }

    #[test]
    fn bcachefs_whiteout_key_predicates() {
        let mut key = super::super::bkey::bkey_packed::default();
        key.type_ = KEY_TYPE_deleted;
        assert!(bkey_whiteout(&key));
        assert!(bkey_extent_whiteout(&key));

        key.type_ = KEY_TYPE_whiteout;
        assert!(bkey_whiteout(&key));
        assert!(bkey_extent_whiteout(&key));

        key.type_ = KEY_TYPE_extent_whiteout;
        assert!(!bkey_whiteout(&key));
        assert!(bkey_extent_whiteout(&key));

        key.type_ = KEY_TYPE_extent;
        assert!(!bkey_whiteout(&key));
        assert!(!bkey_extent_whiteout(&key));
    }

    #[test]
    fn bcachefs_key_type_numbers_match_format() {
        let types = [
            KEY_TYPE_deleted,
            KEY_TYPE_whiteout,
            KEY_TYPE_error,
            KEY_TYPE_cookie,
            KEY_TYPE_hash_whiteout,
            KEY_TYPE_btree_ptr,
            KEY_TYPE_extent,
            KEY_TYPE_reservation,
            KEY_TYPE_inode,
            KEY_TYPE_inode_generation,
            KEY_TYPE_dirent,
            KEY_TYPE_xattr,
            KEY_TYPE_alloc,
            KEY_TYPE_quota,
            KEY_TYPE_stripe,
            KEY_TYPE_reflink_p,
            KEY_TYPE_reflink_v,
            KEY_TYPE_inline_data,
            KEY_TYPE_btree_ptr_v2,
            KEY_TYPE_indirect_inline_data,
            KEY_TYPE_alloc_v2,
            KEY_TYPE_subvolume,
            KEY_TYPE_snapshot,
            KEY_TYPE_inode_v2,
            KEY_TYPE_alloc_v3,
            KEY_TYPE_set,
            KEY_TYPE_lru,
            KEY_TYPE_alloc_v4,
            KEY_TYPE_backpointer,
            KEY_TYPE_inode_v3,
            KEY_TYPE_bucket_gens,
            KEY_TYPE_snapshot_tree,
            KEY_TYPE_logged_op_truncate,
            KEY_TYPE_logged_op_finsert,
            KEY_TYPE_accounting,
            KEY_TYPE_inode_alloc_cursor,
            KEY_TYPE_extent_whiteout,
            KEY_TYPE_logged_op_stripe_update,
        ];
        assert_eq!(types, core::array::from_fn(|i| i as u8));
        assert_eq!(KEY_TYPE_MAX, types.len() as u8);
        assert_eq!(BKEY_TYPE_strict_btree_checks, 1);
        assert_eq!(bch2_bkey_type_flags.len(), KEY_TYPE_MAX as usize);
        assert_eq!(bch2_bkey_type_flags[KEY_TYPE_deleted as usize], 0);
        assert_eq!(
            bch2_bkey_type_flags[KEY_TYPE_extent as usize],
            BKEY_TYPE_strict_btree_checks
        );
        assert_eq!(bch2_bkey_type_flags[KEY_TYPE_set as usize], 0);
        assert_eq!(
            bch2_bkey_type_flags[KEY_TYPE_extent_whiteout as usize],
            BKEY_TYPE_strict_btree_checks
        );
    }

    #[test]
    fn bcachefs_bset_and_node_bitfields() {
        let mut set = bset::default();
        SET_BSET_CSUM_TYPE(&mut set, 9);
        SET_BSET_BIG_ENDIAN(&mut set, 1);
        SET_BSET_SEPARATE_WHITEOUTS(&mut set, 1);
        SET_BSET_OFFSET(&mut set, 0xabcd);
        assert_eq!(BSET_CSUM_TYPE(&set), 9);
        assert_eq!(BSET_BIG_ENDIAN(&set), 1);
        assert_eq!(BSET_SEPARATE_WHITEOUTS(&set), 1);
        assert_eq!(BSET_OFFSET(&set), 0xabcd);

        let mut node = btree_node::default();
        SET_BTREE_NODE_ID(&mut node, 0xabcde);
        SET_BTREE_NODE_LEVEL(&mut node, 3);
        SET_BTREE_NODE_NEW_EXTENT_OVERWRITE(&mut node, 1);
        SET_BTREE_NODE_SEQ(&mut node, 0x1234_5678);
        assert_eq!(BTREE_NODE_ID(&node), 0xabcde);
        assert_eq!(BTREE_NODE_LEVEL(&node), 3);
        assert_eq!(BTREE_NODE_NEW_EXTENT_OVERWRITE(&node), 1);
        assert_eq!(BTREE_NODE_SEQ(&node), 0x1234_5678);
    }

    #[test]
    fn bcachefs_extent_ptr_bits_and_btree_ptr_range() {
        assert_eq!(core::mem::size_of::<bch_stripe>(), 8);
        let mut ptr = bch_extent_ptr::default();
        SET_BCH_EXTENT_PTR_TYPE(&mut ptr, 1);
        SET_BCH_EXTENT_PTR_CACHED(&mut ptr, 1);
        SET_BCH_EXTENT_PTR_UNUSED(&mut ptr, 1);
        SET_BCH_EXTENT_PTR_UNWRITTEN(&mut ptr, 1);
        SET_BCH_EXTENT_PTR_OFFSET(&mut ptr, 0x0abc_def0_1234);
        SET_BCH_EXTENT_PTR_DEV(&mut ptr, 0x5a);
        SET_BCH_EXTENT_PTR_GEN(&mut ptr, 0xc3);
        assert_eq!(BCH_EXTENT_PTR_TYPE(&ptr), 1);
        assert_eq!(BCH_EXTENT_PTR_CACHED(&ptr), 1);
        assert_eq!(BCH_EXTENT_PTR_UNUSED(&ptr), 1);
        assert_eq!(BCH_EXTENT_PTR_UNWRITTEN(&ptr), 1);
        assert_eq!(BCH_EXTENT_PTR_OFFSET(&ptr), 0x0abc_def0_1234);
        assert_eq!(BCH_EXTENT_PTR_DEV(&ptr), 0x5a);
        assert_eq!(BCH_EXTENT_PTR_GEN(&ptr), 0xc3);

        unsafe {
            let mut words = [0u64; 11];
            let key = words.as_mut_ptr().cast::<bkey_i_btree_ptr_v2>();
            (*key).k = bkey {
                u64s: 10,
                format: super::super::bkey::KEY_FORMAT_CURRENT,
                type_: KEY_TYPE_btree_ptr_v2,
                ..Default::default()
            };
            bch2_bkey_append_ptr(
                core::ptr::null(),
                key.cast::<bkey_i>(),
                bch_extent_ptr {
                    v: (37 << 4) | (2 << 48) | (9 << 56),
                },
            );
            assert_eq!((*key).k.u64s, 11);
            let ptrs = bch2_bkey_ptrs_c(bkey_s_c {
                k: &(*key).k,
                v: (&(*key).v as *const bch_btree_ptr_v2).cast::<bch_val>(),
            });
            assert_eq!(
                ptrs.end.cast::<u8>().offset_from(ptrs.start.cast::<u8>()),
                8
            );
            let stored = (*ptrs.start).ptr;
            assert_eq!(BCH_EXTENT_PTR_TYPE(&stored), 1);
            assert_eq!(BCH_EXTENT_PTR_OFFSET(&stored), 37);
            assert_eq!(BCH_EXTENT_PTR_DEV(&stored), 2);
            assert_eq!(BCH_EXTENT_PTR_GEN(&stored), 9);
            let devices = bch2_bkey_devs(
                core::ptr::null(),
                bkey_s_c {
                    k: &(*key).k,
                    v: core::ptr::addr_of!((*key).v).cast::<bch_val>(),
                },
            );
            assert_eq!(devices.nr, 1);
            assert_eq!(devices.data[0], 2);
            let mut list = bch2_dev_list_single(2);
            assert!(bch2_dev_list_has_dev(list, 2));
            bch2_dev_list_add_dev(&mut list, 3);
            bch2_dev_list_add_dev(&mut list, 3);
            assert_eq!(list.nr, 2);
            bch2_dev_list_drop_dev(&mut list, 2);
            assert_eq!(list.nr, 1);
            assert_eq!(list.data[0], 3);
            let mask = bch_devs_mask {
                d: [0b1000_0001, 0, 0, 1usize << (usize::BITS - 1)],
            };
            assert_eq!(dev_mask_nr(&mask), 3);
            assert_eq!(
                bch2_bkey_replicas(
                    core::ptr::null_mut(),
                    bkey_s_c {
                        k: &(*key).k,
                        v: core::ptr::addr_of!((*key).v).cast::<bch_val>(),
                    },
                ),
                1
            );
            assert!(bch2_extent_ptr_eq(stored, stored));
            let mut changed = stored;
            SET_BCH_EXTENT_PTR_CACHED(&mut changed, 1);
            assert!(!bch2_extent_ptr_eq(stored, changed));
            assert_eq!(
                bch2_bkey_has_device_c(
                    core::ptr::null(),
                    bkey_s_c {
                        k: &(*key).k,
                        v: core::ptr::addr_of!((*key).v).cast::<bch_val>(),
                    },
                    2,
                ),
                ptrs.start.cast::<bch_extent_ptr>()
            );
            assert_eq!(
                bch2_bkey_has_device(
                    core::ptr::null(),
                    bkey_s {
                        k: &mut (*key).k,
                        v: (&mut (*key).v as *mut bch_btree_ptr_v2).cast::<bch_val>(),
                    },
                    2,
                ),
                ptrs.start.cast::<bch_extent_ptr>().cast_mut()
            );
            assert_eq!(
                bch2_bkey_dev_ptr_bit(
                    core::ptr::null(),
                    bkey_s_c {
                        k: &(*key).k,
                        v: core::ptr::addr_of!((*key).v).cast::<bch_val>(),
                    },
                    2,
                ),
                1
            );
            assert_eq!(
                bch2_bkey_dev_ptr_bit(
                    core::ptr::null(),
                    bkey_s_c {
                        k: &(*key).k,
                        v: core::ptr::addr_of!((*key).v).cast::<bch_val>(),
                    },
                    3,
                ),
                0
            );
            assert!(bch2_bkey_has_device_c(
                core::ptr::null(),
                bkey_s_c {
                    k: &(*key).k,
                    v: core::ptr::addr_of!((*key).v).cast::<bch_val>(),
                },
                3,
            )
            .is_null());
            let mut decoded = extent_ptr_decoded::default();
            assert!(bch2_bkey_has_device_decode(
                core::ptr::null(),
                bkey_s_c {
                    k: &(*key).k,
                    v: core::ptr::addr_of!((*key).v).cast::<bch_val>(),
                },
                2,
                &mut decoded,
            ));
            assert_eq!(BCH_EXTENT_PTR_DEV(&decoded.ptr), 2);
            let mut words2 = [0u64; 11];
            words2.copy_from_slice(&words);
            let key2 = words2.as_mut_ptr().cast::<bkey_i_btree_ptr_v2>();
            (*key).k.size = 4;
            (*key2).k.size = 4;
            (*key).k.p.offset = 4;
            (*key2).k.p.offset = 4;
            assert!(bch2_extents_match(
                core::ptr::null(),
                bkey_s_c {
                    k: &(*key).k,
                    v: core::ptr::addr_of!((*key).v).cast::<bch_val>(),
                },
                bkey_s_c {
                    k: &(*key2).k,
                    v: core::ptr::addr_of!((*key2).v).cast::<bch_val>(),
                },
            ));
            let matched = bch2_extent_has_ptr(
                core::ptr::null(),
                bkey_s_c {
                    k: &(*key).k,
                    v: core::ptr::addr_of!((*key).v).cast::<bch_val>(),
                },
                decoded,
                bkey_s {
                    k: core::ptr::addr_of_mut!((*key2).k),
                    v: core::ptr::addr_of_mut!((*key2).v).cast::<bch_val>(),
                },
            );
            assert!(!matched.is_null());
            assert_eq!(BCH_EXTENT_PTR_DEV(&*matched), 2);
            assert!(bch2_bkey_matches_ptr(
                core::ptr::null(),
                bkey_s_c {
                    k: &(*key).k,
                    v: core::ptr::addr_of!((*key).v).cast::<bch_val>(),
                },
                stored,
                0,
            ));
            assert!(!bch2_bkey_matches_ptr(
                core::ptr::null(),
                bkey_s_c {
                    k: &(*key).k,
                    v: core::ptr::addr_of!((*key).v).cast::<bch_val>(),
                },
                stored,
                1,
            ));

            let mut stripe_words = [0u64; 4];
            let stripe = stripe_words.as_mut_ptr().cast::<bch_stripe>();
            (*stripe).nr_blocks = 2;
            let stripe_key = bkey {
                type_: KEY_TYPE_stripe,
                u64s: super::super::bkey::BKEY_U64S + 3,
                ..Default::default()
            };
            let stripe_ptrs = bch2_bkey_ptrs_c(bkey_s_c {
                k: &stripe_key,
                v: stripe.cast::<bch_val>(),
            });
            assert_eq!(
                stripe_ptrs
                    .end
                    .cast::<u8>()
                    .offset_from(stripe_ptrs.start.cast::<u8>()),
                16
            );
        }
    }

    #[test]
    fn bcachefs_extent_pointer_match_checks_disk_overlap_and_generation() {
        unsafe {
            let key1 = bkey {
                size: 4,
                p: bpos {
                    offset: 4,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut key2 = key1;
            key2.p.offset = 6;
            let mut ptr1 = bch_extent_ptr::default();
            let mut ptr2 = bch_extent_ptr::default();
            SET_BCH_EXTENT_PTR_OFFSET(&mut ptr1, 100);
            SET_BCH_EXTENT_PTR_OFFSET(&mut ptr2, 102);
            SET_BCH_EXTENT_PTR_DEV(&mut ptr1, 2);
            SET_BCH_EXTENT_PTR_DEV(&mut ptr2, 2);
            SET_BCH_EXTENT_PTR_GEN(&mut ptr1, 7);
            SET_BCH_EXTENT_PTR_GEN(&mut ptr2, 7);
            let p1 = extent_ptr_decoded {
                ptr: ptr1,
                crc: bch_extent_crc_unpacked {
                    compressed_size: 4,
                    ..Default::default()
                },
                ..Default::default()
            };
            let p2 = extent_ptr_decoded {
                ptr: ptr2,
                crc: bch_extent_crc_unpacked {
                    compressed_size: 4,
                    ..Default::default()
                },
                ..Default::default()
            };
            assert!(bch2_bkey_ptrs_match(
                bkey_s_c {
                    k: &key1,
                    v: core::ptr::null(),
                },
                p1,
                bkey_s_c {
                    k: &key2,
                    v: core::ptr::null(),
                },
                p2,
            ));
            SET_BCH_EXTENT_PTR_GEN(&mut ptr2, 8);
            assert!(!bch2_bkey_ptrs_match(
                bkey_s_c {
                    k: &key1,
                    v: core::ptr::null(),
                },
                p1,
                bkey_s_c {
                    k: &key2,
                    v: core::ptr::null(),
                },
                extent_ptr_decoded { ptr: ptr2, ..p2 },
            ));
            let front = bkey {
                size: 4,
                p: bpos {
                    offset: 10,
                    ..Default::default()
                },
                ..Default::default()
            };
            let back = bkey {
                size: 4,
                p: bpos {
                    offset: 8,
                    ..Default::default()
                },
                ..Default::default()
            };
            assert_eq!(
                bch2_extent_overlap(&front, &back),
                bch_extent_overlap::BCH_EXTENT_OVERLAP_BACK
            );
        }
    }

    #[test]
    fn bcachefs_extent_entry_layout_and_types() {
        assert_eq!(core::mem::size_of::<bch_extent_crc32>(), 8);
        assert_eq!(core::mem::size_of::<bch_extent_crc64>(), 16);
        assert_eq!(core::mem::size_of::<bch_extent_crc128>(), 24);
        assert_eq!(core::mem::size_of::<bch_extent_rebalance_v1>(), 8);
        assert_eq!(core::mem::size_of::<bch_extent_crc>(), 24);
        assert_eq!(core::mem::size_of::<bch_extent_entry>(), 24);
        assert_eq!(
            (0..BCH_EXTENT_ENTRY_MAX)
                .map(extent_entry_u64s_known)
                .collect::<Vec<_>>(),
            vec![1, 1, 2, 3, 1, 1, 1, 1, 1]
        );
        unsafe {
            let mut entry = bch_extent_entry {
                type_: 1 << BCH_EXTENT_ENTRY_crc64,
            };
            assert_eq!(extent_entry_type(&entry), BCH_EXTENT_ENTRY_crc64 as u32);
            assert!(!extent_entry_is_ptr(&entry));
            assert!(extent_entry_is_crc(&entry));
            entry.type_ = 1 << BCH_EXTENT_ENTRY_ptr;
            assert!(extent_entry_is_ptr(&entry));
            entry.type_ = 1 << BCH_EXTENT_ENTRY_stripe_ptr;
            assert!(extent_entry_is_stripe_ptr(&entry));
            assert!(!extent_entry_is_crc(&entry));
        }
    }

    #[test]
    fn bcachefs_extent_entry_next_uses_known_u64_sizes() {
        unsafe {
            let mut words = [0u64; 5];
            let first = words.as_mut_ptr().cast::<bch_extent_entry>();
            (*first).type_ = 1 << BCH_EXTENT_ENTRY_crc64;
            let next = extent_entry_next(core::ptr::null(), first);
            assert_eq!(next.cast::<u8>().offset_from(first.cast::<u8>()), 16);

            let end = first.add(5);
            let unknown = bch_extent_entry { type_: 0 };
            assert_eq!(
                extent_entry_next_safe(core::ptr::null(), &unknown, end),
                end
            );
        }
    }

    #[test]
    fn bcachefs_extent_entry_insert_and_drop_update_packed_value() {
        unsafe {
            let mut words = [0u64; 12];
            let key = words.as_mut_ptr().cast::<bkey_i>();
            (*key).k = bkey {
                u64s: super::super::bkey::BKEY_U64S + 2,
                type_: KEY_TYPE_extent,
                ..Default::default()
            };
            let first = core::ptr::addr_of_mut!((*key).v).cast::<bch_extent_entry>();
            (*first).type_ = 1 << BCH_EXTENT_ENTRY_crc64;
            (*first).crc64.word0 = 1 << BCH_EXTENT_ENTRY_crc64;
            let inserted = bch_extent_entry {
                type_: 1 << BCH_EXTENT_ENTRY_ptr,
            };
            __extent_entry_insert(core::ptr::null(), key, first, &inserted);
            assert_eq!((*key).k.u64s, super::super::bkey::BKEY_U64S + 3);
            assert_eq!((*first).type_, 1 << BCH_EXTENT_ENTRY_ptr);
            assert_eq!(
                extent_entry_type(first.cast::<u64>().add(1).cast()),
                BCH_EXTENT_ENTRY_crc64 as u32
            );

            extent_entry_drop(
                core::ptr::null(),
                bkey_s {
                    k: &mut (*key).k,
                    v: core::ptr::addr_of_mut!((*key).v),
                },
                first,
            );
            assert_eq!((*key).k.u64s, super::super::bkey::BKEY_U64S + 2);
            assert_eq!((*first).type_, 1 << BCH_EXTENT_ENTRY_crc64);
            assert_eq!((*first).crc64.word0, 1 << BCH_EXTENT_ENTRY_crc64);
        }
    }

    #[test]
    fn bcachefs_extent_flags_read_from_leading_flags_entry() {
        unsafe {
            let mut words = [0u64; 8];
            let key = words.as_mut_ptr().cast::<bkey_i>();
            (*key).k = bkey {
                u64s: super::super::bkey::BKEY_U64S + 1,
                type_: KEY_TYPE_extent,
                ..Default::default()
            };
            let value = core::ptr::addr_of_mut!((*key).v).cast::<bch_extent_flags>();
            (*value).v = (1 << BCH_EXTENT_ENTRY_flags) | (0x123 << 7);
            assert_eq!(
                bch2_bkey_extent_flags(bkey_s_c {
                    k: &(*key).k,
                    v: core::ptr::addr_of!((*key).v),
                }),
                0x123
            );
        }
    }

    #[test]
    fn bcachefs_bkey_sectors_compressed_counts_uncached_pointers() {
        unsafe {
            assert_eq!(core::mem::size_of::<bch_reservation>(), 8);
            let mut words = [0u64; 8];
            let key = words.as_mut_ptr().cast::<bkey_i>();
            (*key).k = bkey {
                u64s: super::super::bkey::BKEY_U64S + 2,
                type_: KEY_TYPE_extent,
                ..Default::default()
            };
            let crc = core::ptr::addr_of_mut!((*key).v).cast::<bch_extent_crc>();
            bch2_extent_crc_pack(
                crc,
                bch_extent_crc_unpacked {
                    compressed_size: 7,
                    uncompressed_size: 9,
                    live_size: 4,
                    csum_type: crate::checksum::BCH_CSUM_crc32c as u8,
                    compression_type: 2,
                    ..Default::default()
                },
                BCH_EXTENT_ENTRY_crc32,
            );
            let ptr = crc.cast::<u64>().add(1).cast::<bch_extent_ptr>();
            *ptr = bch_extent_ptr::default();
            SET_BCH_EXTENT_PTR_TYPE(&mut *ptr, 1);
            SET_BCH_EXTENT_PTR_DEV(&mut *ptr, 1);
            SET_BCH_EXTENT_PTR_UNWRITTEN(&mut *ptr, 1);
            let key_sc = bkey_s_c {
                k: &(*key).k,
                v: core::ptr::addr_of!((*key).v),
            };
            assert_eq!(bch2_bkey_nr_dirty_ptrs(core::ptr::null(), key_sc), 1);
            assert_eq!(
                bch2_bkey_nr_ptrs_fully_allocated(core::ptr::null(), key_sc),
                0
            );
            assert!(bkey_extent_is_unwritten(core::ptr::null(), key_sc));
            let reservation = bch_reservation {
                nr_replicas: 3,
                ..Default::default()
            };
            let reservation_key = bkey {
                type_: KEY_TYPE_reservation,
                ..Default::default()
            };
            assert_eq!(
                bch2_bkey_nr_ptrs_allocated(
                    core::ptr::null(),
                    bkey_s_c {
                        k: &reservation_key,
                        v: (&reservation.v as *const bch_val),
                    },
                ),
                3
            );
            assert_eq!(bch2_bkey_sectors_compressed(core::ptr::null(), key_sc,), 7);
        }
    }

    #[test]
    fn bcachefs_narrow_crc_moves_following_pointer_and_repacks_crc() {
        unsafe {
            let mut words = [0u64; 10];
            let key = words.as_mut_ptr().cast::<bkey_i>();
            (*key).k = bkey {
                u64s: super::super::bkey::BKEY_U64S + 2,
                size: 4,
                type_: KEY_TYPE_extent,
                ..Default::default()
            };
            let crc = core::ptr::addr_of_mut!((*key).v).cast::<bch_extent_crc>();
            let old = bch_extent_crc_unpacked {
                compressed_size: 4,
                uncompressed_size: 4,
                live_size: 4,
                offset: 2,
                csum_type: crate::checksum::BCH_CSUM_crc32c as u8,
                ..Default::default()
            };
            bch2_extent_crc_pack(crc, old, BCH_EXTENT_ENTRY_crc32);
            let ptr = crc.cast::<u64>().add(1).cast::<bch_extent_ptr>();
            SET_BCH_EXTENT_PTR_TYPE(&mut *ptr, 1 << BCH_EXTENT_ENTRY_ptr);
            SET_BCH_EXTENT_PTR_OFFSET(&mut *ptr, 10);
            let new = bch_extent_crc_unpacked {
                compressed_size: 4,
                uncompressed_size: 4,
                live_size: 4,
                csum_type: crate::checksum::BCH_CSUM_crc32c as u8,
                ..Default::default()
            };
            assert!(bch2_bkey_narrow_crc(core::ptr::null(), key, old, new));
            assert_eq!(BCH_EXTENT_PTR_OFFSET(&*ptr), 12);
            assert_eq!(bch2_extent_crc_unpack(&(*key).k, crc).offset, 0);
        }
    }

    #[test]
    fn bcachefs_reservation_merge_requires_matching_generation_and_replicas() {
        unsafe {
            let mut left_words = [0u64; 8];
            let mut right_words = [0u64; 8];
            let left = left_words.as_mut_ptr().cast::<bkey_i>();
            let right = right_words.as_mut_ptr().cast::<bkey_i>();
            (*left).k = bkey {
                size: 2,
                p: bpos {
                    offset: 2,
                    ..Default::default()
                },
                type_: KEY_TYPE_reservation,
                ..Default::default()
            };
            (*right).k = bkey {
                size: 3,
                type_: KEY_TYPE_reservation,
                ..Default::default()
            };
            core::ptr::addr_of_mut!((*left).v)
                .cast::<bch_reservation>()
                .write(bch_reservation {
                    generation: 9,
                    nr_replicas: 2,
                    ..Default::default()
                });
            core::ptr::addr_of_mut!((*right).v)
                .cast::<bch_reservation>()
                .write(bch_reservation {
                    generation: 9,
                    nr_replicas: 2,
                    ..Default::default()
                });
            assert!(bch2_reservation_merge(
                bkey_s {
                    k: &mut (*left).k,
                    v: &mut (*left).v
                },
                bkey_s_c {
                    k: &(*right).k,
                    v: &(*right).v
                },
            ));
            assert_eq!((*left).k.size, 5);
            assert_eq!(core::ptr::addr_of!((*left).k.p.offset).read_unaligned(), 5);
            core::ptr::addr_of_mut!((*right).v)
                .cast::<bch_reservation>()
                .write(bch_reservation {
                    generation: 10,
                    nr_replicas: 2,
                    ..Default::default()
                });
            assert!(!bch2_reservation_merge(
                bkey_s {
                    k: &mut (*left).k,
                    v: &mut (*left).v
                },
                bkey_s_c {
                    k: &(*right).k,
                    v: &(*right).v
                },
            ));
        }
    }

    #[test]
    fn bcachefs_reservation_merge_size_wraps_like_c_unsigned_addition() {
        unsafe {
            let mut left_words = [0u64; 8];
            let mut right_words = [0u64; 8];
            let left = left_words.as_mut_ptr().cast::<bkey_i>();
            let right = right_words.as_mut_ptr().cast::<bkey_i>();
            (*left).k = bkey {
                size: u32::MAX,
                p: bpos {
                    offset: 7,
                    ..Default::default()
                },
                type_: KEY_TYPE_reservation,
                ..Default::default()
            };
            (*right).k = bkey {
                size: 1,
                type_: KEY_TYPE_reservation,
                ..Default::default()
            };
            core::ptr::addr_of_mut!((*left).v)
                .cast::<bch_reservation>()
                .write(bch_reservation {
                    generation: 4,
                    nr_replicas: 1,
                    ..Default::default()
                });
            core::ptr::addr_of_mut!((*right).v)
                .cast::<bch_reservation>()
                .write(bch_reservation {
                    generation: 4,
                    nr_replicas: 1,
                    ..Default::default()
                });

            assert!(bch2_reservation_merge(
                bkey_s {
                    k: &mut (*left).k,
                    v: &mut (*left).v
                },
                bkey_s_c {
                    k: &(*right).k,
                    v: &(*right).v
                },
            ));
            assert_eq!((*left).k.size, 0);
            assert_eq!(
                core::ptr::addr_of!((*left).k.p.offset).read_unaligned(),
                7u64.wrapping_sub(u32::MAX as u64)
            );
        }
    }

    #[test]
    fn bcachefs_bkey_can_read_skips_cached_and_invalid_pointers() {
        unsafe {
            let mut words = [0u64; 8];
            let key = words.as_mut_ptr().cast::<bkey_i>();
            (*key).k = bkey {
                u64s: super::super::bkey::BKEY_U64S + 1,
                type_: KEY_TYPE_extent,
                ..Default::default()
            };
            let ptr = core::ptr::addr_of_mut!((*key).v).cast::<bch_extent_ptr>();
            SET_BCH_EXTENT_PTR_TYPE(&mut *ptr, 1 << BCH_EXTENT_ENTRY_ptr);
            SET_BCH_EXTENT_PTR_DEV(&mut *ptr, 1);
            let view = bkey_s_c {
                k: &(*key).k,
                v: core::ptr::addr_of!((*key).v),
            };
            assert!(bch2_bkey_can_read(core::ptr::null(), view));
            SET_BCH_EXTENT_PTR_CACHED(&mut *ptr, 1);
            assert!(!bch2_bkey_can_read(core::ptr::null(), view));
            SET_BCH_EXTENT_PTR_CACHED(&mut *ptr, 0);
            SET_BCH_EXTENT_PTR_DEV(&mut *ptr, crate::sb::BCH_SB_MEMBER_INVALID as u64);
            assert!(!bch2_bkey_can_read(core::ptr::null(), view));
        }
    }

    #[test]
    fn bcachefs_extent_key_classification_matches_format_types() {
        assert_eq!(core::mem::size_of::<bch_inline_data>(), 0);
        assert_eq!(core::mem::size_of::<bch_indirect_inline_data>(), 8);
        assert_eq!(core::mem::size_of::<bch_reflink_v>(), 8);
        let mut key = bkey::default();
        key.type_ = KEY_TYPE_extent;
        assert!(bkey_extent_is_direct_data(&key));
        assert!(bkey_extent_is_data(&key));
        assert!(bkey_extent_is_allocation(&key));
        assert!(bkey_is_user_data(&key));
        key.type_ = KEY_TYPE_inline_data;
        assert!(!bkey_extent_is_direct_data(&key));
        assert!(bkey_extent_is_inline_data(&key));
        assert!(bkey_extent_is_data(&key));
        assert!(bkey_is_user_data(&key));
        key.u64s = super::super::bkey::BKEY_U64S + 2;
        assert_eq!(unsafe { bkey_inline_data_offset(&key) }, 0);
        assert_eq!(unsafe { bkey_inline_data_bytes(&key) }, 16);
        key.type_ = KEY_TYPE_indirect_inline_data;
        assert_eq!(unsafe { bkey_inline_data_offset(&key) }, 8);
        assert_eq!(unsafe { bkey_inline_data_bytes(&key) }, 8);
        key.type_ = KEY_TYPE_reflink_p;
        assert!(!bkey_extent_is_direct_data(&key));
        assert!(bkey_extent_is_data(&key));
        key.type_ = KEY_TYPE_reflink_v;
        assert!(bkey_extent_is_direct_data(&key));
        key.type_ = KEY_TYPE_btree_ptr;
        assert!(bkey_is_btree_ptr(&key));
        assert!(bkey_extent_is_direct_data(&key));
        key.type_ = KEY_TYPE_btree_ptr_v2;
        assert!(bkey_is_btree_ptr(&key));
        key.type_ = KEY_TYPE_set;
        assert!(!bkey_extent_is_data(&key));
        assert!(!bkey_extent_is_allocation(&key));
        assert!(!bkey_is_user_data(&key));
    }

    #[test]
    fn bcachefs_extent_entry_drop_wrappers_shift_and_shrink_values() {
        unsafe {
            let mut words = [0u64; 16];
            let key = words.as_mut_ptr().cast::<bkey_i>();
            (*key).k = bkey {
                u64s: super::super::bkey::BKEY_U64S + 2,
                type_: KEY_TYPE_extent,
                ..Default::default()
            };
            let entries = core::ptr::addr_of_mut!((*key).v).cast::<bch_extent_entry>();
            (*entries).ptr = bch_extent_ptr::default();
            SET_BCH_EXTENT_PTR_TYPE(&mut (*entries).ptr, 1 << BCH_EXTENT_ENTRY_ptr);
            let second = entries.cast::<u8>().add(8).cast::<bch_extent_entry>();
            (*second).crc32 = bch_extent_crc32 {
                word0: 1 << BCH_EXTENT_ENTRY_crc32,
                ..Default::default()
            };
            bch2_bkey_extent_entry_drop(core::ptr::null(), key, entries);
            assert_eq!((*key).k.u64s, super::super::bkey::BKEY_U64S + 1);
            assert_eq!(extent_entry_type(entries), BCH_EXTENT_ENTRY_crc32 as u32);

            (*key).k.u64s = super::super::bkey::BKEY_U64S + 2;
            (*entries).ptr = bch_extent_ptr::default();
            SET_BCH_EXTENT_PTR_TYPE(&mut (*entries).ptr, 1 << BCH_EXTENT_ENTRY_ptr);
            (*second).crc32 = bch_extent_crc32 {
                word0: 1 << BCH_EXTENT_ENTRY_crc32,
                ..Default::default()
            };
            bch2_bkey_extent_entry_drop_s(
                core::ptr::null(),
                bkey_s {
                    k: core::ptr::addr_of_mut!((*key).k),
                    v: core::ptr::addr_of_mut!((*key).v),
                },
                entries,
            );
            assert_eq!((*key).k.u64s, super::super::bkey::BKEY_U64S + 1);
            assert_eq!(extent_entry_type(entries), BCH_EXTENT_ENTRY_crc32 as u32);
        }
    }

    #[test]
    fn bcachefs_extent_replicas_counts_ec_redundancy_and_cached_skip() {
        unsafe {
            let mut words = [0u64; 10];
            let key = words.as_mut_ptr().cast::<bkey_i>();
            (*key).k = bkey {
                u64s: super::super::bkey::BKEY_U64S + 3,
                size: 4,
                type_: KEY_TYPE_extent,
                ..Default::default()
            };
            let first = core::ptr::addr_of_mut!((*key).v).cast::<bch_extent_entry>();
            (*first).crc32 = bch_extent_crc32 {
                word0: 1 << BCH_EXTENT_ENTRY_crc32,
                ..Default::default()
            };
            let second = first.cast::<u8>().add(8).cast::<bch_extent_entry>();
            (*second).stripe_ptr = bch_extent_stripe_ptr {
                v: (2 << 13) | (1 << BCH_EXTENT_ENTRY_stripe_ptr) | (1 << 17),
            };
            assert_eq!(
                extent_entry_type(second),
                BCH_EXTENT_ENTRY_stripe_ptr as u32
            );
            let third = first.cast::<u8>().add(16).cast::<bch_extent_entry>();
            (*third).ptr = bch_extent_ptr::default();
            SET_BCH_EXTENT_PTR_TYPE(&mut (*third).ptr, 1 << BCH_EXTENT_ENTRY_ptr);
            SET_BCH_EXTENT_PTR_DEV(&mut (*third).ptr, 7);
            let view = bkey_s_c {
                k: &(*key).k,
                v: core::ptr::addr_of!((*key).v),
            };
            assert_eq!(bch2_bkey_replicas(core::ptr::null_mut(), view), 3);
            SET_BCH_EXTENT_PTR_CACHED(&mut (*third).ptr, 1);
            assert_eq!(bch2_bkey_replicas(core::ptr::null_mut(), view), 0);
        }
    }

    #[test]
    fn bcachefs_propagate_incompressible_updates_none_crc_entries() {
        unsafe {
            let mut source_words = [0u64; 8];
            let source = source_words.as_mut_ptr().cast::<bkey_i>();
            (*source).k = bkey {
                u64s: super::super::bkey::BKEY_U64S + 2,
                size: 4,
                type_: KEY_TYPE_extent,
                ..Default::default()
            };
            bch2_extent_crc_pack(
                core::ptr::addr_of_mut!((*source).v).cast(),
                bch_extent_crc_unpacked {
                    compressed_size: 4,
                    uncompressed_size: 4,
                    live_size: 4,
                    compression_type: BCH_COMPRESSION_TYPE_incompressible,
                    ..Default::default()
                },
                BCH_EXTENT_ENTRY_crc32,
            );

            let mut dest_words = [0u64; 8];
            let dest = dest_words.as_mut_ptr().cast::<bkey_i>();
            (*dest).k = (*source).k;
            bch2_extent_crc_pack(
                core::ptr::addr_of_mut!((*dest).v).cast(),
                bch_extent_crc_unpacked {
                    compressed_size: 4,
                    uncompressed_size: 4,
                    live_size: 4,
                    compression_type: BCH_COMPRESSION_TYPE_none,
                    ..Default::default()
                },
                BCH_EXTENT_ENTRY_crc32,
            );
            let source_sc = bkey_s_c {
                k: &(*source).k,
                v: core::ptr::addr_of!((*source).v),
            };
            assert!(bch2_bkey_is_incompressible(core::ptr::null(), source_sc));
            bch2_bkey_propagate_incompressible(core::ptr::null(), dest, source_sc);
            let got = bch2_extent_crc_unpack(&(*dest).k, core::ptr::addr_of!((*dest).v).cast());
            assert_eq!(got.compression_type, BCH_COMPRESSION_TYPE_incompressible);
        }
    }

    #[test]
    fn bcachefs_extent_crc_pack_unpack_round_trip() {
        unsafe {
            let key = bkey {
                size: 12,
                ..Default::default()
            };
            let mut words = [0u64; 3];
            let entry = words.as_mut_ptr().cast::<bch_extent_entry>();
            let source = bch_extent_crc_unpacked {
                compressed_size: 7,
                uncompressed_size: 9,
                live_size: 12,
                csum_type: crate::checksum::BCH_CSUM_crc32c as u8,
                compression_type: BCH_COMPRESSION_TYPE_none,
                offset: 3,
                nonce: 0,
                csum: bch_csum {
                    lo: 0x1234_5678,
                    hi: 0,
                },
            };
            bch2_extent_crc_pack(entry.cast(), source, BCH_EXTENT_ENTRY_crc32);
            assert_eq!(bch2_extent_crc_unpack(&key, entry.cast()), source);
            assert!(!crc_is_compressed(source));
            assert!(crc_is_encoded(source));
            let mut compressed = source;
            compressed.compression_type = 2;
            assert!(crc_is_compressed(compressed));
            assert!(crc_is_encoded(compressed));
            compressed.csum_type = crate::checksum::BCH_CSUM_none as u8;
            assert!(crc_is_encoded(compressed));
            compressed.compression_type = BCH_COMPRESSION_TYPE_incompressible;
            assert!(!crc_is_compressed(compressed));
            assert!(!crc_is_encoded(compressed));
            assert_eq!(
                bch2_crc_field_size_max[BCH_EXTENT_ENTRY_crc64 as usize],
                1 << 9
            );
        }
    }

    #[test]
    fn bcachefs_extent_crc_append_selects_smallest_valid_encoding() {
        unsafe {
            let mut words = [0u64; 8];
            let key = words.as_mut_ptr().cast::<bkey_i>();
            (*key).k = bkey {
                u64s: super::super::bkey::BKEY_U64S,
                type_: KEY_TYPE_extent,
                ..Default::default()
            };
            bch2_extent_crc_append(
                core::ptr::null(),
                key,
                bch_extent_crc_unpacked {
                    compressed_size: 4,
                    uncompressed_size: 4,
                    live_size: 4,
                    csum_type: crate::checksum::BCH_CSUM_crc32c as u8,
                    ..Default::default()
                },
            );
            assert_eq!((*key).k.u64s, super::super::bkey::BKEY_U64S + 1);
            let unpacked = bch2_extent_crc_unpack(
                &(*key).k,
                core::ptr::addr_of!((*key).v)
                    .cast::<bch_extent_entry>()
                    .cast(),
            );
            assert_eq!(unpacked.compressed_size, 4);
            assert_eq!(unpacked.csum_type, crate::checksum::BCH_CSUM_crc32c as u8);
        }
    }

    #[test]
    fn bcachefs_extent_ptr_decoded_append_keeps_crc_and_ec_order() {
        unsafe {
            let mut words = [0u64; 10];
            let key = words.as_mut_ptr().cast::<bkey_i>();
            (*key).k = bkey {
                u64s: super::super::bkey::BKEY_U64S,
                size: 4,
                type_: KEY_TYPE_extent,
                ..Default::default()
            };
            let mut decoded = extent_ptr_decoded {
                has_ec: true,
                crc: bch_extent_crc_unpacked {
                    compressed_size: 4,
                    uncompressed_size: 4,
                    live_size: 4,
                    csum_type: crate::checksum::BCH_CSUM_crc32c as u8,
                    ..Default::default()
                },
                ..Default::default()
            };
            bch2_extent_ptr_decoded_append(core::ptr::null(), key, &mut decoded);
            assert_eq!((*key).k.u64s, super::super::bkey::BKEY_U64S + 3);
            let first = core::ptr::addr_of_mut!((*key).v).cast::<bch_extent_entry>();
            assert_eq!(extent_entry_type(first), BCH_EXTENT_ENTRY_crc32 as u32);
            assert_eq!(
                extent_entry_type(first.cast::<u8>().add(8).cast()),
                BCH_EXTENT_ENTRY_stripe_ptr as u32
            );
            assert_eq!(
                extent_entry_type(first.cast::<u8>().add(16).cast()),
                BCH_EXTENT_ENTRY_ptr as u32
            );
        }
    }

    #[test]
    fn bcachefs_drop_ptr_noerror_removes_orphaned_crc() {
        unsafe {
            let mut words = [0u64; 8];
            let key = words.as_mut_ptr().cast::<bkey_i>();
            (*key).k = bkey {
                u64s: super::super::bkey::BKEY_U64S + 2,
                size: 4,
                type_: KEY_TYPE_extent,
                ..Default::default()
            };
            let first = core::ptr::addr_of_mut!((*key).v).cast::<bch_extent_entry>();
            (*first).crc32 = bch_extent_crc32 {
                word0: 1 << BCH_EXTENT_ENTRY_crc32,
                ..Default::default()
            };
            let second = first.cast::<u8>().add(8).cast::<bch_extent_entry>();
            (*second).ptr = bch_extent_ptr::default();
            SET_BCH_EXTENT_PTR_TYPE(&mut (*second).ptr, 1 << BCH_EXTENT_ENTRY_ptr);
            bch2_bkey_drop_ptr_noerror(
                core::ptr::null(),
                bkey_s {
                    k: core::ptr::addr_of_mut!((*key).k),
                    v: core::ptr::addr_of_mut!((*key).v),
                },
                core::ptr::addr_of_mut!((*second).ptr),
            );
            assert_eq!((*key).k.u64s, super::super::bkey::BKEY_U64S);

            let mut ec_words = [0u64; 10];
            let ec_key = ec_words.as_mut_ptr().cast::<bkey_i>();
            (*ec_key).k = bkey {
                u64s: super::super::bkey::BKEY_U64S + 3,
                size: 4,
                type_: KEY_TYPE_extent,
                ..Default::default()
            };
            let ec_first = core::ptr::addr_of_mut!((*ec_key).v).cast::<bch_extent_entry>();
            (*ec_first).crc32 = bch_extent_crc32 {
                word0: 1 << BCH_EXTENT_ENTRY_crc32,
                ..Default::default()
            };
            let ec_second = ec_first.cast::<u8>().add(8).cast::<bch_extent_entry>();
            (*ec_second).stripe_ptr = bch_extent_stripe_ptr {
                v: 1 << BCH_EXTENT_ENTRY_stripe_ptr,
            };
            let ec_third = ec_first.cast::<u8>().add(16).cast::<bch_extent_entry>();
            (*ec_third).ptr = bch_extent_ptr::default();
            SET_BCH_EXTENT_PTR_TYPE(&mut (*ec_third).ptr, 1 << BCH_EXTENT_ENTRY_ptr);
            SET_BCH_EXTENT_PTR_DEV(&mut (*ec_third).ptr, 7);
            bch2_bkey_drop_ptr(
                core::ptr::null(),
                bkey_s {
                    k: core::ptr::addr_of_mut!((*ec_key).k),
                    v: core::ptr::addr_of_mut!((*ec_key).v),
                },
                core::ptr::addr_of_mut!((*ec_third).ptr),
            );
            assert_eq!(BCH_EXTENT_PTR_DEV(&(*ec_third).ptr), 255);
            assert_eq!((*ec_key).k.u64s, super::super::bkey::BKEY_U64S + 3);

            let mut multi_words = [0u64; 8];
            let multi = multi_words.as_mut_ptr().cast::<bkey_i>();
            (*multi).k = bkey {
                u64s: super::super::bkey::BKEY_U64S + 2,
                type_: KEY_TYPE_extent,
                ..Default::default()
            };
            let multi_first = core::ptr::addr_of_mut!((*multi).v).cast::<bch_extent_entry>();
            (*multi_first).ptr = bch_extent_ptr::default();
            SET_BCH_EXTENT_PTR_TYPE(&mut (*multi_first).ptr, 1 << BCH_EXTENT_ENTRY_ptr);
            SET_BCH_EXTENT_PTR_DEV(&mut (*multi_first).ptr, 7);
            let multi_second = multi_first.cast::<u8>().add(8).cast::<bch_extent_entry>();
            (*multi_second).ptr = bch_extent_ptr::default();
            SET_BCH_EXTENT_PTR_TYPE(&mut (*multi_second).ptr, 1 << BCH_EXTENT_ENTRY_ptr);
            SET_BCH_EXTENT_PTR_DEV(&mut (*multi_second).ptr, 8);
            let multi_s = bkey_s {
                k: core::ptr::addr_of_mut!((*multi).k),
                v: core::ptr::addr_of_mut!((*multi).v),
            };
            bch2_bkey_drop_device_noerror(core::ptr::null(), multi_s, 7);
            assert_eq!((*multi).k.u64s, super::super::bkey::BKEY_U64S + 1);
            assert_eq!(BCH_EXTENT_PTR_DEV(&(*multi_first).ptr), 8);

            (*multi).k.u64s = super::super::bkey::BKEY_U64S + 2;
            (*multi_first).ptr = bch_extent_ptr::default();
            SET_BCH_EXTENT_PTR_TYPE(&mut (*multi_first).ptr, 1 << BCH_EXTENT_ENTRY_ptr);
            SET_BCH_EXTENT_PTR_DEV(&mut (*multi_first).ptr, 7);
            (*multi_second).ptr = bch_extent_ptr::default();
            SET_BCH_EXTENT_PTR_TYPE(&mut (*multi_second).ptr, 1 << BCH_EXTENT_ENTRY_ptr);
            SET_BCH_EXTENT_PTR_DEV(&mut (*multi_second).ptr, 8);
            bch2_bkey_drop_ptrs_mask(core::ptr::null(), multi, 1 << 1);
            assert_eq!((*multi).k.u64s, super::super::bkey::BKEY_U64S + 1);
            assert_eq!(BCH_EXTENT_PTR_DEV(&(*multi_first).ptr), 7);
        }
    }

    #[test]
    fn bcachefs_extent_key_ptr_range_starts_at_value() {
        unsafe {
            let mut words = [0u64; 6];
            let key = words.as_mut_ptr().cast::<bkey_i>();
            (*key).k = bkey {
                u64s: super::super::bkey::BKEY_U64S + 1,
                format: super::super::bkey::KEY_FORMAT_CURRENT,
                type_: KEY_TYPE_extent,
                ..Default::default()
            };
            let value = words.as_mut_ptr().add(5).cast::<bch_extent_ptr>();
            *value = bch_extent_ptr { v: 0x1234 };
            let ptrs = bch2_bkey_ptrs_c(bkey_s_c {
                k: &(*key).k,
                v: (&mut (*key).v as *mut bch_val),
            });
            assert_eq!(ptrs.start, value.cast());
            assert_eq!(ptrs.end, value.add(1).cast());
        }
    }

    #[test]
    fn bcachefs_extent_cut_front_and_back_adjust_range_and_pointer() {
        unsafe {
            let mut words = [0u64; 6];
            let key = words.as_mut_ptr().cast::<bkey_i>();
            (*key).k = bkey {
                u64s: super::super::bkey::BKEY_U64S + 1,
                format: super::super::bkey::KEY_FORMAT_CURRENT,
                type_: KEY_TYPE_extent,
                size: 10,
                p: super::super::bkey::SPOS(3, 20, 0),
                ..Default::default()
            };
            let value = words.as_mut_ptr().add(5).cast::<bch_extent_ptr>();
            *value = bch_extent_ptr { v: 1 | (1 << 4) };
            let value_ptr = &mut (*key).v as *mut bch_val;
            assert_eq!(
                bch2_cut_front_s(
                    core::ptr::null(),
                    super::super::bkey::SPOS(3, 15, 0),
                    bkey_s {
                        k: &mut (*key).k,
                        v: value_ptr,
                    },
                ),
                0
            );
            assert_eq!((*key).k.size, 5);
            assert_eq!(core::ptr::addr_of!((*key).k.p.offset).read_unaligned(), 20);
            assert_eq!(BCH_EXTENT_PTR_OFFSET(&*value), 6);

            assert_eq!(
                bch2_cut_back_s(
                    super::super::bkey::SPOS(3, 17, 0),
                    bkey_s {
                        k: &mut (*key).k,
                        v: value_ptr,
                    },
                ),
                0
            );
            assert_eq!((*key).k.size, 2);
            assert_eq!(core::ptr::addr_of!((*key).k.p.offset).read_unaligned(), 17);
        }
    }

    #[test]
    fn bcachefs_extent_cut_front_updates_crc_offset_and_stops_pointer_shift() {
        unsafe {
            let mut words = [0u64; 7];
            let key = words.as_mut_ptr().cast::<bkey_i>();
            (*key).k = bkey {
                u64s: super::super::bkey::BKEY_U64S + 2,
                format: super::super::bkey::KEY_FORMAT_CURRENT,
                type_: KEY_TYPE_extent,
                size: 10,
                p: super::super::bkey::SPOS(4, 20, 0),
                ..Default::default()
            };
            let crc = words.as_mut_ptr().add(5).cast::<bch_extent_crc32>();
            (*crc).word0 = (1 << BCH_EXTENT_ENTRY_crc32) | (2 << 16);
            let ptr = words.as_mut_ptr().add(6).cast::<bch_extent_ptr>();
            *ptr = bch_extent_ptr { v: 1 | (4 << 4) };
            bch2_cut_front_s(
                core::ptr::null(),
                super::super::bkey::SPOS(4, 11, 0),
                bkey_s {
                    k: &mut (*key).k,
                    v: &mut (*key).v as *mut bch_val,
                },
            );
            let crc_word = core::ptr::addr_of!((*crc).word0).read_unaligned();
            assert_eq!((crc_word >> 16) & 0x7f, 3);
            assert_eq!(BCH_EXTENT_PTR_OFFSET(&*ptr), 4);
        }
    }
}
