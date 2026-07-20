use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::btree::bkey::*;
use crate::btree::bset::*;
use crate::btree::types::*;
use crate::errcode::*;

/// Key cache entry
pub struct BtreeKeyCacheEntry {
    pub k: BkeyI,
    pub seq: u32,
    pub journal_seq: u64,
}

/// Key cache for a btree node
pub struct BtreeKeyCache {
    pub entries: Vec<BtreeKeyCacheEntry>,
    pub max_entries: usize,
    pub seq: u32,
}

impl BtreeKeyCache {
    pub fn new(max: usize) -> Self {
        BtreeKeyCache {
            entries: Vec::with_capacity(max),
            max_entries: max,
            seq: 0,
        }
    }

    /// Lookup a key in the cache
    pub fn lookup(&self, pos: &Bpos) -> Option<&BtreeKeyCacheEntry> {
        // Linear scan (in production, use a hash/map)
        self.entries.iter().find(|e| {
            bkey_cmp(&e.k.k.p, pos) == 0
        })
    }

    /// Insert a key into the cache
    pub fn insert(&mut self, k: &BkeyI, journal_seq: u64) {
        // Remove existing entry at same position
        self.entries.retain(|e| bkey_cmp(&e.k.k.p, &k.k.p) != 0);

        // Evict if full
        if self.entries.len() >= self.max_entries {
            self.entries.remove(0);
        }

        self.seq += 1;
        self.entries.push(BtreeKeyCacheEntry {
            k: k.clone(),
            seq: self.seq,
            journal_seq,
        });
    }

    /// Remove entries for a range
    pub fn discard(&mut self, start: &Bpos, end: &Bpos) {
        self.entries.retain(|e| {
            bkey_cmp(&e.k.k.p, start) < 0 || bkey_cmp(&e.k.k.p, end) >= 0
        });
    }

    /// Flush all cache entries to btree
    pub fn flush(
        &mut self,
        trans: &mut BtreeTrans,
        btree_id: u8,
    ) -> Result<(), BchError> {
        for entry in self.entries.drain(..) {
            if entry.journal_seq > 0 {
                let _ = bch2_btree_insert(trans, btree_id, &entry.k);
            }
        }
        Ok(())
    }
}

/// Enable key cache for a btree
pub fn bch2_btree_key_cache_enable(c: &mut BchFs, btree_id: u8) {
    if c.btree.key_cache.is_none() {
        c.btree.key_cache = Some(Vec::with_capacity(BTREE_ID_NR as usize));
    }
    if let Some(ref mut caches) = c.btree.key_cache {
        while caches.len() <= btree_id as usize {
            caches.push(BtreeKeyCache::new(1024));
        }
    }
}

/// Flush key cache for a given btree
pub fn bch2_btree_key_cache_flush(
    trans: &mut BtreeTrans,
    btree_id: u8,
) -> Result<(), BchError> {
    let c = &mut trans.c;
    if let Some(ref mut caches) = c.btree.key_cache {
        if let Some(cache) = caches.get_mut(btree_id as usize) {
            cache.flush(trans, btree_id)?;
        }
    }
    Ok(())
}
