use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::errcode::*;
use crate::journal::journal::*;
use crate::journal::read::*;
use crate::journal::write::*;

pub fn bch2_journal_reclaim_start(j: &mut Journal) {
    j.reclaim_started = true;
}

pub fn bch2_journal_reclaim_fast(j: &mut Journal) -> i32 {
    let c = &mut j.fs;
    let mut reclaimed = 0;

    if j.reclaim_started {
        return 0;
    }

    let mut to_remove = Vec::new();
    for (seq, replay) in &c.journal_entries {
        if replay.ignore_blacklisted || replay.ignore_not_dirty {
            to_remove.push(*seq);
        }
    }
    for seq in to_remove {
        c.journal_entries.remove(&seq);
        reclaimed += 1;
    }

    reclaimed
}

pub fn bch2_journal_reclaim(j: &mut Journal) -> Result<(), BchError> {
    let _c = &j.fs;
    let mut _freed = 0;

    if j.reclaim_started {
        return Ok(());
    }

    let mut to_remove = Vec::new();
    for i in 0..j.devs_written_nr as usize {
        let dev_idx = j.devs_written[i] as usize;
        if dev_idx >= j.devices.len() {
            continue;
        }
        let ja = &mut j.devices[dev_idx];
        if ja.nr == 0 {
            continue;
        }

        for _ in 0..ja.nr {
            let cur_idx = ja.cur_idx as usize;
            let reclaim_idx = (cur_idx + 1) % ja.nr as usize;

            if ja.bucket_seq[reclaim_idx] == 0 {
                break;
            }

            if ja.bucket_seq[reclaim_idx] <= j.flushed_seq_ondisk {
                if ja.sectors_free < ja.bucket_size {
                    ja.sectors_free += ja.bucket_size;
                }
                ja.bucket_seq[reclaim_idx] = 0;

                to_remove.push(reclaim_idx);
                break;
            }
        }
    }

    Ok(())
}

pub fn bch2_journal_space_available(j: &mut Journal) -> u32 {
    let c = &j.fs;
    let mut available = 0;

    for ja in &j.devices {
        if ja.nr == 0 {
            continue;
        }

        available += ja.sectors_free;
        available += ja.sectors_reserved;
    }

    available
}

pub fn bch2_journal_dev_buckets_available(
    j: &Journal,
    ja: &JournalDevice,
    _from: JournalSpaceFrom,
) -> u32 {
    let mut buckets = 0;

    let cur = ja.cur_idx as usize;
    for i in 0..ja.nr as usize {
        let idx = (cur + i) % ja.nr as usize;
        if idx == cur {
            continue;
        }
        if ja.bucket_seq[idx] == 0 {
            buckets += 1;
        }
    }

    buckets
}

pub fn bch2_journal_update_last_seq(j: &mut Journal) {
    let _c = &j.fs;
    let mut last_seq = j.cur_seq;

    for i in 0..j.devs_written_nr as usize {
        let dev_idx = j.devs_written[i] as usize;
        if dev_idx >= j.devices.len() {
            continue;
        }
        let ja = &mut j.devices[dev_idx];
        if ja.nr == 0 {
            continue;
        }

        for idx in 0..ja.nr as usize {
            let seq = ja.bucket_seq[idx];
            if seq > 0 && seq < last_seq {
                last_seq = seq;
            }
        }
    }

    let old = j.flushed_seq_ondisk;
    if last_seq > old {
        j.flushed_seq_ondisk = last_seq;
    }
}

pub fn journal_clear_pin(j: &mut Journal, seq: u64) {
    if seq > j.flushed_seq_ondisk && seq <= j.cur_seq {
        let offset = (seq - j.cur_seq - 1) as usize;
        if offset < j.pin.len() {
            j.pin[offset] = 0;
        }
    }
}

pub fn journal_set_pin(j: &mut Journal, seq: u64, pin: u64) {
    if seq > j.flushed_seq_ondisk && seq <= j.cur_seq {
        let offset = (seq - j.cur_seq - 1) as usize;
        if offset < j.pin.len() {
            j.pin[offset] = j.pin[offset].max(pin);
        }
    }
}

pub fn journal_last_seq_reclaimable(j: &Journal) -> u64 {
    let mut ret = j.cur_seq;

    for i in 0..j.pin.len() {
        if j.pin[i] > 0 {
            let seq = j.cur_seq - (i + 1) as u64 + 1;
            if seq < ret {
                ret = seq;
            }
        }
    }

    ret
}

pub fn bch2_journal_space_reserved(j: &Journal, reserved: u64) -> bool {
    let available = match j.space_available {
        0 => true,
        sa => sa >= reserved as u32,
    };
    available
}

pub fn journal_buf_must_flush(w: &JournalBuf) -> bool {
    w.flush
}

pub fn journal_buf_must_not_flush(w: &JournalBuf) -> bool {
    w.no_flush
}

pub enum JournalSpaceFrom {
    Discarded,
}
