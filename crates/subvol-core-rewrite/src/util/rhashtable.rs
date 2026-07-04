use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Mutex, RwLock};

use super::bit_spinlock::{bit_spin_lock, bit_spin_unlock};
use super::jhash::{jhash, jhash2};
use super::rcu::{call_rcu, rcu_head, rcu_read_lock, rcu_read_unlock, synchronize_rcu};
use super::workqueue::{cancel_work_sync, schedule_work, work_struct, INIT_WORK};
use crate::btree::types::{list_head, rhash_head};

const HASH_DEFAULT_SIZE: usize = 64;
const HASH_MIN_SIZE: usize = 4;
const RHT_ELASTICITY: usize = 16;

pub type rht_hashfn_t = unsafe fn(*const core::ffi::c_void, u32, u32) -> u32;
pub type rht_obj_hashfn_t = unsafe fn(*const core::ffi::c_void, u32, u32) -> u32;
pub type rht_obj_cmpfn_t =
    unsafe fn(*const rhashtable_compare_arg, *const core::ffi::c_void) -> i32;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct rhashtable_params {
    pub nelem_hint: u16,
    pub key_len: u16,
    pub key_offset: u16,
    pub head_offset: u16,
    pub max_size: u32,
    pub min_size: u16,
    pub automatic_shrinking: bool,
    pub hashfn: Option<rht_hashfn_t>,
    pub obj_hashfn: Option<rht_obj_hashfn_t>,
    pub obj_cmpfn: Option<rht_obj_cmpfn_t>,
}

#[repr(C)]
pub struct rhashtable_compare_arg {
    pub ht: *mut rhashtable,
    pub key: *const core::ffi::c_void,
}

#[repr(C)]
pub struct bucket_table {
    pub size: u32,
    pub nest: u32,
    pub hash_rnd: u32,
    pub walkers: list_head,
    pub rcu: rcu_head,
    pub future_tbl: AtomicPtr<bucket_table>,
    pub buckets: Box<[AtomicUsize]>,
}

#[repr(C)]
pub struct rhashtable {
    pub tbl: AtomicPtr<bucket_table>,
    pub key_len: u32,
    pub max_elems: u32,
    pub p: rhashtable_params,
    pub rhlist: bool,
    pub mutex: Mutex<()>,
    pub lock: Mutex<()>,
    pub nelems: AtomicUsize,
    pub run_work: work_struct,
    gate: RwLock<()>,
}

unsafe impl Send for rhashtable {}
unsafe impl Sync for rhashtable {}

impl Default for rhashtable {
    fn default() -> Self {
        Self {
            tbl: AtomicPtr::new(core::ptr::null_mut()),
            key_len: 0,
            max_elems: 0,
            p: rhashtable_params {
                nelem_hint: 0,
                key_len: 0,
                key_offset: 0,
                head_offset: 0,
                max_size: 0,
                min_size: 0,
                automatic_shrinking: false,
                hashfn: None,
                obj_hashfn: None,
                obj_cmpfn: None,
            },
            rhlist: false,
            mutex: Mutex::new(()),
            lock: Mutex::new(()),
            nelems: AtomicUsize::new(0),
            run_work: work_struct::default(),
            gate: RwLock::new(()),
        }
    }
}

fn next_hash_seed() -> u32 {
    static SEED: AtomicU32 = AtomicU32::new(0x9e37_79b9);
    let mut old = SEED.load(Ordering::Relaxed);
    loop {
        let mut next = old ^ old.wrapping_shl(13);
        next ^= next.wrapping_shr(17);
        next ^= next.wrapping_shl(5);
        match SEED.compare_exchange_weak(old, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(actual) => old = actual,
        }
    }
}

unsafe fn rhashtable_jhash2(key: *const core::ffi::c_void, length: u32, seed: u32) -> u32 {
    jhash2(key.cast(), length, seed)
}

fn roundup_pow2(mut value: usize) -> usize {
    if value <= 1 {
        return 1;
    }
    value -= 1;
    value |= value >> 1;
    value |= value >> 2;
    value |= value >> 4;
    value |= value >> 8;
    value |= value >> 16;
    if usize::BITS > 32 {
        value |= value >> 32;
    }
    value + 1
}

fn rounded_hashtable_size(params: &rhashtable_params) -> usize {
    let hinted = if params.nelem_hint != 0 {
        (params.nelem_hint as usize * 4) / 3
    } else {
        HASH_DEFAULT_SIZE
    };
    roundup_pow2(hinted.max(params.min_size as usize).max(HASH_MIN_SIZE))
}

