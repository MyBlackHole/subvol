use crate::recovery::RecoveryState;
use crate::types::StorageError;

/// 对应本地 `bch2_fs_journal_alloc()` (`fs/journal/init.c:305-320`)。
pub async fn run(state: &mut RecoveryState) -> Result<(), StorageError> {
    crate::journal::bch2_fs_journal_alloc(&state.vol)?;

    let ca = state
        .vol
        .primary_device_rcu_noerror()
        .expect("fs_journal_alloc: primary device not registered");
    state.superblock = ca.disk_sb.lock().unwrap().clone();
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::*;
    use crate::alloc::{
        BchDataType, BLOCKS_PER_BUCKET, DEFAULT_BLOCK_SIZE, DEFAULT_BTREE_NODE_SIZE,
        SECTORS_PER_BLOCK,
    };
    use crate::bch_vol::{BchVol, VolumeConfig};
    use crate::block_device::{BchDev, MockBlockDevice};
    use crate::journal::Journal;
    use crate::storage::superblock::{member_bits, BchSb, BchSbMember};

    #[tokio::test]
    async fn allocated_buckets_survive_recovery_progress_persistence() {
        let backend = Arc::new(MockBlockDevice::new());
        let dev = Arc::new(BchDev::new(backend, 0));
        let mut sb = BchSb::new();
        let mut member = BchSbMember::new(0, "dev-0");
        member.mark_alive([1; 16]);
        member.nbuckets = 1024;
        member.bucket_size = (BLOCKS_PER_BUCKET * SECTORS_PER_BLOCK) as u16;
        member.flags |= (1 << BchDataType::Journal as u8) << member_bits::DATA_ALLOWED_SHIFT;
        sb.capacity = member.nbuckets * BLOCKS_PER_BUCKET * DEFAULT_BLOCK_SIZE;
        sb.block_size = DEFAULT_BLOCK_SIZE as u32;
        sb.primary_dev_idx = 0;
        sb.members = vec![member];
        let vol = BchVol::alloc_with_devices(
            sb.clone(),
            [dev.clone()],
            VolumeConfig {
                block_size: DEFAULT_BLOCK_SIZE as u32,
                capacity: sb.capacity,
                btree_node_size: DEFAULT_BTREE_NODE_SIZE,
                ..VolumeConfig::default()
            },
            "fs-journal-alloc-test".into(),
            PathBuf::from("/tmp/fs-journal-alloc-test"),
        );
        let mut state = RecoveryState::new(Box::new(vol), Journal::new(vec![]), sb);

        run(&mut state).await.unwrap();
        assert_eq!(state.superblock.journal_buckets.len(), 8);

        state.persist_progress().await.unwrap();
        let persisted = BchSb::read_from_device(&dev).await.unwrap();
        assert_eq!(persisted.journal_buckets, state.superblock.journal_buckets);
        assert_eq!(persisted.journal_bucket_seq, vec![0; 8]);
    }
}
