use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::btree::bkey::*;
use crate::btree::bset::*;
use crate::btree::cache::*;
use crate::btree::interior::*;
use crate::btree::locking::*;
use crate::btree::types::*;
use crate::btree::update::*;
use crate::btree::write::*;
use crate::errcode::*;

/// Journal reservation
pub struct JournalReservation {
    pub sectors: u32,
    pub seq: u64,
}

/// Make journal reservation
pub fn bch2_journal_res_reserve(
    c: &mut BchFs,
    sectors: u32,
) -> Result<JournalReservation, BchError> {
    let seq = c.journal.last_seq + 1;
    c.journal.last_seq = seq;
    Ok(JournalReservation { sectors, seq })
}

/// Commit a btree transaction
pub fn bch2_trans_commit(
    trans: &mut BtreeTrans,
    trigger_flags: BtreeIterUpdateTriggerFlags,
) -> Result<u64, BchError> {
    let c = &mut trans.c;

    // Build list of updates from trans->updates
    let updates = core::mem::take(&mut trans.updates);
    if updates.is_empty() {
        return Ok(0);
    }

    // Journal reservation
    let journal_sectors = 1u32;
    let journal = bch2_journal_res_reserve(c, journal_sectors)?;

    // For each update, insert into the corresponding btree node
    let mut journal_seq = journal.seq;
    for update in &updates {
        let btree_id = update.btree_id;

        // Find the path for this btree
        let path_idx = trans.paths.iter().position(|p| p.btree_id == btree_id);
        let path_idx = match path_idx {
            Some(i) => i,
            None => continue,
        };
        let path = &mut trans.paths[path_idx];

        let level = update.level as usize;
        let b = match btree_path_node_mut(path, level) {
            Some(b) => b,
            None => continue,
        };

        // Lock for write
        mark_btree_node_locked_noreset(path, level, BtreeNodeLockedType::WriteLocked);
        if !b.c.lock.try_write() {
            b.c.lock.lock_write();
        }

        // Insert key
        let key_u64s = update.k.k.u64s as usize;
        bch2_btree_node_insert(trans, path, b, &update.k, key_u64s)?;

        // Set dirty
        bch2_btree_node_set_dirty(c, b);

        // Unlock
        mark_btree_node_locked_noreset(path, level, BtreeNodeLockedType::IntentLocked);
        b.c.lock.unlock_write();

        // Check if split needed
        if btree_node_free_u64s(b) < key_u64s * 2 {
            bch2_btree_node_split(trans, path, b)?;
        }
    }

    // Flush write buffer
    let _ = bch2_btree_write_buffer_flush(trans, &mut c.write_buffer);

    // Flush key cache
    let _ = bch2_btree_key_cache_flush(trans, 0);

    // Relock all paths
    for path in &mut trans.paths {
        if !bch2_btree_path_relock_norestart(trans, path) {
            return Err(BchError::EINVAL);
        }
    }

    Ok(journal_seq)
}

/// Insert a key into a btree (simple path)
pub fn bch2_btree_insert(
    trans: &mut BtreeTrans,
    btree_id: u8,
    k: &BkeyI,
) -> Result<u64, BchError> {
    // Create update entry
    let update = BtreeUpdate {
        btree_id,
        level: 0, // leaf
        k: k.clone(),
        old_k: BkeyI::default(),
    };
    trans.updates.push(update);

    // Commit
    bch2_trans_commit(trans, BtreeIterUpdateTriggerFlags(0))
}

/// Delete a key from btree
pub fn bch2_btree_delete(
    trans: &mut BtreeTrans,
    btree_id: u8,
    k: &BkeyI,
) -> Result<u64, BchError> {
    // Create delete update (type = 0 or empty key)
    let mut delete_k = k.clone();
    delete_k.k.type_val = 0; // Mark as delete
    let update = BtreeUpdate {
        btree_id,
        level: 0,
        k: delete_k,
        old_k: k.clone(),
    };
    trans.updates.push(update);

    bch2_trans_commit(trans, BtreeIterUpdateTriggerFlags(0))
}