fn bucket_table_alloc(size: usize) -> Box<bucket_table> {
    let buckets = (0..size)
        .map(|_| AtomicUsize::new(0))
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let mut table = Box::new(bucket_table {
        size: size as u32,
        nest: 0,
        hash_rnd: next_hash_seed(),
        walkers: list_head::default(),
        rcu: rcu_head::default(),
        future_tbl: AtomicPtr::new(core::ptr::null_mut()),
        buckets,
    });
    unsafe {
        crate::btree::types::INIT_LIST_HEAD(&mut table.walkers);
        crate::util::rcu::rcu_head_init(&mut table.rcu);
    }
    table
}

pub unsafe fn rhashtable_init(ht: *mut rhashtable, params: *const rhashtable_params) -> i32 {
    if ht.is_null() || params.is_null() {
        return -22;
    }
    if ((*params).key_len == 0 && (*params).obj_hashfn.is_none())
        || ((*params).obj_hashfn.is_some() && (*params).obj_cmpfn.is_none())
    {
        return -22;
    }

    (*ht).p = *params;
    (*ht).p.min_size = roundup_pow2((*params).min_size as usize).max(HASH_MIN_SIZE) as u16;
    (*ht).max_elems = 1u32 << 31;
    if (*params).max_size != 0 {
        let max_size = (*params).max_size.next_power_of_two() >> 1;
        (*ht).p.max_size = max_size as u32;
        if max_size < (*ht).max_elems / 2 {
            (*ht).max_elems = max_size * 2;
        }
    }
    (*ht).key_len = (*ht).p.key_len as u32;
    if (*params).hashfn.is_none() {
        if (*ht).key_len & 3 == 0 {
            (*ht).key_len /= 4;
            (*ht).p.hashfn = Some(rhashtable_jhash2);
        } else {
            (*ht).p.hashfn = Some(jhash);
        }
    }
    let table = Box::into_raw(bucket_table_alloc(rounded_hashtable_size(&(*ht).p)));
    (*ht).tbl.store(table, Ordering::Release);
    (*ht).nelems.store(0, Ordering::Release);
    INIT_WORK(&mut (*ht).run_work, rht_deferred_worker);
    0
}

unsafe fn rht_obj(ht: *const rhashtable, head: *const rhash_head) -> *const core::ffi::c_void {
    (head.cast::<u8>().sub((*ht).p.head_offset as usize)).cast()
}

unsafe fn rht_obj_mut(ht: *const rhashtable, head: *mut rhash_head) -> *mut core::ffi::c_void {
    (head.cast::<u8>().sub((*ht).p.head_offset as usize)).cast()
}

unsafe fn rht_key_hashfn(
    ht: *mut rhashtable,
    table: *const bucket_table,
    key: *const core::ffi::c_void,
) -> usize {
    let hash = ((*ht).p.hashfn.unwrap())(key, (*ht).key_len, (*table).hash_rnd);
    (hash as usize) & ((*table).size as usize - 1)
}

unsafe fn rht_head_hashfn(
    ht: *mut rhashtable,
    table: *const bucket_table,
    head: *const rhash_head,
) -> usize {
    let object = rht_obj(ht, head);
    let hash = if let Some(hashfn) = (*ht).p.obj_hashfn {
        hashfn(object, (*ht).p.key_len as u32, (*table).hash_rnd)
    } else {
        let key = object.cast::<u8>().add((*ht).p.key_offset as usize).cast();
        ((*ht).p.hashfn.unwrap())(key, (*ht).key_len, (*table).hash_rnd)
    };
    (hash as usize) & ((*table).size as usize - 1)
}

unsafe fn bucket_head(table: *const bucket_table, index: usize) -> *mut rhash_head {
    ((*table).buckets[index].load(Ordering::Acquire) & !1usize) as *mut rhash_head
}

unsafe fn objects_equal(
    ht: *mut rhashtable,
    key: *const core::ffi::c_void,
    object: *const core::ffi::c_void,
) -> bool {
    let arg = rhashtable_compare_arg { ht, key };
    if let Some(compare) = (*ht).p.obj_cmpfn {
        compare(&arg, object) == 0
    } else {
        core::slice::from_raw_parts(key.cast::<u8>(), (*ht).p.key_len as usize)
            == core::slice::from_raw_parts(
                object.cast::<u8>().add((*ht).p.key_offset as usize),
                (*ht).p.key_len as usize,
            )
    }
}

