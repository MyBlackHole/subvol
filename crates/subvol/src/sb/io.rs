use super::{
    bcachefs_metadata_version_current, bch_member, bch_sb, bch_sb_field, bch_sb_field_journal_v2,
    bch_sb_field_journal_v2_entry, bch_sb_field_members_v2, bch_sb_handle, bch_sb_layout,
    BCH_SB_FIELD_journal_v2, BCH_SB_FIELD_members_v2, BCHFS_MAGIC, BCH_SB_HANDLE_FS_SB,
    BCH_SB_HANDLE_HAVE_LAYOUT, BCH_SB_LAYOUT_SIZE_BITS_MAX, BCH_SB_SECTOR, BCH_VERSION_MAJOR,
};

const BCH_ERR_invalid_sb_layout: i32 = -1;
const BCH_ERR_invalid_sb_layout_type: i32 = -2;
const BCH_ERR_invalid_sb_layout_nr_superblocks: i32 = -3;
const BCH_ERR_invalid_sb_layout_superblocks_overlap: i32 = -4;
const BCH_ERR_invalid_sb_layout_sb_max_size_bits: i32 = -5;
const BCH_ERR_invalid_sb_version: i32 = -6;
const BCH_ERR_ENOSPC_sb: i32 = -7;
const BCH_ERR_ENOMEM_sb_buf_realloc: i32 = -8;
const BCH_ERR_invalid_sb_features: i32 = -15;
const BCH_ERR_invalid_sb_uuid: i32 = -16;
const BCH_ERR_invalid_sb_offset: i32 = -17;
const BCH_ERR_invalid_sb_too_many_members: i32 = -18;
const BCH_ERR_invalid_sb_dev_idx: i32 = -19;
const BCH_ERR_invalid_sb_time_precision: i32 = -20;
const BCH_ERR_invalid_sb_field_size: i32 = -21;
const BCH_ERR_invalid_sb_members_missing: i32 = -22;
const BCH_ERR_invalid_sb_members: i32 = -23;
const BCH_ERR_invalid_sb_journal: i32 = -24;
const BCH_ERR_invalid_sb_field_type: i32 = -25;

pub const fn __vstruct_bytes(u64s: u32) -> usize {
    core::mem::size_of::<bch_sb>() + u64s as usize * core::mem::size_of::<u64>()
}

unsafe fn vstruct_last(sb: *mut bch_sb) -> *mut bch_sb_field {
    sb.cast::<u8>()
        .add(core::mem::size_of::<bch_sb>() + (*sb).u64s as usize * 8)
        .cast()
}

unsafe fn vstruct_next(field: *mut bch_sb_field) -> *mut bch_sb_field {
    field.cast::<u64>().add((*field).u64s as usize).cast()
}

pub unsafe fn bch2_sb_field_get_id(sb: *mut bch_sb, type_: u32) -> *mut bch_sb_field {
    let last = vstruct_last(sb);
    let mut field = (*sb).start.as_mut_ptr();
    while field < last {
        if (*field).type_ == type_ {
            return field;
        }
        field = vstruct_next(field);
    }
    core::ptr::null_mut()
}

unsafe fn __bch2_sb_field_resize(
    sb: *mut bch_sb_handle,
    mut field: *mut bch_sb_field,
    u64s: u32,
) -> *mut bch_sb_field {
    let old_u64s = if field.is_null() { 0 } else { (*field).u64s };
    let sb_u64s = (*(*sb).sb).u64s + u64s - old_u64s;
    assert!(__vstruct_bytes(sb_u64s) <= (*sb).buffer_size);

    if field.is_null() && u64s == 0 {
    } else if field.is_null() {
        field = vstruct_last((*sb).sb);
        core::ptr::write_bytes(field.cast::<u8>(), 0, u64s as usize * 8);
        (*field).u64s = u64s;
        (*field).type_ = 0;
    } else {
        let src = field.cast::<u64>().add(old_u64s as usize).cast::<u8>();
        let dst = if u64s != 0 {
            (*field).u64s = u64s;
            field.cast::<u64>().add(u64s as usize).cast::<u8>()
        } else {
            field.cast::<u8>()
        };
        let end = (*sb).sb.cast::<u8>().add(__vstruct_bytes((*(*sb).sb).u64s));
        core::ptr::copy(src, dst, end.offset_from(src) as usize);
        if dst > src {
            core::ptr::write_bytes(src, 0, dst.offset_from(src) as usize);
        }
    }

    (*(*sb).sb).u64s = sb_u64s;
    if u64s != 0 {
        field
    } else {
        core::ptr::null_mut()
    }
}

pub unsafe fn bch2_free_super(sb: *mut bch_sb_handle) {
    if !(*sb).s_bdev_file.is_null() {
        drop(Box::from_raw((*sb).s_bdev_file.cast::<std::fs::File>()));
    }
    if !(*sb).sb.is_null() {
        let words = (*sb).buffer_size / core::mem::size_of::<u64>();
        let slice = core::ptr::slice_from_raw_parts_mut((*sb).sb.cast::<u64>(), words);
        drop(Box::from_raw(slice));
    }
    *sb = bch_sb_handle::default();
}

