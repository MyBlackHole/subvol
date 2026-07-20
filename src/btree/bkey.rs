use crate::bcachefs_format::*;
use crate::errcode::*;

pub const BKEY_FIELD_INODE: usize = 0;
pub const BKEY_FIELD_OFFSET: usize = 1;
pub const BKEY_FIELD_SNAPSHOT: usize = 2;
pub const BKEY_FIELD_SIZE: usize = 3;
pub const BKEY_FIELD_VERSION_HI: usize = 4;
pub const BKEY_FIELD_VERSION_LO: usize = 5;
pub const BKEY_NR_FIELDS: usize = 6;

pub fn bkey_field_bytes(name: usize) -> u32 {
    match name {
        BKEY_FIELD_INODE => 8,
        BKEY_FIELD_OFFSET => 8,
        BKEY_FIELD_SNAPSHOT => 4,
        BKEY_FIELD_SIZE => 4,
        BKEY_FIELD_VERSION_HI => 4,
        BKEY_FIELD_VERSION_LO => 8,
        _ => 0,
    }
}

pub const BKEY_FORMAT_CURRENT: BkeyFormat = BkeyFormat {
    key_u64s: BKEY_U64S as u8,
    nr_fields: BKEY_NR_FIELDS as u8,
    bits_per_field: [
        8 * 8, 8 * 8, 4 * 8, 4 * 8, 4 * 8, 8 * 8,
    ],
    field_offset: [0; 6],
};

pub fn bkey_cmp_left(p: &Bpos, b: &Bpos) -> std::cmp::Ordering {
    p.cmp(b)
}

pub fn bkey_cmp_right(p: &Bpos, b: &Bpos) -> std::cmp::Ordering {
    b.cmp(p)
}

pub fn bkey_cmp(p: &Bkey, b: &Bkey) -> std::cmp::Ordering {
    p.p.cmp(&b.p)
}

pub fn bpos_cmp(p: &Bpos, b: &Bpos) -> std::cmp::Ordering {
    p.cmp(b)
}

pub fn bkey_start_pos(k: &Bkey) -> Bpos {
    let mut pos = k.p;
    if k.size > 0 {
        pos.offset = pos.offset.saturating_sub(k.size as u64);
    }
    pos
}

pub fn bkey_written(b: &BtreeNode) -> u16 {
    b.written
}

pub fn bkey_whiteout(k: &Bkey) -> bool {
    matches!(k.type_, BchBkeyType::Whiteout | BchBkeyType::HashWhiteout | BchBkeyType::Deleted)
}

pub fn bkey_deleted(k: &Bkey) -> bool {
    k.type_ == BchBkeyType::Deleted
}

pub fn bkey_is_inode(k: &Bkey) -> bool {
    matches!(k.type_,
        BchBkeyType::Inode |
        BchBkeyType::InodeV2 |
        BchBkeyType::InodeV3
    )
}

pub fn bkey_is_btree_ptr(k: &Bkey) -> bool {
    matches!(k.type_, BchBkeyType::BtreePtr | BchBkeyType::BtreePtrV2)
}

pub fn bkey_is_ptr(k: &Bkey) -> bool {
    matches!(k.type_,
        BchBkeyType::Extent |
        BchBkeyType::BtreePtr |
        BchBkeyType::BtreePtrV2 |
        BchBkeyType::Stripe |
        BchBkeyType::ReflinkV |
        BchBkeyType::ReflinkP
    )
}

pub fn bkey_extent_is_data(k: &Bkey) -> bool {
    k.size > 0
}

pub fn bkey_val_u64s(k: &Bkey) -> u8 {
    k.u64s - BKEY_U64S as u8
}

pub fn bkey_val_bytes(k: &Bkey) -> usize {
    bkey_val_u64s(k) as usize * 8
}

pub fn bkey_packed(k: &Bkey) -> bool {
    k.format != KEY_FORMAT_CURRENT
}

pub fn bkey_init(k: &mut Bkey) {
    *k = Bkey::init();
}

pub fn bkey_reassemble(dst: &mut BkeyI, src: &Bkey) {
    dst.k = *src;
}

pub fn bch2_bkey_pack_pos(dst: &mut [u8], pos: &Bpos, format: &BkeyFormat) -> bool {
    false
}

pub fn bch2_bkey_unpack_pos(format: &BkeyFormat, src: &[u8]) -> Bpos {
    Bpos::ZERO
}

pub fn bch2_bkey_pack_key(dst: &mut [u8], src: &Bkey, format: &BkeyFormat) -> bool {
    false
}

pub fn bch2_bkey_unpack_key(format: &BkeyFormat, src: &[u8]) -> Bkey {
    Bkey::init()
}

pub fn bch2_bkey_unpack(bkey_format: &BkeyFormat, src: &[u8]) -> Bkey {
    Bkey::init()
}

pub fn bch2_bkey_packed_cmp(l: &BkeyPacked, r: &BkeyPacked, format: &BkeyFormat) -> i32 {
    0
}

pub fn bch2_bkey_cmp_packed(format: &BkeyFormat, l: &[u8], r: &[u8]) -> i32 {
    0
}

pub fn bch2_bkey_unpack_key_format(packed: &[u8], unpacked: &mut Bkey, format: &BkeyFormat) {
}

pub fn bch2_bkey_pack_key_format(unpacked: &Bkey, packed: &mut [u8], format: &BkeyFormat) -> bool {
    false
}

pub fn bch2_bkey_format_add_key(format: &mut BkeyFormat, k: &Bkey) {
}

pub fn bch2_bkey_format_negative_acks(format: &BkeyFormat) -> bool {
    for i in 0..format.nr_fields as usize {
        if format.field_offset[i] != 0 || format.bits_per_field[i] < 1 {
            continue;
        }
    }
    false
}

pub fn bch2_bkey_format_min_bits(format: &BkeyFormat) -> u32 {
    let mut bits = 0u32;
    for i in 0..format.nr_fields as usize {
        bits += format.bits_per_field[i] as u32;
    }
    bits
}

pub fn bch2_bkey_format_add_pos(format: &mut BkeyFormat, pos: &Bpos) {
    let fields = [pos.inode, pos.offset, pos.snapshot as u64];
    for (i, &v) in fields.iter().enumerate() {
        let bits = (64 - v.leading_zeros()).max(1);
        if format.bits_per_field[i] < bits as u8 {
            format.bits_per_field[i] = bits as u8;
        }
    }
}

pub fn bch2_bkey_format_field_max(format: &BkeyFormat, field: u32) -> u64 {
    if (field as usize) < format.nr_fields as usize {
        (1u64 << format.bits_per_field[field as usize]) - 1
    } else {
        0
    }
}

pub fn bch2_bkey_format_field_min(format: &BkeyFormat, field: u32) -> u64 {
    if (field as usize) < format.nr_fields as usize {
        format.field_offset[field as usize]
    } else {
        0
    }
}
