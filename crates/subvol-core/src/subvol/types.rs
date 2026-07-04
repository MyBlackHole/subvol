use serde::{Deserialize, Serialize};

/// bcachefs 根 inode 号
///
/// 对应 bcachefs `BCACHEFS_ROOT_INO = 1`，根子卷的根 inode 固定为 1。
/// 子卷 0 保留，子卷 1 是隐式根子卷。
pub const BCACHEFS_ROOT_INO: u64 = 1;

/// bcachefs 根子卷 ID
///
/// 对应 bcachefs `BCACHEFS_ROOT_SUBVOL = 1`，文件系统的根子卷 ID 固定为 1。
/// 子卷 0 保留（用于未分配/初始状态），子卷 1 是隐式根子卷。
pub const BCACHEFS_ROOT_SUBVOL: u64 = 1;

/// 子卷标志位 (bcachefs bitmask 对齐)
///
/// bcachefs 对应:
/// - `BCH_SUBVOLUME_RO`      → bit 0
/// - `BCH_SUBVOLUME_SNAP`    → bit 1
/// - `BCH_SUBVOLUME_UNLINKED`→ bit 2
///
/// 使用 u32 位操作，避免额外依赖 bitflags crate。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BchSubvolumeFlags(u32);

impl BchSubvolumeFlags {
    /// 只读 (BCH_SUBVOLUME_RO)
    pub const READ_ONLY: Self = Self(1 << 0);
    /// 快照子卷 (BCH_SUBVOLUME_SNAP)
    pub const SNAPSHOT: Self = Self(1 << 1);
    /// 已删除 (BCH_SUBVOLUME_UNLINKED)
    pub const UNLINKED: Self = Self(1 << 2);

    /// 无标志
    pub const fn empty() -> Self {
        Self(0)
    }

    /// 是否包含指定标志
    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 != 0
    }

    /// 添加标志
    pub fn insert(&mut self, flag: Self) {
        self.0 |= flag.0;
    }

    /// 移除标志
    pub fn remove(&mut self, flag: Self) {
        self.0 &= !flag.0;
    }
}

/// 允许 BchSubvolumeFlags 与 u32 互转（用于序列化兼容）
impl From<BchSubvolumeFlags> for u32 {
    fn from(f: BchSubvolumeFlags) -> Self {
        f.0
    }
}

impl std::ops::BitOr for BchSubvolumeFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// 子卷值，存储在 Subvolumes btree
///
/// 字段名与 bcachefs `struct bch_subvolume` 对齐：
///
/// | bcachefs 字段     | subvol 字段     | 说明                    |
/// |-------------------|-------------------|------------------------|
/// | flags             | flags             | BCH_SUBVOLUME_* 标志位  |
/// | snapshot          | snapshot          | 当前 snapshot ID        |
/// | inode             | inode             | 根 inode 号             |
/// | creation_parent   | creation_parent   | 创建来源子卷 ID          |
/// | fs_path_parent    | fs_path_parent    | 文件系统路径父级子卷 ID   |
/// | otime             | otime_lo/hi       | 创建时间戳 (bch_le128)   |
/// | —                 | size              | 子卷大小（subvol 扩展） |
///
/// bpos: `Bpos { vol_id: subvol_id, offset: 0, snapshot: 0 }`
/// 子卷 0 保留（bcachefs root subvol = 1）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BchSubvolume {
    /// 子卷标志位 (BCH_SUBVOLUME_RO / SNAP / UNLINKED)
    pub flags: BchSubvolumeFlags,
    /// 当前 snapshot ID (↔ struct bch_subvolume.snapshot)
    pub snapshot: u32,
    /// 根 inode 号 (↔ struct bch_subvolume.inode)
    pub inode: u64,
    /// 创建来源子卷 ID (↔ struct bch_subvolume.creation_parent)
    pub creation_parent: u32,
    /// 文件系统路径父级子卷 ID (↔ struct bch_subvolume.fs_path_parent)
    pub fs_path_parent: u32,
    /// 创建时间戳低 64 位 (↔ struct bch_subvolume.otime.lo)
    pub otime_lo: u64,
    /// 创建时间戳高 64 位 (↔ struct bch_subvolume.otime.hi)
    pub otime_hi: u64,
    /// 子卷大小（字节）— subvol 扩展字段，不在 bcachefs 原始结构中
    pub size: u64,
    /// 当前已用扇区数（配额记账用，btree + 原子缓存）
    #[serde(default)]
    pub cur_sectors: u64,
}

impl BchSubvolume {
    /// 创建新的读写子卷
    pub fn new(snapshot: u32, inode: u64, size: u64, otime: u64) -> Self {
        Self {
            flags: BchSubvolumeFlags::empty(),
            snapshot,
            inode,
            creation_parent: 0,
            fs_path_parent: 0,
            otime_lo: otime,
            otime_hi: 0,
            size,
            cur_sectors: 0,
        }
    }

