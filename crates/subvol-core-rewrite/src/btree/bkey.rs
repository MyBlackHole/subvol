pub const KEY_INODE_MAX: u64 = u64::MAX;
pub const KEY_OFFSET_MAX: u64 = u64::MAX;
pub const KEY_SNAPSHOT_MAX: u32 = u32::MAX;
pub const KEY_SIZE_MAX: u32 = u32::MAX;

pub const KEY_FORMAT_LOCAL_BTREE: u8 = 0;
pub const KEY_FORMAT_CURRENT: u8 = 1;
pub const KEY_PACKED_BITS_START: u32 = 24;

pub const BKEY_U64S: u8 = 5;
pub const BKEY_U64S_MAX: u8 = u8::MAX;
pub const BKEY_VAL_U64S_MAX: u8 = BKEY_U64S_MAX - BKEY_U64S;

pub const BKEY_FIELD_INODE: usize = 0;
pub const BKEY_FIELD_OFFSET: usize = 1;
pub const BKEY_FIELD_SNAPSHOT: usize = 2;
pub const BKEY_FIELD_SIZE: usize = 3;
pub const BKEY_FIELD_VERSION_HI: usize = 4;
pub const BKEY_FIELD_VERSION_LO: usize = 5;
pub const BKEY_NR_FIELDS: u8 = 6;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bkey_format {
    pub key_u64s: u8,
    pub nr_fields: u8,
    pub bits_per_field: [u8; 6],
    pub field_offset: [u64; 6],
}

#[repr(C, packed(4))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bpos {
    pub snapshot: u32,
    pub offset: u64,
    pub inode: u64,
}

pub const POS_MIN: bpos = SPOS(0, 0, 0);
pub const POS_MAX: bpos = SPOS(KEY_INODE_MAX, KEY_OFFSET_MAX, 0);
pub const SPOS_MAX: bpos = SPOS(KEY_INODE_MAX, KEY_OFFSET_MAX, KEY_SNAPSHOT_MAX);

#[allow(non_snake_case)]
pub const fn SPOS(inode: u64, offset: u64, snapshot: u32) -> bpos {
    bpos {
        inode,
        offset,
        snapshot,
    }
}

#[allow(non_snake_case)]
pub const fn POS(inode: u64, offset: u64) -> bpos {
    SPOS(inode, offset, 0)
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_val {
    pub __nothing: [u64; 0],
}

#[repr(C, packed(4))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bversion {
    pub lo: u64,
    pub hi: u32,
}

pub const ZERO_VERSION: bversion = bversion { hi: 0, lo: 0 };
pub const MAX_VERSION: bversion = bversion {
    hi: u32::MAX,
    lo: u64::MAX,
};

#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bkey {
    pub u64s: u8,
    pub format: u8,
    pub type_: u8,
    pub pad: [u8; 1],
    pub bversion: bversion,
    pub size: u32,
    pub p: bpos,
}

#[repr(C, align(8))]
#[derive(Clone, Copy)]
pub struct bkey_packed {
    pub u64s: u8,
    pub format: u8,
    pub type_: u8,
    pub pad: [u8; 37],
}

