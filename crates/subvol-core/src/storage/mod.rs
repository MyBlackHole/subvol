//! Storage — 块设备存储层
//!
//! 管理块设备级别的布局：超块区（BlockAddr 0 的元数据块）、保留区、
//! 以及 superblock/journal 元数据。

pub mod service;
pub mod superblock;

#[cfg(test)]
pub mod null_device;

pub use service::StorageService;
pub use superblock::{BackupSbLayout, BchSb};
