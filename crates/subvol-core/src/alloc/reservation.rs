//! DiskReservation — bcachefs 对齐的扇区预留系统
//!
//! 对应 bcachefs `struct disk_reservation`（`buckets_types.h:98-102`）+ `__bch2_disk_reservation_add()`
//!（`buckets.c:1215-1240`）+ `disk_reservation_recalc_sectors_available()`（`buckets.c:1190-1213`）。
//!
//! ## 作用
//!
//! 在分配操作之前预留一定数量的扇区，确保分配不会因空间不足失败。
//! 预留生命周期：init → add 预留扇区 → 分配操作 → commit 消耗已用扇区 → put 释放剩余
//!
//! ## 结构
//!
//! - `DiskReservation`：单次预留的记录（扇区数 + gen + 副本数），对应 `struct disk_reservation`
//! - 新 free functions（`bch2_disk_reservation_put/add/get/init`）：按 bcachefs 本地 C 代码逐行翻译，
//!   操作 `&BchVol`/`&DiskReservation`，使用 `BchFsCapacity` 三级缓存架构。
//!   生产分配器已切换到新函数。
//!
//! ## bcachefs 对照
//!
//! | bcachefs | 本模块 |
//! |----------|--------|
//! | `bch2_disk_reservation_add()` | `bch2_disk_reservation_add()`（free fn）|
//! | `__bch2_disk_reservation_add()` | `__bch2_disk_reservation_add()` |
//! | `disk_reservation_recalc_sectors_available()` | `disk_reservation_recalc_sectors_available()` |
//! | `bch2_disk_reservation_put()` | `bch2_disk_reservation_put()`（free fn）|
//! | `bch2_disk_reservation_init()` | `bch2_disk_reservation_init()`（free fn）|
//! | `bch2_disk_reservation_get()` | `bch2_disk_reservation_get()`（free fn）|
//! | `struct disk_reservation` | `DiskReservation` |
//!
//! 参考: `fs/alloc/buckets_types.h:98-102`, `fs/alloc/buckets.c:1186-1240`

use std::sync::atomic::Ordering;

use crate::alloc::{BchFsCapacityPcpu, BchFsUsageShort};
use crate::bch_vol::BchVol;
use crate::types::StorageError;

const RESERVE_FACTOR: u32 = 6;
/// 对应本地 `SECTORS_CACHE` (`fs/alloc/buckets.c:1188`)。
const SECTORS_CACHE: u64 = 1024;

/// 对应本地 `reserve_factor()` (`fs/alloc/buckets.c:65-68`)。
fn reserve_factor(r: u64) -> u64 {
    r.wrapping_add(
        (r.wrapping_add((1 << RESERVE_FACTOR) - 1) & !((1 << RESERVE_FACTOR) - 1))
            >> RESERVE_FACTOR,
    )
}

/// 对应本地 `avail_factor()` (`fs/alloc/buckets.h:413-418`)。
pub fn avail_factor(r: u64) -> u64 {
    r.wrapping_shl(RESERVE_FACTOR) / ((1 << RESERVE_FACTOR) + 1)
}

/// 对应本地 `__bch2_fs_usage_read_short()` (`fs/alloc/buckets.c:70-91`)。
/// 调用者必须持有 `c.capacity.mark_lock` 的读锁或写锁。
fn __bch2_fs_usage_read_short(c: &BchVol) -> BchFsUsageShort {
    let capacity = unsafe { &*c.capacity.get() };
    let mut b = BchFsCapacityPcpu::default();

    for usage in &capacity.pcpu {
        b.usage.hidden = b.usage.hidden.wrapping_add(usage.usage.hidden);
        b.usage.btree = b.usage.btree.wrapping_add(usage.usage.btree);
        b.usage.data = b.usage.data.wrapping_add(usage.usage.data);
        b.usage.cached = b.usage.cached.wrapping_add(usage.usage.cached);
        b.usage.reserved = b.usage.reserved.wrapping_add(usage.usage.reserved);
        b.sectors_available = b.sectors_available.wrapping_add(usage.sectors_available);
        b.online_reserved = b.online_reserved.wrapping_add(usage.online_reserved);
    }

    let ret_capacity = capacity.capacity.wrapping_sub(b.usage.hidden);
    let data = b.usage.data.wrapping_add(b.usage.btree);
    let reserved = b.usage.reserved.wrapping_add(b.online_reserved);
    let used = ret_capacity.min(data.wrapping_add(reserve_factor(reserved)));

    BchFsUsageShort {
        capacity: ret_capacity,
        used,
        free: ret_capacity.wrapping_sub(used),
    }
}