impl Default for bkey_packed {
    fn default() -> Self {
        Self {
            u64s: 0,
            format: 0,
            type_: 0,
            pad: [0; 37],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct bkey_i {
    pub k: bkey,
    pub v: bch_val,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct bkey_s_c {
    pub k: *const bkey,
    pub v: *const bch_val,
}

const BKEY_ERRNO_MAX: isize = 4095;

pub const fn bkey_s_c_err(err: i32) -> bkey_s_c {
    bkey_s_c {
        k: err as isize as *const bkey,
        v: core::ptr::null(),
    }
}

pub fn bkey_err(k: bkey_s_c) -> i32 {
    let value = k.k as isize;
    if value < 0 && -value <= BKEY_ERRNO_MAX {
        value as i32
    } else {
        0
    }
}

impl Default for bkey_s_c {
    fn default() -> Self {
        Self {
            k: core::ptr::null(),
            v: core::ptr::null(),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct bkey_s {
    pub k: *mut bkey,
    pub v: *mut bch_val,
}

impl Default for bkey_s {
    fn default() -> Self {
        Self {
            k: core::ptr::null_mut(),
            v: core::ptr::null_mut(),
        }
    }
}

pub const BKEY_FORMAT_CURRENT: bkey_format = bkey_format {
    key_u64s: BKEY_U64S,
    nr_fields: BKEY_NR_FIELDS,
    bits_per_field: [64, 64, 32, 32, 32, 64],
    field_offset: [0; 6],
};

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct bkey_format_state {
    pub field_min: [u64; BKEY_NR_FIELDS as usize],
    pub field_max: [u64; BKEY_NR_FIELDS as usize],
}

impl Default for bkey_format_state {
    fn default() -> Self {
        let mut state = Self {
            field_min: [0; BKEY_NR_FIELDS as usize],
            field_max: [0; BKEY_NR_FIELDS as usize],
        };
        bch2_bkey_format_init(&mut state);
        state
    }
}

struct pack_state<'a> {
    format: &'a bkey_format,
    bits: u32,
    w: u64,
    p: isize,
    words: *mut u64,
}

struct unpack_state<'a> {
    format: &'a bkey_format,
    bits: u32,
    w: u64,
    p: isize,
    words: *const u64,
}

fn pack_state_init<'a>(format: &'a bkey_format, k: &mut bkey_packed) -> pack_state<'a> {
    assert_ne!(format.key_u64s, 0);
    assert!(format.key_u64s as usize <= core::mem::size_of::<bkey_packed>() / 8);
    pack_state {
        format,
        bits: 64,
        w: 0,
        p: format.key_u64s as isize - 1,
        words: k as *mut bkey_packed as *mut u64,
    }
}

fn pack_state_finish(state: &mut pack_state<'_>) {
    assert!(state.p >= 0);
    assert!(state.p < state.format.key_u64s as isize);
    unsafe { *state.words.offset(state.p) = state.w };
}

fn unpack_state_init<'a>(format: &'a bkey_format, k: &bkey_packed) -> unpack_state<'a> {
    assert_ne!(format.key_u64s, 0);
    assert!(format.key_u64s as usize <= core::mem::size_of::<bkey_packed>() / 8);
    let p = format.key_u64s as isize - 1;
    let words = k as *const bkey_packed as *const u64;
    unpack_state {
        format,
        bits: 64,
        w: unsafe { *words.offset(p) },
        p,
        words,
    }
}

fn get_inc_field(state: &mut unpack_state<'_>, field: usize) -> u64 {
    let mut bits = state.format.bits_per_field[field] as u32;
    let mut v = 0;
    let offset = state.format.field_offset[field];

    if bits >= state.bits {
        v = state.w >> (64 - bits);
        bits -= state.bits;

        state.p -= 1;
        assert!(state.p >= 0 || bits == 0);
        if state.p >= 0 {
            state.w = unsafe { *state.words.offset(state.p) };
        } else {
            state.w = 0;
        }
        state.bits = 64;
    }

    v |= (state.w >> 1) >> (63 - bits);
    state.w <<= bits;
    state.bits -= bits;

    v.wrapping_add(offset)
}

fn __set_inc_field(state: &mut pack_state<'_>, field: usize, v: u64) {
    let mut bits = state.format.bits_per_field[field] as u32;

    if bits != 0 {
        if bits > state.bits {
            bits -= state.bits;
            state.w |= (v >> 1) >> (bits - 1);

            unsafe { *state.words.offset(state.p) = state.w };
            state.p -= 1;
            assert!(state.p >= 0);
            state.w = 0;
            state.bits = 64;
        }

        state.bits -= bits;
        state.w |= v << state.bits;
    }
}

fn set_inc_field(state: &mut pack_state<'_>, field: usize, mut v: u64) -> bool {
    let bits = state.format.bits_per_field[field] as u32;
    let offset = state.format.field_offset[field];

    if v < offset {
        return false;
    }

    v -= offset;
    if 64 - v.leading_zeros() > bits {
        return false;
    }

    __set_inc_field(state, field, v);
    true
}

fn set_inc_field_lossy(state: &mut pack_state<'_>, field: usize, mut v: u64) -> bool {
    let bits = state.format.bits_per_field[field] as u32;
    let offset = state.format.field_offset[field];
    let mut ret = true;

    assert!(v >= offset);
    v -= offset;
    if 64 - v.leading_zeros() > bits {
        v = if bits == 64 {
            u64::MAX
        } else {
            (1u64 << bits) - 1
        };
        ret = false;
    }

    __set_inc_field(state, field, v);
    ret
}

fn bkey_field_values(k: &bkey) -> [u64; BKEY_NR_FIELDS as usize] {
    [
        k.p.inode,
        k.p.offset,
        k.p.snapshot as u64,
        k.size as u64,
        k.bversion.hi as u64,
        k.bversion.lo,
    ]
}

pub fn bch2_bkey_pack_key(out: &mut bkey_packed, input: &bkey, format: &bkey_format) -> bool {
    assert_eq!(format.nr_fields, BKEY_NR_FIELDS);
    assert_eq!(input.format & 0x7f, KEY_FORMAT_CURRENT);

    let mut state = pack_state_init(format, out);
    unsafe {
        for i in 0..format.key_u64s as isize {
            *state.words.offset(i) = 0;
        }
    }

    for (field, value) in bkey_field_values(input).into_iter().enumerate() {
        if !set_inc_field(&mut state, field, value) {
            return false;
        }
    }

    pack_state_finish(&mut state);
    let out_u64s = format.key_u64s as u32 + input.u64s as u32 - BKEY_U64S as u32;
    assert!(out_u64s <= u8::MAX as u32);
    out.u64s = out_u64s as u8;
    out.format = input.format & 0x80;
    out.type_ = input.type_;
    true
}

pub fn bch2_bkey_transform(
    out_f: &bkey_format,
    out: &mut bkey_packed,
    in_f: &bkey_format,
    input: &bkey_packed,
) -> bool {
    assert!(input.u64s >= in_f.key_u64s);
    let out_u64s = out_f.key_u64s as u32 + input.u64s as u32 - in_f.key_u64s as u32;
    assert!(out_u64s <= u8::MAX as u32);

    let mut out_state = pack_state_init(out_f, out);
    unsafe {
        for i in 0..out_f.key_u64s as isize {
            *out_state.words.offset(i) = 0;
        }
    }
    let mut in_state = unpack_state_init(in_f, input);
    for field in 0..BKEY_NR_FIELDS as usize {
        let value = get_inc_field(&mut in_state, field);
        if !set_inc_field(&mut out_state, field, value) {
            return false;
        }
    }
    pack_state_finish(&mut out_state);
    out.u64s = out_u64s as u8;
    out.format = input.format & 0x80;
    out.type_ = input.type_;

    unsafe {
        core::ptr::copy(
            (input as *const bkey_packed as *const u64).add(in_f.key_u64s as usize),
            (out as *mut bkey_packed as *mut u64).add(out_f.key_u64s as usize),
            (input.u64s - in_f.key_u64s) as usize,
        );
    }
    true
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum bkey_pack_pos_ret {
    BKEY_PACK_POS_EXACT,
    BKEY_PACK_POS_SMALLER,
    BKEY_PACK_POS_FAIL,
}

fn __bch2_bkey_pack_pos_exact(out: &mut bkey_packed, input: bpos, format: &bkey_format) -> bool {
    let mut state = pack_state_init(format, out);
    unsafe {
        for i in 0..format.key_u64s as isize {
            *state.words.offset(i) = 0;
        }
    }

    if (input.snapshot as u64) < format.field_offset[BKEY_FIELD_SNAPSHOT]
        || input.offset < format.field_offset[BKEY_FIELD_OFFSET]
        || input.inode < format.field_offset[BKEY_FIELD_INODE]
    {
        return false;
    }

    if !set_inc_field(&mut state, BKEY_FIELD_INODE, input.inode)
        || !set_inc_field(&mut state, BKEY_FIELD_OFFSET, input.offset)
        || !set_inc_field(&mut state, BKEY_FIELD_SNAPSHOT, input.snapshot as u64)
    {
        return false;
    }

    pack_state_finish(&mut state);
    out.u64s = format.key_u64s;
    out.format = KEY_FORMAT_LOCAL_BTREE;
    out.type_ = 0;
    true
}

pub fn bch2_bkey_pack_pos(out: &mut bkey_packed, input: bpos, b: &super::types::btree) -> bool {
    __bch2_bkey_pack_pos_exact(out, input, &b.format)
}

pub fn bch2_bkey_pack_pos_lossy(
    out: &mut bkey_packed,
    input: &bpos,
    b: &super::types::btree,
) -> bkey_pack_pos_ret {
    let format = &b.format;
    let mut state = pack_state_init(format, out);
    let mut input = *input;
    let mut exact = true;

    unsafe {
        for i in 0..format.key_u64s as isize {
            *state.words.offset(i) = 0;
        }
    }

    if (input.snapshot as u64) < format.field_offset[BKEY_FIELD_SNAPSHOT] {
        let old_offset = input.offset;
        input.offset = input.offset.wrapping_sub(1);
        if old_offset == 0 {
            let old_inode = input.inode;
            input.inode = input.inode.wrapping_sub(1);
            if old_inode == 0 {
                return bkey_pack_pos_ret::BKEY_PACK_POS_FAIL;
            }
        }
        input.snapshot = KEY_SNAPSHOT_MAX;
        exact = false;
    }

    if input.offset < format.field_offset[BKEY_FIELD_OFFSET] {
        let old_inode = input.inode;
        input.inode = input.inode.wrapping_sub(1);
        if old_inode == 0 {
            return bkey_pack_pos_ret::BKEY_PACK_POS_FAIL;
        }
        input.offset = KEY_OFFSET_MAX;
        input.snapshot = KEY_SNAPSHOT_MAX;
        exact = false;
    }

    if input.inode < format.field_offset[BKEY_FIELD_INODE] {
        return bkey_pack_pos_ret::BKEY_PACK_POS_FAIL;
    }

    if !set_inc_field_lossy(&mut state, BKEY_FIELD_INODE, input.inode) {
        input.offset = KEY_OFFSET_MAX;
        input.snapshot = KEY_SNAPSHOT_MAX;
        exact = false;
    }
    if !set_inc_field_lossy(&mut state, BKEY_FIELD_OFFSET, input.offset) {
        input.snapshot = KEY_SNAPSHOT_MAX;
        exact = false;
    }
    if !set_inc_field_lossy(&mut state, BKEY_FIELD_SNAPSHOT, input.snapshot as u64) {
        exact = false;
    }

    pack_state_finish(&mut state);
    out.u64s = format.key_u64s;
    out.format = KEY_FORMAT_LOCAL_BTREE;
    out.type_ = 0;

    if exact {
        bkey_pack_pos_ret::BKEY_PACK_POS_EXACT
    } else {
        bkey_pack_pos_ret::BKEY_PACK_POS_SMALLER
    }
}

pub fn __bch2_bkey_unpack_key(format: &bkey_format, out: &mut bkey, input: &bkey_packed) {
    assert_eq!(format.nr_fields, BKEY_NR_FIELDS);
    assert!(input.u64s >= format.key_u64s);
    assert_eq!(input.format & 0x7f, KEY_FORMAT_LOCAL_BTREE);
    let out_u64s = input.u64s as u32 - format.key_u64s as u32 + BKEY_U64S as u32;
    assert!(out_u64s <= u8::MAX as u32);

    let mut state = unpack_state_init(format, input);
    let inode = get_inc_field(&mut state, BKEY_FIELD_INODE);
    let offset = get_inc_field(&mut state, BKEY_FIELD_OFFSET);
    let snapshot = get_inc_field(&mut state, BKEY_FIELD_SNAPSHOT);
    let size = get_inc_field(&mut state, BKEY_FIELD_SIZE);
    let version_hi = get_inc_field(&mut state, BKEY_FIELD_VERSION_HI);
    let version_lo = get_inc_field(&mut state, BKEY_FIELD_VERSION_LO);

    *out = bkey {
        u64s: out_u64s as u8,
        format: KEY_FORMAT_CURRENT | (input.format & 0x80),
        type_: input.type_,
        pad: [0],
        bversion: bversion {
            hi: version_hi as u32,
            lo: version_lo,
        },
        size: size as u32,
        p: bpos {
            inode,
            offset,
            snapshot: snapshot as u32,
        },
    };
}

pub unsafe fn bch2_bkey_unpack(
    b: *const super::types::btree,
    dst: *mut bkey_i,
    src: *const bkey_packed,
) {
    if bkey_packed(&*src) {
        __bch2_bkey_unpack_key(&(*b).format, &mut (*dst).k, &*src);
    } else {
        core::ptr::copy_nonoverlapping(
            src.cast::<u64>(),
            core::ptr::addr_of_mut!((*dst).k).cast::<u64>(),
            BKEY_U64S as usize,
        );
    }
    core::ptr::copy(
        src.cast::<u64>()
            .add(bkeyp_key_u64s(&(*b).format, &*src) as usize),
        core::ptr::addr_of_mut!((*dst).v).cast::<u64>(),
        bkeyp_val_u64s(&(*b).format, &*src) as usize,
    );
}

pub fn __bkey_unpack_pos(format: &bkey_format, input: &bkey_packed) -> bpos {
    assert_eq!(format.nr_fields, BKEY_NR_FIELDS);
    assert!(input.u64s >= format.key_u64s);
    assert_eq!(input.format & 0x7f, KEY_FORMAT_LOCAL_BTREE);

    let mut state = unpack_state_init(format, input);
    bpos {
        inode: get_inc_field(&mut state, BKEY_FIELD_INODE),
        offset: get_inc_field(&mut state, BKEY_FIELD_OFFSET),
        snapshot: get_inc_field(&mut state, BKEY_FIELD_SNAPSHOT) as u32,
    }
}

pub unsafe fn __bch2_bkey_cmp_left_packed(
    b: *const super::types::btree,
    left: *const bkey_packed,
    right: *const bpos,
) -> i32 {
    let left_pos = if bkey_packed(&*left) {
        __bkey_unpack_pos(&(*b).format, &*left)
    } else {
        (*left.cast::<bkey>()).p
    };
    bpos_cmp(left_pos, *right)
}

pub unsafe fn bkey_cmp_left_packed(
    b: *const super::types::btree,
    left: *const bkey_packed,
    right: *const bpos,
) -> i32 {
    __bch2_bkey_cmp_left_packed(b, left, right)
}

pub fn bch2_bkey_format_init(state: &mut bkey_format_state) {
    for i in 0..state.field_min.len() {
        state.field_min[i] = u64::MAX;
    }
    for i in 0..state.field_max.len() {
        state.field_max[i] = 0;
    }
    state.field_min[BKEY_FIELD_SIZE] = 0;
}

pub fn __bkey_format_add(state: &mut bkey_format_state, field: u32, v: u64) {
    let field = field as usize;
    state.field_min[field] = state.field_min[field].min(v);
    state.field_max[field] = state.field_max[field].max(v);
}

pub fn bch2_bkey_format_add_key(state: &mut bkey_format_state, k: &bkey) {
    for (field, value) in bkey_field_values(k).into_iter().enumerate() {
        __bkey_format_add(state, field as u32, value);
    }
}

pub fn bch2_bkey_format_add_pos(state: &mut bkey_format_state, p: bpos) {
    let mut field = 0;
    __bkey_format_add(state, field, p.inode);
    field += 1;
    __bkey_format_add(state, field, p.offset);
    field += 1;
    __bkey_format_add(state, field, p.snapshot as u64);
}

fn set_format_field(format: &mut bkey_format, field: usize, mut bits: u32, mut offset: u64) {
    let unpacked_bits = BKEY_FORMAT_CURRENT.bits_per_field[field] as u32;
    let unpacked_max = if unpacked_bits == 64 {
        u64::MAX
    } else {
        (1u64 << unpacked_bits) - 1
    };

    bits = bits.min(unpacked_bits);
    if bits == unpacked_bits {
        offset = 0;
    } else {
        offset = offset.min(unpacked_max - ((1u64 << bits) - 1));
    }

    format.bits_per_field[field] = bits as u8;
    format.field_offset[field] = offset;
}

pub fn bch2_bkey_format_done(state: &mut bkey_format_state) -> bkey_format {
    let mut bits = KEY_PACKED_BITS_START;
    let mut ret = bkey_format {
        key_u64s: 0,
        nr_fields: BKEY_NR_FIELDS,
        bits_per_field: [0; BKEY_NR_FIELDS as usize],
        field_offset: [0; BKEY_NR_FIELDS as usize],
    };

    for i in 0..state.field_min.len() {
        state.field_min[i] = state.field_min[i].min(state.field_max[i]);
        set_format_field(
            &mut ret,
            i,
            64 - state.field_max[i]
                .wrapping_sub(state.field_min[i])
                .leading_zeros(),
            state.field_min[i],
        );
        bits += ret.bits_per_field[i] as u32;
    }

    ret.key_u64s = bits.div_ceil(64) as u8;
    bits = ret.key_u64s as u32 * 64 - bits;

    for i in 0..ret.bits_per_field.len() {
        let rounded = (ret.bits_per_field[i] as u32).div_ceil(8) * 8;
        let extra = rounded - ret.bits_per_field[i] as u32;
        if extra <= bits {
            let offset = ret.field_offset[i];
            let field_bits = ret.bits_per_field[i] as u32 + extra;
            set_format_field(&mut ret, i, field_bits, offset);
            bits -= extra;
        }
    }

    ret
}

pub unsafe fn bch2_compute_bkey_unpack_consts(b: *mut super::types::btree) {
    #[cfg(target_endian = "little")]
    {
        let format = &(*b).format;
        let mut bit_offset = format.key_u64s as u32 * 64;
        (*b).byte_aligned_fields = true;

        for field in 0..BKEY_NR_FIELDS as usize {
            (*b).unpack[field].byte_offset = 0;
            (*b).unpack[field].shift_right = 64;
            let bits = format.bits_per_field[field] as u32;
            if bits == 0 {
                continue;
            }
            if bits > 64 {
                (*b).byte_aligned_fields = false;
                return;
            }

            bit_offset -= bits;
            let field_msb_bit = bit_offset + bits - 1;
            if field_msb_bit % 8 != 7 {
                (*b).byte_aligned_fields = false;
                return;
            }

            (*b).unpack[field].byte_offset = ((field_msb_bit + 1) as i32 / 8 - 8) as i8;
            (*b).unpack[field].shift_right = (64 - bits) as u8;
        }
    }
    #[cfg(target_endian = "big")]
    {
        (*b).byte_aligned_fields = false;
    }
}

pub fn bch2_bkey_format_field_overflows(format: &bkey_format, i: u32) -> bool {
    let i = i as usize;
    let format_bits = format.bits_per_field[i] as u32;
    let unpacked_bits = BKEY_FORMAT_CURRENT.bits_per_field[i] as u32;
    let unpacked_mask = if unpacked_bits == 64 {
        u64::MAX
    } else {
        (1u64 << unpacked_bits) - 1
    };
    let field_offset = format.field_offset[i];

    if format_bits > unpacked_bits {
        return true;
    }
    if format_bits == unpacked_bits && field_offset != 0 {
        return true;
    }

    let format_mask = if format_bits == 0 {
        0
    } else if format_bits == 64 {
        u64::MAX
    } else {
        (1u64 << format_bits) - 1
    };

    field_offset.wrapping_add(format_mask) & unpacked_mask < field_offset
}

pub const fn bpos_eq(l: bpos, r: bpos) -> bool {
    ((l.inode ^ r.inode) | (l.offset ^ r.offset) | (l.snapshot ^ r.snapshot) as u64) == 0
}

pub const fn bpos_lt(l: bpos, r: bpos) -> bool {
    if l.inode != r.inode {
        l.inode < r.inode
    } else if l.offset != r.offset {
        l.offset < r.offset
    } else if l.snapshot != r.snapshot {
        l.snapshot < r.snapshot
    } else {
        false
    }
}

pub const fn bpos_le(l: bpos, r: bpos) -> bool {
    if l.inode != r.inode {
        l.inode < r.inode
    } else if l.offset != r.offset {
        l.offset < r.offset
    } else if l.snapshot != r.snapshot {
        l.snapshot < r.snapshot
    } else {
        true
    }
}

pub const fn bpos_gt(l: bpos, r: bpos) -> bool {
    bpos_lt(r, l)
}

pub const fn bpos_ge(l: bpos, r: bpos) -> bool {
    bpos_le(r, l)
}

pub const fn bpos_cmp(l: bpos, r: bpos) -> i32 {
    if l.inode < r.inode {
        -1
    } else if l.inode > r.inode {
        1
    } else if l.offset < r.offset {
        -1
    } else if l.offset > r.offset {
        1
    } else if l.snapshot < r.snapshot {
        -1
    } else if l.snapshot > r.snapshot {
        1
    } else {
        0
    }
}

pub const fn bpos_min(l: bpos, r: bpos) -> bpos {
    if bpos_lt(l, r) {
        l
    } else {
        r
    }
}

pub fn bch2_key_resize(k: &mut bkey, new_size: u32) {
    k.p.offset = k
        .p
        .offset
        .wrapping_sub(k.size as u64)
        .wrapping_add(new_size as u64);
    k.size = new_size;
}

pub const fn bpos_max(l: bpos, r: bpos) -> bpos {
    if bpos_gt(l, r) {
        l
    } else {
        r
    }
}

pub const fn bkey_eq(l: bpos, r: bpos) -> bool {
    ((l.inode ^ r.inode) | (l.offset ^ r.offset)) == 0
}

pub const fn bkey_lt(l: bpos, r: bpos) -> bool {
    if l.inode != r.inode {
        l.inode < r.inode
    } else {
        l.offset < r.offset
    }
}

pub const fn bkey_le(l: bpos, r: bpos) -> bool {
    if l.inode != r.inode {
        l.inode < r.inode
    } else {
        l.offset <= r.offset
    }
}

pub const fn bkey_gt(l: bpos, r: bpos) -> bool {
    bkey_lt(r, l)
}

pub const fn bkey_ge(l: bpos, r: bpos) -> bool {
    bkey_le(r, l)
}

pub const fn bkey_cmp(l: bpos, r: bpos) -> i32 {
    if l.inode < r.inode {
        -1
    } else if l.inode > r.inode {
        1
    } else if l.offset < r.offset {
        -1
    } else if l.offset > r.offset {
        1
    } else {
        0
    }
}

pub const fn bkey_min(l: bpos, r: bpos) -> bpos {
    if bkey_lt(l, r) {
        l
    } else {
        r
    }
}

pub const fn bkey_max(l: bpos, r: bpos) -> bpos {
    if bkey_gt(l, r) {
        l
    } else {
        r
    }
}

pub const fn bversion_cmp(l: bversion, r: bversion) -> i32 {
    if l.hi < r.hi {
        -1
    } else if l.hi > r.hi {
        1
    } else if l.lo < r.lo {
        -1
    } else if l.lo > r.lo {
        1
    } else {
        0
    }
}

pub const fn bversion_eq(l: bversion, r: bversion) -> bool {
    l.hi == r.hi && l.lo == r.lo
}

pub const fn bversion_zero(v: bversion) -> bool {
    bversion_cmp(v, ZERO_VERSION) == 0
}

/// Matches bcachefs `bch2_bkey_maybe_mergable()`.
pub const fn bch2_bkey_maybe_mergable(l: &bkey, r: &bkey) -> bool {
    l.type_ == r.type_
        && bversion_cmp(l.bversion, r.bversion) == 0
        && bkey_eq(l.p, bkey_start_pos(r))
}

/// Matches the currently implemented `key_merge` operation in bcachefs.
pub unsafe fn bch2_bkey_merge(
    _c: *mut super::types::bch_fs,
    l: bkey_s,
    r: bkey_s_c,
) -> bool {
    if l.k.is_null() || r.k.is_null() {
        return false;
    }
    if !bch2_bkey_maybe_mergable(&*l.k, &*r.k) {
        return false;
    }
    let Some(size) = (*l.k).size.checked_add((*r.k).size) else {
        return false;
    };
    if (*l.k).type_ == super::bset::KEY_TYPE_reservation {
        return super::bset::bch2_reservation_merge(l, r);
    }
    if (*l.k).type_ != super::bset::KEY_TYPE_set {
        return false;
    }
    bch2_key_resize(&mut *l.k, size);
    true
}

pub const fn bkey_fields_eq(l: &bkey, r: &bkey) -> bool {
    l.u64s == r.u64s
        && l.type_ == r.type_
        && bpos_eq(l.p, r.p)
        && bversion_eq(l.bversion, r.bversion)
        && l.size == r.size
}

pub unsafe fn bkey_and_val_eq(l: bkey_s_c, r: bkey_s_c) -> bool {
    if l.k.is_null() || r.k.is_null() || !bkey_fields_eq(&*l.k, &*r.k) {
        return false;
    }
    let bytes = bkey_val_bytes(&*l.k);
    if bytes == 0 {
        return true;
    }
    if l.v.is_null() || r.v.is_null() {
        return false;
    }
    core::slice::from_raw_parts(l.v.cast::<u8>(), bytes)
        == core::slice::from_raw_parts(r.v.cast::<u8>(), bytes)
}

pub const fn bkey_packed(k: &bkey_packed) -> bool {
    (k.format & 0x7f) != KEY_FORMAT_CURRENT
}

pub const fn bkey_deleted(k: &bkey_packed) -> bool {
    k.type_ == 0
}

pub unsafe fn bkey_p_next(k: *mut bkey_packed) -> *mut bkey_packed {
    (k as *mut u64).add((*k).u64s as usize).cast()
}

pub unsafe fn bkey_next(k: *mut bkey_i) -> *mut bkey_i {
    k.cast::<u64>().add((*k).k.u64s as usize).cast()
}

pub unsafe fn bkey_copy(dst: *mut bkey_i, src: *const bkey_i) {
    core::ptr::copy_nonoverlapping(src.cast::<u64>(), dst.cast::<u64>(), (*src).k.u64s as usize);
}

pub const fn bkey_format_key_bits(format: &bkey_format) -> u32 {
    format.bits_per_field[BKEY_FIELD_INODE] as u32
        + format.bits_per_field[BKEY_FIELD_OFFSET] as u32
        + format.bits_per_field[BKEY_FIELD_SNAPSHOT] as u32
}

pub const fn bkeyp_key_u64s(format: &bkey_format, k: &bkey_packed) -> u32 {
    if bkey_packed(k) {
        format.key_u64s as u32
    } else {
        BKEY_U64S as u32
    }
}

pub const fn bkeyp_u64s_valid(format: &bkey_format, k: &bkey_packed) -> bool {
    (k.u64s as u32).wrapping_sub(bkeyp_key_u64s(format, k)) <= (u8::MAX - BKEY_U64S) as u32
}

pub const fn bkeyp_key_bytes(format: &bkey_format, k: &bkey_packed) -> u32 {
    bkeyp_key_u64s(format, k) * core::mem::size_of::<u64>() as u32
}

pub const fn bkeyp_val_u64s(format: &bkey_format, k: &bkey_packed) -> u32 {
    (k.u64s as u32).wrapping_sub(bkeyp_key_u64s(format, k))
}

pub const fn bkeyp_val_bytes(format: &bkey_format, k: &bkey_packed) -> usize {
    bkeyp_val_u64s(format, k) as usize * core::mem::size_of::<u64>()
}

pub const fn set_bkeyp_val_u64s(format: &bkey_format, k: &mut bkey_packed, val_u64s: u32) {
    k.u64s = bkeyp_key_u64s(format, k).wrapping_add(val_u64s) as u8;
}

pub const fn bkey_bytes(k: &bkey) -> usize {
    k.u64s as usize * core::mem::size_of::<u64>()
}

pub const fn bkey_val_u64s(k: &bkey) -> u32 {
    (k.u64s as u32).wrapping_sub(BKEY_U64S as u32)
}

pub const fn bkey_val_bytes(k: &bkey) -> usize {
    bkey_val_u64s(k) as usize * core::mem::size_of::<u64>()
}

pub const fn set_bkey_val_u64s(k: &mut bkey, val_u64s: u32) {
    let u64s = BKEY_U64S as u32 + val_u64s;
    assert!(u64s <= u8::MAX as u32);
    k.u64s = u64s as u8;
}

pub const fn set_bkey_val_bytes(k: &mut bkey, bytes: u32) {
    set_bkey_val_u64s(k, bytes.div_ceil(core::mem::size_of::<u64>() as u32));
}

pub fn bpos_successor(mut p: bpos) -> bpos {
    let (snapshot, snapshot_overflow) = p.snapshot.overflowing_add(1);
    p.snapshot = snapshot;
    if snapshot_overflow {
        let (offset, offset_overflow) = p.offset.overflowing_add(1);
        p.offset = offset;
        if offset_overflow {
            let (inode, inode_overflow) = p.inode.overflowing_add(1);
            p.inode = inode;
            assert!(!inode_overflow);
        }
    }
    p
}

pub fn bpos_predecessor(mut p: bpos) -> bpos {
    let (snapshot, snapshot_overflow) = p.snapshot.overflowing_sub(1);
    p.snapshot = snapshot;
    if snapshot_overflow {
        let (offset, offset_overflow) = p.offset.overflowing_sub(1);
        p.offset = offset;
        if offset_overflow {
            let (inode, inode_overflow) = p.inode.overflowing_sub(1);
            p.inode = inode;
            assert!(!inode_overflow);
        }
    }
    p
}

pub fn bpos_nosnap_successor(mut p: bpos) -> bpos {
    p.snapshot = 0;
    let (offset, offset_overflow) = p.offset.overflowing_add(1);
    p.offset = offset;
    if offset_overflow {
        let (inode, inode_overflow) = p.inode.overflowing_add(1);
        p.inode = inode;
        assert!(!inode_overflow);
    }
    p
}

pub fn bpos_nosnap_predecessor(mut p: bpos) -> bpos {
    p.snapshot = 0;
    let (offset, offset_overflow) = p.offset.overflowing_sub(1);
    p.offset = offset;
    if offset_overflow {
        let (inode, inode_overflow) = p.inode.overflowing_sub(1);
        p.inode = inode;
        assert!(!inode_overflow);
    }
    p
}

pub const fn bkey_start_offset(k: &bkey) -> u64 {
    k.p.offset.wrapping_sub(k.size as u64)
}

pub const fn bkey_start_pos(k: &bkey) -> bpos {
    bpos {
        inode: k.p.inode,
        offset: bkey_start_offset(k),
        snapshot: k.p.snapshot,
    }
}

pub const fn bpos_with_snapshot(mut p: bpos, snapshot: u32) -> bpos {
    p.snapshot = snapshot;
    p
}

pub const fn bkey_init(k: &mut bkey) {
    *k = bkey {
        u64s: BKEY_U64S,
        format: KEY_FORMAT_CURRENT,
        type_: 0,
        pad: [0],
        bversion: ZERO_VERSION,
        size: 0,
        p: POS(0, 0),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bkey_and_val_eq_matches_local_memcmp_rule() {
        let mut left = bkey_i::default();
        bkey_init(&mut left.k);
        left.k.u64s = BKEY_U64S + 1;
        left.k.type_ = 7;
        left.k.p = POS(1, 2);
        let right = left;
        let left_value = [0x55u64];
        let mut right_value = [0x55u64];
        unsafe {
            assert!(bkey_and_val_eq(
                bkey_s_c {
                    k: &left.k,
                    v: left_value.as_ptr().cast(),
                },
                bkey_s_c {
                    k: &right.k,
                    v: right_value.as_ptr().cast(),
                }
            ));
            right_value[0] ^= 1;
            assert!(!bkey_and_val_eq(
                bkey_s_c {
                    k: &left.k,
                    v: left_value.as_ptr().cast(),
                },
                bkey_s_c {
                    k: &right.k,
                    v: right_value.as_ptr().cast(),
                }
            ));
        }
    }

    #[test]
    fn bcachefs_disk_layout() {
        assert_eq!(core::mem::size_of::<bpos>(), 20);
        assert_eq!(core::mem::align_of::<bpos>(), 4);
        assert_eq!(core::mem::size_of::<bversion>(), 12);
        assert_eq!(core::mem::align_of::<bversion>(), 4);
        assert_eq!(core::mem::size_of::<bkey>(), 40);
        assert_eq!(core::mem::align_of::<bkey>(), 8);
        assert_eq!(core::mem::offset_of!(bkey, bversion), 4);
        assert_eq!(core::mem::offset_of!(bkey, size), 16);
        assert_eq!(core::mem::offset_of!(bkey, p), 20);
        assert_eq!(core::mem::size_of::<bkey_packed>(), 40);
        assert_eq!(core::mem::align_of::<bkey_packed>(), 8);
        assert_eq!(core::mem::size_of::<bkey_format>(), 56);
    }

    #[test]
    fn maybe_mergable_matches_bcachefs_predicate() {
        let mut left = bkey::default();
        bkey_init(&mut left);
        left.type_ = 7;
        left.p = POS(1, 10);
        left.size = 4;

        let mut right = bkey::default();
        bkey_init(&mut right);
        right.type_ = left.type_;
        right.p = POS(1, 14);
        right.size = 4;

        assert!(bch2_bkey_maybe_mergable(&left, &right));
        right.p = POS(1, 15);
        assert!(!bch2_bkey_maybe_mergable(&left, &right));
    }

    #[test]
    fn set_key_merge_resizes_only_matching_adjacent_keys() {
        let mut left = bkey::default();
        bkey_init(&mut left);
        left.type_ = super::super::bset::KEY_TYPE_set;
        left.p = POS(1, 10);
        left.size = 4;

        let mut right = bkey::default();
        bkey_init(&mut right);
        right.type_ = left.type_;
        right.p = POS(1, 13);
        right.size = 3;

        let merged = unsafe {
            bch2_bkey_merge(
                core::ptr::null_mut(),
                bkey_s {
                    k: &mut left,
                    v: core::ptr::null_mut(),
                },
                bkey_s_c {
                    k: &right,
                    v: core::ptr::null(),
                },
            )
        };
        assert!(merged);
        assert_eq!(left.size, 7);
    }

    #[test]
    fn reservation_key_merge_dispatches_to_extent_merge_table() {
        let mut left = bkey::default();
        bkey_init(&mut left);
        left.type_ = super::super::bset::KEY_TYPE_reservation;
        left.p = POS(1, 10);
        left.size = 4;
        let mut right = left;
        right.p = POS(1, 13);
        right.size = 3;
        let mut left_v = super::super::bset::bch_reservation {
            generation: 7,
            nr_replicas: 2,
            ..Default::default()
        };
        let right_v = left_v;
        let merged = unsafe {
            bch2_bkey_merge(
                core::ptr::null_mut(),
                bkey_s {
                    k: &mut left,
                    v: core::ptr::addr_of_mut!(left_v.v),
                },
                bkey_s_c {
                    k: &right,
                    v: core::ptr::addr_of!(right_v.v),
                },
            )
        };
        assert!(merged);
        assert_eq!(left.size, 7);
    }

    #[test]
    fn bcachefs_position_order_and_wrap() {
        let p = SPOS(1, 2, u32::MAX);
        assert_eq!(bpos_successor(p), SPOS(1, 3, 0));
        assert_eq!(bpos_predecessor(SPOS(1, 3, 0)), p);
        assert_eq!(bpos_nosnap_successor(SPOS(1, u64::MAX, 7)), SPOS(2, 0, 0));
        assert_eq!(bpos_nosnap_predecessor(SPOS(2, 0, 7)), SPOS(1, u64::MAX, 0));
        assert!(bpos_lt(SPOS(1, 2, 3), SPOS(1, 2, 4)));
        assert!(bkey_eq(SPOS(1, 2, 3), SPOS(1, 2, 4)));
    }

    #[test]
    fn bcachefs_pack_unpack_key_fields() {
        let format = bkey_format {
            key_u64s: 2,
            nr_fields: BKEY_NR_FIELDS,
            bits_per_field: [8, 9, 4, 5, 6, 16],
            field_offset: [100, 200, 3, 0, 0, 1_000],
        };
        let input = bkey {
            u64s: BKEY_U64S,
            format: KEY_FORMAT_CURRENT | 0x80,
            type_: 7,
            pad: [0],
            bversion: bversion { lo: 12_345, hi: 17 },
            size: 23,
            p: SPOS(123, 456, 9),
        };
        let mut packed = bkey_packed::default();
        assert!(bch2_bkey_pack_key(&mut packed, &input, &format));
        assert_eq!(packed.u64s, format.key_u64s);
        assert_eq!(packed.format & 0x7f, KEY_FORMAT_LOCAL_BTREE);
        assert_eq!(packed.format & 0x80, 0x80);

        let mut unpacked = bkey::default();
        __bch2_bkey_unpack_key(&format, &mut unpacked, &packed);
        assert_eq!(unpacked, input);
        assert_eq!(__bkey_unpack_pos(&format, &packed), input.p);
    }

    #[test]
    fn bcachefs_pack_rejects_field_underflow_and_overflow() {
        let format = bkey_format {
            key_u64s: 1,
            nr_fields: BKEY_NR_FIELDS,
            bits_per_field: [4, 4, 4, 4, 4, 4],
            field_offset: [10, 20, 0, 0, 0, 0],
        };
        let mut packed = bkey_packed::default();
        let mut input = bkey::default();
        bkey_init(&mut input);
        input.p = SPOS(9, 20, 0);
        assert!(!bch2_bkey_pack_key(&mut packed, &input, &format));
        input.p = SPOS(10, 36, 0);
        assert!(!bch2_bkey_pack_key(&mut packed, &input, &format));
    }

    #[test]
    fn bcachefs_precomputes_byte_aligned_unpack_fields() {
        unsafe {
            let mut b = crate::btree::types::btree::default();
            b.format = bkey_format {
                key_u64s: 1,
                nr_fields: BKEY_NR_FIELDS,
                bits_per_field: [8, 8, 8, 0, 0, 0],
                field_offset: [1, 2, 3, 0, 0, 0],
            };
            bch2_compute_bkey_unpack_consts(&mut b);
            assert!(b.byte_aligned_fields);
            assert_eq!(b.unpack[0].byte_offset, 0);
            assert_eq!(b.unpack[1].byte_offset, -1);
            assert_eq!(b.unpack[2].byte_offset, -2);
            assert_eq!(b.unpack[0].shift_right, 56);
            assert_eq!(b.unpack[3].shift_right, 64);

            b.format.bits_per_field = [7, 8, 8, 0, 0, 0];
            bch2_compute_bkey_unpack_consts(&mut b);
            assert!(!b.byte_aligned_fields);
        }
    }

    #[test]
    fn bcachefs_pack_pos_exact_and_lossy_roll_down() {
        let mut b = crate::btree::types::btree::default();
        b.format = bkey_format {
            key_u64s: 1,
            nr_fields: BKEY_NR_FIELDS,
            bits_per_field: [8, 8, 4, 0, 0, 0],
            field_offset: [10, 20, 3, 0, 0, 0],
        };

        let mut packed = bkey_packed::default();
        let exact = SPOS(11, 21, 5);
        assert!(bch2_bkey_pack_pos(&mut packed, exact, &b));
        assert_eq!(__bkey_unpack_pos(&b.format, &packed), exact);
        assert_eq!(
            bch2_bkey_pack_pos_lossy(&mut packed, &exact, &b),
            bkey_pack_pos_ret::BKEY_PACK_POS_EXACT
        );

        let underflow = SPOS(11, 21, 2);
        assert_eq!(
            bch2_bkey_pack_pos_lossy(&mut packed, &underflow, &b),
            bkey_pack_pos_ret::BKEY_PACK_POS_SMALLER
        );
        assert_eq!(__bkey_unpack_pos(&b.format, &packed), SPOS(11, 20, 18));
        assert!(bpos_lt(__bkey_unpack_pos(&b.format, &packed), underflow));
    }

    #[test]
    fn bcachefs_format_state_builds_packable_format() {
        let mut state = bkey_format_state::default();
        let mut first = bkey::default();
        bkey_init(&mut first);
        first.p = SPOS(100, 1_000, 3);
        first.size = 8;
        let mut last = first;
        last.p = SPOS(130, 1_500, 9);
        last.size = 32;
        bch2_bkey_format_add_key(&mut state, &first);
        bch2_bkey_format_add_key(&mut state, &last);

        let format = bch2_bkey_format_done(&mut state);
        assert_eq!(format.nr_fields, BKEY_NR_FIELDS);
        assert!(format.key_u64s >= 1);
        for i in 0..BKEY_NR_FIELDS as u32 {
            assert!(!bch2_bkey_format_field_overflows(&format, i));
        }

        let mut packed = bkey_packed::default();
        assert!(bch2_bkey_pack_key(&mut packed, &first, &format));
        assert!(bch2_bkey_pack_key(&mut packed, &last, &format));
    }
}
