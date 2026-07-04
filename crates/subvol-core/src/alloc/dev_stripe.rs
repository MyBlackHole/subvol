use super::BCH_DATA_NR;
use crate::block_device::BchDevsMask;
use crate::storage::superblock::BchMemberState;
use crate::BchVol;

/// bcachefs `struct dev_stripe_state` — WFQ 比例偏置分配状态。
///
/// 每设备维护一个虚拟时钟指针 `next_alloc[i]`，每次分配后增量 = 1/free_space。
/// 时钟指针最小的设备在下次分配中获胜，从而实现按空闲比例偏置的轮询分配。
#[derive(Debug, Clone)]
pub struct DevStripeState {
    /// 每设备虚拟时钟指针（next_alloc[i] 越小越优先分配）。
    pub next_alloc: [u64; 256],
    /// 缓存的设备掩码，用于快速判断设备集是否变更。
    pub cached_devs: BchDevsMask,
}

/// 时钟指针重缩放阈值 — 超过此值后所有指针右移 1 位防止溢出。
pub const STRIPE_CLOCK_HAND_RESCALE: u64 = 1u64 << 62;

/// 时钟指针最大值（56 位有效）。
pub const STRIPE_CLOCK_HAND_MAX: u64 = 1u64 << 56;

/// 时钟指针增量分母 — 设备空闲时的最大增量值。
pub const STRIPE_CLOCK_HAND_INV: u64 = 1u64 << 52;

impl DevStripeState {
    pub fn new() -> Self {
        Self {
            next_alloc: [0u64; 256],
            cached_devs: BchDevsMask::new(),
        }
    }

    /// 同步缓存的设备掩码。对应 bcachefs `dev_stripe_state_sync()`：
    /// 仅将已不在 `devs` 中的设备的时钟指针归零。
    pub fn sync(&mut self, devs: &BchDevsMask) {
        if self.cached_devs == *devs {
            return;
        }

        // Match local `dev_stripe_state_sync()`:
        // newly-added members inherit the minimum virtual clock of the
        // members already in the candidate set.  Leaving them at zero would
        // make every newly online device win allocation ordering until its
        // clock caught up, defeating the weighted-fair stripe scheduler.
        let mut min_va = u64::MAX;
        for dev_idx in devs.iter() {
            if self.cached_devs.is_set(dev_idx) {
                min_va = min_va.min(self.next_alloc[dev_idx as usize]);
            }
        }

        if min_va != u64::MAX {
            for dev_idx in devs.iter() {
                if !self.cached_devs.is_set(dev_idx) {
                    self.next_alloc[dev_idx as usize] = min_va;
                }
            }
        }

        for dev_idx in self.cached_devs.iter() {
            if !devs.is_set(dev_idx) {
                self.next_alloc[dev_idx as usize] = 0;
            }
        }
        self.cached_devs = *devs;
    }

    /// 对应 bcachefs `bch2_dev_stripe_increment()`：
    /// 按设备空闲比例递增时钟指针。
    ///
    /// `free_space`：设备的空闲 bucket/block 数。
    /// 增量 = `STRIPE_CLOCK_HAND_INV / free_space`，空闲越小增量越大。
    pub fn increment(&mut self, dev_idx: u8, free_space: u64) {
        let v = &mut self.next_alloc[dev_idx as usize];
        let free_space_inv = if free_space > 0 {
            STRIPE_CLOCK_HAND_INV / free_space
        } else {
            STRIPE_CLOCK_HAND_INV
        };

        let (sum, overflow) = v.overflowing_add(free_space_inv);
        *v = if overflow { u64::MAX } else { sum };

        if *v > STRIPE_CLOCK_HAND_RESCALE {
            self.rescale();
        }
    }

    /// 重缩放：所有时钟指针右移 1 位（无符号除 2），防止溢出。
    fn rescale(&mut self) {
        // Match local `bch2_stripe_state_rescale()`: subtract one common
        // scale instead of shifting each clock independently.  The common
        // offset preserves all pairwise ordering/distance while preventing
        // overflow for devices that are rarely selected.
        let mut scale_max = u64::MAX;
        let mut scale_min = 0u64;
        for &v in &self.next_alloc {
            if v != 0 {
                scale_max = scale_max.min(v);
            }
            if v > STRIPE_CLOCK_HAND_MAX {
                scale_min = scale_min.max(v - STRIPE_CLOCK_HAND_MAX);
            }
        }
        let scale = scale_max.max(scale_min);
        for v in self.next_alloc.iter_mut() {
            *v = if *v < scale { 0 } else { *v - scale };
        }
    }

