//! BchVol — bcachefs volume 容器
//!
//! 管理 Journal、BlockDevice 和 Btree 实例的生命周期。
//! 对应 bcachefs `struct bch_fs`。

use crate::block_device::BchDev;
use crate::journal::Journal;
use crate::types::StorageError;
use std::sync::{Arc, OnceLock};

// ═══════════════════════════════════════════════════════════════
// BchVol
// ═══════════════════════════════════════════════════════════════

/// BchVol — bcachefs volume 容器
///
/// 对应 bcachefs `struct bch_fs` (bcachefs.h)
/// 管理 Journal、设备列表等顶层资源
pub struct BchVol {
    /// Journal 实例（WAL）
    pub(crate) journal: Option<Arc<Journal>>,

    /// Device 列表
    devices: Vec<Arc<crate::block_device::BchDev>>,

    /// Btree 节点列表（简化版，实际 bcachefs 中有 btree_cache）
    btrees: Vec<crate::btree::BtreeNode>,

    /// 初始化标志
    initialized: OnceLock<bool>,
}

// ═══════════════════════════════════════════════════════════════
// 构造函数
// ═══════════════════════════════════════════════════════════════

impl BchVol {
    /// 创建空 BchVol
    pub fn new() -> Self {
        BchVol {
            journal: None,
            devices: Vec::new(),
            btrees: Vec::new(),
            initialized: OnceLock::new(),
        }
    }

    /// 使用已有 Journal 创建 BchVol
    pub fn with_journal(j: Arc<Journal>) -> Self {
        BchVol {
            journal: Some(j),
            devices: Vec::new(),
            btrees: Vec::new(),
            initialized: OnceLock::new(),
        }
    }

    /// 使用设备引用和 journal bucket 列表创建 BchVol
    pub fn with_dev(dev: Arc<BchDev>, journal_buckets: Vec<u64>) -> Arc<Self> {
        let journal = Arc::new(Journal::new(journal_buckets));
        let vol = Arc::new(BchVol {
            journal: Some(journal.clone()),
            devices: vec![dev.clone()],
            btrees: Vec::new(),
            initialized: OnceLock::new(),
        });
        journal.set_vol_ref(&vol);
        journal.set_device_ref(dev);
        vol.initialized.set(true).ok();
        vol
    }

    /// 使用 bucket 地址列表创建 BchVol，自动初始化 Journal
    ///
    /// 流程:
    /// 1. 用 bucket_addrs 创建 Journal
    /// 2. 为每个地址创建 BchDev
    /// 3. 设置 Journal 的 vol 回引用
    ///
    /// # 参数
    /// - `addrs`: journal bucket 的块设备地址列表
    pub fn new_with_devices(addrs: Vec<u64>) -> Arc<Self> {
        let journal = Arc::new(Journal::new(addrs.clone()));

        let vol = Arc::new(BchVol {
            journal: Some(journal.clone()),
            devices: Vec::new(),
            btrees: Vec::new(),
            initialized: OnceLock::new(),
        });

        // 设置 Journal → BchVol 回引用
        journal.set_vol_ref(&vol);

        // 为每个 bucket addr 创建 BchDev
        for _addr in &addrs {
            let dev = Arc::new(crate::block_device::BchDev::new(vol.clone()));
            journal.set_device_ref(dev);
        }

        // 标记初始化完成
        vol.initialized.set(true).ok();

        vol
    }

    /// 初始化 Journal（设置 vol 回引用）
    ///
    /// 需要在 BchVol 被 Arc 包装后调用。
    /// `new_with_devices()` 自动调用此方法。
    pub fn init_journal(self: &Arc<Self>) {
        if let Some(j) = &self.journal {
            j.set_vol_ref(self);
        }
        self.initialized.set(true).ok();
    }
}

// ═══════════════════════════════════════════════════════════════
// Journal 访问
// ═══════════════════════════════════════════════════════════════

impl BchVol {
    /// 获取 Journal 引用（Arc）
    pub fn journal_arc(&self) -> Arc<Journal> {
        self.journal
            .clone()
            .unwrap_or_else(|| Arc::new(Journal::new(vec![])))
    }

    /// 获取 Journal 引用（借用）
    pub fn journal_ref(&self) -> &Journal {
        self.journal
            .as_deref()
            .expect("BchVol: journal not initialized")
    }

    /// 获取 Journal 引用（Result 版本）
    pub fn get_journal(&self) -> Result<&Journal, StorageError> {
        self.journal
            .as_deref()
            .ok_or_else(|| StorageError::Internal("journal not initialized".into()))
    }
}

// ═══════════════════════════════════════════════════════════════
// Device 管理
// ═══════════════════════════════════════════════════════════════

impl BchVol {
    /// 添加设备
    pub fn add_device(&mut self, dev: Arc<crate::block_device::BchDev>) {
        self.devices.push(dev);
    }

    /// 设备数量
    pub fn device_count(&self) -> usize {
        self.devices.len()
    }

    /// 获取设备
    pub fn device(&self, idx: usize) -> Option<&Arc<crate::block_device::BchDev>> {
        self.devices.get(idx)
    }
}

// ═══════════════════════════════════════════════════════════════
// Btree 管理
// ═══════════════════════════════════════════════════════════════

impl BchVol {
    /// 添加 btree 节点
    pub fn add_btree(&mut self, node: crate::btree::BtreeNode) {
        self.btrees.push(node);
    }

    /// 获取 btree 节点
    pub fn btree(&self, idx: usize) -> Option<&crate::btree::BtreeNode> {
        self.btrees.get(idx)
    }

    /// 获取 btree 节点（可变）
    pub fn btree_mut(&mut self, idx: usize) -> Option<&mut crate::btree::BtreeNode> {
        self.btrees.get_mut(idx)
    }

    /// btree 节点数量
    pub fn btree_count(&self) -> usize {
        self.btrees.len()
    }

    /// 清空 btree 节点
    pub fn clear_btrees(&mut self) {
        self.btrees.clear();
    }
}

// ═══════════════════════════════════════════════════════════════
// 生命周期
// ═══════════════════════════════════════════════════════════════

impl BchVol {
    /// 是否已初始化
    pub fn is_initialized(&self) -> bool {
        self.initialized.get().copied().unwrap_or(false)
    }

    /// 检查 journal 是否已设置
    pub fn has_journal(&self) -> bool {
        self.journal.is_some()
    }
}

// ═══════════════════════════════════════════════════════════════
// Trait impls
// ═══════════════════════════════════════════════════════════════

impl std::fmt::Debug for BchVol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BchVol")
            .field("has_journal", &self.journal.is_some())
            .field("devices", &self.devices.len())
            .field("btrees", &self.btrees.len())
            .field("initialized", &self.is_initialized())
            .finish()
    }
}

impl Default for BchVol {
    fn default() -> Self {
        Self::new()
    }
}
