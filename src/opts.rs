use crate::errcode::{BchError, BchResult};
use crate::bcachefs_format::*;

pub const BCH_OPT_BOOL: u32 = 0;
pub const BCH_OPT_UINT: u32 = 1;
pub const BCH_OPT_STR: u32 = 2;
pub const BCH_OPT_BITFIELD: u32 = 3;
pub const BCH_OPT_FN: u32 = 4;

bitflags::bitflags! {
    pub struct OptFlags: u32 {
        const FS = 1;
        const DEVICE = 2;
        const INODE = 4;
        const FORMAT = 8;
        const MOUNT = 16;
        const RUNTIME = 32;
        const HUMAN_READABLE = 64;
        const MUST_BE_POW_2 = 128;
        const SB_FIELD_SECTORS = 256;
        const SB_FIELD_ILOG2 = 512;
        const SB_FIELD_ONE_BIAS = 1024;
        const HIDDEN = 2048;
        const MOUNT_OLD = 4096;
        const NODOC = 8192;
    }
}

pub struct BchOptFn {
    pub parse: fn(&BchFs, &str, &mut u64, &mut Printbuf) -> i32,
    pub to_text: fn(&mut Printbuf, &BchFs, &BchSb, u64),
    pub validate: fn(u64, &mut Printbuf) -> i32,
}

pub struct BchOption {
    pub attr: BchAttribute,
    pub mode: OptFlags,
    pub r#type: u32,
    pub sb_opt: u32,
    pub min: u64,
    pub max: u64,
    pub default_val: u64,
    pub hint: Option<&'static str>,
    pub help: Option<&'static str>,
    pub choices: Option<&'static [&'static str]>,
    pub fn_: Option<BchOptFn>,
}

pub struct BchAttribute {
    pub name: &'static str,
    pub mode: u16,
}

pub struct BchAttributeGroup {
    pub name: &'static str,
    pub attrs: &'static [&'static BchAttribute],
}

