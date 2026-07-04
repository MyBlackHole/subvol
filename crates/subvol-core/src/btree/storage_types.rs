// ═══════════════════════════════════════════════════════════════
// 常量
// ═══════════════════════════════════════════════════════════════

/// 默认块大小 (4K)
pub const BLOCK_SIZE: u64 = 4096;

/// 分配粒度
pub const ALLOC_GRANULARITY: u64 = 4096;

// ═══════════════════════════════════════════════════════════════
// AllocEntry — 分配 btree 条目
// ═══════════════════════════════════════════════════════════════

/// 数据类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    Free = 0,
    Dirty = 1,
    Cached = 2,
    NeedDiscard = 3,
}

impl DataType {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => DataType::Free,
            1 => DataType::Dirty,
            2 => DataType::Cached,
            _ => DataType::NeedDiscard,
        }
    }

    pub fn to_u8(self) -> u8 {
        self as u8
    }
}

/// AllocEntry — 单块的分配状态
///
/// 对应 bcachefs `struct bch_alloc_v4` (alloc/format.h:82)
/// 记录一个 bucket/block 的世代号、数据类型、脏/缓存扇区数。
/// 序列化格式: gen(1) + data_type(1) + dirty_sectors(4) + cached_sectors(4) = 10 bytes
#[derive(Debug, Clone)]
pub struct AllocEntry {
    /// 世代号（用于 stale 检测）
    pub gen: u8,
    /// 数据类型 (free/dirty/cached/need_discard)
    pub data_type: u8,
    /// 脏扇区数（已使用）
    pub dirty_sectors: u32,
    /// 缓存扇区数
    pub cached_sectors: u32,
}

impl AllocEntry {
    pub const ALLOC_BYTES: usize = 10;

    pub const fn new() -> Self {
        AllocEntry { gen: 0, data_type: 0, dirty_sectors: 0, cached_sectors: 0 }
    }

    pub fn free() -> Self {
        AllocEntry { gen: 0, data_type: DataType::Free as u8, dirty_sectors: 0, cached_sectors: 0 }
    }

    pub fn is_free(&self) -> bool {
        self.data_type == DataType::Free as u8 && self.dirty_sectors == 0
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(10);
        buf.push(self.gen);
        buf.push(self.data_type);
        buf.extend_from_slice(&self.dirty_sectors.to_le_bytes());
        buf.extend_from_slice(&self.cached_sectors.to_le_bytes());
        buf
    }

    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < 10 {
            return None;
        }
        Some(AllocEntry {
            gen: data[0],
            data_type: data[1],
            dirty_sectors: u32::from_le_bytes(data[2..6].try_into().ok()?),
            cached_sectors: u32::from_le_bytes(data[6..10].try_into().ok()?),
        })
    }
}

impl Default for AllocEntry {
    fn default() -> Self {
        Self::new()
    }
}

// ═══════════════════════════════════════════════════════════════
// ExtentPtr — 物理 extent 指针
// ═══════════════════════════════════════════════════════════════

/// ExtentPtr — 指向物理数据的指针
///
/// 对应 bcachefs `struct bch_extent_ptr` (extents_format.h:224)
/// 标识数据在哪个设备、哪个块偏移、长度和校验和。
/// 序列化格式: dev(1) + block(8) + len(4) + csum(8) = 21 bytes
#[derive(Debug, Clone)]
pub struct ExtentPtr {
    /// 设备 ID
    pub dev: u8,
    /// 块偏移（扇区）
    pub block: u64,
    /// 数据长度（字节）
    pub len: u32,
    /// 校验和（CRC64）
    pub csum: u64,
}

impl ExtentPtr {
    pub const PTR_BYTES: usize = 21;

    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(21);
        buf.push(self.dev);
        buf.extend_from_slice(&self.block.to_le_bytes());
        buf.extend_from_slice(&self.len.to_le_bytes());
        buf.extend_from_slice(&self.csum.to_le_bytes());
        buf
    }

    fn from_bytes(data: &[u8]) -> Option<(Self, usize)> {
        if data.len() < 21 {
            return None;
        }
        let ptr = ExtentPtr {
            dev: data[0],
            block: u64::from_le_bytes(data[1..9].try_into().ok()?),
            len: u32::from_le_bytes(data[9..13].try_into().ok()?),
            csum: u64::from_le_bytes(data[13..21].try_into().ok()?),
        };
        Some((ptr, 21))
    }
}

// ═══════════════════════════════════════════════════════════════
// ExtentEntry — 数据索引 btree 条目
// ═══════════════════════════════════════════════════════════════

/// ExtentEntry — 逻辑位置到物理数据的映射
///
/// 对应 bcachefs `struct bch_extent` (extents_format.h:318)
/// 包含一个或多个物理指针和校验和。
/// 序列化格式: ptr_count(4) + [ExtentPtr...]
#[derive(Debug, Clone)]
pub struct ExtentEntry {
    /// 物理指针列表
    pub ptrs: Vec<ExtentPtr>,
}

impl ExtentEntry {
    pub fn new() -> Self {
        ExtentEntry { ptrs: Vec::new() }
    }

    pub fn add_ptr(&mut self, dev: u8, block: u64, len: u32, csum: u64) {
        self.ptrs.push(ExtentPtr { dev, block, len, csum });
    }

    pub fn total_len(&self) -> u64 {
        self.ptrs.iter().map(|p| p.len as u64).sum()
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let count = self.ptrs.len() as u32;
        let mut buf = Vec::with_capacity(4 + count as usize * 21);
        buf.extend_from_slice(&count.to_le_bytes());
        for p in &self.ptrs {
            buf.extend_from_slice(&p.to_bytes());
        }
        buf
    }

    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < 4 {
            return None;
        }
        let count = u32::from_le_bytes(data[..4].try_into().ok()?) as usize;
        let mut off = 4;
        let mut ptrs = Vec::with_capacity(count);
        for _ in 0..count {
            let (ptr, sz) = ExtentPtr::from_bytes(&data[off..])?;
            ptrs.push(ptr);
            off += sz;
        }
        Some(ExtentEntry { ptrs })
    }
}

impl Default for ExtentEntry {
    fn default() -> Self {
        Self::new()
    }
}

// ═══════════════════════════════════════════════════════════════
// 空闲空间条目 (freespace btree)
// ═══════════════════════════════════════════════════════════════

/// FreespaceEntry — 空闲区间
///
/// key = 起始块偏移
/// payload = 区间长度（字节）
/// 对应 bcachefs KEY_TYPE_set — key 位置即表示空闲区间起点
#[derive(Debug, Clone)]
pub struct FreespaceEntry {
    /// 区间长度（字节）
    pub len: u64,
}

impl FreespaceEntry {
    pub fn new(len: u64) -> Self {
        FreespaceEntry { len }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        self.len.to_le_bytes().to_vec()
    }

    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < 8 {
            return None;
        }
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&data[..8]);
        Some(FreespaceEntry { len: u64::from_le_bytes(buf) })
    }
}

// ═══════════════════════════════════════════════════════════════
// 工具函数
// ═══════════════════════════════════════════════════════════════

/// 计算 data 的简单校验和（XOR fold）
pub fn calc_csum(data: &[u8]) -> u64 {
    let mut h: u64 = 0;
    for chunk in data.chunks(8) {
        let mut buf = [0u8; 8];
        let n = chunk.len();
        buf[..n].copy_from_slice(chunk);
        h ^= u64::from_le_bytes(buf);
    }
    h
}
