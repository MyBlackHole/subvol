use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::errcode::*;
use crate::journal::journal::*;
use crate::journal::validate::*;
use crate::journal::seq_blacklist::*;

pub const JOURNAL_ENTRY_ADD_OK: i32 = 0;
pub const JOURNAL_ENTRY_ADD_OUT_OF_RANGE: i32 = 5;

pub struct JournalReadBuf {
    pub data: Vec<u8>,
    pub size: usize,
}

pub struct JournalList {
    pub last_seq: u64,
    pub ret: i32,
    pub full_read: bool,
}

pub fn journal_nonce(jset: &Jset) -> Nonce {
    Nonce {
        d: [
            0,
            (jset.seq & 0xffffffff) as u32,
            ((jset.seq >> 32) & 0xffffffff) as u32,
            6, // BCH_NONCE_JOURNAL
        ],
    }
}

pub fn jset_datetime(j: &Jset) -> u64 {
    for entry in &j.entries {
        if entry.type_ == BchJsetEntryType::Datetime {
            return entry._data().get(0).copied().unwrap_or(0);
        }
    }
    0
}

pub fn journal_replay_ignore(i: &Option<JournalReplay>) -> bool {
    match i {
        None => true,
        Some(r) => r.ignore_blacklisted || r.ignore_not_dirty,
    }
}

pub fn jset_csum_good(c: &BchFs, j: &Jset, csum: &mut BchCsum) -> bool {
    if !bch2_checksum_type_valid(c, JSET_CSUM_TYPE(j)) {
        *csum = BchCsum { lo: 0, hi: 0 };
        return false;
    }
    *csum = csum_vstruct(c, JSET_CSUM_TYPE(j), journal_nonce(j), j);
    j.csum == *csum
}

pub fn journal_read_buf_realloc(c: &BchFs, b: &mut JournalReadBuf, new_size: usize) -> Result<(), BchError> {
    let new_size = new_size.next_power_of_two();
    b.data.resize(new_size, 0);
    b.size = new_size;
    Ok(())
}

pub fn journal_entry_add(
    c: &mut BchFs,
    ca: &BchDev,
    entry_ptr: JournalPtr,
    jlist: &mut JournalList,
    j: &Jset,
) -> Result<i32, BchError> {
    let last_seq = if !JSET_NO_FLUSH(j) { j.last_seq } else { 0 };
    let seq = j.seq;

    if !c.journal.oldest_seq_found_ondisk || seq < c.journal.oldest_seq_found_ondisk {
        c.journal.oldest_seq_found_ondisk = seq;
    }

    if !c.opts.read_entire_journal && seq < jlist.last_seq {
        return Ok(JOURNAL_ENTRY_ADD_OUT_OF_RANGE);
    }

    if last_seq > jlist.last_seq && !c.opts.read_entire_journal {
        let mut to_remove = Vec::new();
        for (s, i) in &c.journal_entries {
            if journal_replay_ignore(&Some(i.clone())) {
                continue;
            }
            if i.j.seq >= last_seq {
                break;
            }
            to_remove.push(s);
        }
        for s in to_remove {
            c.journal_entries.remove(&s);
        }
    }

    jlist.last_seq = jlist.last_seq.max(last_seq);

    if let Some(dup) = c.journal_entries.get(&seq) {
        let identical = dup.j == *j;
        let not_identical = !identical && entry_ptr.csum_good && dup.csum_good;

        if identical || !entry_ptr.csum_good {
            return Ok(JOURNAL_ENTRY_ADD_OK);
        }

        return Ok(JOURNAL_ENTRY_ADD_OK);
    }

    let i = JournalReplay {
        j: j.clone(),
        ptrs: vec![entry_ptr],
        csum_good: entry_ptr.csum_good,
        ignore_blacklisted: false,
        ignore_not_dirty: false,
    };

    c.journal_entries.insert(seq, i);
    Ok(JOURNAL_ENTRY_ADD_OK)
}

