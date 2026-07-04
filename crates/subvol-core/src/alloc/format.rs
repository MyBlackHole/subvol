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
#[derive(Debug, Clone)]
pub struct AllocEntry {
    pub gen: u8,
    pub data_type: u8,
    pub dirty_sectors: u32,
    pub cached_sectors: u32,
}

impl AllocEntry {
    pub const ALLOC_BYTES: usize = 10;

    pub const fn new() -> Self {
        AllocEntry {
            gen: 0,
            data_type: 0,
            dirty_sectors: 0,
            cached_sectors: 0,
        }
    }

    pub fn free() -> Self {
        AllocEntry {
            gen: 0,
            data_type: DataType::Free as u8,
            dirty_sectors: 0,
            cached_sectors: 0,
        }
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

/// FreespaceEntry — 空闲区间
///
/// key = 起始块偏移，payload = 区间长度
#[derive(Debug, Clone)]
pub struct FreespaceEntry {
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
        Some(FreespaceEntry {
            len: u64::from_le_bytes(buf),
        })
    }
}
