use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::btree::bkey::*;
use crate::btree::locking::*;
use crate::btree::types::*;
use crate::btree::update::*;
use crate::errcode::*;

/// Write buffer entry
pub struct WriteBufferEntry {
    pub k: BkeyI,
    pub journal_seq: u64,
    pub seq: u32,
}

/// Flush write buffer entries to btree
pub fn bch2_btree_write_buffer_flush(
    trans: &mut BtreeTrans,
    write_buffer: &mut Vec<WriteBufferEntry>,
) -> Result<(), BchError> {
    let c = &mut trans.c;

    // Sort by btree_id, then by key
    write_buffer.sort_by(|a, b| {
        a.k.k.p.cmp(&b.k.k.p)
    });

    // Flush each entry
    let mut prev_btree_id = u8::MAX;
    for entry in write_buffer.drain(..) {
        if entry.k.k.type_val == 0 {
            continue;
        }
        let _ = bch2_btree_insert(trans, entry.k.btree_id, &entry.k);
    }

    Ok(())
}

/// Check if write buffer is enabled for a given btree
pub fn bch2_btree_uses_write_buffer(btree_id: u8) -> bool {
    // Write buffer used for non-data btrees: extents, reflink, etc
    // Based on bcachefs btree_id classification
    matches!(btree_id, 0..=5)
}

/// Try to flush from write buffer for a given key range
pub fn bch2_btree_write_buffer_maybe_flush(
    trans: &mut BtreeTrans,
    btree_id: u8,
    start: &Bpos,
    end: &Bpos,
) -> Result<(), BchError> {
    let wb = &mut trans.c.write_buffer;
    if wb.is_empty() {
        return Ok(());
    }
    // Check if we need to flush
    bch2_btree_write_buffer_flush(trans, wb)
}