pub fn journal_read_bucket(
    ca: &mut BchDev,
    buf: &mut JournalReadBuf,
    jlist: &mut JournalList,
    bucket: u32,
) -> Result<(), BchError> {
    let c = &mut ca.fs;
    let ja = &mut ca.journal;
    let offset = bucket_to_sector(ca, ja.buckets[bucket as usize]);
    let end = offset + ca.mi.bucket_size as u64;

    let mut saw_bad = false;

    let mut j_offset = offset;
    while j_offset < end {
        let j = JournalReadBuf::parse_jset(&buf.data, (j_offset - offset) as usize);
        let j = match j {
            Some(j) => j,
            None => {
                if !saw_bad {
                    return Ok(());
                }
                j_offset += c.block_sectors() as u64;
                continue;
            }
        };

        let sectors = vstruct_sectors(&j, c.block_bits);
        let idx = bucket as usize;

        if j.seq > ja.highest_seq_found {
            ja.highest_seq_found = j.seq;
            ja.cur_idx = bucket;
            ja.sectors_free = (end - j_offset - sectors as u64) as u32;
        }

        if j.seq < ja.bucket_seq[idx] {
            return Ok(());
        }

        ja.bucket_seq[idx] = j.seq;

        let mut csum = BchCsum { lo: 0, hi: 0 };
        let csum_good = jset_csum_good(c, &j, &mut csum);

        if !csum_good {
            saw_bad = true;
        }

        let ptr = JournalPtr {
            csum_good,
            csum,
            dev: ca.dev_idx,
            bucket,
            bucket_offset: (j_offset - bucket_to_sector(ca, ja.buckets[idx])) as u32,
            sector: j_offset,
        };

        let ret = journal_entry_add(c, ca, ptr, jlist, &j)?;

        j_offset += sectors as u64;
    }

    Ok(())
}

pub fn journal_peek_bucket(
    ca: &mut BchDev,
    buf: &JournalReadBuf,
    bucket: u32,
) -> Result<(), BchError> {
    let c = &ca.fs;
    let ja = &mut ca.journal;
    let offset = bucket_to_sector(ca, ja.buckets[bucket as usize]);

    if buf.data.len() < c.block_bytes() as usize {
        return Ok(());
    }

    let magic = u64::from_le_bytes(buf.data[0..8].try_into().unwrap());
    if magic != jset_magic(c) {
        return Ok(());
    }

    if buf.data.len() >= 24 {
        let seq = u64::from_le_bytes(buf.data[16..24].try_into().unwrap());
        ja.bucket_seq[bucket as usize] = seq;
    }

    Ok(())
}

pub fn journal_bsearch_collect(
    ca: &mut BchDev,
    buf: &JournalReadBuf,
    order: &mut Vec<JournalBucketEntry>,
) -> Result<(), BchError> {
    let ja = &ca.journal;
    let mut peeked = vec![false; ja.nr as usize];

    let anchor = journal_anchor_bucket(ca, buf, &mut peeked)?;
    if anchor < 0 {
        return Ok(());
    }

    let head = journal_bsearch_head(ca, buf, &mut peeked, anchor as usize)?;
    journal_walk_inuse(ca, buf, &mut peeked, head as usize, order)
}

pub fn journal_anchor_bucket(
    ca: &mut BchDev,
    buf: &JournalReadBuf,
    peeked: &mut Vec<bool>,
) -> Result<i32, BchError> {
    let ja = &ca.journal;

    let s = journal_peek_once(ca, buf, peeked, 0)?;
    if s > 0 {
        return Ok(0);
    }

    if ja.nr <= 1 {
        return Ok(-1);
    }

    let mut step = 1 << (31 - (ja.nr - 1).leading_zeros());
    while step > 0 {
        let mut pos = step;
        while pos < ja.nr {
            let s = journal_peek_once(ca, buf, peeked, pos)?;
            if s > 0 {
                return Ok(pos as i32);
            }
            pos += step * 2;
        }
        step >>= 1;
    }

    Ok(-1)
}

pub fn journal_bsearch_head(
    ca: &mut BchDev,
    buf: &JournalReadBuf,
    peeked: &mut Vec<bool>,
    anchor: usize,
) -> Result<usize, BchError> {
    let ja = &ca.journal;
    let mut lo = anchor;
    let mut hi = anchor + ja.nr as usize - 1;

    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        let mid_b = mid % ja.nr as usize;
        let lo_b = lo % ja.nr as usize;

        let s_mid = journal_peek_once(ca, buf, peeked, mid_b)?;
        let s_lo = ja.bucket_seq[lo_b];

        if s_mid == 0 {
            hi = mid - 1;
        } else if s_mid as u64 > s_lo {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }

    Ok(lo % ja.nr as usize)
}

pub fn journal_walk_inuse(
    ca: &mut BchDev,
    buf: &JournalReadBuf,
    peeked: &mut Vec<bool>,
    head: usize,
    order: &mut Vec<JournalBucketEntry>,
) -> Result<(), BchError> {
    let ja = &ca.journal;
    let prev_seq = ja.bucket_seq[head];
    if prev_seq == 0 {
        return Ok(());
    }

    order.push(JournalBucketEntry { bucket: head as u32, seq: prev_seq });

    for k in 1..ja.nr as usize {
        let idx = (head + ja.nr as usize - k) % ja.nr as usize;

        let s = journal_peek_once(ca, buf, peeked, idx)?;
        if s == 0 {
            break;
        }
        if s as u64 >= prev_seq {
            break;
        }

        order.push(JournalBucketEntry { bucket: idx as u32, seq: s as u64 });
    }

    Ok(())
}

