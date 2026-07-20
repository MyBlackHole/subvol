use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::data::extents::BchExtentCrcUnpacked;
use crate::errcode::BchResult;
use crate::opts::Printbuf;

pub const CHACHA_BLOCK_SIZE: u32 = 64;
pub const CRC32_SIZE_MAX: u32 = 1 << 7;
pub const CRC64_SIZE_MAX: u32 = 1 << 9;
pub const CRC128_SIZE_MAX: u32 = 1 << 13;

#[derive(Clone, Copy, Debug)]
pub struct Nonce(pub [u32; 4]);

impl Nonce {
    pub fn new(d: [u32; 4]) -> Self {
        Nonce(d)
    }

    pub fn add(&self, offset: u32) -> Self {
        let mut d = self.0;
        d[0] = d[0].wrapping_add(offset / CHACHA_BLOCK_SIZE);
        Nonce(d)
    }

    pub fn null() -> Self {
        Nonce([0; 4])
    }
}

pub fn bch2_crc_cmp(l: BchCsum, r: BchCsum) -> bool {
    (l.lo ^ r.lo) | (l.hi ^ r.hi) != 0
}

pub fn bch2_csum_type_is_encryption(csum_type: BchCsumType) -> bool {
    csum_type.is_encryption()
}

pub fn bch2_checksum_mergeable(csum_type: BchCsumType) -> bool {
    matches!(csum_type, BchCsumType::None | BchCsumType::Crc32c | BchCsumType::Crc64)
}

pub fn bch2_checksum(
    _c: &BchFs,
    _csum_type: BchCsumType,
    _nonce: Nonce,
    _data: &[u8],
) -> BchCsum {
    todo!()
}

pub fn bch2_checksum_bio(
    _c: &BchFs,
    _csum_type: BchCsumType,
    _nonce: Nonce,
    _bio: *mut std::ffi::c_void,
) -> BchCsum {
    todo!()
}

pub fn bch2_checksum_merge(
    _csum_type: BchCsumType,
    _l: BchCsum,
    _r: BchCsum,
    _r_size: usize,
) -> BchCsum {
    todo!()
}

pub fn bch2_checksum_type_valid(c: &BchFs, csum_type: BchCsumType) -> bool {
    if csum_type as u32 >= 8 {
        return false;
    }
    if csum_type.is_encryption() && c.key_version == 0 {
        return false;
    }
    true
}

pub fn bch2_data_checksum_type(c: &BchFs, _opts: u64) -> BchCsumType {
    if c.opts.nocow {
        return BchCsumType::None;
    }
    c.opts.data_csum
}

pub fn bch2_meta_checksum_type(c: &BchFs) -> BchCsumType {
    c.opts.metadata_csum
}

pub fn bch2_encrypt_bio(
    _c: &BchFs,
    _csum_type: BchCsumType,
    _nonce: Nonce,
    _bio: *mut std::ffi::c_void,
) -> BchResult<i32> {
    todo!()
}

pub fn bch2_rechecksum_bio(
    _c: &BchFs,
    _bio: *mut std::ffi::c_void,
    _version: Bversion,
    _old_crc: &BchExtentCrcUnpacked,
    _new_crc: *mut BchExtentCrcUnpacked,
    _offset: u32,
    _sectors: u32,
    _csum_type: BchCsumType,
) -> BchResult<i32> {
    todo!()
}

pub fn extent_nonce(version: Bversion, _crc: &BchExtentCrcUnpacked) -> Nonce {
    Nonce::new([
        0,
        version.lo as u32,
        (version.lo >> 32) as u32,
        version.hi | 0,
    ])
}

pub fn bch2_csum_to_text(out: &mut Printbuf, csum_type: BchCsumType, csum: BchCsum) {
    use std::fmt::Write;
    let bytes = if (csum_type as usize) < 8 {
        BCH_CRC_BYTES[csum_type as usize]
    } else {
        16
    };
    let p = &csum as *const BchCsum as *const u8;
    for i in 0..bytes as usize {
        let _ = write!(out, "{:02x}", unsafe { *p.add(i) });
    }
}

pub fn bch2_chacha20(
    _key: &BchKey_,
    _nonce: Nonce,
    _data: &mut [u8],
) {
    todo!()
}

pub fn bch2_fs_encryption_init(_c: &BchFs) -> BchResult<()> {
    todo!()
}

pub fn bch2_fs_encryption_exit(_c: &BchFs) {
    todo!()
}
