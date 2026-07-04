use crate::alloc::bucket::BchDataType;
use crate::alloc::Watermark;
use crate::bch_vol::BchVol;
use std::sync::atomic::Ordering;

/// 对应本地 `bch2_alloc_wake_all()` (`fs/alloc/foreground.c`)。
pub fn bch2_alloc_wake_all(c: &BchVol) {
    for dev_idx in c.device_registry.dev_indices() {
        if let Some(ca) = c.device_registry.resolve_bch_dev(dev_idx) {
            ca.alloc_wake_counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    c.allocator().freelist_wait.notify_waiters();
}

// ─── P2-12: prio_hint 映射 ──────────────────────────────────

/// 分配优先级提示——subvol 特有（bcachefs 无直接对应 `alloc_prio_hint`，该符号不存在）
///
/// P2-12: 增加 `UNSPECIFIED → USER/SYSTEM/META` 映射。
/// bcachefs 使用 prio_hint 作为 bucket 分配的优先级提示，
/// 影响 alloc_group 的选择和预留桶的分配顺序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrioHint {
    /// 未指定（默认）——根据 Watermark 自动映射
    Unspecified,
    /// 系统级数据（journal/超块）
    System,
    /// 元数据（btree 节点）
    Meta,
    /// 用户数据
    User,
}

impl PrioHint {
    /// 从 Watermark 推导 prio_hint（P2-12 映射）
    ///
    /// - Watermark::Stripe / Normal → User
    /// - Watermark::CopyGC / Reclaim → System
    /// - Watermark::Btree / BtreeCopyGC → Meta
    /// - Watermark::InteriorUpdate → System（内部更新最紧急）
    pub fn from_watermark(wm: Watermark) -> Self {
        match wm {
            Watermark::Stripe | Watermark::Normal => PrioHint::User,
            Watermark::CopyGC | Watermark::Reclaim => PrioHint::System,
            Watermark::Btree | Watermark::BtreeCopyGC => PrioHint::Meta,
            Watermark::InteriorUpdate => PrioHint::System,
        }
    }

    /// 返回该 hint 对应的数值（越大优先级越高）
    pub fn priority_value(self) -> u8 {
        match self {
            PrioHint::Unspecified => 0,
            PrioHint::System => 1,
            PrioHint::Meta => 2,
            PrioHint::User => 3,
        }
    }
}

// ─── P1-7: alloc_group 分配亲和性——prio_hint/target 复合算法 ──

/// Allocation target —— 分配目标组选择器
///
/// P1-7: 从线性扫描改为 prio_hint/target 复合算法。
/// 分配时根据 prio_hint 和 target 选择合适的 allocation group：
/// 1. 如果 target > 0，优先使用 target 指定的 group
/// 2. 否则使用 prio_hint 从匹配的 group 列表中选择
/// 3. 退回到 round-robin
#[derive(Debug, Clone, Copy)]
pub struct AllocTarget {
    /// 目标 allocation group（0 = 自动选择）
    pub target: u32,
    /// 优先级提示
    pub prio_hint: PrioHint,
    /// 数据类型（用于 group 兼容性检查）
    pub data_type: BchDataType,
}

impl AllocTarget {
    pub fn new(target: u32, prio_hint: PrioHint, data_type: BchDataType) -> Self {
        Self {
            target,
            prio_hint,
            data_type,
        }
    }

    /// 从 Watermark + data_type + target 创建分配目标
    pub fn from_request(target: u32, watermark: Watermark, data_type: BchDataType) -> Self {
        let prio_hint = if target == 0 {
            PrioHint::from_watermark(watermark)
        } else {
            // 明确指定 target 时，prio_hint 退化为默认
            PrioHint::Unspecified
        };
        Self {
            target,
            prio_hint,
            data_type,
        }
    }
}

/// 选择分配起始 group——复合算法入口
///
/// P1-7: 替代原线性 `hint % num_groups` 策略。
/// 实现 prio_hint/target 复合算法：
/// 1. target > 0 → 直接使用 target（如果 target 在范围内）
/// 2. prio_hint != Unspecified → 从匹配的 group 中选取 hint
/// 3. 退回到原 round-robin hint
///
/// # 参数
///
/// * `target` — `AllocTarget` 分配目标
/// * `num_groups` — 总 group 数量
/// * `round_robin_hint` — 当前 round-robin hint 值
#[cfg(test)]
mod tests {
    use super::*;

    // ─── P2-12: PrioHint tests ─────────────────────────────

    #[test]
    fn test_prio_hint_from_watermark() {
        assert_eq!(PrioHint::from_watermark(Watermark::Stripe), PrioHint::User);
        assert_eq!(PrioHint::from_watermark(Watermark::Normal), PrioHint::User);
        assert_eq!(PrioHint::from_watermark(Watermark::Btree), PrioHint::Meta);
        assert_eq!(
            PrioHint::from_watermark(Watermark::InteriorUpdate),
            PrioHint::System
        );
    }

    #[test]
    fn test_prio_hint_priority_value() {
        assert_eq!(PrioHint::Unspecified.priority_value(), 0);
        assert_eq!(PrioHint::System.priority_value(), 1);
        assert_eq!(PrioHint::Meta.priority_value(), 2);
        assert_eq!(PrioHint::User.priority_value(), 3);
    }

    // ─── P1-7: AllocTarget tests ──────────────────────────

    #[test]
    fn test_alloc_target_from_request() {
        let t = AllocTarget::from_request(0, Watermark::Stripe, BchDataType::User);
        assert_eq!(t.prio_hint, PrioHint::User);
        assert_eq!(t.target, 0);

        let t2 = AllocTarget::from_request(2, Watermark::Btree, BchDataType::Btree);
        assert_eq!(t2.prio_hint, PrioHint::Unspecified);
        assert_eq!(t2.target, 2);
    }
}
