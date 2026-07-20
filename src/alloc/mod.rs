pub mod accounting;
pub mod backpointers;
pub mod background;
pub mod buckets;
pub mod check;
pub mod discard;
pub mod disk_groups;
pub mod foreground;
pub mod lru;
pub mod replicas;

pub use buckets::{
    bucket_to_sector, bucket_valid, data_type_is_empty, data_type_is_hidden,
    data_type_movable, dev_buckets_reserved, dev_ptr_stale, disk_reservation_init,
    disk_reservation_put, gen_after, gen_cmp, ptr_bucket_nr, ptr_data_type,
    sector_to_bucket, BucketGens, DevStripeState, DiskReservation, OpenBuckets,
    BchFsCapacity, BchWatermark, BCH_WATERMARK_NR,
};

pub use foreground::{
    alloc_trace_add, ob_dev, ob_ptr, ob_push, open_bucket_for_each, open_bucket_get,
    open_bucket_hashslot, open_buckets_put, writepoint_hashed, writepoint_ptr,
    AllocRequest, DevAllocList, WritePointSpecifier,
};

pub use background::{
    alloc_data_type, alloc_data_type_set, alloc_gc_gen, alloc_lru_idx_fragmentation,
    alloc_lru_idx_read, bch2_bucket_io_time_reset, bch2_bucket_sectors_dirty,
    bch2_bucket_sectors_total, bch2_recalc_capacity, bucket_data_type,
    bucket_data_type_mismatch, bucket_sectors, bucket_sectors_fragmented,
    bucket_sectors_unstriped, bucket_to_u64, dev_bucket_exists,
};

pub use discard::{
    bch2_dev_do_discards, bch2_do_discards_async, bch2_do_discards_going_ro,
    bch2_fast_discard_bucket_add, bch2_fast_discard_bucket_del,
    bch2_fs_discards_init, bch2_fs_discards_exit, bch2_dev_discards_init,
    bch2_dev_discards_exit, should_invalidate_buckets, DiscardState,
};

pub use lru::{
    bch2_lru_change, bch2_lru_pos_to_text, lru_end, lru_pos, lru_pos_id,
    lru_pos_time, lru_start, BchLruType,
};

pub use replicas::{
    devlist_to_replicas, replicas_entry_cached, replicas_entry_eq, replicas_entry_has_dev,
    replicas_entry_sort, sb_has_journal, sb_dev_has_data, dev_has_data,
    replicas_marked, mark_replicas,
};

pub use disk_groups::{
    dev_in_target, dev_to_target, group_to_target, target_decode, target_to_mask,
    target_accepts_data, target_rw_devs, disk_groups_nr, Target, TargetType,
};

pub use accounting::{
    bch2_accounting_read, bch2_accounting_validate, bch2_disk_accounting_mod,
    bch2_fs_accounting_read, bch2_fs_accounting_exit, bch2_fs_replicas_usage_read,
    bch2_dev_usage_init, bch2_dev_usage_remove,
    BCH_ACCOUNTING_NORMAL, BCH_ACCOUNTING_GC, BCH_ACCOUNTING_READ,
};

pub use backpointers::{
    backpointer_btree, bp_pos_to_bucket, bp_pos_to_bucket_and_offset,
    bucket_pos_to_bp, bucket_pos_to_bp_end, bucket_pos_to_bp_start,
    bch2_backpointer_get_key, bch2_backpointer_get_node, bch2_backpointer_validate,
    bch2_bucket_backpointer_mod,
};

pub use check::{
    bch2_check_alloc_info, bch2_check_alloc_to_lru_refs,
    bch2_dev_freespace_init, bch2_fs_freespace_init,
};
