use crate::types::StorageError;

// ═══════════════════════════════════════════════════════════════
// Constants
// ═══════════════════════════════════════════════════════════════

pub const SUPERBLOCK_MAGIC: &[u8; 8] = b"SUBVOL\0\0";
pub const SUPERBLOCK_VERSION: u32 = 1;

/// Superblock 大小（4K，与块对齐）
pub const SUPERBLOCK_SIZE: u64 = 4096;

/// 默认 journal bucket 数量
pub const DEFAULT_NR_JOURNAL_BUCKETS: u32 = 4;
/// 默认 journal bucket 大小（32K）
pub const DEFAULT_JOURNAL_BUCKET_SIZE: u32 = 32768;

/// 数据区起始偏移（superblock 后 + journal 预留区后）
pub fn data_area_offset(nr_buckets: u32, bucket_size: u32) -> u64 {
    SUPERBLOCK_SIZE + (nr_buckets as u64) * (bucket_size as u64)
}

// ═══════════════════════════════════════════════════════════════
// BtreeRootEntry
// ═══════════════════════════════════════════════════════════════

/// btree 根节点记录，存储于 superblock
///
/// 对应 bcachefs 中 jset_entry(type=btree_root) + btree_ptr_v2 的概念。
/// subvol 简化版：仅记录 btree_id、level、以及根节点在设备上的偏移地址。
#[derive(Debug, Clone, Copy)]
pub struct BtreeRootEntry {
    pub btree_id: u8,
    pub level: u8,
    pub root_offset: u64,
}

impl BtreeRootEntry {
    pub const SERIALIZED_SIZE: usize = 12; // 1 + 1 + 2(pad) + 8

    pub fn to_bytes(&self) -> [u8; 12] {
        let mut buf = [0u8; 12];
        buf[0] = self.btree_id;
        buf[1] = self.level;
        // pad[2..4] = 0 (already zero)
        buf[4..12].copy_from_slice(&self.root_offset.to_le_bytes());
        buf
    }

    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < 12 {
            return None;
        }
        Some(BtreeRootEntry {
            btree_id: data[0],
            level: data[1],
            root_offset: u64::from_le_bytes(data[4..12].try_into().ok()?),
        })
    }
}

// ═══════════════════════════════════════════════════════════════
// Superblock
// ═══════════════════════════════════════════════════════════════

/// Superblock — 设备元数据
///
/// 存储于设备偏移 0，4K 对齐。
/// 包含魔数、版本号、布局信息和 btree 根节点记录。
#[derive(Debug, Clone)]
pub struct Superblock {
    pub magic: [u8; 8],
    pub version: u32,
    pub dev_size: u64,
    pub journal_bucket_count: u32,
    pub journal_bucket_size: u32,
    /// journal bucket 的偏移地址列表
    pub journal_buckets: Vec<u64>,
    /// btree 根节点记录（对应 bcachefs 的 clean 区 btree_root entry）
    pub root_entries: Vec<BtreeRootEntry>,
    pub crc: u64,
}

impl Superblock {
    /// 创建默认布局的 Superblock
    pub fn new(dev_size: u64, nr_buckets: u32, bucket_size: u32) -> Self {
        let mut buckets = Vec::with_capacity(nr_buckets as usize);
        let mut off = SUPERBLOCK_SIZE;
        for _ in 0..nr_buckets {
            buckets.push(off);
            off += bucket_size as u64;
        }

        let mut sb = Superblock {
            magic: *SUPERBLOCK_MAGIC,
            version: SUPERBLOCK_VERSION,
            dev_size,
            journal_bucket_count: nr_buckets,
            journal_bucket_size: bucket_size,
            journal_buckets: buckets,
            root_entries: Vec::new(),
            crc: 0,
        };
        sb.crc = sb.calc_crc();
        sb
    }

    /// 设置 btree 根节点记录
    pub fn set_root(&mut self, btree_id: u8, level: u8, root_offset: u64) {
        // 更新已有记录或追加
        for e in &mut self.root_entries {
            if e.btree_id == btree_id {
                e.level = level;
                e.root_offset = root_offset;
                self.crc = self.calc_crc();
                return;
            }
        }
        self.root_entries.push(BtreeRootEntry {
            btree_id,
            level,
            root_offset,
        });
        self.crc = self.calc_crc();
    }

    /// 读取 btree 根节点记录
    pub fn get_root(&self, btree_id: u8) -> Option<&BtreeRootEntry> {
        self.root_entries.iter().find(|e| e.btree_id == btree_id)
    }