#[derive(Clone, Debug)]
pub struct BchOpts {
    pub block_size: u32,
    pub block_size_defined: bool,
    pub btree_node_size: u32,
    pub btree_node_size_defined: bool,
    pub cache_size: u32,
    pub cache_size_defined: bool,
    pub bucket_size: u32,
    pub bucket_size_defined: bool,
    pub inode_32bit: bool,
    pub inode_32bit_defined: bool,
    pub discard: bool,
    pub discard_defined: bool,
    pub fsck: bool,
    pub fsck_defined: bool,
    pub compression: BchCompressionType,
    pub compression_defined: bool,
    pub background_compression: BchCompressionType,
    pub background_compression_defined: bool,
    pub str_hash: BchStrHashType,
    pub str_hash_defined: bool,
    pub metadata_csum: BchCsumType,
    pub metadata_csum_defined: bool,
    pub data_csum: BchCsumType,
    pub data_csum_defined: bool,
    pub metadata_replicas: u32,
    pub metadata_replicas_defined: bool,
    pub data_replicas: u32,
    pub data_replicas_defined: bool,
    pub metadata_replicas_required: u32,
    pub metadata_replicas_required_defined: bool,
    pub data_replicas_required: u32,
    pub data_replicas_required_defined: bool,
    pub foreground_target: u32,
    pub foreground_target_defined: bool,
    pub background_target: u32,
    pub background_target_defined: bool,
    pub promote_target: u32,
    pub promote_target_defined: bool,
    pub metadata_target: u32,
    pub metadata_target_defined: bool,
    pub nocow: bool,
    pub nocow_defined: bool,
    pub erasure_code: u32,
    pub erasure_code_defined: bool,
    pub gc_reserve: u32,
    pub gc_reserve_defined: bool,
    pub gc_reserve_bytes: u64,
    pub gc_reserve_bytes_defined: bool,
    pub root_reserve: u32,
    pub root_reserve_defined: bool,
    pub journal_flush_delay: u32,
    pub journal_flush_delay_defined: bool,
    pub journal_reclaim_delay: u32,
    pub journal_reclaim_delay_defined: bool,
    pub journal_transaction_names: bool,
    pub journal_transaction_names_defined: bool,
    pub write_buffer_size: u32,
    pub write_buffer_size_defined: bool,
    pub version_upgrade: u32,
    pub version_upgrade_defined: bool,
    pub usrquota: bool,
    pub usrquota_defined: bool,
    pub grpquota: bool,
    pub grpquota_defined: bool,
    pub prjquota: bool,
    pub prjquota_defined: bool,
    pub acl: bool,
    pub acl_defined: bool,
    pub degraded_action: u32,
    pub degraded_action_defined: bool,
    pub error_action: u32,
    pub error_action_defined: bool,
    pub ratelimit_errors: bool,
    pub ratelimit_errors_defined: bool,
    pub version_upgrade_complete: u16,
    pub version_upgrade_complete_defined: bool,
    pub allocator_stuck_timeout: u32,
    pub allocator_stuck_timeout_defined: bool,
    pub write_error_timeout: u32,
    pub write_error_timeout_defined: bool,
    pub csum_err_retry_nr: u32,
    pub csum_err_retry_nr_defined: bool,
    pub caseless: bool,
    pub caseless_defined: bool,
    pub rebalance_ac_only: bool,
    pub rebalance_ac_only_defined: bool,
    pub writeback_timeout: u32,
    pub writeback_timeout_defined: bool,
    pub extent_bp_shift: u32,
    pub extent_bp_shift_defined: bool,
    pub scrub_journal: u32,
    pub scrub_journal_defined: bool,
    pub ec_max_data_blocks: u32,
    pub ec_max_data_blocks_defined: bool,
    pub dev_readahead: u32,
    pub dev_readahead_defined: bool,
    pub ec_stripe_buf_limit: u32,
    pub ec_stripe_buf_limit_defined: bool,
    pub scrub_max_rewind_secs: u32,
    pub scrub_max_rewind_secs_defined: bool,
    pub discard_buffer: u32,
    pub discard_buffer_defined: bool,
    pub shard_inode_bits: u32,
    pub shard_inode_bits_defined: bool,
    pub casefold: bool,
    pub casefold_defined: bool,
    pub casefold_disabled: bool,
    pub casefold_disabled_defined: bool,
}

