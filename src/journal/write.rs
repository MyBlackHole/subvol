use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::errcode::*;
use crate::journal::journal::*;
use crate::journal::read::*;
use crate::journal::reclaim::*;
use crate::journal::validate::*;

pub fn jset_entry_init(end: &mut Vec<JsetEntry>, size: usize) -> &mut JsetEntry {
    let u64s = (size + 7) / 8;
    let entry = JsetEntry {
        u64s: (u64s as u16).saturating_sub(1),
        btree_id: 0,
        level: 0,
        type_: BchJsetEntryType::BtreeKeys,
    };
    end.push(entry);
    let last = end.last_mut().unwrap();
    last
}

pub fn journal_advance_devs_to_next_bucket(
    j: &mut Journal,
    devs: &[u8],
    sectors: u32,
    seq: u64,
) {
    for &dev_idx in devs {
        let dev_idx = dev_idx as usize;
        if dev_idx >= j.devices.len() {
            continue;
        }
        let ja = &mut j.devices[dev_idx];

        if sectors > ja.sectors_free
            && sectors <= ja.bucket_size
            && bch2_journal_dev_buckets_available(j, ja, JournalSpaceFrom::Discarded) > 0
        {
            ja.cur_idx = (ja.cur_idx + 1) % ja.nr;
            ja.sectors_free = ja.bucket_size;
            ja.bucket_seq[ja.cur_idx as usize] = seq;
        }
    }
}

pub fn __journal_write_alloc(
    j: &mut Journal,
    w: &mut JournalBuf,
    devs: &[u8],
    sectors: u32,
    replicas: &mut u32,
    replicas_want: u32,
) {
    for &dev_idx in devs {
        let dev_idx = dev_idx as usize;
        if dev_idx >= j.devices.len() {
            continue;
        }
        let ja = &mut j.devices[dev_idx];

        if ja.nr == 0 || sectors > ja.sectors_free {
            continue;
        }

        ja.sectors_free -= sectors;
        ja.bucket_seq[ja.cur_idx as usize] = w.data.seq;
        w.devs_written[w.devs_written_nr as usize] = dev_idx as u8;
        w.devs_written_nr += 1;

        *replicas += 1;

        if *replicas >= replicas_want {
            break;
        }
    }
}

pub fn journal_write_alloc(
    j: &mut Journal,
    w: &mut JournalBuf,
    replicas: &mut u32,
) -> Result<(), BchError> {
    let sectors = vstruct_sectors(&w.data, j.block_bits);
    let replicas_want = j.metadata_replicas;
    let mut devs = Vec::new();

    for i in 0..j.devices.len() {
        devs.push(i as u8);
    }

    __journal_write_alloc(j, w, &devs, sectors, replicas, replicas_want);

    if *replicas >= replicas_want {
        return Ok(());
    }

    if *replicas == 0 {
        return Err(BchError::from_raw(-1));
    }

    Ok(())
}

pub fn journal_buf_realloc(j: &mut Journal, buf: &mut JournalBuf) {
    let new_size = j.buf_size_want;
    if buf.buf_size >= new_size {
        return;
    }
    let new_buf = vec![0u8; new_size as usize];
    buf.buf_size = new_size;
}

fn replicas_refs_put(c: &mut BchFs, refs: &mut Vec<ReplicasEntryRefs>) {
    for r in refs.iter() {
        // bch2_replicas_entry_put_many(c, &r.replicas.e, r.nr_refs);
    }
    refs.clear();
}

pub fn journal_write_done(j: &mut Journal, w: &mut JournalBuf) {
    let seq = w.data.seq;
    let c = &mut j.fs;

    if w.devs_written_nr > 0 {
        j.flushed_seq_ondisk = seq;
    }

    j.seq_ondisk = seq;
    w.write_done = true;

    bch2_journal_update_last_seq(j);
    bch2_journal_space_available(j);
}

pub fn journal_write_submit(j: &mut Journal, w: &mut JournalBuf) {
    let sectors = vstruct_sectors(&w.data, j.block_bits);
    w.write_started = true;

    for i in 0..w.devs_written_nr as usize {
        let dev_idx = w.devs_written[i] as usize;
        if dev_idx < j.devices.len() {
            j.devices[dev_idx].bucket_seq[j.devices[dev_idx].cur_idx as usize] = w.data.seq;
        }
    }

    journal_write_done(j, w);
}

