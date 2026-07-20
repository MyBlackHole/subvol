use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::errcode::*;
use crate::journal::journal::*;
use crate::journal::reclaim::*;
use crate::opts::BchOpts;

pub fn bch2_journal_init(c: &mut BchFs, opts: &BchOpts) -> Result<(), BchError> {
    let j = &mut c.journal;

    if opts.journal_seq_max != 0 {
        j.seq_max = opts.journal_seq_max;
    }

    j.cur_seq = 1;
    j.seq_ondisk = 0;
    j.flushed_seq_ondisk = 0;
    j.last_seq = 0;

    j.unwritten_seq = 0;
    j.err_seq = 0;

    j.block_bits = opts.block_size as u8;
    j.bucket_size_max = opts.bucket_size_max;

    j.buf_size_want = 0;
    j.buf_size_want_replicas = 0;

    j.space_available = 0;
    j.reclaim_started = false;

    j.oldest_seq_found_ondisk = u64::MAX;

    j.reservations = 0;
    j.entries_nr = 0;
    j.cur_entry = None;

    c.journal_entries.clear();

    Ok(())
}

pub fn bch2_journal_init_dev(c: &mut BchFs, ca: &mut BchDev) -> Result<(), BchError> {
    let j = &mut c.journal;
    let ja = &mut ca.journal;

    ja.nr = ca.mi.bucket_size as u32 / c.block_sectors();
    if ja.nr == 0 {
        ja.nr = 1;
    }

    ja.bucket_seq.resize(ja.nr as usize, 0);
    ja.buckets.resize(ja.nr as usize, 0);

    ja.cur_idx = 0;
    ja.sectors_free = 0;
    ja.dirty_idx = 0;
    ja.dirty_idx_ondisk = 0;
    ja.bucket_size = ca.mi.bucket_size as u32 / c.block_sectors();

    ja.highest_seq_found = 0;

    j.devices.push(ja.clone());

    Ok(())
}

pub fn bch2_journal_init_early(c: &mut BchFs, ca: &mut BchDev) -> Result<(), BchError> {
    let j = &mut c.journal;

    let ja = &mut ca.journal;
    ja.nr = 0;
    ja.cur_idx = 0;
    ja.sectors_free = 0;
    ja.dirty_idx = 0;
    ja.dirty_idx_ondisk = 0;
    ja.bucket_size = 0;

    j.devices.push(ja.clone());

    Ok(())
}

pub fn bch2_journal_dev_nr(j: &Journal) -> u32 {
    j.devices.len() as u32
}
