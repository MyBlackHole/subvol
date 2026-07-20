use crate::bcachefs::*;
use crate::bcachefs_format::*;
use crate::errcode::*;
use crate::journal::journal::*;
use crate::journal::read::*;

pub fn bch2_journal_entry_validate(
    _ca: &BchFs,
    entry: &JsetEntry,
    _j: &Jset,
    _nonce: Nonce,
) -> Result<i32, BchError> {
    let type_ = &entry.type_;

    if entry.u64s == 0 {
        return Err(BchError::from_raw(-1));
    }

    match type_ {
        BchJsetEntryType::BtreeKeys => {
            if entry.level != 0 && entry.level != 1 {
                return Err(BchError::from_raw(-1));
            }
        }
        BchJsetEntryType::Datetime => {
            if entry.u64s != 1 {
                return Err(BchError::from_raw(-1));
            }
        }
        BchJsetEntryType::Blacklist => {
            if entry.u64s != 1 {
                return Err(BchError::from_raw(-1));
            }
        }
        BchJsetEntryType::BlacklistFront => {
            if entry.u64s != 0 {
                return Err(BchError::from_raw(-1));
            }
        }
        _ => {
            return Err(BchError::from_raw(-1));
        }
    }

    Ok(0)
}

pub fn bch2_jset_validate(
    c: &BchFs,
    j: &Jset,
    _dev: u8,
    _nonce: Nonce,
) -> Result<i32, BchError> {
    if j.version > 0 && j.version < 11 {
        return Err(BchError::from_raw(-1));
    }

    if j.flags & !((1 | (7 << 1) | (1 << 4)) as u32) != 0 {
        return Err(BchError::from_raw(-1));
    }

    let csum_type = JSET_CSUM_TYPE(j);
    if !bch2_checksum_type_valid(c, csum_type) {
        return Err(BchError::from_raw(-1));
    }

    if !JSET_NO_FLUSH(j) && j.last_seq > j.seq {
        return Err(BchError::from_raw(-1));
    }

    Ok(0)
}

pub fn bch2_journal_validate(
    _c: &BchFs,
    _j: &Jset,
    _dev: u8,
    _nonce: Nonce,
    _ret: &mut i32,
) -> Result<i32, BchError> {
    Ok(0)
}

pub fn bch2_journal_entry_jset_validate(
    c: &BchFs,
    j: &Jset,
    dev: u8,
    nonce: Nonce,
    ret: &mut i32,
) -> Result<i32, BchError> {
    bch2_jset_validate(c, j, dev, nonce)?;
    bch2_journal_validate(c, j, dev, nonce, ret)
}

pub fn bch2_journal_entry_csum_type(
    _j: &Jset,
    _entry: &mut JsetEntry,
    _csum_type: BchCsumType,
    _nonce: Nonce,
) {
}

pub fn bch2_journal_entry_csum(
    _j: &Jset,
    _entry: &mut JsetEntry,
    _nonce: Nonce,
) -> u64 {
    0
}

pub fn bch2_journal_entry_check_csum(
    _c: &BchFs,
    _j: &Jset,
    _entry: &JsetEntry,
    _nonce: Nonce,
) -> bool {
    true
}

pub fn bch2_journal_meta_validate(
    _c: &BchFs,
    _data: &[u8],
    _nonce: Nonce,
) -> Result<i32, BchError> {
    Ok(0)
}
