pub mod io;

pub const BCH_SB_LABEL_SIZE: usize = 32;
pub const BCH_SB_SECTOR: u64 = 8;
pub const BCH_SB_LAYOUT_SECTOR: u64 = 7;
pub const BCH_SB_LAYOUT_SIZE_BITS_MAX: u8 = 16;
pub const BCH_SB_MEMBERS_MAX: usize = 256;
pub const BCH_SB_MEMBER_INVALID: u8 = 255;
pub const BCH_SB_MEMBER_DELETED_UUID: [u8; 16] = [
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xd9, 0x6a, 0x60, 0xcf, 0x80, 0x3d, 0xf7, 0xef,
];
pub const BCH_SB_FIELD_journal_v2: u32 = 9;
pub const BCH_SB_FIELD_members_v2: u32 = 11;

pub const BCHFS_MAGIC: [u8; 16] = [
    0xc6, 0x85, 0x73, 0xf6, 0x66, 0xce, 0x90, 0xa9, 0xd9, 0x6a, 0x60, 0xcf, 0x80, 0x3d, 0xf7, 0xef,
];

pub const fn BCH_VERSION(major: u16, minor: u16) -> u16 {
    (major << 10) | minor
}

pub const fn BCH_VERSION_MAJOR(version: u16) -> u16 {
    version >> 10
}

pub const fn BCH_VERSION_MINOR(version: u16) -> u16 {
    version & ((1 << 10) - 1)
}

pub const bcachefs_metadata_version_current: u16 = BCH_VERSION(1, 38);

pub fn bch2_member_alive(member: &bch_member) -> bool {
    member.uuid != [0; 16] && member.uuid != BCH_SB_MEMBER_DELETED_UUID
}