    /// 创建快照子卷（从指定子卷 snapshot）
    pub fn new_snapshot(
        creation_parent: u32,
        snapshot: u32,
        inode: u64,
        size: u64,
        otime: u64,
    ) -> Self {
        Self {
            flags: BchSubvolumeFlags::SNAPSHOT | BchSubvolumeFlags::READ_ONLY,
            snapshot,
            inode,
            creation_parent,
            fs_path_parent: 0,
            otime_lo: otime,
            otime_hi: 0,
            size,
            cur_sectors: 0,
        }
    }

    /// 标记为已删除 (SET_BCH_SUBVOLUME_UNLINKED)
    pub fn mark_unlinked(&mut self) {
        self.flags.insert(BchSubvolumeFlags::UNLINKED);
    }

    /// 是否只读 (BCH_SUBVOLUME_RO)
    pub fn is_read_only(&self) -> bool {
        self.flags.contains(BchSubvolumeFlags::READ_ONLY)
    }

    /// 是否快照 (BCH_SUBVOLUME_SNAP)
    pub fn is_snapshot(&self) -> bool {
        self.flags.contains(BchSubvolumeFlags::SNAPSHOT)
    }

    /// 是否已删除 (BCH_SUBVOLUME_UNLINKED)
    pub fn is_unlinked(&self) -> bool {
        self.flags.contains(BchSubvolumeFlags::UNLINKED)
    }

    /// 设置只读标志 (SET_BCH_SUBVOLUME_RO)
    pub fn set_read_only(&mut self, ro: bool) {
        if ro {
            self.flags.insert(BchSubvolumeFlags::READ_ONLY);
        } else {
            self.flags.remove(BchSubvolumeFlags::READ_ONLY);
        }
    }

    /// 设置快照标志 (SET_BCH_SUBVOLUME_SNAP)
    pub fn set_snapshot(&mut self, snap: bool) {
        if snap {
            self.flags.insert(BchSubvolumeFlags::SNAPSHOT);
        } else {
            self.flags.remove(BchSubvolumeFlags::SNAPSHOT);
        }
    }

    /// 序列化为字节（使用 bincode）
    pub fn to_bytes(&self) -> Vec<u8> {
        bincode::serialize(self).expect("BchSubvolume serialization failed")
    }

    /// 从字节反序列化
    pub fn from_bytes(data: &[u8]) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(bincode::deserialize(data)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_subvolume_flags_default() {
        let sv = BchSubvolume::new(1, 100, 4096, 500);
        assert!(!sv.is_read_only());
        assert!(!sv.is_snapshot());
        assert!(!sv.is_unlinked());
        assert_eq!(sv.snapshot, 1);
        assert_eq!(sv.inode, 100);
        assert_eq!(sv.size, 4096);
        assert_eq!(sv.otime_lo, 500);
    }

    #[test]
    fn test_subvolume_flags_snapshot() {
        let sv = BchSubvolume::new_snapshot(2, 3, 200, 8192, 600);
        assert!(sv.is_read_only());
        assert!(sv.is_snapshot());
        assert!(!sv.is_unlinked());
        assert_eq!(sv.creation_parent, 2);
        assert_eq!(sv.snapshot, 3);
        assert_eq!(sv.size, 8192);
    }

    #[test]
    fn test_subvolume_mark_unlinked() {
        let mut sv = BchSubvolume::new(10, 300, 4096, 700);
        assert!(!sv.is_unlinked());
        sv.mark_unlinked();
        assert!(sv.is_unlinked());
    }

    #[test]
    fn test_subvolume_serde_roundtrip() {
        let sv = BchSubvolume::new(42, 500, 65536, 800);
        let data = bincode::serialize(&sv).unwrap();
        let restored: BchSubvolume = bincode::deserialize(&data).unwrap();
        assert_eq!(restored.snapshot, sv.snapshot);
        assert_eq!(restored.inode, sv.inode);
        assert_eq!(restored.size, sv.size);
        assert!(!restored.is_unlinked());
    }

    #[test]
    fn test_subvolume_new_snapshot_inherits_parent_snapshot() {
        let sv = BchSubvolume::new_snapshot(3, 7, 400, 16384, 900);
        // snapshot 继承父 snapshot_id
        assert_eq!(sv.snapshot, 7);
        assert_eq!(sv.creation_parent, 3);
    }

    #[test]
    fn test_subvolume_set_read_only() {
        let mut sv = BchSubvolume::new(1, 100, 4096, 0);
        assert!(!sv.is_read_only());
        sv.set_read_only(true);
        assert!(sv.is_read_only());
        sv.set_read_only(false);
        assert!(!sv.is_read_only());
    }

    #[test]
    fn test_subvolume_set_snapshot() {
        let mut sv = BchSubvolume::new(1, 100, 4096, 0);
        assert!(!sv.is_snapshot());
        sv.set_snapshot(true);
        assert!(sv.is_snapshot());
        sv.set_snapshot(false);
        assert!(!sv.is_snapshot());
    }
}