/// 对应本地 `bch2_fs_usage_read_short()` (`fs/alloc/buckets.c:93-98`)。
pub fn bch2_fs_usage_read_short(c: &BchVol) -> BchFsUsageShort {
    let capacity = unsafe { &*c.capacity.get() };
    let _mark_lock = capacity.mark_lock.read().unwrap();
    __bch2_fs_usage_read_short(c)
}

/// 预留标志 — 对应 bcachefs `enum bch_reservation_flags`
///
/// `#[repr(u8)]` 使得 combined flags（如 Nofail | Partial）可以通过 transmute 安全构造，
/// 保留可组合 bit semantics（R4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BchReservationFlags(u8);

impl BchReservationFlags {
    /// 无特殊标志
    pub const None: Self = Self(0);
    /// 不允许失败 — 即使空间不足也强制分配（用于不可回滚的元数据写入）
    pub const Nofail: Self = Self(1 << 0);
    /// 允许部分分配 — 能分配多少算多少
    pub const Partial: Self = Self(1 << 1);
}

impl std::ops::BitOr for BchReservationFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for BchReservationFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// 扇区预留 — 对应 bcachefs `struct disk_reservation`
///
/// 纯内存结构，不序列化。记录一次分配操作预留的扇区数和 gen。
///
/// 生命周期：
/// 1. `init()` — 创建空预留
/// 2. `bch2_disk_reservation_add()` — 追加预留扇区
/// 3. 分配操作实际使用扇区 → 调用 `commit()` 消耗已用空间
/// 4. 分配完成或失败 → 调用 `put()` 释放剩余预留
#[derive(Debug, Clone)]
pub struct DiskReservation {
    /// 预留的扇区数（0 = 无预留）
    pub sectors: u64,
    /// 预留代际/版本号（对齐 bcachefs `gen` 字段）
    pub gen: u32,
    /// 副本数
    pub nr_replicas: u32,
}

// ─── bcachefs 对齐的新 free functions ───────────────────────────
//
// 以下函数完全依据本地 bcachefs C 代码：
//   fs/alloc/buckets.c:1188-1240
//   fs/alloc/buckets.h:341-401
//
// Production callers and focused tests use these capacity-backed primitives.

/// 检查 BchReservationFlags 中是否设置了指定位。
fn flags_has(flags: BchReservationFlags, bit: BchReservationFlags) -> bool {
    (flags.0 & bit.0) != 0
}

