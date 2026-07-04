use std::fmt;
use std::ops::BitAnd;
use std::ops::BitAndAssign;
use std::ops::BitOr;
use std::ops::BitOrAssign;
use std::ops::BitXor;
use std::ops::BitXorAssign;
use std::ops::Not;

/// bcachefs `bch_devs_mask` — 最多 256 个设备的固定位图。
///
/// 每 bit 对应一个 `dev_idx`（0..255），为 1 表示该设备在集合中。
/// 内部存储为 4 × u64（256 bit），所有操作均为纯内存位运算。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct BchDevsMask {
    d: [u64; 4],
}

impl BchDevsMask {
    pub const BITS: usize = 256;
    pub const MAX_DEV_IDX: u8 = 255;

    pub const fn new() -> Self {
        Self { d: [0u64; 4] }
    }

    pub const fn all() -> Self {
        Self {
            d: [!0u64, !0u64, !0u64, !0u64],
        }
    }

    pub fn from_idx(dev_idx: u8) -> Self {
        let mut m = Self::new();
        m.set(dev_idx);
        m
    }

    pub fn set(&mut self, dev_idx: u8) {
        let (word, bit) = Self::split(dev_idx);
        self.d[word] |= 1u64 << bit;
    }

    pub fn clear(&mut self, dev_idx: u8) {
        let (word, bit) = Self::split(dev_idx);
        self.d[word] &= !(1u64 << bit);
    }

    pub fn is_set(&self, dev_idx: u8) -> bool {
        let (word, bit) = Self::split(dev_idx);
        (self.d[word] >> bit) & 1u64 == 1
    }

    pub fn contains(&self, other: &Self) -> bool {
        (self.d[0] & other.d[0]) == other.d[0]
            && (self.d[1] & other.d[1]) == other.d[1]
            && (self.d[2] & other.d[2]) == other.d[2]
            && (self.d[3] & other.d[3]) == other.d[3]
    }

    pub fn overlaps(&self, other: &Self) -> bool {
        (self.d[0] & other.d[0]) != 0
            || (self.d[1] & other.d[1]) != 0
            || (self.d[2] & other.d[2]) != 0
            || (self.d[3] & other.d[3]) != 0
    }

    pub fn is_empty(&self) -> bool {
        self.d[0] == 0 && self.d[1] == 0 && self.d[2] == 0 && self.d[3] == 0
    }

    pub fn count(&self) -> u32 {
        self.d[0].count_ones()
            + self.d[1].count_ones()
            + self.d[2].count_ones()
            + self.d[3].count_ones()
    }

    pub fn iter(&self) -> BchDevsMaskIter {
        BchDevsMaskIter {
            mask: *self,
            pos: 0,
        }
    }

    pub fn to_indices(&self) -> Vec<u8> {
        self.iter().collect()
    }

    fn split(dev_idx: u8) -> (usize, u64) {
        let word = (dev_idx as usize) / 64;
        let bit = (dev_idx as u64) % 64;
        (word, bit)
    }
}

impl Default for BchDevsMask {
    fn default() -> Self {
        Self::new()
    }
}

impl BitAnd for BchDevsMask {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self {
            d: [
                self.d[0] & rhs.d[0],
                self.d[1] & rhs.d[1],
                self.d[2] & rhs.d[2],
                self.d[3] & rhs.d[3],
            ],
        }
    }
}

impl BitAndAssign for BchDevsMask {
    fn bitand_assign(&mut self, rhs: Self) {
        self.d[0] &= rhs.d[0];
        self.d[1] &= rhs.d[1];
        self.d[2] &= rhs.d[2];
        self.d[3] &= rhs.d[3];
    }
}

impl BitOr for BchDevsMask {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self {
            d: [
                self.d[0] | rhs.d[0],
                self.d[1] | rhs.d[1],
                self.d[2] | rhs.d[2],
                self.d[3] | rhs.d[3],
            ],
        }
    }
}

impl BitOrAssign for BchDevsMask {
    fn bitor_assign(&mut self, rhs: Self) {
        self.d[0] |= rhs.d[0];
        self.d[1] |= rhs.d[1];
        self.d[2] |= rhs.d[2];
        self.d[3] |= rhs.d[3];
    }
}

impl BitXor for BchDevsMask {
    type Output = Self;
    fn bitxor(self, rhs: Self) -> Self {
        Self {
            d: [
                self.d[0] ^ rhs.d[0],
                self.d[1] ^ rhs.d[1],
                self.d[2] ^ rhs.d[2],
                self.d[3] ^ rhs.d[3],
            ],
        }
    }
}

