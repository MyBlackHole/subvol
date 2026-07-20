use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::btree::types::*;
use crate::errcode::*;
use crate::opts::Printbuf;

pub const BCH_EXTENT_ENTRY_ptr: u32 = 0;
pub const BCH_EXTENT_ENTRY_crc32: u32 = 1;
pub const BCH_EXTENT_ENTRY_crc64: u32 = 2;
pub const BCH_EXTENT_ENTRY_crc128: u32 = 3;
pub const BCH_EXTENT_ENTRY_stripe_ptr: u32 = 4;
pub const BCH_EXTENT_ENTRY_flags: u32 = 6;

pub const BKEY_EXTENT_VAL_U64S_MAX: u32 = 5 + (4 * (16 * 2 + 1));

#[derive(Clone, Copy, Debug)]
pub struct BchExtentStripePtr {
    pub block: u8,
    pub redundancy: u8,
    pub idx: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct BchExtentCrcUnpacked {
    pub compressed_size: u32,
    pub uncompressed_size: u32,
    pub live_size: u32,
    pub csum_type: u8,
    pub compression_type: u8,
    pub offset: u16,
    pub nonce: u16,
    pub csum: BchCsum,
}

impl BchExtentCrcUnpacked {
    pub fn new(size: u32) -> Self {
        BchExtentCrcUnpacked {
            compressed_size: size,
            uncompressed_size: size,
            live_size: size,
            csum_type: 0,
            compression_type: 0,
            offset: 0,
            nonce: 0,
            csum: BchCsum { lo: 0, hi: 0 },
        }
    }

    pub fn is_compressed(&self) -> bool {
        self.compression_type != BchCompressionType::None as u8
            && self.compression_type != BchCompressionType::Incompressible as u8
    }