pub unsafe fn rhashtable_lookup_fast(
    ht: *mut rhashtable,
    key: *const core::ffi::c_void,
) -> *mut core::ffi::c_void {
    let _gate = (*ht).gate.read().unwrap();
    rcu_read_lock();
    let table = (*ht).tbl.load(Ordering::Acquire);
    let index = rht_key_hashfn(ht, table, key);
    let mut head = bucket_head(table, index);
    let mut steps = 0;
    while !head.is_null() && steps < RHT_ELASTICITY {
        if objects_equal(ht, key, rht_obj(ht, head)) {
            let object = rht_obj_mut(ht, head);
            rcu_read_unlock();
            drop(_gate);
            return object;
        }
        head = (*head).next;
        steps += 1;
    }
    rcu_read_unlock();
    core::ptr::null_mut()
}

unsafe fn rehash_locked(ht: *mut rhashtable, new_size: usize) {
    let old = (*ht).tbl.load(Ordering::Acquire);
    if (*old).size as usize == new_size {
        return;
    }
    synchronize_rcu();
    let new = Box::into_raw(bucket_table_alloc(new_size));
    for index in 0..(*old).size as usize {
        let mut head = bucket_head(old, index);
        while !head.is_null() {
            let next = (*head).next;
            let new_index = rht_head_hashfn(ht, new, head);
            (*head).next = bucket_head(new, new_index);
            (*new).buckets[new_index].store(head as usize, Ordering::Release);
            head = next;
        }
    }
    (*ht).tbl.store(new, Ordering::Release);
    let old_head = &mut (*old).rcu as *mut rcu_head;
    call_rcu(old_head, bucket_table_free_rcu);
}

unsafe extern "C" fn bucket_table_free_rcu(head: *mut rcu_head) {
    let offset = core::mem::offset_of!(bucket_table, rcu);
    let table = head.cast::<u8>().sub(offset).cast::<bucket_table>();
    drop(Box::from_raw(table));
}

unsafe fn rht_deferred_worker(work: *mut work_struct) {
    let offset = core::mem::offset_of!(rhashtable, run_work);
    let ht = work.cast::<u8>().sub(offset).cast::<rhashtable>();
    let _gate = (*ht).gate.write().unwrap();
    let _mutex = (*ht).mutex.lock().unwrap();
    let table = (*ht).tbl.load(Ordering::Acquire);
    if table.is_null() {
        return;
    }
    let size = (*table).size as usize;
    let count = (*ht).nelems.load(Ordering::Acquire);
    if count > size * 3 / 4 {
        let max_size = if (*ht).p.max_size == 0 {
            usize::MAX
        } else {
            (*ht).p.max_size as usize
        };
        let next = (size * 2).min(max_size);
        if next > size {
            rehash_locked(ht, next);
        }
    } else if (*ht).p.automatic_shrinking
        && size > (*ht).p.min_size as usize
        && count < size * 3 / 10
    {
        rehash_locked(ht, (size / 2).max((*ht).p.min_size as usize));
    }
}

pub unsafe fn rhashtable_lookup_insert_fast(ht: *mut rhashtable, obj: *mut rhash_head) -> i32 {
    let _gate = (*ht).gate.write().unwrap();
    let _mutex = (*ht).mutex.lock().unwrap();
    let table = (*ht).tbl.load(Ordering::Acquire);
    let index = rht_head_hashfn(ht, table, obj);
    let bucket = &(*table).buckets[index];
    bit_spin_lock(0, bucket);
    let key = rht_obj(ht, obj);
    let mut head = bucket_head(table, index);
    let mut steps = 0;
    while !head.is_null() && steps < RHT_ELASTICITY {
        if objects_equal(
            ht,
            key.cast::<u8>().add((*ht).p.key_offset as usize).cast(),
            rht_obj(ht, head),
        ) {
            bit_spin_unlock(0, bucket);
            return -17;
        }
        head = (*head).next;
        steps += 1;
    }
    if !head.is_null() {
        bit_spin_unlock(0, bucket);
        schedule_work(&mut (*ht).run_work);
        return -7;
    }
    (*obj).next = bucket_head(table, index);
    bucket.store(obj as usize, Ordering::Release);
    bit_spin_unlock(0, bucket);
    let count = (*ht).nelems.fetch_add(1, Ordering::AcqRel) + 1;
    if count > ((*table).size as usize * 3) / 4 {
        let max_size = if (*ht).p.max_size == 0 {
            usize::MAX
        } else {
            (*ht).p.max_size as usize
        };
        let next = ((*table).size as usize * 2).min(max_size);
        if next > (*table).size as usize {
            schedule_work(&mut (*ht).run_work);
        }
    }
    0
}

