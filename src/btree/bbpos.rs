use crate::bcachefs_format::Bpos;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bbpos {
    pub btree: u8,
    pub pos: Bpos,
}

impl Bbpos {
    pub const ZERO: Bbpos = Bbpos { btree: 0, pos: Bpos::ZERO };
    pub const MIN: Bbpos = Bbpos { btree: 0, pos: Bpos::MIN };
    pub const MAX: Bbpos = Bbpos { btree: u8::MAX, pos: Bpos::MAX };

    pub fn new(btree: u8, pos: Bpos) -> Self {
        Bbpos { btree, pos }
    }
}

impl Ord for Bbpos {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.btree.cmp(&other.btree)
            .then(self.pos.cmp(&other.pos))
    }
}

impl PartialOrd for Bbpos {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

pub fn bbpos_cmp(l: &Bbpos, r: &Bbpos) -> std::cmp::Ordering {
    l.cmp(r)
}

pub fn bbpos_start(l: &Bbpos, r: &Bbpos) -> Bbpos {
    if l <= r { *l } else { *r }
}

pub fn bbpos_end(l: &Bbpos, r: &Bbpos) -> Bbpos {
    if l >= r { *l } else { *r }
}