pub fn bch2_journal_write_prep(j: &mut Journal, w: &mut JournalBuf) -> Result<(), BchError> {
    let jset = &mut w.data;
    let mut empty = jset.seq == jset.last_seq;

    if w.need_flush_to_write_buffer {
        w.need_flush_to_write_buffer = false;
    }

    let mut entries_to_keep = Vec::new();
    for entry in jset.entries.drain(..) {
        if entry.u64s == 0 {
            continue;
        }
        if entry.type_ == BchJsetEntryType::BtreeKeys {
            empty = false;
        }
        entries_to_keep.push(entry);
    }
    jset.entries = entries_to_keep;

    if empty {
        w.empty = true;
    }

    Ok(())
}

pub fn bch2_journal_write_checksum(j: &mut Journal, w: &mut JournalBuf) -> Result<(), BchError> {
    let jset = &mut w.data;
    jset.magic = jset_magic(&j.fs);
    jset.version = j.fs.sb.version as u32;

    let csum_type = BchCsumType::Crc32c;
    jset.csum = csum_vstruct(&j.fs, csum_type, journal_nonce(jset), jset);

    Ok(())
}

pub fn bch2_journal_write(j: &mut Journal, w: &mut JournalBuf) {
    if !w.write_started {
        return;
    }
    if w.write_allocated {
        return;
    }
    if w.write_done {
        return;
    }

    if bch2_journal_write_prep(j, w).is_err() {
        return;
    }

    let mut replicas = 0;
    if journal_write_alloc(j, w, &mut replicas).is_err() {
        return;
    }

    if bch2_journal_write_checksum(j, w).is_err() {
        return;
    }

    w.write_allocated = true;

    journal_write_submit(j, w);
}

pub fn __should_flush(j: &Journal, w: &JournalBuf, _seq: u64) -> bool {
    if j.err_seq != 0 {
        return false;
    }
    if j.need_flush_write {
        return true;
    }
    if journal_buf_must_not_flush(w) {
        return false;
    }
    if journal_buf_must_flush(w) {
        return true;
    }
    false
}

pub fn should_flush(j: &mut Journal, w: &mut JournalBuf, seq: u64) -> i32 {
    let ret = __should_flush(j, w, seq);
    if !ret {
        return 0;
    }
    1
}

pub fn bch2_journal_do_writes_locked(j: &mut Journal) {
    if j.in_flight == 0 {
        return;
    }

    let mut found = false;
    for ring_idx in 0..JOURNAL_STATE_BUF_NR as usize {
        let buf = &mut j.ring[ring_idx];
        if buf.write_started || journal_state_seq_count(j, j.reservations, ring_idx as u64) > 0 {
            continue;
        }

        let seq = journal_cur_seq(j) - (journal_cur_seq(j) - ring_idx as u64) & JOURNAL_STATE_BUF_MASK as u64;
        if seq == 0 || seq > journal_cur_seq(j) {
            continue;
        }

        if !buf.flush_picked {
            let flush = should_flush(j, buf, seq);
            if flush > 0 {
                buf.flush = true;
                j.nr_flush_writes += 1;
            } else {
                buf.flush = false;
                j.nr_noflush_writes += 1;
            }
            buf.flush_picked = true;
        }

        if buf.flush && j.seq_ondisk + 1 != seq {
            continue;
        }

        buf.write_started = true;
        bch2_journal_write(j, buf);
        found = true;
    }
}

pub fn bch2_journal_do_writes(j: &mut Journal) {
    bch2_journal_do_writes_locked(j);
}

pub fn flush_would_free_space(j: &Journal, new_last_seq: u64) -> bool {
    for ja in &j.devices {
        if ja.dirty_idx_ondisk != ja.dirty_idx
            && ja.bucket_seq[ja.dirty_idx_ondisk as usize] < new_last_seq
        {
            return true;
        }
    }
    false
}

#[derive(Clone, Debug)]
pub struct ReplicasEntryRefs {
    pub nr_refs: u32,
    pub replicas: Vec<u8>,
}

pub struct BchFsJournalWriteState {
    pub writes_outstanding: u32,
}