    /// 获取设备 `dev_idx` 的当前时钟指针值。
    pub fn get(&self, dev_idx: u8) -> u64 {
        self.next_alloc[dev_idx as usize]
    }
}

impl Default for DevStripeState {
    fn default() -> Self {
        Self::new()
    }
}

/// 按 WFQ 时钟指针排序的设备分配列表。
/// 对应 bcachefs `struct dev_alloc_list`。
#[derive(Debug, Clone)]
pub struct DevAllocList {
    pub nr: u8,
    pub data: [u8; 16], // BCH_BKEY_PTRS_MAX = 16
}

impl DevAllocList {
    pub fn new() -> Self {
        Self {
            nr: 0,
            data: [0u8; 16],
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = u8> + '_ {
        self.data[..self.nr as usize].iter().copied()
    }
}

/// 对应 bcachefs `bch2_dev_alloc_list()`：
/// 从 `devs` 掩码中按 WFQ 时钟指针升序排列设备。
///
/// 返回值中的设备按 `stripe.next_alloc` 升序排列（小 = 优先）。
pub fn bch2_dev_alloc_list(stripe: &mut DevStripeState, devs: &BchDevsMask) -> DevAllocList {
    stripe.sync(devs);

    let mut list = DevAllocList::new();
    for dev_idx in devs.iter() {
        if list.nr < 16 {
            list.data[list.nr as usize] = dev_idx;
            list.nr += 1;
        }
    }

    if list.nr > 1 {
        bubble_sort_devs(&mut list, stripe);
    }

    list
}

/// 冒泡排序：按 WFQ 时钟指针升序排列。
fn bubble_sort_devs(list: &mut DevAllocList, stripe: &DevStripeState) {
    let n = list.nr as usize;
    for i in 0..n {
        for j in 0..(n - 1 - i) {
            let a = list.data[j] as usize;
            let b = list.data[j + 1] as usize;
            if stripe.next_alloc[a] > stripe.next_alloc[b] {
                list.data.swap(j, j + 1);
            }
        }
    }
}

const TARGET_DEV_START: u16 = 1;
const TARGET_GROUP_START: u16 = 256 + TARGET_DEV_START;

/// 对应本地 `bch2_target_to_mask()` (`alloc/disk_groups.c:172-197`)。
pub fn bch2_target_to_mask(c: &BchVol, target: u16) -> Option<BchDevsMask> {
    if target == 0 {
        return None;
    }

    let mut mask = BchDevsMask::new();
    if target < TARGET_GROUP_START {
        let dev_idx = (target - TARGET_DEV_START) as u8;
        c.device_registry.resolve_bch_dev(dev_idx)?;
        mask.set(dev_idx);
        return Some(mask);
    }

    let group = target - TARGET_GROUP_START + 1;
    for dev_idx in c.device_registry.dev_indices() {
        let Some(ca) = c.device_registry.resolve_bch_dev(dev_idx) else {
            continue;
        };
        if unsafe { &*ca.mi.get() }.group == group {
            mask.set(dev_idx);
        }
    }
    (!mask.is_empty()).then_some(mask)
}

/// 对应本地 `target_rw_devs()` (`alloc/disk_groups.h:61-71`)。
pub fn target_rw_devs(
    c: &BchVol,
    data_type: crate::alloc::BchDataType,
    target: u16,
) -> BchDevsMask {
    if data_type as usize >= BCH_DATA_NR {
        return BchDevsMask::new();
    }
    let mut devs = BchDevsMask::new();
    for dev_idx in c.device_registry.dev_indices() {
        let Some(ca) = c.device_registry.resolve_bch_dev(dev_idx) else {
            continue;
        };
        let mi = unsafe { &*ca.mi.get() };
        let data_type_rw = ca.is_online()
            && ca.member_state() == BchMemberState::Rw
            && (data_type == crate::alloc::BchDataType::Free
                || mi.data_allowed & (1 << data_type as u8) != 0)
            && (!matches!(
                data_type,
                crate::alloc::BchDataType::Journal | crate::alloc::BchDataType::Btree
            ) || mi.durability != 0);
        if data_type_rw {
            devs.set(dev_idx);
        }
    }
    if let Some(target_devs) = bch2_target_to_mask(c, target) {
        devs &= target_devs;
    }
    devs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alloc::BchDataType;
    use crate::BchVol;

    #[test]
    fn test_stripe_new() {
        let s = DevStripeState::new();
        assert_eq!(s.next_alloc[0], 0);
        assert_eq!(s.next_alloc[255], 0);
        assert!(s.cached_devs.is_empty());
    }

    #[test]
    fn test_stripe_increment() {
        let mut s = DevStripeState::new();

        // free_space=100 → increment = STRIPE_CLOCK_HAND_INV / 100
        let expected_inc = STRIPE_CLOCK_HAND_INV / 100;
        s.increment(0, 100);
        assert_eq!(s.get(0), expected_inc);

        // free_space=200 → increment = STRIPE_CLOCK_HAND_INV / 200 (half)
        s.increment(0, 200);
        assert_eq!(s.get(0), expected_inc + STRIPE_CLOCK_HAND_INV / 200);
    }

    #[test]
    fn test_stripe_increment_zero_free() {
        let mut s = DevStripeState::new();
        // free_space=0 → increment = max (STRIPE_CLOCK_HAND_INV)
        s.increment(0, 0);
        assert_eq!(s.get(0), STRIPE_CLOCK_HAND_INV);
    }

    #[test]
    fn test_dev_alloc_list_sorts_by_clock() {
        let mut stripe = DevStripeState::new();
        let mut devs = BchDevsMask::new();
        devs.set(0);
        devs.set(1);
        devs.set(2);

        // Increment device 0 the most, 2 the least
        stripe.increment(0, 1); // max increment
        stripe.increment(1, 100);
        stripe.increment(2, 1000); // smallest increment

        let list = bch2_dev_alloc_list(&mut stripe, &devs);

        // Should be sorted: 2 (smallest clock) first, then 1, then 0
        let indices: Vec<u8> = list.iter().collect();
        assert_eq!(indices, vec![2u8, 1, 0], "expected sort by clock asc");
    }

    #[test]
    fn test_new_device_inherits_existing_minimum_clock() {
        let mut stripe = DevStripeState::new();
        let mut initial = BchDevsMask::new();
        initial.set(0);
        initial.set(1);
        stripe.sync(&initial);
        stripe.increment(0, 1);
        stripe.increment(1, 100);

        let minimum = stripe.get(1);
        let mut expanded = initial;
        expanded.set(2);
        stripe.sync(&expanded);

        assert_eq!(stripe.get(2), minimum);
        assert_eq!(
            bch2_dev_alloc_list(&mut stripe, &expanded).iter().next(),
            Some(1)
        );
    }

    #[test]
    fn test_sync_clears_removed_devices() {
        let mut stripe = DevStripeState::new();
        // Initialize cached_devs with device 0
        let mut old_devs = BchDevsMask::new();
        old_devs.set(0);
        stripe.sync(&old_devs);
        stripe.increment(0, 1);
        assert!(stripe.get(0) > 0);

        // Sync with device 1 (device 0 removed)
        let mut new_devs = BchDevsMask::new();
        new_devs.set(1);
        stripe.sync(&new_devs);
        // Device 0's clock should be reset to 0
        assert_eq!(stripe.get(0), 0);
        // Device 1 is new → clock is 0
        assert_eq!(stripe.get(1), 0);
    }

    #[test]
    fn test_rescale() {
        let mut s = DevStripeState::new();
        s.next_alloc[0] = 1u64 << 63;
        s.next_alloc[1] = 1u64 << 62;
        s.increment(0, 1); // Should trigger rescale (value > 1<<62)
                           // The common subtraction leaves the high clock near the configured
                           // max and preserves the lower clock's relative position.
        assert_eq!(s.get(0), STRIPE_CLOCK_HAND_MAX);
        assert_eq!(s.get(1), 0);
    }

    #[test]
    fn target_rw_devs_excludes_offline_rw_member() {
        let vol = BchVol::test_trees();
        vol.primary_device_rcu_noerror().unwrap().set_offline();

        assert!(target_rw_devs(&vol, BchDataType::Journal, 0).is_empty());
    }
}