impl BitXorAssign for BchDevsMask {
    fn bitxor_assign(&mut self, rhs: Self) {
        self.d[0] ^= rhs.d[0];
        self.d[1] ^= rhs.d[1];
        self.d[2] ^= rhs.d[2];
        self.d[3] ^= rhs.d[3];
    }
}

impl Not for BchDevsMask {
    type Output = Self;
    fn not(self) -> Self {
        Self {
            d: [!self.d[0], !self.d[1], !self.d[2], !self.d[3]],
        }
    }
}

impl fmt::Debug for BchDevsMask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "BchDevsMask({:016x}_{:016x}_{:016x}_{:016x})",
            self.d[3], self.d[2], self.d[1], self.d[0]
        )
    }
}

pub struct BchDevsMaskIter {
    mask: BchDevsMask,
    pos: u16,
}

impl Iterator for BchDevsMaskIter {
    type Item = u8;

    fn next(&mut self) -> Option<u8> {
        while self.pos <= BchDevsMask::MAX_DEV_IDX as u16 {
            if self.mask.is_set(self.pos as u8) {
                let idx = self.pos as u8;
                self.pos += 1;
                return Some(idx);
            }
            self.pos += 1;
        }
        None
    }
}

impl FromIterator<u8> for BchDevsMask {
    fn from_iter<I: IntoIterator<Item = u8>>(iter: I) -> Self {
        let mut m = Self::new();
        for idx in iter {
            m.set(idx);
        }
        m
    }
}

impl From<&[u8]> for BchDevsMask {
    fn from(dev_indices: &[u8]) -> Self {
        dev_indices.iter().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_mask() {
        let m = BchDevsMask::new();
        assert!(m.is_empty());
        assert_eq!(m.count(), 0);
    }

    #[test]
    fn test_set_and_test() {
        let mut m = BchDevsMask::new();
        m.set(0);
        m.set(127);
        m.set(255);
        assert!(m.is_set(0));
        assert!(m.is_set(127));
        assert!(m.is_set(255));
        assert!(!m.is_set(1));
        assert_eq!(m.count(), 3);
    }

    #[test]
    fn test_clear() {
        let mut m = BchDevsMask::all();
        m.clear(0);
        m.clear(255);
        assert!(!m.is_set(0));
        assert!(!m.is_set(255));
        assert!(m.is_set(1));
    }

    #[test]
    fn test_bit_ops() {
        let a: BchDevsMask = vec![0u8, 1, 2].into_iter().collect();
        let b: BchDevsMask = vec![2u8, 3, 4].into_iter().collect();

        let intersection = a & b;
        assert!(intersection.is_set(2));
        assert_eq!(intersection.count(), 1);

        let union = a | b;
        assert!(union.is_set(0));
        assert!(union.is_set(4));
        assert_eq!(union.count(), 5);

        let symmetric = a ^ b;
        assert_eq!(symmetric.count(), 4);
    }

    #[test]
    fn test_complement() {
        let mut m = BchDevsMask::new();
        m.set(0);
        let not_m = !m;
        assert!(!not_m.is_set(0));
        assert!(not_m.is_set(1));
        assert!(not_m.is_set(255));
    }

    #[test]
    fn test_contains() {
        let a: BchDevsMask = vec![0u8, 1, 2, 3].into_iter().collect();
        let b: BchDevsMask = vec![1u8, 2].into_iter().collect();
        assert!(a.contains(&b));
        assert!(!b.contains(&a));
    }

    #[test]
    fn test_overlaps() {
        let a: BchDevsMask = vec![0u8, 1].into_iter().collect();
        let b: BchDevsMask = vec![1u8, 2].into_iter().collect();
        let c: BchDevsMask = vec![3u8, 4].into_iter().collect();
        assert!(a.overlaps(&b));
        assert!(!a.overlaps(&c));
    }

    #[test]
    fn test_from_idx() {
        let m = BchDevsMask::from_idx(42);
        assert!(m.is_set(42));
        assert_eq!(m.count(), 1);
    }

    #[test]
    fn test_to_indices() {
        let m: BchDevsMask = vec![0u8, 5, 10, 255].into_iter().collect();
        let indices = m.to_indices();
        assert_eq!(indices, vec![0u8, 5, 10, 255]);
    }

    #[test]
    fn test_iteration() {
        let indices = vec![0u8, 1, 100, 200, 255];
        let m: BchDevsMask = indices.iter().copied().collect();
        let collected: Vec<u8> = m.iter().collect();
        assert_eq!(collected, indices);
    }

    #[test]
    fn test_from_slice() {
        let m = BchDevsMask::from(&[0u8, 1, 2][..]);
        assert!(m.is_set(0));
        assert!(m.is_set(2));
        assert_eq!(m.count(), 3);
    }
}
