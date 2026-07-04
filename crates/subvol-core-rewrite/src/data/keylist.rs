use crate::btree::bkey::{bkey_copy, bkey_i, bkey_next};

#[repr(C)]
#[derive(Clone, Copy)]
pub union keylist_start {
    pub keys: *mut bkey_i,
    pub keys_p: *mut u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union keylist_top {
    pub top: *mut bkey_i,
    pub top_p: *mut u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct keylist {
    pub start: keylist_start,
    pub end: keylist_top,
}

impl Default for keylist {
    fn default() -> Self {
        Self {
            start: keylist_start {
                keys_p: core::ptr::null_mut(),
            },
            end: keylist_top {
                top_p: core::ptr::null_mut(),
            },
        }
    }
}

pub unsafe fn bch2_keylist_init(list: *mut keylist, inline_keys: *mut u64) {
    (*list).start.keys_p = inline_keys;
    (*list).end.top_p = inline_keys;
}

pub unsafe fn bch2_keylist_push(list: *mut keylist) {
    (*list).end.top = bkey_next((*list).end.top);
}

pub unsafe fn bch2_keylist_add(list: *mut keylist, key: *const bkey_i) {
    bkey_copy((*list).end.top, key);
    bch2_keylist_push(list);
}

pub unsafe fn bch2_keylist_empty(list: *const keylist) -> bool {
    (*list).end.top == (*list).start.keys
}

pub unsafe fn bch2_keylist_u64s(list: *const keylist) -> usize {
    (*list).end.top_p.offset_from((*list).start.keys_p) as usize
}

pub unsafe fn bch2_keylist_bytes(list: *const keylist) -> usize {
    bch2_keylist_u64s(list) * core::mem::size_of::<u64>()
}

pub unsafe fn bch2_keylist_front(list: *mut keylist) -> *mut bkey_i {
    (*list).start.keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::bkey::{bkey, BKEY_U64S, KEY_FORMAT_CURRENT, SPOS};

    #[test]
    fn keylist_layout_and_inline_operations_match_local_source() {
        unsafe {
            assert_eq!(core::mem::size_of::<keylist>(), 16);
            let mut words = [0u64; 10];
            let mut list = keylist::default();
            bch2_keylist_init(&mut list, words.as_mut_ptr());
            assert!(bch2_keylist_empty(&list));

            let key = bkey_i {
                k: bkey {
                    u64s: BKEY_U64S,
                    format: KEY_FORMAT_CURRENT,
                    type_: 2,
                    p: SPOS(1, 7, 0),
                    ..Default::default()
                },
                ..Default::default()
            };
            bch2_keylist_add(&mut list, &key);
            assert!(!bch2_keylist_empty(&list));
            assert_eq!(bch2_keylist_u64s(&list), BKEY_U64S as usize);
            assert_eq!(bch2_keylist_bytes(&list), BKEY_U64S as usize * 8);
            assert_eq!((*bch2_keylist_front(&mut list)).k.p, SPOS(1, 7, 0));
        }
    }
}