pub fn journal_peek_once(
    ca: &mut BchDev,
    buf: &JournalReadBuf,
    peeked: &mut Vec<bool>,
    bucket: u32,
) -> Result<u64, BchError> {
    let ja = &ca.journal;
    if !peeked[bucket as usize] {
        journal_peek_bucket(ca, buf, bucket)?;
        peeked[bucket as usize] = true;
    }
    Ok(ja.bucket_seq[bucket as usize])
}

pub fn journal_bucket_entry_cmp(a: &JournalBucketEntry, b: &JournalBucketEntry) -> std::cmp::Ordering {
    b.seq.cmp(&a.seq)
}

#[derive(Clone, Debug)]
pub struct JournalReplay {
    pub j: Jset,
    pub ptrs: Vec<JournalPtr>,
    pub csum_good: bool,
    pub ignore_blacklisted: bool,
    pub ignore_not_dirty: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct JournalPtr {
    pub csum_good: bool,
    pub csum: BchCsum,
    pub dev: u8,
    pub bucket: u32,
    pub bucket_offset: u32,
    pub sector: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct JournalBucketEntry {
    pub bucket: u32,
    pub seq: u64,
}

pub struct U64Range {
    pub start: u64,
    pub end: u64,
}

pub fn bch2_journal_entry_missing_range(c: &BchFs, start: u64, end: u64) -> U64Range {
    if start >= end {
        return U64Range { start: 0, end: 0 };
    }

    let start = bch2_journal_seq_next_nonblacklisted(c, start);
    if start >= end {
        return U64Range { start: 0, end: 0 };
    }

    let missing = U64Range {
        start,
        end: end.min(bch2_journal_seq_next_blacklisted(c, start)),
    };

    if missing.start == missing.end {
        return U64Range { start: 0, end: 0 };
    }

    missing
}

impl JournalReadBuf {
    pub fn parse_jset(data: &[u8], offset: usize) -> Option<Jset> {
        if offset + 40 > data.len() {
            return None;
        }

        let magic = u64::from_le_bytes(data[offset..offset + 8].try_into().ok()?);
        if magic == 0 {
            return None;
        }

        Some(Jset {
            csum: BchCsum {
                lo: u64::from_le_bytes(data[offset + 8..offset + 16].try_into().ok()?),
                hi: u64::from_le_bytes(data[offset + 16..offset + 24].try_into().ok()?),
            },
            magic,
            seq: u64::from_le_bytes(data[offset + 24..offset + 32].try_into().ok()?),
            version: u32::from_le_bytes(data[offset + 32..offset + 36].try_into().ok()?),
            flags: u32::from_le_bytes(data[offset + 36..offset + 40].try_into().ok()?),
            u64s: 0,
            last_seq: 0,
            entries: Vec::new(),
        })
    }
}

pub fn vstruct_sectors(j: &Jset, block_bits: u8) -> u32 {
    let bytes = j.entries.len() * std::mem::size_of::<JsetEntry>() + 40;
    ((bytes + (1 << block_bits) - 1) >> block_bits) as u32
}

pub fn jset_magic(c: &BchFs) -> u64 {
    0x245235c1a3625032u64
}

pub fn JSET_NO_FLUSH(j: &Jset) -> bool {
    j.flags & 1 != 0
}

pub fn JSET_CSUM_TYPE(j: &Jset) -> BchCsumType {
    let csum_type_val = ((j.flags >> 1) & 7) as u8;
    match csum_type_val {
        0 => BchCsumType::None,
        1 => BchCsumType::Crc32cNonzero,
        2 => BchCsumType::Crc64Nonzero,
        3 => BchCsumType::Chacha20Poly1305_80,
        4 => BchCsumType::Chacha20Poly1305_128,
        5 => BchCsumType::Crc32c,
        6 => BchCsumType::Crc64,
        _ => BchCsumType::Xxhash,
    }
}

pub fn bch2_checksum_type_valid(_c: &BchFs, _type: BchCsumType) -> bool {
    true
}

pub fn csum_vstruct(_c: &BchFs, _type: BchCsumType, _nonce: Nonce, _j: &Jset) -> BchCsum {
    BchCsum { lo: 0, hi: 0 }
}

pub fn bucket_to_sector(ca: &BchDev, b: u64) -> u64 {
    b * ca.mi.bucket_size as u64
}

pub fn JSET_BIG_ENDIAN(_j: &Jset) -> i32 {
    0
}