pub unsafe fn bch2_sb_realloc(sb: *mut bch_sb_handle, u64s: u32) -> i32 {
    let new_bytes = __vstruct_bytes(u64s);
    let new_buffer_size = new_bytes.next_power_of_two();

    if !(*sb).sb.is_null() && (*sb).buffer_size >= new_buffer_size {
        return 0;
    }
    if !(*sb).sb.is_null() && (*sb).flags & BCH_SB_HANDLE_HAVE_LAYOUT != 0 {
        let max_bytes = 512usize << (*(*sb).sb).layout.sb_max_size_bits;
        if new_bytes > max_bytes {
            return BCH_ERR_ENOSPC_sb;
        }
    }
    if (*sb).buffer_size >= new_buffer_size && !(*sb).sb.is_null() {
        return 0;
    }

    let new_words = new_buffer_size / core::mem::size_of::<u64>();
    let mut words = Vec::<u64>::new();
    if words.try_reserve_exact(new_words).is_err() {
        return BCH_ERR_ENOMEM_sb_buf_realloc;
    }
    words.resize(new_words, 0);
    let mut new_sb = words.into_boxed_slice();
    if !(*sb).sb.is_null() {
        core::ptr::copy_nonoverlapping(
            (*sb).sb.cast::<u8>(),
            new_sb.as_mut_ptr().cast::<u8>(),
            (*sb).buffer_size,
        );
        let old_words = (*sb).buffer_size / core::mem::size_of::<u64>();
        let old = core::ptr::slice_from_raw_parts_mut((*sb).sb.cast::<u64>(), old_words);
        drop(Box::from_raw(old));
    }
    (*sb).sb = Box::into_raw(new_sb).cast::<u64>().cast();
    (*sb).buffer_size = new_buffer_size;
    0
}

pub unsafe fn bch2_sb_field_resize_id(
    sb: *mut bch_sb_handle,
    type_: u32,
    u64s: u32,
) -> *mut bch_sb_field {
    let field = bch2_sb_field_get_id((*sb).sb, type_);
    let old_u64s = if field.is_null() { 0 } else { (*field).u64s };
    let new_sb_u64s = ((*(*sb).sb).u64s as i64 - old_u64s as i64 + u64s as i64) as u32;
    if bch2_sb_realloc(sb, new_sb_u64s) != 0 {
        return core::ptr::null_mut();
    }
    if (*sb).flags & BCH_SB_HANDLE_FS_SB != 0 {
        return core::ptr::null_mut();
    }

    let field = bch2_sb_field_get_id((*sb).sb, type_);
    let field = __bch2_sb_field_resize(sb, field, u64s);
    if !field.is_null() {
        (*field).type_ = type_;
    }
    field
}

pub unsafe fn bch2_sb_field_get_minsize_id(
    sb: *mut bch_sb_handle,
    type_: u32,
    u64s: u32,
) -> *mut bch_sb_field {
    let mut field = bch2_sb_field_get_id((*sb).sb, type_);
    if field.is_null() || (*field).u64s < u64s {
        field = bch2_sb_field_resize_id(sb, type_, u64s);
    }
    field
}

pub unsafe fn bch2_sb_field_delete(sb: *mut bch_sb_handle, type_: u32) {
    let field = bch2_sb_field_get_id((*sb).sb, type_);
    if !field.is_null() {
        __bch2_sb_field_resize(sb, field, 0);
    }
}

pub fn validate_sb_layout(layout: &bch_sb_layout) -> i32 {
    if layout.magic != BCHFS_MAGIC {
        return BCH_ERR_invalid_sb_layout;
    }
    if layout.layout_type != 0 {
        return BCH_ERR_invalid_sb_layout_type;
    }
    if layout.nr_superblocks == 0 {
        return BCH_ERR_invalid_sb_layout_nr_superblocks;
    }
    if layout.nr_superblocks as usize > layout.sb_offset.len() {
        return BCH_ERR_invalid_sb_layout_nr_superblocks;
    }
    if layout.sb_max_size_bits > BCH_SB_LAYOUT_SIZE_BITS_MAX {
        return BCH_ERR_invalid_sb_layout_sb_max_size_bits;
    }

    let max_sectors = 1u64 << layout.sb_max_size_bits;
    let mut prev_offset = layout.sb_offset[0];
    for i in 1..layout.nr_superblocks as usize {
        let offset = layout.sb_offset[i];
        if offset < prev_offset.wrapping_add(max_sectors) {
            return BCH_ERR_invalid_sb_layout_superblocks_overlap;
        }
        prev_offset = offset;
    }
    0
}

pub fn bch2_sb_compatible(sb: &bch_sb) -> i32 {
    if sb.version != bcachefs_metadata_version_current
        || sb.version_min != bcachefs_metadata_version_current
    {
        return BCH_ERR_invalid_sb_version;
    }
    0
}

unsafe fn __bch2_members_v2_get_mut(
    members: *mut bch_sb_field_members_v2,
    index: usize,
) -> *mut bch_member {
    members
        .cast::<u8>()
        .add(
            core::mem::size_of::<bch_sb_field_members_v2>()
                + index * (*members).member_bytes as usize,
        )
        .cast()
}

unsafe fn bch2_members_v2_get(members: *mut bch_sb_field_members_v2, index: usize) -> bch_member {
    let mut ret = bch_member::default();
    core::ptr::copy_nonoverlapping(
        __bch2_members_v2_get_mut(members, index).cast::<u8>(),
        (&mut ret as *mut bch_member).cast::<u8>(),
        core::cmp::min(
            (*members).member_bytes as usize,
            core::mem::size_of::<bch_member>(),
        ),
    );
    ret
}

pub unsafe fn bch2_sb_member_get(sb: *mut bch_sb, index: usize) -> bch_member {
    bch2_members_v2_get(
        bch2_sb_field_get_id(sb, BCH_SB_FIELD_members_v2).cast(),
        index,
    )
}

pub fn BCH_SB_BTREE_NODE_SIZE(sb: &bch_sb) -> u64 {
    (sb.flags[0] >> 12) & 0xffff
}

fn BCH_SB_VERSION_INCOMPAT(sb: &bch_sb) -> u16 {
    ((sb.flags[5] >> 32) & 0xffff) as u16
}

fn BCH_SB_VERSION_INCOMPAT_ALLOWED(sb: &bch_sb) -> u16 {
    ((sb.flags[5] >> 48) & 0xffff) as u16
}

fn SET_BCH_SB_VERSION_INCOMPAT_ALLOWED(sb: &mut bch_sb, value: u16) {
    sb.flags[5] &= !(0xffffu64 << 48);
    sb.flags[5] |= (value as u64) << 48;
}