pub fn bch2_mi_to_cpu(member: &bch_member) -> bch_member_cpu {
    let flags = member.flags;
    let durability = ((flags >> 28) & 0x3) as u8;
    bch_member_cpu {
        nbuckets: member.nbuckets,
        nbuckets_minus_first: member.nbuckets.wrapping_sub(member.first_bucket as u64),
        first_bucket: member.first_bucket,
        bucket_size: member.bucket_size,
        group: ((flags >> 20) & 0xff) as u16,
        state: (flags & 0xf) as u8,
        discard: ((flags >> 14) & 1) as u8,
        data_allowed: ((flags >> 15) & 0x1f) as u8,
        durability: if durability != 0 { durability - 1 } else { 1 },
        freespace_initialized: ((flags >> 30) & 1) as u8,
        initialized: ((flags >> 34) & 0xf) as u8,
        resize_on_mount: ((flags >> 31) & 1) as u8,
        rotational: ((flags >> 32) & 1) as u8,
        valid: bch2_member_alive(member) as u8,
        btree_bitmap_shift: member.btree_bitmap_shift,
        btree_allocated_bitmap: member.btree_allocated_bitmap,
    }
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct bch_sb_layout {
    pub magic: [u8; 16],
    pub layout_type: u8,
    pub sb_max_size_bits: u8,
    pub nr_superblocks: u8,
    pub pad: [u8; 5],
    pub sb_offset: [u64; 61],
}

impl Default for bch_sb_layout {
    fn default() -> Self {
        Self {
            magic: [0; 16],
            layout_type: 0,
            sb_max_size_bits: 0,
            nr_superblocks: 0,
            pad: [0; 5],
            sb_offset: [0; 61],
        }
    }
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_sb_field {
    pub u64s: u32,
    pub type_: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_member_cpu {
    pub nbuckets: u64,
    pub nbuckets_minus_first: u64,
    pub first_bucket: u16,
    pub bucket_size: u16,
    pub group: u16,
    pub state: u8,
    pub discard: u8,
    pub data_allowed: u8,
    pub durability: u8,
    pub freespace_initialized: u8,
    pub initialized: u8,
    pub resize_on_mount: u8,
    pub rotational: u8,
    pub valid: u8,
    pub btree_bitmap_shift: u8,
    pub btree_allocated_bitmap: u64,
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct bch_member {
    pub uuid: [u8; 16],
    pub nbuckets: u64,
    pub first_bucket: u16,
    pub bucket_size: u16,
    pub btree_bitmap_shift: u8,
    pub pad: [u8; 3],
    pub last_mount: u64,
    pub flags: u64,
    pub iops: [u32; 4],
    pub errors: [u64; 3],
    pub errors_at_reset: [u64; 3],
    pub errors_reset_time: u64,
    pub seq: u64,
    pub btree_allocated_bitmap: u64,
    pub last_journal_bucket: u32,
    pub last_journal_bucket_offset: u32,
    pub device_name: [u8; 16],
    pub device_model: [u8; 64],
    pub flush_errors: u64,
    pub device_serial: [u8; 64],
}

impl Default for bch_member {
    fn default() -> Self {
        Self {
            uuid: [0; 16],
            nbuckets: 0,
            first_bucket: 0,
            bucket_size: 0,
            btree_bitmap_shift: 0,
            pad: [0; 3],
            last_mount: 0,
            flags: 0,
            iops: [0; 4],
            errors: [0; 3],
            errors_at_reset: [0; 3],
            errors_reset_time: 0,
            seq: 0,
            btree_allocated_bitmap: 0,
            last_journal_bucket: 0,
            last_journal_bucket_offset: 0,
            device_name: [0; 16],
            device_model: [0; 64],
            flush_errors: 0,
            device_serial: [0; 64],
        }
    }
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_sb_field_members_v2 {
    pub field: bch_sb_field,
    pub member_bytes: u16,
    pub pad: [u8; 6],
    pub _members: [bch_member; 0],
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_sb_field_journal_v2_entry {
    pub start: u64,
    pub nr: u64,
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct bch_sb_field_journal_v2 {
    pub field: bch_sb_field,
    pub d: [bch_sb_field_journal_v2_entry; 0],
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct bch_sb {
    pub csum: crate::btree::bset::bch_csum,
    pub version: u16,
    pub version_min: u16,
    pub pad: [u16; 2],
    pub magic: [u8; 16],
    pub uuid: [u8; 16],
    pub user_uuid: [u8; 16],
    pub label: [u8; BCH_SB_LABEL_SIZE],
    pub offset: u64,
    pub seq: u64,
    pub block_size: u16,
    pub dev_idx: u8,
    pub nr_devices: u8,
    pub u64s: u32,
    pub time_base_lo: u64,
    pub time_base_hi: u32,
    pub time_precision: u32,
    pub flags: [u64; 7],
    pub write_time: u64,
    pub features: [u64; 2],
    pub compat: [u64; 2],
    pub layout: bch_sb_layout,
    pub start: [bch_sb_field; 0],
    pub _data: [u64; 0],
}

impl Default for bch_sb {
    fn default() -> Self {
        Self {
            csum: Default::default(),
            version: 0,
            version_min: 0,
            pad: [0; 2],
            magic: [0; 16],
            uuid: [0; 16],
            user_uuid: [0; 16],
            label: [0; BCH_SB_LABEL_SIZE],
            offset: 0,
            seq: 0,
            block_size: 0,
            dev_idx: 0,
            nr_devices: 0,
            u64s: 0,
            time_base_lo: 0,
            time_base_hi: 0,
            time_precision: 0,
            flags: [0; 7],
            write_time: 0,
            features: [0; 2],
            compat: [0; 2],
            layout: Default::default(),
            start: [],
            _data: [],
        }
    }
}

#[repr(C)]
#[derive(Debug)]
pub struct bch_sb_handle {
    pub sb: *mut bch_sb,
    pub s_bdev_file: *mut core::ffi::c_void,
    pub bdev: *mut core::ffi::c_void,
    pub sb_name: *mut u8,
    pub bio: *mut core::ffi::c_void,
    pub holder: *mut core::ffi::c_void,
    pub buffer_size: usize,
    pub mode: u32,
    pub flags: u32,
    pub seq: u64,
}

impl Default for bch_sb_handle {
    fn default() -> Self {
        Self {
            sb: core::ptr::null_mut(),
            s_bdev_file: core::ptr::null_mut(),
            bdev: core::ptr::null_mut(),
            sb_name: core::ptr::null_mut(),
            bio: core::ptr::null_mut(),
            holder: core::ptr::null_mut(),
            buffer_size: 0,
            mode: 0,
            flags: 0,
            seq: 0,
        }
    }
}

pub const BCH_SB_HANDLE_HAVE_LAYOUT: u32 = 1 << 0;
pub const BCH_SB_HANDLE_HAVE_BIO: u32 = 1 << 1;
pub const BCH_SB_HANDLE_FS_SB: u32 = 1 << 2;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn superblock_fixed_layout_matches_local_format() {
        assert_eq!(core::mem::size_of::<bch_member_cpu>(), 40);
        assert_eq!(core::mem::align_of::<bch_member_cpu>(), 8);
        assert_eq!(core::mem::size_of::<bch_sb_field>(), 8);
        assert_eq!(core::mem::align_of::<bch_sb_field>(), 8);
        assert_eq!(core::mem::size_of::<bch_sb_layout>(), 512);
        assert_eq!(core::mem::align_of::<bch_sb_layout>(), 8);
        assert_eq!(core::mem::size_of::<bch_sb>(), 752);
        assert_eq!(core::mem::align_of::<bch_sb>(), 8);
        assert_eq!(core::mem::size_of::<bch_sb_handle>(), 72);
        assert_eq!(core::mem::align_of::<bch_sb_handle>(), 8);
        assert_eq!(core::mem::size_of::<bch_member>(), 296);
        assert_eq!(core::mem::align_of::<bch_member>(), 8);
        let member = bch_member {
            uuid: [1; 16],
            nbuckets: 100,
            first_bucket: 4,
            bucket_size: 512,
            btree_bitmap_shift: 7,
            flags: (2 << 28) | (3 << 20) | (1 << 32) | (5 << 15) | 2,
            btree_allocated_bitmap: 0x55,
            ..Default::default()
        };
        let cpu = bch2_mi_to_cpu(&member);
        assert!(bch2_member_alive(&member));
        assert_eq!(cpu.nbuckets_minus_first, 96);
        assert_eq!(cpu.group, 3);
        assert_eq!(cpu.state, 2);
        assert_eq!(cpu.data_allowed, 5);
        assert_eq!(cpu.durability, 1);
        assert_eq!(cpu.rotational, 1);
        assert_eq!(cpu.valid, 1);
        assert_eq!(core::mem::size_of::<bch_sb_field_members_v2>(), 16);
        assert_eq!(core::mem::size_of::<bch_sb_field_journal_v2>(), 8);
        assert_eq!(core::mem::size_of::<bch_sb_field_journal_v2_entry>(), 16);

        let sb = bch_sb::default();
        let base = (&sb as *const bch_sb) as usize;
        assert_eq!((&sb.magic as *const [u8; 16]) as usize - base, 24);
        assert_eq!((&sb.offset as *const u64) as usize - base, 104);
        assert_eq!((&sb.u64s as *const u32) as usize - base, 124);
        assert_eq!((&sb.layout as *const bch_sb_layout) as usize - base, 240);
    }

    #[test]
    fn metadata_version_encoding_matches_local_macros() {
        assert_eq!(BCH_VERSION(1, 38), 1062);
        assert_eq!(BCH_VERSION_MAJOR(BCH_VERSION(1, 38)), 1);
        assert_eq!(BCH_VERSION_MINOR(BCH_VERSION(1, 38)), 38);
    }
}