impl BchOpts {
    pub fn empty() -> Self {
        BchOpts {
            block_size: 0, block_size_defined: false,
            btree_node_size: 0, btree_node_size_defined: false,
            cache_size: 0, cache_size_defined: false,
            bucket_size: 0, bucket_size_defined: false,
            inode_32bit: false, inode_32bit_defined: false,
            discard: false, discard_defined: false,
            fsck: false, fsck_defined: false,
            compression: BchCompressionType::None, compression_defined: false,
            background_compression: BchCompressionType::None, background_compression_defined: false,
            str_hash: BchStrHashType::Crc32c, str_hash_defined: false,
            metadata_csum: BchCsumType::None, metadata_csum_defined: false,
            data_csum: BchCsumType::None, data_csum_defined: false,
            metadata_replicas: 0, metadata_replicas_defined: false,
            data_replicas: 0, data_replicas_defined: false,
            metadata_replicas_required: 0, metadata_replicas_required_defined: false,
            data_replicas_required: 0, data_replicas_required_defined: false,
            foreground_target: 0, foreground_target_defined: false,
            background_target: 0, background_target_defined: false,
            promote_target: 0, promote_target_defined: false,
            metadata_target: 0, metadata_target_defined: false,
            nocow: false, nocow_defined: false,
            erasure_code: 0, erasure_code_defined: false,
            gc_reserve: 0, gc_reserve_defined: false,
            gc_reserve_bytes: 0, gc_reserve_bytes_defined: false,
            root_reserve: 0, root_reserve_defined: false,
            journal_flush_delay: 0, journal_flush_delay_defined: false,
            journal_reclaim_delay: 0, journal_reclaim_delay_defined: false,
            journal_transaction_names: false, journal_transaction_names_defined: false,
            write_buffer_size: 0, write_buffer_size_defined: false,
            version_upgrade: 0, version_upgrade_defined: false,
            usrquota: false, usrquota_defined: false,
            grpquota: false, grpquota_defined: false,
            prjquota: false, prjquota_defined: false,
            acl: false, acl_defined: false,
            degraded_action: 0, degraded_action_defined: false,
            error_action: 0, error_action_defined: false,
            ratelimit_errors: false, ratelimit_errors_defined: false,
            version_upgrade_complete: 0, version_upgrade_complete_defined: false,
            allocator_stuck_timeout: 0, allocator_stuck_timeout_defined: false,
            write_error_timeout: 0, write_error_timeout_defined: false,
            csum_err_retry_nr: 0, csum_err_retry_nr_defined: false,
            caseless: false, caseless_defined: false,
            rebalance_ac_only: false, rebalance_ac_only_defined: false,
            writeback_timeout: 0, writeback_timeout_defined: false,
            extent_bp_shift: BCH_SB_EXTENT_BP_SHIFT_DEFAULT as u32, extent_bp_shift_defined: false,
            scrub_journal: 0, scrub_journal_defined: false,
            ec_max_data_blocks: 0, ec_max_data_blocks_defined: false,
            dev_readahead: 0, dev_readahead_defined: false,
            ec_stripe_buf_limit: 0, ec_stripe_buf_limit_defined: false,
            scrub_max_rewind_secs: 0, scrub_max_rewind_secs_defined: false,
            discard_buffer: 0, discard_buffer_defined: false,
            shard_inode_bits: 0, shard_inode_bits_defined: false,
            casefold: false, casefold_defined: false,
            casefold_disabled: false, casefold_disabled_defined: false,
        }
    }
}

pub fn parse_mount_opts(optstr: Option<&str>, _ignore_unknown: bool) -> BchResult<BchOpts> {
    let mut opts = BchOpts::empty();
    if let Some(s) = optstr {
        for part in s.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if let Some((k, v)) = part.split_once('=') {
                let k = k.trim();
                let v = v.trim();
                match k {
                    "block_size" | "block_size_defined" => {
                        opts.block_size = v.parse().unwrap_or(0);
                        opts.block_size_defined = true;
                    }
                    "btree_node_size" => {
                        opts.btree_node_size = v.parse().unwrap_or(0);
                        opts.btree_node_size_defined = true;
                    }
                    "bucket_size" => {
                        opts.bucket_size = v.parse().unwrap_or(0);
                        opts.bucket_size_defined = true;
                    }
                    "discard" => {
                        opts.discard = v.parse().unwrap_or(false);
                        opts.discard_defined = true;
                    }
                    "compression" => {
                        opts.compression_defined = true;
                    }
                    "background_compression" => {
                        opts.background_compression_defined = true;
                    }
                    _ => {
                        if !_ignore_unknown {
                            return Err(BchError::from_raw(-22));
                        }
                    }
                }
            }
        }
    }
    Ok(opts)
}

pub struct Printbuf {
    pub buf: String,
    pub tabstop: u32,
}

impl Printbuf {
    pub fn new() -> Self {
        Printbuf { buf: String::new(), tabstop: 8 }
    }

    pub fn indent(&mut self, _n: u32) -> PrintbufIndent {
        PrintbufIndent { inner: self }
    }
}

pub struct PrintbufIndent<'a> {
    inner: &'a mut Printbuf,
}

impl std::fmt::Write for Printbuf {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.buf.push_str(s);
        Ok(())
    }
}

pub use crate::BchFs;