fn validate_member(member: bch_member, sb: &bch_sb, _index: usize) -> i32 {
    const BCH_MEMBER_NBUCKETS_MAX: u64 = i32::MAX as u64 - 64;
    const BCH_MIN_NR_NBUCKETS: u64 = 1 << 9;
    const BCH_MI_BTREE_BITMAP_SHIFT_MAX: u8 = 58;

    if member.nbuckets > BCH_MEMBER_NBUCKETS_MAX {
        return BCH_ERR_invalid_sb_members;
    }
    if member.nbuckets.wrapping_sub(member.first_bucket as u64) < BCH_MIN_NR_NBUCKETS {
        return BCH_ERR_invalid_sb_members;
    }
    if member.bucket_size < sb.block_size {
        return BCH_ERR_invalid_sb_members;
    }
    if (member.bucket_size as u64) < BCH_SB_BTREE_NODE_SIZE(sb) {
        return BCH_ERR_invalid_sb_members;
    }
    if member.btree_bitmap_shift >= BCH_MI_BTREE_BITMAP_SHIFT_MAX {
        return BCH_ERR_invalid_sb_members;
    }
    if member.flags & (1 << 30) != 0 && sb.features[0] & (1 << 21) != 0 {
        return BCH_ERR_invalid_sb_members;
    }
    0
}

unsafe fn bch2_sb_members_v2_validate(sb: *mut bch_sb, field: *mut bch_sb_field) -> i32 {
    let members = field.cast::<bch_sb_field_members_v2>();
    let required = core::mem::size_of::<bch_sb_field_members_v2>()
        + (*sb).nr_devices as usize * (*members).member_bytes as usize;
    if required > (*field).u64s as usize * 8 {
        return BCH_ERR_invalid_sb_members;
    }
    for index in 0..(*sb).nr_devices as usize {
        let ret = validate_member(bch2_members_v2_get(members, index), &*sb, index);
        if ret != 0 {
            return ret;
        }
    }
    0
}

pub unsafe fn bch2_sb_field_journal_v2_nr_entries(journal: *mut bch_sb_field_journal_v2) -> usize {
    ((*journal).field.u64s as usize - 1) / 2
}

unsafe fn bch2_sb_journal_v2_validate(sb: *mut bch_sb, field: *mut bch_sb_field) -> i32 {
    let journal = field.cast::<bch_sb_field_journal_v2>();
    let member = bch2_sb_member_get(sb, (*sb).dev_idx as usize);
    let mut sum = 0u64;
    let nr = bch2_sb_field_journal_v2_nr_entries(journal);
    if nr == 0 {
        return 0;
    }

    let entries = journal
        .cast::<u8>()
        .add(core::mem::size_of::<bch_sb_field_journal_v2>())
        .cast::<bch_sb_field_journal_v2_entry>();
    let mut ranges = Vec::with_capacity(nr);
    for i in 0..nr {
        let start = (*entries.add(i)).start;
        let count = (*entries.add(i)).nr;
        let end = start.wrapping_add(count);
        if end <= start {
            return BCH_ERR_invalid_sb_journal;
        }
        sum = sum.wrapping_add(count);
        ranges.push((start, end));
    }

    ranges.sort_unstable_by_key(|range| range.0);
    if ranges[0].0 == 0 {
        return BCH_ERR_invalid_sb_journal;
    }
    if ranges[0].0 < member.first_bucket as u64 {
        return BCH_ERR_invalid_sb_journal;
    }
    if ranges[nr - 1].1 > member.nbuckets {
        return BCH_ERR_invalid_sb_journal;
    }
    for i in 0..nr - 1 {
        if ranges[i].1 > ranges[i + 1].0 {
            return BCH_ERR_invalid_sb_journal;
        }
    }
    if sum > u32::MAX as u64 {
        return BCH_ERR_invalid_sb_journal;
    }
    0
}

unsafe fn bch2_sb_field_validate(sb: *mut bch_sb, field: *mut bch_sb_field) -> i32 {
    match (*field).type_ {
        BCH_SB_FIELD_journal_v2 => bch2_sb_journal_v2_validate(sb, field),
        BCH_SB_FIELD_members_v2 => bch2_sb_members_v2_validate(sb, field),
        _ => BCH_ERR_invalid_sb_field_type,
    }
}