/// 对应本地 `disk_reservation_recalc_sectors_available()` (`fs/alloc/buckets.c:1190-1213`)。
///
/// 在 sectors_available_lock 保护下：
/// 1. 清零所有 pcpu 的 sectors_available（丢弃旧的局部缓存值）
/// 2. 通过 `__bch2_fs_usage_read_short` + `avail_factor` 重新计算实际可用量
/// 3. PARTIAL 时取 min
/// 4. 成功 / ENOSPC 二分路
fn disk_reservation_recalc_sectors_available(
    c: &BchVol,
    res: &mut DiskReservation,
    sectors: u64,
    flags: BchReservationFlags,
) -> Result<(), StorageError> {
    // guard(spinlock)(&c->capacity.sectors_available_lock);
    let capacity = unsafe { &mut *c.capacity.get() };
    let _lock = capacity.sectors_available_lock.lock().unwrap();

    // percpu_u64_set(&c->capacity.pcpu->sectors_available, 0);
    for pcpu in &mut capacity.pcpu {
        pcpu.sectors_available = 0;
    }

    // u64 sectors_available = avail_factor(__bch2_fs_usage_read_short(c).free);
    let usage = __bch2_fs_usage_read_short(c);
    let sectors_available = avail_factor(usage.free);

    // if (sectors_available && (flags & BCH_DISK_RESERVATION_PARTIAL))
    //     sectors = min(sectors, sectors_available);
    let effective_sectors =
        if sectors_available != 0 && flags_has(flags, BchReservationFlags::Partial) {
            sectors.min(sectors_available)
        } else {
            sectors
        };

    // if (sectors <= sectors_available ||
    //     (flags & BCH_DISK_RESERVATION_NOFAIL))
    if effective_sectors <= sectors_available || flags_has(flags, BchReservationFlags::Nofail) {
        // success path
        // atomic64_set(…, max_t(s64, 0, sectors_available - sectors));
        capacity.sectors_available.store(
            (sectors_available as i64 - effective_sectors as i64).max(0) as u64,
            Ordering::Release,
        );
        // this_cpu_add(c->capacity.pcpu->online_reserved, sectors);
        capacity.pcpu[0].online_reserved = capacity.pcpu[0]
            .online_reserved
            .wrapping_add(effective_sectors);
        // res->sectors += sectors;
        res.sectors = res.sectors.wrapping_add(effective_sectors);
        Ok(())
    } else {
        // ENOSPC path
        // atomic64_set(&c->capacity.sectors_available, sectors_available);
        capacity
            .sectors_available
            .store(sectors_available, Ordering::Release);
        Err(StorageError::AddressSpaceExhausted {
            max_raw_addr: sectors_available,
        })
    }
}

/// 对应本地 `__bch2_disk_reservation_add()` (`fs/alloc/buckets.c:1215-1240`)。
///
/// 没有 kernel per-CPU 指令，直接用 pcpu[0] 模拟单 CPU 路径。
pub fn __bch2_disk_reservation_add(
    c: &BchVol,
    res: &mut DiskReservation,
    sectors: u64,
    flags: BchReservationFlags,
) -> Result<(), StorageError> {
    if sectors == 0 {
        return Ok(());
    }

    // guard(preempt)();
    // struct bch_fs_capacity_pcpu *pcpu = this_cpu_ptr(c->capacity.pcpu);
    let capacity = unsafe { &mut *c.capacity.get() };
    let sectors_available = &mut capacity.pcpu[0].sectors_available;

    // if (unlikely(sectors > pcpu->sectors_available))
    if sectors > *sectors_available {
        // u64 get, old = atomic64_read(&c->capacity.sectors_available);
        let mut old = capacity.sectors_available.load(Ordering::Acquire);

        loop {
            // get = min((u64) sectors + SECTORS_CACHE, old);
            let get = sectors.wrapping_add(SECTORS_CACHE).min(old);

            // if (unlikely(get < sectors))
            if get < sectors {
                return disk_reservation_recalc_sectors_available(c, res, sectors, flags);
            }

            // atomic64_try_cmpxchg(…, &old, old - get)
            match capacity.sectors_available.compare_exchange_weak(
                old,
                old - get,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // pcpu->sectors_available += get;
                    *sectors_available = sectors_available.wrapping_add(get);
                    break;
                }
                Err(actual) => {
                    // actual becomes the new old for the next iteration
                    old = actual;
                }
            }
        }
    }

    // pcpu->sectors_available -= sectors;
    *sectors_available = sectors_available.wrapping_sub(sectors);
    // pcpu->online_reserved += sectors;
    capacity.pcpu[0].online_reserved = capacity.pcpu[0].online_reserved.wrapping_add(sectors);
    // res->sectors += sectors;
    res.sectors = res.sectors.wrapping_add(sectors);
    Ok(())
}

/// 对应本地 `bch2_disk_reservation_add()` (`fs/alloc/buckets.h:358-378`)。
///
/// userspace 构建（`#else` 分支）：始终委托给 `__bch2_disk_reservation_add`。
pub fn bch2_disk_reservation_add(
    c: &BchVol,
    res: &mut DiskReservation,
    sectors: u64,
    flags: BchReservationFlags,
) -> Result<(), StorageError> {
    let capacity = unsafe { &*c.capacity.get() };
    let _mark_lock = capacity.mark_lock.write().unwrap();
    __bch2_disk_reservation_add(c, res, sectors, flags)
}

