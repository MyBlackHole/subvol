use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BtreeEntry {
    pub btree_type: u8,
    pub level: u8,
    pub entry_type: u8,
    pub pos: Bpos,
    pub payload: Vec<u8>,
}

#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Bpos {
    pub inode: u64,
    pub offset: u64,
    pub snapshot: u32,
}

impl Bpos {
    pub const MIN: Bpos = Bpos {
        inode: 0,
        offset: 0,
        snapshot: 0,
    };
    pub const MAX: Bpos = Bpos {
        inode: u64::MAX,
        offset: u64::MAX,
        snapshot: u32::MAX,
    };
}