pub unsafe fn rhashtable_insert_fast(ht: *mut rhashtable, obj: *mut rhash_head) -> i32 {
    rhashtable_lookup_insert_fast(ht, obj)
}

pub unsafe fn rhashtable_remove_fast(ht: *mut rhashtable, obj: *mut rhash_head) -> i32 {
    let _gate = (*ht).gate.write().unwrap();
    let _mutex = (*ht).mutex.lock().unwrap();
    let table = (*ht).tbl.load(Ordering::Acquire);
    let index = rht_head_hashfn(ht, table, obj);
    let bucket = &(*table).buckets[index];
    bit_spin_lock(0, bucket);
    let mut previous: *mut rhash_head = core::ptr::null_mut();
    let mut head = bucket_head(table, index);
    while !head.is_null() {
        if head == obj {
            if previous.is_null() {
                bucket.store((*head).next as usize, Ordering::Release);
            } else {
                (*previous).next = (*head).next;
            }
            (*head).next = core::ptr::null_mut();
            bit_spin_unlock(0, bucket);
            let count = (*ht).nelems.fetch_sub(1, Ordering::AcqRel) - 1;
            if (*ht).p.automatic_shrinking
                && (*table).size as usize > (*ht).p.min_size as usize
                && count < ((*table).size as usize * 3) / 10
            {
                schedule_work(&mut (*ht).run_work);
            }
            return 0;
        }
        previous = head;
        head = (*head).next;
    }
    bit_spin_unlock(0, bucket);
    -2
}

pub unsafe fn rhashtable_destroy(ht: *mut rhashtable) {
    cancel_work_sync(&mut (*ht).run_work);
    let _gate = (*ht).gate.write().unwrap();
    synchronize_rcu();
    let table = (*ht).tbl.swap(core::ptr::null_mut(), Ordering::AcqRel);
    if !table.is_null() {
        drop(Box::from_raw(table));
    }
    (*ht).nelems.store(0, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(C)]
    struct object {
        key: u64,
        hash: rhash_head,
        value: u64,
    }

    #[test]
    fn fixed_key_lookup_insert_remove_and_resize_preserve_objects() {
        unsafe {
            let params = rhashtable_params {
                nelem_hint: 3,
                key_len: 8,
                key_offset: 0,
                head_offset: core::mem::offset_of!(object, hash) as u16,
                max_size: 0,
                min_size: 4,
                automatic_shrinking: true,
                hashfn: None,
                obj_hashfn: None,
                obj_cmpfn: None,
            };
            let mut table = rhashtable::default();
            assert_eq!(rhashtable_init(&mut table, &params), 0);
            let mut objects = (0..8)
                .map(|i| {
                    Box::new(object {
                        key: i,
                        hash: rhash_head::default(),
                        value: i * 3,
                    })
                })
                .collect::<Vec<_>>();
            for object in &mut objects {
                assert_eq!(
                    rhashtable_lookup_insert_fast(&mut table, &mut object.hash),
                    0
                );
            }
            assert_eq!(table.nelems.load(Ordering::Acquire), 8);
            let _ = super::super::workqueue::flush_work(&mut table.run_work);
            assert!((*table.tbl.load(Ordering::Acquire)).size >= 8);
            for object in &objects {
                let found =
                    rhashtable_lookup_fast(&mut table, &object.key as *const u64 as *const _)
                        .cast::<object>();
                assert_eq!(found, &**object as *const object as *mut object);
                assert_eq!((*found).value, object.key * 3);
            }
            assert_eq!(
                rhashtable_lookup_insert_fast(&mut table, &mut objects[0].hash),
                -17
            );
            assert_eq!(rhashtable_remove_fast(&mut table, &mut objects[3].hash), 0);
            let _ = super::super::workqueue::flush_work(&mut table.run_work);
            assert!(
                rhashtable_lookup_fast(&mut table, &objects[3].key as *const u64 as *const _)
                    .is_null()
            );
            rhashtable_destroy(&mut table);
        }
    }
}
