use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::data::extents::BchExtentCrcUnpacked;
use crate::errcode::BchResult;
use crate::opts::Printbuf;

pub fn bch2_bio_compress(
    _c: &BchFs,
    _dst: *mut std::ffi::c_void,
    _dst_size: *mut usize,
    _src: *mut std::ffi::c_void,
    _src_size: *mut usize,
    _compression_type: BchCompressionType,
    _pos: Bpos,
    _crc: bool,
) -> u32 {
    todo!()
}

pub fn bch2_bio_uncompress(
    _c: &BchFs,
    _src: *mut std::ffi::c_void,
    _dst: *mut std::ffi::c_void,
    _dst_iter: *mut std::ffi::c_void,
    _crc: BchExtentCrcUnpacked,
) -> BchResult<()> {
    todo!()
}

pub fn bch2_bio_uncompress_inplace(
    _op: *mut std::ffi::c_void,
    _bio: *mut std::ffi::c_void,
) -> BchResult<()> {
    todo!()
}

pub fn bch2_compression_opt_to_type(v: u32) -> BchCompressionType {
    let opt_type = (v & 0x0f) as u8;
    match opt_type {
        0 => BchCompressionType::None,
        1 => BchCompressionType::Lz4Old,
        2 => BchCompressionType::Gzip,
        3 => BchCompressionType::Lz4,
        4 => BchCompressionType::Zstd,
        5 => BchCompressionType::Incompressible,
        _ => BchCompressionType::None,
    }
}

pub fn bch2_compression_opt_valid(v: u32) -> bool {
    let opt_type = (v & 0x0f) as u8;
    let level = ((v >> 4) & 0x0f) as u8;
    matches!(opt_type, 0..=5) && !(opt_type == 0 && level != 0)
}

pub fn bch2_check_set_has_compressed_data(_c: &BchFs, _dev: u32) -> BchResult<()> {
    todo!()
}

pub fn bch2_fs_compress_init(_c: &BchFs) -> BchResult<()> {
    todo!()
}

pub fn bch2_fs_compress_exit(_c: &BchFs) {
    todo!()
}

pub fn bch2_compression_opt_to_text(_out: &mut Printbuf, _v: u64) {
    todo!()
}