pub unsafe fn bch2_sb_validate(
    sb: *mut bch_sb,
    no_version_check: bool,
    read_offset: u64,
    flags: u32,
) -> i32 {
    const BCH_VALIDATE_WRITE: u32 = 1 << 0;
    const BCH_FEATURE_NR: u32 = 24;

    let ret = bch2_sb_compatible(&*sb);
    if ret != 0 {
        return ret;
    }
    if !no_version_check {
        let incompat = (*sb).features[0] & (!0u64 << BCH_FEATURE_NR);
        let incompat_bit = if incompat != 0 {
            incompat.trailing_zeros()
        } else if (*sb).features[1] != 0 {
            64 + (*sb).features[1].trailing_zeros()
        } else {
            0
        };
        if incompat_bit != 0
            || BCH_VERSION_MAJOR((*sb).version)
                > BCH_VERSION_MAJOR(bcachefs_metadata_version_current)
            || BCH_SB_VERSION_INCOMPAT(&*sb) > bcachefs_metadata_version_current
        {
            return BCH_ERR_invalid_sb_features;
        }
    }
    if (*sb).user_uuid.iter().all(|byte| *byte == 0) || (*sb).uuid.iter().all(|byte| *byte == 0) {
        return BCH_ERR_invalid_sb_uuid;
    }
    if flags & BCH_VALIDATE_WRITE == 0 && (*sb).offset != read_offset {
        return BCH_ERR_invalid_sb_offset;
    }
    if (*sb).nr_devices == 0 {
        return BCH_ERR_invalid_sb_too_many_members;
    }
    if (*sb).dev_idx >= (*sb).nr_devices {
        return BCH_ERR_invalid_sb_dev_idx;
    }
    if (*sb).time_precision == 0 || (*sb).time_precision > 1_000_000_000 {
        return BCH_ERR_invalid_sb_time_precision;
    }

    if BCH_SB_VERSION_INCOMPAT_ALLOWED(&*sb) > (*sb).version {
        SET_BCH_SB_VERSION_INCOMPAT_ALLOWED(&mut *sb, (*sb).version);
    }
    if BCH_SB_VERSION_INCOMPAT(&*sb) > BCH_SB_VERSION_INCOMPAT_ALLOWED(&*sb) {
        if flags & BCH_VALIDATE_WRITE != 0 {
            return BCH_ERR_invalid_sb_version;
        }
        let incompat = BCH_SB_VERSION_INCOMPAT(&*sb);
        SET_BCH_SB_VERSION_INCOMPAT_ALLOWED(&mut *sb, incompat);
    }
    if (*sb).nr_devices > 1 {
        (*sb).flags[3] |= 1 << 63;
    }

    let ret = validate_sb_layout(&(*sb).layout);
    if ret != 0 {
        return ret;
    }
    let last = vstruct_last(sb);
    let mut field = (*sb).start.as_mut_ptr();
    while field < last {
        if (*field).u64s == 0 || vstruct_next(field) > last {
            return BCH_ERR_invalid_sb_field_size;
        }
        field = vstruct_next(field);
    }

    let members = bch2_sb_field_get_id(sb, BCH_SB_FIELD_members_v2);
    if members.is_null() {
        return BCH_ERR_invalid_sb_members_missing;
    }
    let ret = bch2_sb_field_validate(sb, members);
    if ret != 0 {
        return ret;
    }

    field = (*sb).start.as_mut_ptr();
    while field < last {
        let ret = bch2_sb_field_validate(sb, field);
        if ret != 0 {
            return ret;
        }
        field = vstruct_next(field);
    }
    if flags & BCH_VALIDATE_WRITE != 0
        && bch2_sb_member_get(sb, (*sb).dev_idx as usize).seq != (*sb).seq
    {
        return BCH_ERR_invalid_sb_members_missing;
    }
    0
}

fn BCH_SB_CSUM_TYPE(sb: &bch_sb) -> u32 {
    ((sb.flags[0] >> 2) & 0x3f) as u32
}

#[cfg(test)]
fn SET_BCH_SB_CSUM_TYPE(sb: &mut bch_sb, value: u32) {
    sb.flags[0] &= !(0x3f << 2);
    sb.flags[0] |= (value as u64 & 0x3f) << 2;
}

unsafe fn read_one_super(sb: *mut bch_sb_handle, offset: u64) -> i32 {
    use std::os::unix::fs::FileExt;

    loop {
        let file = &*(*sb).s_bdev_file.cast::<std::fs::File>();
        let buffer = core::slice::from_raw_parts_mut((*sb).sb.cast::<u8>(), (*sb).buffer_size);
        let mut done = 0usize;
        while done < buffer.len() {
            match file.read_at(&mut buffer[done..], offset * 512 + done as u64) {
                Ok(0) => return -9,
                Ok(read) => done += read,
                Err(_) => return -9,
            }
        }

        if (*(*sb).sb).magic != BCHFS_MAGIC {
            return -10;
        }
        let ret = bch2_sb_compatible(&*(*sb).sb);
        if ret != 0 {
            return ret;
        }

        let bytes = __vstruct_bytes((*(*sb).sb).u64s);
        let sb_size = 512usize
            << core::cmp::min(
                BCH_SB_LAYOUT_SIZE_BITS_MAX,
                (*(*sb).sb).layout.sb_max_size_bits,
            );
        if bytes > sb_size {
            return -11;
        }
        if bytes > (*sb).buffer_size {
            let ret = bch2_sb_realloc(sb, (*(*sb).sb).u64s);
            if ret != 0 {
                return ret;
            }
            continue;
        }

        let csum_type = BCH_SB_CSUM_TYPE(&*(*sb).sb);
        if csum_type >= crate::checksum::BCH_CSUM_NR
            || csum_type == crate::checksum::BCH_CSUM_chacha20_poly1305_80
            || csum_type == crate::checksum::BCH_CSUM_chacha20_poly1305_128
        {
            return -12;
        }
        let data = core::slice::from_raw_parts((*sb).sb.cast::<u8>().add(16), bytes - 16);
        let csum = crate::checksum::bch2_checksum(csum_type, data);
        if csum != (*(*sb).sb).csum {
            return -13;
        }

        (*sb).seq = (*(*sb).sb).seq;
        return 0;
    }
}

unsafe fn read_layout_sector(sb: *mut bch_sb_handle, layout: *mut bch_sb_layout) -> i32 {
    use std::os::unix::fs::FileExt;

    let file = &*(*sb).s_bdev_file.cast::<std::fs::File>();
    let buffer = core::slice::from_raw_parts_mut((*sb).sb.cast::<u8>(), 512);
    let mut done = 0usize;
    while done < buffer.len() {
        match file.read_at(
            &mut buffer[done..],
            super::BCH_SB_LAYOUT_SECTOR * 512 + done as u64,
        ) {
            Ok(0) => return -9,
            Ok(read) => done += read,
            Err(_) => return -9,
        }
    }
    core::ptr::copy_nonoverlapping((*sb).sb.cast::<bch_sb_layout>(), layout, 1);
    validate_sb_layout(&*layout)
}

