pub const BLOCK_SIZE: u64 = 4096;
pub const ALLOC_GRANULARITY: u64 = 4096;

/// btree 内部节点的 entry type
/// 对应 bcachefs `KEY_TYPE_btree_ptr_v2` (bcachefs_format.h:459)
pub const ENTRY_TYPE_BTREE_PTR: u8 = 18;

/// BtreePtr — 指向子节点的指针
///
/// 对应 bcachefs `struct bch_btree_ptr_v2` (extents_format.h:304)
#[derive(Debug, Clone, Copy)]
pub struct BtreePtr {
    pub offset: u64,
    pub child_level: u8,
}

impl BtreePtr {
    pub const SERIALIZED_SIZE: usize = 9;

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(9);
        buf.extend_from_slice(&self.offset.to_le_bytes());
        buf.push(self.child_level);
        buf
    }

    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < 9 {
            return None;
        }
        Some(BtreePtr {
            offset: u64::from_le_bytes(data[0..8].try_into().ok()?),
            child_level: data[8],
        })
    }
}

/// ExtentPtr — 指向物理数据的指针
///
/// 对应 bcachefs `struct bch_extent_ptr` (extents_format.h:224)
#[derive(Debug, Clone)]
pub struct ExtentPtr {
    pub dev: u8,
    pub block: u64,
    pub len: u32,
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

/// ExtentEntry — 逻辑位置到物理数据的映射
///
/// 对应 bcachefs `struct bch_extent` (extents_format.h:318)
#[derive(Debug, Clone)]
pub struct ExtentEntry {
    pub ptrs: Vec<ExtentPtr>,
}

impl ExtentEntry {
    pub fn new() -> Self {
        ExtentEntry { ptrs: Vec::new() }
    }

    pub fn add_ptr(&mut self, dev: u8, block: u64, len: u32, csum: u64) {
        self.ptrs.push(ExtentPtr {
            dev,
            block,
            len,
            csum,
        });
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
