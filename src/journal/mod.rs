pub mod journal;
pub mod read;
pub mod write;
pub mod reclaim;
pub mod validate;
pub mod init;
pub mod sb;
pub mod seq_blacklist;

pub use journal::*;
pub use read::*;
pub use write::*;
pub use reclaim::*;
pub use validate::*;
pub use init::*;
pub use sb::*;
pub use seq_blacklist::*;

use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::errcode::*;

use std::mem;
use std::ptr;

pub const JOURNAL_ENTRY_U64S_MIN: u16 = 4;
pub const JOURNAL_ENTRY_SEQ_BITS: u32 = 60;

#[derive(Clone, Debug)]
pub struct JournalEntry {
    pub jset: Jset,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct JournalEntryPin {
    pub seq: u64,
    pub list: *mut std::ffi::c_void,
}

pub struct JournalReplayList {
    pub entries: Vec<JournalEntry>,
    pub seq: u64,
    pub last_seq: u64,
}

pub fn bch2_journal_buf_put(c: &mut BchFs) -> BchResult<u64> {
    let seq = c.journal.seq.wrapping_add(1);
    c.journal.seq = seq;
    Ok(seq)
}

pub fn journal_entry_put(
    trans: &BtreeTrans,
    entry: &mut JournalEntry,
    journal_seq: &mut u64,
) {
    *journal_seq = trans.journal_seq;
}

pub fn journal_entry_close(c: &mut BchFs) {
    c.journal.nr_entries = c.journal.nr_entries.wrapping_add(1);
}

pub fn journal_entry_is_open(c: &BchFs) -> bool {
    c.journal.nr_entries > 0 || c.journal.seq > 0
}

pub fn bch2_journal_last_unwritten_seq(c: &BchFs) -> u64 {
    c.journal.seq
}

pub fn bch2_journal_cur_seq(c: &BchFs) -> u64 {
    c.journal.seq
}