/// 对应本地 `bch2_disk_reservation_put()` (`fs/alloc/buckets.h:341-348`)。
///
/// 如果 res.sectors != 0：从第一个 pcpu 的 online_reserved 中扣除，然后清零 sectors。
pub fn bch2_disk_reservation_put(c: &BchVol, res: &mut DiskReservation) {
    let capacity = unsafe { &mut *c.capacity.get() };
    let _mark_lock = capacity.mark_lock.write().unwrap();
    let sectors = res.sectors;
    res.sectors = 0;
    if sectors != 0 {
        capacity.pcpu[0].online_reserved = capacity.pcpu[0].online_reserved.wrapping_sub(sectors);
    }
}

/// 对应本地 `bch2_disk_reservation_init()` (`fs/alloc/buckets.h:380-391`)。
///
/// 返回一个 sectors=0 的空预留。`_c` 保留供 future capacity_gen 字段使用，
/// 与 bcachefs 签名保持一致。
pub fn bch2_disk_reservation_init(_c: &BchVol, nr_replicas: u32) -> DiskReservation {
    DiskReservation {
        sectors: 0,
        gen: 0,
        nr_replicas,
    }
}

/// 对应本地 `bch2_disk_reservation_get()` (`fs/alloc/buckets.h:393-401`)。
///
/// 组合 init + add，`sectors * nr_replicas` 作为预留总量。
pub fn bch2_disk_reservation_get(
    c: &BchVol,
    res: &mut DiskReservation,
    sectors: u64,
    nr_replicas: u32,
    flags: BchReservationFlags,
) -> Result<(), StorageError> {
    *res = bch2_disk_reservation_init(c, nr_replicas);
    bch2_disk_reservation_add(c, res, sectors.wrapping_mul(nr_replicas as u64), flags)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fs_usage_reserve_and_avail_factor() {
        assert_eq!(reserve_factor(0), 0);
        assert_eq!(reserve_factor(64), 65);
        assert_eq!(reserve_factor(65), 67);

        assert_eq!(avail_factor(0), 0);
        assert_eq!(avail_factor(1), 0);
        assert_eq!(avail_factor(65), 64);
    }

    #[test]
    fn test_fs_usage_read_short() {
        let vol = BchVol::test_trees();
        let capacity = unsafe { &mut *vol.capacity.get() };
        {
            let _mark_lock = capacity.mark_lock.write().unwrap();
            capacity.capacity = 1_000;
            capacity.pcpu[0].usage.hidden = 100;
            capacity.pcpu[0].usage.data = 200;
            capacity.pcpu[0].usage.btree = 50;
            capacity.pcpu[0].usage.cached = 777;
            capacity.pcpu[0].usage.reserved = 64;
            capacity.pcpu[0].online_reserved = 64;
            capacity.pcpu[0].sectors_available = 888;
            capacity.sectors_available.store(999, Ordering::Release);
        }

        assert_eq!(
            bch2_fs_usage_read_short(&vol),
            BchFsUsageShort {
                capacity: 900,
                used: 380,
                free: 520,
            }
        );
    }

    #[test]
    fn test_fs_usage_read_short_ignores_available_caches() {
        let vol = BchVol::test_trees();
        let before = bch2_fs_usage_read_short(&vol);
        let capacity = unsafe { &mut *vol.capacity.get() };
        {
            let _mark_lock = capacity.mark_lock.write().unwrap();
            capacity.pcpu[0].sectors_available = u64::MAX;
            capacity
                .sectors_available
                .store(u64::MAX, Ordering::Release);
        }

        assert_eq!(bch2_fs_usage_read_short(&vol), before);
    }

    // ─── bcachefs 对齐新 free functions 的测试 ─────────────────

    /// 构造带有 combined flags 的 BchReservationFlags 用于测试。
    /// `BchReservationFlags` 的位模式（None=0, Nofail=1, Partial=2）与
    /// bcachefs `enum bch_reservation_flags` 完全一致。
    fn flags(nofail: bool, partial: bool) -> BchReservationFlags {
        match (nofail, partial) {
            (false, false) => BchReservationFlags::None,
            (true, false) => BchReservationFlags::Nofail,
            (false, true) => BchReservationFlags::Partial,
            (true, true) => BchReservationFlags::Nofail | BchReservationFlags::Partial,
        }
    }

    /// 创建一个设置了指定总 capacity 和 pcpu online_reserved 的测试环境。
    /// 返回 vol 和写入后的 usage 快照值。
    fn setup_capacity(capacity_sectors: u64, online_reserved: u64) -> BchVol {
        let vol = BchVol::test_trees();
        let cap = unsafe { &mut *vol.capacity.get() };
        {
            let _lock = cap.mark_lock.write().unwrap();
            cap.capacity = capacity_sectors;
            cap.pcpu[0].online_reserved = online_reserved;
            cap.pcpu[0].sectors_available = 0;
            cap.sectors_available.store(0, Ordering::Release);
        }
        vol
    }

    #[test]
    fn test_new_disk_reservation_put() {
        let vol = setup_capacity(1_000_000, 0);
        let mut res = bch2_disk_reservation_init(&vol, 1);
        // 直接设置 sectors（模拟一次成功的 add 之后的状态）
        res.sectors = 500;
        // 手动设一些 online_reserved 供 put 扣除
        let cap = unsafe { &mut *vol.capacity.get() };
        cap.pcpu[0].online_reserved = 500;

        bch2_disk_reservation_put(&vol, &mut res);
        assert_eq!(res.sectors, 0, "put should zero res.sectors");
        assert_eq!(
            cap.pcpu[0].online_reserved, 0,
            "put should subtract from online_reserved"
        );
    }

    #[test]
    fn test_new_disk_reservation_put_zero_sectors_is_noop() {
        let vol = setup_capacity(1_000_000, 100);
        let mut res = bch2_disk_reservation_init(&vol, 1);
        let cap = unsafe { &mut *vol.capacity.get() };
        let before = cap.pcpu[0].online_reserved;

        bch2_disk_reservation_put(&vol, &mut res);
        assert_eq!(
            cap.pcpu[0].online_reserved, before,
            "put with zero sectors should not change online_reserved"
        );
    }

    #[test]
    fn test_new_disk_reservation_add_ok() {
        let vol = setup_capacity(1_000_000, 0);
        let cap = unsafe { &mut *vol.capacity.get() };
        // 设置局部缓存使其立即满足
        cap.pcpu[0].sectors_available = 10_000;

        let mut res = bch2_disk_reservation_init(&vol, 1);
        bch2_disk_reservation_add(&vol, &mut res, 500, BchReservationFlags::None).unwrap();

        assert_eq!(res.sectors, 500, "add should accumulate 500 sectors");
        assert_eq!(
            cap.pcpu[0].online_reserved, 500,
            "online_reserved should increase by 500"
        );
        assert_eq!(
            cap.pcpu[0].sectors_available,
            10_000 - 500,
            "pcpu local cache should decrease by 500"
        );
    }

    #[test]
    fn test_new_disk_reservation_add_uses_global_cache_on_shortage() {
        let vol = setup_capacity(1_000_000, 0);
        let cap = unsafe { &mut *vol.capacity.get() };
        // 局部缓存不足，但全局有值
        cap.pcpu[0].sectors_available = 50;
        cap.sectors_available.store(20_000, Ordering::Release);

        let mut res = bch2_disk_reservation_init(&vol, 1);
        bch2_disk_reservation_add(&vol, &mut res, 500, BchReservationFlags::None).unwrap();

        // sectors + SECTORS_CACHE = 500 + 1024 = 1524, old = 20000
        // get = min(1524, 20000) = 1524
        // pcpu 原 50 + 1524 = 1574, 减去 500 = 1074
        assert_eq!(
            cap.pcpu[0].sectors_available,
            50 + 1524 - 500,
            "pcpu cache should be refilled from global"
        );
        // global: old - get = 20000 - 1524 = 18476
        assert_eq!(
            cap.sectors_available.load(Ordering::Acquire),
            20000 - 1524,
            "global cache should decrease by get"
        );
        assert_eq!(res.sectors, 500);
    }

    #[test]
    fn test_new_disk_reservation_add_enospc() {
        let vol = BchVol::test_trees();
        let cap = unsafe { &mut *vol.capacity.get() };
        // 设 hidden = capacity → free = 0 → avail_factor(0) = 0
        {
            let _lock = cap.mark_lock.write().unwrap();
            cap.capacity = 1000;
            cap.pcpu[0].usage.hidden = 1000; // 使得 free = 0
            cap.pcpu[0].online_reserved = 0;
            cap.pcpu[0].sectors_available = 0;
            cap.sectors_available.store(0, Ordering::Release);
        }

        let mut res = bch2_disk_reservation_init(&vol, 1);
        let result = bch2_disk_reservation_add(&vol, &mut res, 500, BchReservationFlags::None);

        assert!(
            result.is_err(),
            "should return ENOSPC when no space available"
        );
        assert_eq!(res.sectors, 0, "res.sectors should stay 0 on ENOSPC");
    }

    #[test]
    fn test_new_disk_reservation_add_nofail() {
        let vol = BchVol::test_trees();
        let cap = unsafe { &mut *vol.capacity.get() };
        // 设 free = 100 → avail_factor(100) = 98
        // 500 > 98, NOFAIL 应成功并把 global clamp 到 0
        {
            let _lock = cap.mark_lock.write().unwrap();
            cap.capacity = 1000;
            cap.pcpu[0].usage.hidden = 900; // free = 100
            cap.pcpu[0].online_reserved = 0;
            cap.pcpu[0].sectors_available = 0;
            cap.sectors_available.store(0, Ordering::Release);
        }

        let mut res = bch2_disk_reservation_init(&vol, 1);
        bch2_disk_reservation_add(&vol, &mut res, 500, flags(true, false)).unwrap();

        assert_eq!(
            res.sectors, 500,
            "NOFAIL should succeed and accumulate sectors"
        );
        // global = max(0, 98 - 500) = 0
        assert_eq!(
            cap.sectors_available.load(Ordering::Acquire),
            0,
            "NOFAIL should clamp global cache to 0"
        );
        assert_eq!(cap.pcpu[0].online_reserved, 500);
    }

    #[test]
    fn test_new_disk_reservation_add_partial() {
        let vol = setup_capacity(5_000, 0);
        let cap = unsafe { &mut *vol.capacity.get() };
        cap.pcpu[0].sectors_available = 0;
        cap.sectors_available.store(0, Ordering::Release);

        // 设置 usage 让 avail_factor(free) 返回一个中间值
        // avail_factor = (free << 6) / 65
        // 我们需要 free 使得 avail_factor ≈ 300
        // 300 * 65 / 64 = 304.6875 → 设 free ≈ 305, avail_factor(305) = (305*64)/65 = 300
        {
            let _lock = cap.mark_lock.write().unwrap();
            // 让 usage.hidden 占用掉大部分 capacity
            // free = capacity - used, 其中 used = min(capacity, data + reserve_factor(reserved))
            // 我们设 reserved=0, data=0, hidden=0 但 capacity=5000, 所以 free=5000
            // 不对，那 avail_factor(5000) = (5000 * 64) / 65 = 4923
            // 太小了。设 capacity=5000, hidden=4700 → free = 5000 - 4700 = 300
            // avail_factor(300) = (300*64)/65 = 295
            // 请求 500, PARTIAL 应取 min(500, 295) = 295
            cap.capacity = 5000;
            cap.pcpu[0].usage.hidden = 4700;
        }

        let mut res = bch2_disk_reservation_init(&vol, 1);
        bch2_disk_reservation_add(&vol, &mut res, 500, flags(false, true)).unwrap();

        assert_eq!(
            res.sectors, 295,
            "PARTIAL should take min(requested, avail_factor(free))"
        );
        assert_eq!(cap.pcpu[0].online_reserved, 295);
    }

    #[test]
    fn test_new_disk_reservation_add_partial_exhausted() {
        let vol = setup_capacity(1_000, 0);
        let cap = unsafe { &mut *vol.capacity.get() };
        cap.pcpu[0].sectors_available = 0;
        cap.sectors_available.store(0, Ordering::Release);

        // PARTIAL: zero free → ENOSPC
        {
            let _lock = cap.mark_lock.write().unwrap();
            // capacity=1000, hidden=1000 → free=0
            cap.capacity = 1000;
            cap.pcpu[0].usage.hidden = 1000;
        }

        let mut res = bch2_disk_reservation_init(&vol, 1);
        let result = bch2_disk_reservation_add(&vol, &mut res, 100, flags(false, true));

        assert!(
            result.is_err(),
            "PARTIAL with zero free should return ENOSPC"
        );
        assert_eq!(
            res.sectors, 0,
            "res.sectors should stay 0 when PARTIAL exhausts"
        );
    }

    #[test]
    fn test_new_disk_reservation_get_with_replicas() {
        let vol = setup_capacity(1_000_000, 0);
        let cap = unsafe { &mut *vol.capacity.get() };
        cap.pcpu[0].sectors_available = 10_000;

        // 100 sectors with 3 replicas → need 300
        let mut res = bch2_disk_reservation_init(&vol, 3);
        bch2_disk_reservation_get(&vol, &mut res, 100, 3, BchReservationFlags::None).unwrap();

        assert_eq!(res.sectors, 300, "get should multiply by nr_replicas");
        assert_eq!(res.nr_replicas, 3);
        assert_eq!(
            cap.pcpu[0].online_reserved, 300,
            "online_reserved should reflect multiplied sectors"
        );
    }

    #[test]
    fn test_new_disk_reservation_add_zero() {
        let vol = setup_capacity(1_000_000, 0);
        let mut res = bch2_disk_reservation_init(&vol, 1);
        bch2_disk_reservation_add(&vol, &mut res, 0, BchReservationFlags::None).unwrap();
        assert_eq!(res.sectors, 0, "zero sectors add should be noop");
    }

    #[test]
    fn test_new_disk_reservation_add_multiple_cumulative() {
        let vol = setup_capacity(1_000_000, 0);
        let cap = unsafe { &mut *vol.capacity.get() };
        cap.pcpu[0].sectors_available = 10_000;

        let mut res = bch2_disk_reservation_init(&vol, 1);
        bch2_disk_reservation_add(&vol, &mut res, 200, BchReservationFlags::None).unwrap();
        bch2_disk_reservation_add(&vol, &mut res, 300, BchReservationFlags::None).unwrap();

        assert_eq!(res.sectors, 500, "multiple adds should accumulate");
        assert_eq!(cap.pcpu[0].online_reserved, 500);
    }

    #[test]
    fn test_new_disk_reservation_serializes_capacity_updates() {
        let vol = std::sync::Arc::new(setup_capacity(1_000_000, 0));
        {
            let cap = unsafe { &mut *vol.capacity.get() };
            cap.pcpu[0].sectors_available = 100_000;
            cap.sectors_available.store(100_000, Ordering::Release);
        }

        let workers: Vec<_> = (0..8)
            .map(|_| {
                let vol = vol.clone();
                std::thread::spawn(move || {
                    for _ in 0..100 {
                        let mut res = bch2_disk_reservation_init(&vol, 1);
                        bch2_disk_reservation_get(&vol, &mut res, 10, 1, BchReservationFlags::None)
                            .unwrap();
                        bch2_disk_reservation_put(&vol, &mut res);
                    }
                })
            })
            .collect();

        for worker in workers {
            worker.join().unwrap();
        }

        let cap = unsafe { &*vol.capacity.get() };
        let _mark_lock = cap.mark_lock.read().unwrap();
        assert_eq!(cap.pcpu[0].online_reserved, 0);
    }
}