unsafe fn read_backup_supers(
    sb: *mut bch_sb_handle,
    layout: *const bch_sb_layout,
    primary_valid: bool,
    best_offset: *mut u64,
) -> i32 {
    let primary_offset = (*layout).sb_offset[0];
    let mut best_seq = if primary_valid { (*(*sb).sb).seq } else { 0 };
    let mut last_read = if primary_valid { primary_offset } else { 0 };
    let mut any_valid = primary_valid;
    *best_offset = primary_offset;

    for i in 1..(*layout).nr_superblocks as usize {
        let offset = (*layout).sb_offset[i];
        let ret = read_one_super(sb, offset);
        last_read = offset;
        if ret != 0 {
            continue;
        }
        any_valid = true;
        if (*sb).seq >= best_seq {
            best_seq = (*sb).seq;
            *best_offset = offset;
        }
    }

    if !any_valid {
        return -14;
    }
    if last_read != *best_offset {
        let ret = read_one_super(sb, *best_offset);
        if ret != 0 {
            return ret;
        }
    }
    0
}

pub unsafe fn bch2_read_super(
    path: *const core::ffi::c_char,
    _opts: *mut core::ffi::c_void,
    sb: *mut bch_sb_handle,
) -> i32 {
    use std::os::unix::ffi::OsStrExt;

    if path.is_null() || sb.is_null() {
        return -9;
    }

    *sb = bch_sb_handle::default();
    let path = std::path::Path::new(std::ffi::OsStr::from_bytes(
        std::ffi::CStr::from_ptr(path).to_bytes(),
    ));
    let file = match std::fs::OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(_) => return -9,
    };
    (*sb).s_bdev_file = Box::into_raw(Box::new(file)).cast();

    let mut ret = bch2_sb_realloc(sb, 0);
    if ret != 0 {
        bch2_free_super(sb);
        return ret;
    }

    let mut layout = bch_sb_layout::default();
    let mut sb_offset = BCH_SB_SECTOR;
    ret = read_one_super(sb, BCH_SB_SECTOR);
    if ret == 0 {
        layout = (*(*sb).sb).layout;
        ret = validate_sb_layout(&layout);
        if ret == 0 {
            ret = read_backup_supers(sb, &layout, true, &mut sb_offset);
        }
    } else {
        ret = read_layout_sector(sb, &mut layout);
        if ret == 0 {
            ret = read_backup_supers(sb, &layout, false, &mut sb_offset);
        }
    }

    if ret == 0 {
        (*sb).flags |= BCH_SB_HANDLE_HAVE_LAYOUT;
        ret = bch2_sb_validate((*sb).sb, false, sb_offset, 0);
    }
    if ret != 0 {
        bch2_free_super(sb);
    }
    ret
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sb::{BCHFS_MAGIC, BCH_SB_SECTOR, BCH_VERSION};

    fn valid_layout() -> bch_sb_layout {
        let mut layout = bch_sb_layout {
            magic: BCHFS_MAGIC,
            sb_max_size_bits: 3,
            nr_superblocks: 3,
            ..Default::default()
        };
        layout.sb_offset[..3].copy_from_slice(&[BCH_SB_SECTOR, 16, 1024]);
        layout
    }

    #[test]
    fn validates_every_local_layout_branch() {
        let layout = valid_layout();
        assert_eq!(validate_sb_layout(&layout), 0);

        let mut bad = layout;
        bad.magic = [0; 16];
        assert_eq!(validate_sb_layout(&bad), BCH_ERR_invalid_sb_layout);
        bad = layout;
        bad.layout_type = 1;
        assert_eq!(validate_sb_layout(&bad), BCH_ERR_invalid_sb_layout_type);
        bad = layout;
        bad.nr_superblocks = 0;
        assert_eq!(
            validate_sb_layout(&bad),
            BCH_ERR_invalid_sb_layout_nr_superblocks
        );
        bad = layout;
        bad.nr_superblocks = 62;
        assert_eq!(
            validate_sb_layout(&bad),
            BCH_ERR_invalid_sb_layout_nr_superblocks
        );
        bad = layout;
        bad.sb_max_size_bits = 17;
        assert_eq!(
            validate_sb_layout(&bad),
            BCH_ERR_invalid_sb_layout_sb_max_size_bits
        );
        bad = layout;
        bad.sb_offset[1] = 15;
        assert_eq!(
            validate_sb_layout(&bad),
            BCH_ERR_invalid_sb_layout_superblocks_overlap
        );
    }

    #[test]
    fn accepts_only_the_current_metadata_version() {
        let mut sb = bch_sb {
            version: bcachefs_metadata_version_current,
            version_min: bcachefs_metadata_version_current,
            ..Default::default()
        };
        assert_eq!(bch2_sb_compatible(&sb), 0);
        sb.version = BCH_VERSION(2, 0);
        assert_eq!(bch2_sb_compatible(&sb), BCH_ERR_invalid_sb_version);
        sb.version = bcachefs_metadata_version_current;
        sb.version_min = 8;
        assert_eq!(bch2_sb_compatible(&sb), BCH_ERR_invalid_sb_version);
        sb.version_min = BCH_VERSION(1, 39);
        assert_eq!(bch2_sb_compatible(&sb), BCH_ERR_invalid_sb_version);
    }

    #[test]
    fn validates_fixed_fields_and_members_v2_in_local_order() {
        unsafe {
            let mut handle = bch_sb_handle::default();
            assert_eq!(bch2_sb_realloc(&mut handle, 0), 0);
            *handle.sb = bch_sb {
                version: bcachefs_metadata_version_current,
                version_min: bcachefs_metadata_version_current,
                uuid: [1; 16],
                user_uuid: [2; 16],
                offset: BCH_SB_SECTOR,
                seq: 7,
                block_size: 8,
                nr_devices: 1,
                time_precision: 1,
                flags: {
                    let mut flags = [0; 7];
                    flags[0] = 8 << 12;
                    flags
                },
                layout: valid_layout(),
                ..Default::default()
            };
            let field_u64s = ((core::mem::size_of::<bch_sb_field_members_v2>()
                + core::mem::size_of::<bch_member>()
                + 7)
                / 8) as u32;
            let field = bch2_sb_field_resize_id(&mut handle, BCH_SB_FIELD_members_v2, field_u64s);
            let members = field.cast::<bch_sb_field_members_v2>();
            (*members).member_bytes = core::mem::size_of::<bch_member>() as u16;
            *__bch2_members_v2_get_mut(members, 0) = bch_member {
                uuid: [3; 16],
                nbuckets: 1024,
                bucket_size: 8,
                seq: 7,
                ..Default::default()
            };

            assert_eq!(bch2_sb_validate(handle.sb, false, BCH_SB_SECTOR, 0), 0);

            (*handle.sb).features[0] |= 1 << 24;
            assert_eq!(
                bch2_sb_validate(handle.sb, false, BCH_SB_SECTOR, 0),
                BCH_ERR_invalid_sb_features
            );
            (*handle.sb).features[0] = 0;
            (*handle.sb).user_uuid = [0; 16];
            assert_eq!(
                bch2_sb_validate(handle.sb, false, BCH_SB_SECTOR, 0),
                BCH_ERR_invalid_sb_uuid
            );
            (*handle.sb).user_uuid = [2; 16];
            (*handle.sb).offset = 9;
            assert_eq!(
                bch2_sb_validate(handle.sb, false, BCH_SB_SECTOR, 0),
                BCH_ERR_invalid_sb_offset
            );
            (*handle.sb).offset = BCH_SB_SECTOR;
            (*handle.sb).time_precision = 0;
            assert_eq!(
                bch2_sb_validate(handle.sb, false, BCH_SB_SECTOR, 0),
                BCH_ERR_invalid_sb_time_precision
            );
            (*handle.sb).time_precision = 1;

            let member = __bch2_members_v2_get_mut(members, 0);
            (*member).nbuckets = 511;
            assert_eq!(
                bch2_sb_validate(handle.sb, false, BCH_SB_SECTOR, 0),
                BCH_ERR_invalid_sb_members
            );
            (*member).nbuckets = 1024;
            (*member).bucket_size = 4;
            assert_eq!(
                bch2_sb_validate(handle.sb, false, BCH_SB_SECTOR, 0),
                BCH_ERR_invalid_sb_members
            );
            (*member).bucket_size = 8;
            (*member).btree_bitmap_shift = 58;
            assert_eq!(
                bch2_sb_validate(handle.sb, false, BCH_SB_SECTOR, 0),
                BCH_ERR_invalid_sb_members
            );
            (*member).btree_bitmap_shift = 0;
            (*member).flags = 1 << 30;
            (*handle.sb).features[0] = 1 << 21;
            assert_eq!(
                bch2_sb_validate(handle.sb, false, BCH_SB_SECTOR, 0),
                BCH_ERR_invalid_sb_members
            );
            (*member).flags = 0;
            (*handle.sb).features[0] = 0;

            (*field).u64s = 1;
            assert_eq!(
                bch2_sb_members_v2_validate(handle.sb, field),
                BCH_ERR_invalid_sb_members
            );
            (*field).u64s = field_u64s;
            (*member).seq = 6;
            assert_eq!(
                bch2_sb_validate(handle.sb, false, BCH_SB_SECTOR, 1),
                BCH_ERR_invalid_sb_members_missing
            );
            (*member).seq = 7;
            assert_eq!(bch2_sb_validate(handle.sb, false, BCH_SB_SECTOR, 1), 0);

            bch2_free_super(&mut handle);
        }
    }

    #[test]
    fn validates_journal_v2_ranges_in_local_order() {
        unsafe {
            let mut handle = bch_sb_handle::default();
            assert_eq!(bch2_sb_realloc(&mut handle, 0), 0);
            *handle.sb = bch_sb {
                version: bcachefs_metadata_version_current,
                version_min: bcachefs_metadata_version_current,
                uuid: [1; 16],
                user_uuid: [2; 16],
                offset: BCH_SB_SECTOR,
                seq: 7,
                block_size: 8,
                nr_devices: 1,
                time_precision: 1,
                flags: {
                    let mut flags = [0; 7];
                    flags[0] = 8 << 12;
                    flags
                },
                layout: valid_layout(),
                ..Default::default()
            };
            let members_u64s = ((core::mem::size_of::<bch_sb_field_members_v2>()
                + core::mem::size_of::<bch_member>()
                + 7)
                / 8) as u32;
            let members_field =
                bch2_sb_field_resize_id(&mut handle, BCH_SB_FIELD_members_v2, members_u64s);
            let members = members_field.cast::<bch_sb_field_members_v2>();
            (*members).member_bytes = core::mem::size_of::<bch_member>() as u16;
            *__bch2_members_v2_get_mut(members, 0) = bch_member {
                uuid: [3; 16],
                nbuckets: 1024,
                first_bucket: 8,
                bucket_size: 8,
                seq: 7,
                ..Default::default()
            };

            let journal_field = bch2_sb_field_resize_id(&mut handle, BCH_SB_FIELD_journal_v2, 5);
            let journal = journal_field.cast::<bch_sb_field_journal_v2>();
            let entries = journal
                .cast::<u8>()
                .add(core::mem::size_of::<bch_sb_field_journal_v2>())
                .cast::<bch_sb_field_journal_v2_entry>();
            *entries = bch_sb_field_journal_v2_entry { start: 30, nr: 3 };
            *entries.add(1) = bch_sb_field_journal_v2_entry { start: 20, nr: 2 };
            assert_eq!(bch2_sb_validate(handle.sb, false, BCH_SB_SECTOR, 0), 0);

            (*entries).start = 0;
            assert_eq!(
                bch2_sb_validate(handle.sb, false, BCH_SB_SECTOR, 0),
                BCH_ERR_invalid_sb_journal
            );
            (*entries).start = 30;
            (*entries).nr = 0;
            assert_eq!(
                bch2_sb_validate(handle.sb, false, BCH_SB_SECTOR, 0),
                BCH_ERR_invalid_sb_journal
            );
            (*entries).nr = 3;
            (*entries.add(1)).start = 7;
            assert_eq!(
                bch2_sb_validate(handle.sb, false, BCH_SB_SECTOR, 0),
                BCH_ERR_invalid_sb_journal
            );
            (*entries.add(1)).start = 20;
            (*entries).start = 1022;
            assert_eq!(
                bch2_sb_validate(handle.sb, false, BCH_SB_SECTOR, 0),
                BCH_ERR_invalid_sb_journal
            );
            (*entries).start = 21;
            assert_eq!(
                bch2_sb_validate(handle.sb, false, BCH_SB_SECTOR, 0),
                BCH_ERR_invalid_sb_journal
            );
            (*entries).start = 30;
            (*journal_field).type_ = 0;
            assert_eq!(
                bch2_sb_validate(handle.sb, false, BCH_SB_SECTOR, 0),
                BCH_ERR_invalid_sb_field_type
            );
            bch2_free_super(&mut handle);
        }
    }

    #[test]
    fn realloc_and_resize_fields_preserve_vstruct_order() {
        unsafe {
            let mut handle = bch_sb_handle::default();
            assert_eq!(bch2_sb_realloc(&mut handle, 0), 0);
            *handle.sb = bch_sb::default();

            let first = bch2_sb_field_resize_id(&mut handle, 11, 2);
            assert!(!first.is_null());
            *first.cast::<u64>().add(1) = 0x1111;
            let second = bch2_sb_field_resize_id(&mut handle, 22, 2);
            assert!(!second.is_null());
            *second.cast::<u64>().add(1) = 0x2222;
            assert_eq!((*handle.sb).u64s, 4);

            let first = bch2_sb_field_resize_id(&mut handle, 11, 3);
            assert_eq!((*first).u64s, 3);
            assert_eq!(*first.cast::<u64>().add(1), 0x1111);
            assert_eq!(*first.cast::<u64>().add(2), 0);
            let second = bch2_sb_field_get_id(handle.sb, 22);
            assert_eq!(*second.cast::<u64>().add(1), 0x2222);
            assert_eq!((*handle.sb).u64s, 5);

            bch2_sb_field_delete(&mut handle, 11);
            assert!(bch2_sb_field_get_id(handle.sb, 11).is_null());
            let second = bch2_sb_field_get_id(handle.sb, 22);
            assert_eq!(*second.cast::<u64>().add(1), 0x2222);
            assert_eq!((*handle.sb).u64s, 2);

            bch2_free_super(&mut handle);
            assert!(handle.sb.is_null());
        }
    }

    #[test]
    fn realloc_preserves_header_and_enforces_layout_limit() {
        unsafe {
            let mut handle = bch_sb_handle::default();
            assert_eq!(bch2_sb_realloc(&mut handle, 0), 0);
            *handle.sb = bch_sb::default();
            (*handle.sb).seq = 0xfeed_beef;
            assert_eq!(handle.buffer_size, 1024);

            assert_eq!(bch2_sb_realloc(&mut handle, 40), 0);
            assert_eq!(handle.buffer_size, 2048);
            assert_eq!((*handle.sb).seq, 0xfeed_beef);

            (*handle.sb).layout.sb_max_size_bits = 2;
            handle.flags |= BCH_SB_HANDLE_HAVE_LAYOUT;
            assert_eq!(bch2_sb_realloc(&mut handle, 200), BCH_ERR_ENOSPC_sb);
            assert_eq!(handle.buffer_size, 2048);
            assert_eq!((*handle.sb).seq, 0xfeed_beef);

            bch2_free_super(&mut handle);
        }
    }

    #[test]
    fn read_one_super_reallocates_rereads_and_checks_checksum() {
        use std::os::unix::fs::FileExt;

        unsafe {
            let path = std::env::temp_dir().join(format!("subvol-super-{}", std::process::id()));
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            file.set_len(128 * 512).unwrap();

            let u64s = 40u32;
            let bytes = __vstruct_bytes(u64s);
            let mut disk_words = vec![0u64; bytes / 8];
            let disk_sb = disk_words.as_mut_ptr().cast::<bch_sb>();
            *disk_sb = bch_sb {
                version: bcachefs_metadata_version_current,
                version_min: bcachefs_metadata_version_current,
                magic: BCHFS_MAGIC,
                offset: BCH_SB_SECTOR,
                seq: 37,
                u64s,
                layout: bch_sb_layout {
                    magic: BCHFS_MAGIC,
                    sb_max_size_bits: 2,
                    nr_superblocks: 1,
                    sb_offset: {
                        let mut offsets = [0; 61];
                        offsets[0] = BCH_SB_SECTOR;
                        offsets
                    },
                    ..Default::default()
                },
                ..Default::default()
            };
            SET_BCH_SB_CSUM_TYPE(&mut *disk_sb, crate::checksum::BCH_CSUM_crc64);
            let checksum_data =
                core::slice::from_raw_parts(disk_sb.cast::<u8>().add(16), bytes - 16);
            (*disk_sb).csum =
                crate::checksum::bch2_checksum(crate::checksum::BCH_CSUM_crc64, checksum_data);
            let disk_bytes = core::slice::from_raw_parts(disk_sb.cast::<u8>(), bytes);
            assert_eq!(
                file.write_at(disk_bytes, BCH_SB_SECTOR * 512).unwrap(),
                bytes
            );

            let mut handle = bch_sb_handle::default();
            handle.s_bdev_file = Box::into_raw(Box::new(file.try_clone().unwrap())).cast();
            assert_eq!(bch2_sb_realloc(&mut handle, 0), 0);
            assert_eq!(handle.buffer_size, 1024);
            assert_eq!(read_one_super(&mut handle, BCH_SB_SECTOR), 0);
            assert_eq!(handle.buffer_size, 2048);
            assert_eq!(handle.seq, 37);
            assert_eq!((*handle.sb).seq, 37);

            assert_eq!(
                file.write_at(&[0xff], BCH_SB_SECTOR * 512 + 200).unwrap(),
                1
            );
            assert_eq!(read_one_super(&mut handle, BCH_SB_SECTOR), -13);

            bch2_free_super(&mut handle);
            drop(file);
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn backup_scan_selects_highest_seq_and_recovers_without_primary() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::FileExt;

        unsafe {
            let path = std::env::temp_dir().join(format!("subvol-backups-{}", std::process::id()));
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            file.set_len(256 * 512).unwrap();

            let mut layout = bch_sb_layout {
                magic: BCHFS_MAGIC,
                sb_max_size_bits: 3,
                nr_superblocks: 3,
                ..Default::default()
            };
            layout.sb_offset[..3].copy_from_slice(&[8, 16, 32]);
            let layout_bytes = core::slice::from_raw_parts(
                (&layout as *const bch_sb_layout).cast::<u8>(),
                core::mem::size_of::<bch_sb_layout>(),
            );
            assert_eq!(
                file.write_at(layout_bytes, super::super::BCH_SB_LAYOUT_SECTOR * 512)
                    .unwrap(),
                512
            );

            let write_copy = |offset: u64, seq: u64, corrupt: bool| {
                let field_u64s = ((core::mem::size_of::<bch_sb_field_members_v2>()
                    + core::mem::size_of::<bch_member>()
                    + 7)
                    / 8) as u32;
                let bytes = __vstruct_bytes(field_u64s);
                let mut words = vec![0u64; bytes / 8];
                let disk_sb = words.as_mut_ptr().cast::<bch_sb>();
                *disk_sb = bch_sb {
                    version: bcachefs_metadata_version_current,
                    version_min: bcachefs_metadata_version_current,
                    magic: BCHFS_MAGIC,
                    uuid: [1; 16],
                    user_uuid: [2; 16],
                    offset,
                    seq,
                    block_size: 8,
                    nr_devices: 1,
                    u64s: field_u64s,
                    time_precision: 1,
                    flags: {
                        let mut flags = [0; 7];
                        flags[0] = 8 << 12;
                        flags
                    },
                    layout,
                    ..Default::default()
                };
                let members = (*disk_sb)
                    .start
                    .as_mut_ptr()
                    .cast::<bch_sb_field_members_v2>();
                (*members).field = bch_sb_field {
                    u64s: field_u64s,
                    type_: BCH_SB_FIELD_members_v2,
                };
                (*members).member_bytes = core::mem::size_of::<bch_member>() as u16;
                *__bch2_members_v2_get_mut(members, 0) = bch_member {
                    uuid: [3; 16],
                    nbuckets: 1024,
                    first_bucket: 8,
                    bucket_size: 8,
                    seq,
                    ..Default::default()
                };
                SET_BCH_SB_CSUM_TYPE(&mut *disk_sb, crate::checksum::BCH_CSUM_xxhash);
                let checksum_data =
                    core::slice::from_raw_parts(disk_sb.cast::<u8>().add(16), bytes - 16);
                (*disk_sb).csum =
                    crate::checksum::bch2_checksum(crate::checksum::BCH_CSUM_xxhash, checksum_data);
                let disk_bytes = core::slice::from_raw_parts_mut(disk_sb.cast::<u8>(), bytes);
                if corrupt {
                    disk_bytes[200] ^= 1;
                }
                assert_eq!(file.write_at(disk_bytes, offset * 512).unwrap(), bytes);
            };

            write_copy(8, 1, false);
            write_copy(16, 3, false);
            write_copy(32, 2, true);

            let mut handle = bch_sb_handle::default();
            handle.s_bdev_file = Box::into_raw(Box::new(file.try_clone().unwrap())).cast();
            assert_eq!(bch2_sb_realloc(&mut handle, 0), 0);
            assert_eq!(read_one_super(&mut handle, 8), 0);
            let embedded_layout = (*handle.sb).layout;
            let mut best = 0;
            assert_eq!(
                read_backup_supers(&mut handle, &embedded_layout, true, &mut best),
                0
            );
            assert_eq!(best, 16);
            assert_eq!(handle.seq, 3);
            assert_eq!((*handle.sb).offset, 16);

            write_copy(32, 3, false);
            assert_eq!(read_one_super(&mut handle, 8), 0);
            let embedded_layout = (*handle.sb).layout;
            assert_eq!(
                read_backup_supers(&mut handle, &embedded_layout, true, &mut best),
                0
            );
            assert_eq!(best, 32);
            assert_eq!((*handle.sb).offset, 32);

            assert_eq!(file.write_at(&[0], 8 * 512 + 24).unwrap(), 1);
            assert_ne!(read_one_super(&mut handle, 8), 0);
            let mut standalone_layout = bch_sb_layout::default();
            assert_eq!(read_layout_sector(&mut handle, &mut standalone_layout), 0);
            assert_eq!(
                read_backup_supers(&mut handle, &standalone_layout, false, &mut best),
                0
            );
            assert_eq!(best, 32);
            assert_eq!(handle.seq, 3);
            assert_eq!((*handle.sb).offset, 32);

            bch2_free_super(&mut handle);
            let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            let mut public_handle = bch_sb_handle::default();
            assert_eq!(
                bch2_read_super(c_path.as_ptr(), core::ptr::null_mut(), &mut public_handle),
                0
            );
            assert_eq!(public_handle.seq, 3);
            assert_eq!((*public_handle.sb).offset, 32);
            bch2_free_super(&mut public_handle);
            drop(file);
            std::fs::remove_file(path).unwrap();
        }
    }
}