    pub fn is_encoded(&self) -> bool {
        self.csum_type != BchCsumType::None as u8 || self.is_compressed()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ExtentPtrDecoded {
    pub has_ec: bool,
    pub do_ec_reconstruct: bool,
    pub crc_retry_nr: u8,
    pub crc: BchExtentCrcUnpacked,
    pub ptr: BchExtentPtr,
    pub ec: BchExtentStripePtr,
}

impl ExtentPtrDecoded {
    pub fn new() -> Self {
        ExtentPtrDecoded {
            has_ec: false,
            do_ec_reconstruct: false,
            crc_retry_nr: 0,
            crc: BchExtentCrcUnpacked::new(0),
            ptr: BchExtentPtr { dev: 0, gen: 0, offset: 0 },
            ec: BchExtentStripePtr { block: 0, redundancy: 0, idx: 0 },
        }
    }
}

pub fn extent_entry_type(e: u64) -> u32 {
    if e != 0 { e.trailing_zeros() } else { !0u32 }
}

pub fn extent_entry_is_ptr(e: u64) -> bool {
    extent_entry_type(e) == BCH_EXTENT_ENTRY_ptr
}

pub fn extent_entry_is_crc(e: u64) -> bool {
    matches!(extent_entry_type(e), BCH_EXTENT_ENTRY_crc32 | BCH_EXTENT_ENTRY_crc64 | BCH_EXTENT_ENTRY_crc128)
}

pub fn extent_entry_is_stripe_ptr(e: u64) -> bool {
    extent_entry_type(e) == BCH_EXTENT_ENTRY_stripe_ptr
}

pub fn bkey_is_btree_ptr(k: &Bkey) -> bool {
    matches!(k.type_, BchBkeyType::BtreePtr | BchBkeyType::BtreePtrV2)
}

pub fn bkey_extent_is_direct_data(k: &Bkey) -> bool {
    matches!(k.type_, BchBkeyType::BtreePtr | BchBkeyType::BtreePtrV2 | BchBkeyType::Extent | BchBkeyType::ReflinkV)
}

pub fn bkey_is_user_data(k: &Bkey) -> bool {
    matches!(k.type_, BchBkeyType::Extent | BchBkeyType::InlineData | BchBkeyType::Reservation)
}

pub fn bkey_is_indirect(k: &Bkey) -> bool {
    matches!(k.type_, BchBkeyType::ReflinkV | BchBkeyType::IndirectInlineData)
}

pub fn bkey_extent_is_inline_data(k: &Bkey) -> bool {
    matches!(k.type_, BchBkeyType::InlineData | BchBkeyType::IndirectInlineData)
}

pub fn bkey_extent_is_data(k: &Bkey) -> bool {
    bkey_extent_is_direct_data(k) || bkey_extent_is_inline_data(k) || k.type_ == BchBkeyType::ReflinkP
}

pub fn bkey_extent_is_allocation(k: &Bkey) -> bool {
    matches!(k.type_,
        BchBkeyType::Extent | BchBkeyType::Reservation | BchBkeyType::ReflinkP |
        BchBkeyType::ReflinkV | BchBkeyType::InlineData | BchBkeyType::IndirectInlineData |
        BchBkeyType::Error)
}

pub fn bkey_extent_is_reservation(k: &Bkey) -> bool {
    k.type_ == BchBkeyType::Reservation
}

pub fn bch2_extent_overlap(k: &Bkey, m: &Bkey) -> u32 {
    let cmp1 = k.p < m.p;
    let cmp2 = k.p > m.p;
    ((cmp1 as u32) << 1) + cmp2 as u32
}

pub fn bch2_key_resize(k: &mut Bkey, new_size: u32) {
    k.p.offset = k.p.offset.wrapping_sub(k.size as u64);
    k.p.offset = k.p.offset.wrapping_add(new_size as u64);
    k.size = new_size;
}

pub fn bch2_extent_ptr_eq(ptr1: &BchExtentPtr, ptr2: &BchExtentPtr) -> bool {
    ptr1.dev == ptr2.dev && ptr1.gen == ptr2.gen && ptr1.offset == ptr2.offset
}

pub fn crc_is_compressed(crc: &BchExtentCrcUnpacked) -> bool {
    crc.compression_type != BchCompressionType::None as u8
        && crc.compression_type != BchCompressionType::Incompressible as u8
}

pub fn bch2_extent_ptr_set_cached(
    _c: &BchFs,
    _opts: &mut u64,
    _k: (),
    _ptr: &mut BchExtentPtr,
) {
    todo!()
}

pub fn bch2_bkey_ptrs_are_correct(_c: &BchFs, _k: ()) -> BchResult<i32> {
    todo!()
}

pub fn bch2_bkey_append_ptr(_c: &BchFs, _k: &mut BkeyI, _ptr: BchExtentPtr) {
    todo!()
}

pub fn bch2_bkey_drop_ptr_noerror(_c: &BchFs, _k: (), _ptr: &mut BchExtentPtr) {
    todo!()
}

pub fn bch2_bkey_drop_device_noerror(_c: &BchFs, _k: (), _dev: u32) {
    todo!()
}

pub fn bch2_bkey_has_device_c(_c: &BchFs, _k: (), _dev: u32) -> bool {
    todo!()
}

pub fn bch2_bkey_nr_dirty_ptrs(_c: &BchFs, _k: ()) -> u32 {
    todo!()
}

pub fn bch2_bkey_nr_ptrs_allocated(_c: &BchFs, _k: ()) -> u32 {
    todo!()
}

pub fn bch2_bkey_sectors_compressed(_c: &BchFs, _k: ()) -> u32 {
    todo!()
}

pub fn bch2_bkey_replicas(_c: &BchFs, _k: ()) -> u32 {
    todo!()
}

pub fn bch2_bkey_can_read(_c: &BchFs, _k: ()) -> bool {
    todo!()
}

pub fn bch2_bkey_devs(c: &BchFs, k: ()) -> Vec<u32> {
    todo!()
}

pub fn bch2_bkey_has_device_decode(_c: &BchFs, _k: (), _dev: u32, _decoded: &mut ExtentPtrDecoded) -> bool {
    todo!()
}

pub fn bch2_bkey_devs_rw(_c: &BchFs, _k: ()) -> bool {
    todo!()
}

pub fn bch2_bkey_has_target(_c: &BchFs, _k: (), _target: u32) -> bool {
    todo!()
}

pub fn bch2_bkey_has_dev_bad_or_evacuating(_c: &BchFs, _k: ()) -> bool {
    todo!()
}

pub fn bch2_dev_durability(_c: &BchFs, _dev: u32) -> u32 {
    todo!()
}

pub fn bch2_bkey_ptrs_to_text(_out: &mut Printbuf, _c: &BchFs, _k: ()) {
    todo!()
}

pub fn bch2_bkey_ptrs_validate(_c: &BchFs, _k: ()) -> BchResult<i32> {
    todo!()
}

pub fn bch2_extent_merge(_c: &BchFs, _k: &mut BkeyI, _new: ()) -> bool {
    todo!()
}

pub fn bch2_cut_front_s(_c: &BchFs, _where: Bpos, _k: ()) -> BchResult<i32> {
    todo!()
}

pub fn bch2_cut_back_s(_where: Bpos, _k: ()) -> BchResult<i32> {
    todo!()
}

pub fn bch2_extent_crc_unpack(_k: &Bkey, _crc: Option<u64>) -> BchExtentCrcUnpacked {
    todo!()
}
