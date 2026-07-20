use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::errcode::*;
use crate::journal::journal::*;
use crate::journal::read::*;

pub fn bch2_journal_seq_blacklist_add(c: &mut BchFs, seq: u64, end: u64) -> Result<(), BchError> {
    c.journal_seq_blacklist.push(SeqBlacklistEntry { seq, end });
    Ok(())
}

pub fn bch2_journal_seq_blacklist_del(c: &mut BchFs, seq: u64) -> Result<(), BchError> {
    c.journal_seq_blacklist.retain(|e| e.seq != seq);
    Ok(())
}

pub fn bch2_journal_seq_is_blacklisted(c: &BchFs, seq: u64) -> bool {
    c.journal_seq_blacklist.iter().any(|e| seq >= e.seq && seq < e.end)
}

pub fn bch2_journal_seq_next_blacklisted(c: &BchFs, seq: u64) -> u64 {
    let mut ret = u64::MAX;
    for e in &c.journal_seq_blacklist {
        if seq < e.seq && e.seq < ret {
            ret = e.seq;
        }
    }
    if ret == u64::MAX { seq } else { ret }
}

pub fn bch2_journal_seq_next_nonblacklisted(c: &BchFs, seq: u64) -> u64 {
    let mut s = seq;
    loop {
        let found = bch2_journal_seq_is_blacklisted(c, s);
        if !found {
            return s;
        }
        s = bch2_journal_seq_next_blacklisted(c, s);
        if s == seq {
            return s;
        }
    }
}

pub fn bch2_journal_seq_blacklist_blacklisted(c: &BchFs, seq: u64) -> bool {
    c.journal_seq_blacklist.iter().any(|e| seq >= e.seq && seq < e.end)
}

pub fn bch2_journal_seq_blacklist_get(c: &mut BchFs, seq: u64) -> Result<u64, BchError> {
    for e in &c.journal_seq_blacklist {
        if seq >= e.seq && seq < e.end {
            return Ok(e.end);
        }
    }
    Err(BchError::from_raw(-1))
}

pub fn bch2_journal_seq_blacklist_clear(c: &mut BchFs, seq: u64) {
    c.journal_seq_blacklist.retain(|e| e.seq < seq);
}

#[derive(Clone, Debug)]
pub struct SeqBlacklistEntry {
    pub seq: u64,
    pub end: u64,
}