    /// 序列化到字节数组
    pub fn to_bytes(&self) -> Vec<u8> {
        let nr_jb = self.journal_bucket_count as usize;
        let nr_re = self.root_entries.len();
        let data_len = 28 + nr_jb * 8 + 4 + nr_re * 12 + 8;
        let mut buf = Vec::with_capacity(data_len);

        // magic (8)
        buf.extend_from_slice(&self.magic);
        // version (4)
        buf.extend_from_slice(&self.version.to_le_bytes());
        // dev_size (8)
        buf.extend_from_slice(&self.dev_size.to_le_bytes());
        // journal_bucket_count (4)
        buf.extend_from_slice(&self.journal_bucket_count.to_le_bytes());
        // journal_bucket_size (4)
        buf.extend_from_slice(&self.journal_bucket_size.to_le_bytes());
        // journal_buckets (nr_jb * 8)
        for &addr in &self.journal_buckets {
            buf.extend_from_slice(&addr.to_le_bytes());
        }
        // root_entry_count (4)
        buf.extend_from_slice(&(nr_re as u32).to_le_bytes());
        // root_entries (nr_re * 12)
        for e in &self.root_entries {
            buf.extend_from_slice(&e.to_bytes());
        }
        // crc (8)
        buf.extend_from_slice(&self.crc.to_le_bytes());

        // padding 到 SUPERBLOCK_SIZE
        buf.resize(SUPERBLOCK_SIZE as usize, 0);
        buf
    }

    /// 从字节数组反序列化
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < 28 {
            return None;
        }

        let mut magic = [0u8; 8];
        magic.copy_from_slice(&data[0..8]);
        if &magic != SUPERBLOCK_MAGIC {
            return None;
        }

        let version = u32::from_le_bytes(data[8..12].try_into().ok()?);
        let dev_size = u64::from_le_bytes(data[12..20].try_into().ok()?);
        let jb_count = u32::from_le_bytes(data[20..24].try_into().ok()?);
        let jb_size = u32::from_le_bytes(data[24..28].try_into().ok()?);

        let mut off = 28;
        let nr_jb = jb_count as usize;
        let mut buckets = Vec::with_capacity(nr_jb);
        for _ in 0..nr_jb {
            if off + 8 > data.len() {
                return None;
            }
            buckets.push(u64::from_le_bytes(data[off..off + 8].try_into().ok()?));
            off += 8;
        }

        // root_entry_count
        if off + 4 > data.len() {
            return None;
        }
        let nr_re = u32::from_le_bytes(data[off..off + 4].try_into().ok()?) as usize;
        off += 4;

        let mut root_entries = Vec::with_capacity(nr_re);
        for _ in 0..nr_re {
            if off + 12 > data.len() {
                return None;
            }
            let e = BtreeRootEntry::from_bytes(&data[off..off + 12])?;
            root_entries.push(e);
            off += 12;
        }

        let crc = if off + 8 <= data.len() {
            u64::from_le_bytes(data[off..off + 8].try_into().ok()?)
        } else {
            0
        };

        Some(Superblock {
            magic,
            version,
            dev_size,
            journal_bucket_count: jb_count,
            journal_bucket_size: jb_size,
            journal_buckets: buckets,
            root_entries,
            crc,
        })
    }

    pub fn is_valid_magic(&self) -> bool {
        &self.magic == SUPERBLOCK_MAGIC
    }

    fn calc_crc(&self) -> u64 {
        let mut h: u64 = 0;
        h ^= u64::from_le_bytes(self.magic);
        h ^= self.version as u64;
        h ^= self.dev_size;
        h ^= self.journal_bucket_count as u64;
        h ^= self.journal_bucket_size as u64;
        for &a in &self.journal_buckets {
            h ^= a;
        }
        for e in &self.root_entries {
            h ^= e.btree_id as u64;
            h ^= e.level as u64;
            h ^= e.root_offset;
        }
        h
    }
}

/// 读取并解析设备上的 superblock
pub async fn read_superblock(
    dev: &crate::block_device::BchDev,
) -> Result<Option<Superblock>, StorageError> {
    let data = match dev.read_at(0, SUPERBLOCK_SIZE as usize).await {
        Ok(d) => d,
        Err(StorageError::Io(_)) => return Ok(None),
        Err(e) => return Err(e),
    };
    if data.is_empty() || data[0..8] == [0u8; 8] {
        return Ok(None);
    }
    Ok(Superblock::from_bytes(&data))
}

/// 写入 superblock 到设备
pub async fn write_superblock(
    dev: &crate::block_device::BchDev,
    sb: &Superblock,
) -> Result<(), StorageError> {
    let data = sb.to_bytes();
    dev.write_at(0, &data).await?;
    dev.flush().await
}
