use crate::c;
use std::marker::PhantomData;

pub trait Darray {
    type Item;
    fn data(&self) -> *mut Self::Item;
    fn len(&self) -> usize;
}

pub struct DarrayVec<T> {
    data: *mut T,
    len: usize,
    capacity: usize,
    _marker: PhantomData<T>,
}

impl<T> DarrayVec<T> {
    pub fn new() -> Self {
        DarrayVec {
            data: core::ptr::null_mut(),
            len: 0,
            capacity: 0,
            _marker: PhantomData,
        }
    }

    pub fn with_capacity(cap: usize) -> Self {
        let size = cap * core::mem::size_of::<T>();
        let ptr = unsafe { libc::malloc(size) as *mut T };
        DarrayVec {
            data: ptr,
            len: 0,
            capacity: cap,
            _marker: PhantomData,
        }
    }

    pub fn push(&mut self, val: T) {
        if self.len >= self.capacity {
            let new_cap = if self.capacity == 0 { 4 } else { self.capacity * 2 };
            let new_size = new_cap * core::mem::size_of::<T>();
            let new_ptr = unsafe { libc::realloc(self.data as *mut _, new_size) as *mut T };
            self.data = new_ptr;
            self.capacity = new_cap;
        }
        unsafe {
            core::ptr::write(self.data.add(self.len), val);
        }
        self.len += 1;
    }

    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            None
        } else {
            self.len -= 1;
            unsafe { Some(core::ptr::read(self.data.add(self.len))) }
        }
    }

    pub fn as_slice(&self) -> &[T] {
        if self.data.is_null() {
            &[]
        } else {
            unsafe { core::slice::from_raw_parts(self.data, self.len) }
        }
    }

    pub fn as_mut_slice(&mut self) -> &mut [T] {
        if self.data.is_null() {
            &mut []
        } else {
            unsafe { core::slice::from_raw_parts_mut(self.data, self.len) }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

impl<T> Darray for DarrayVec<T> {
    type Item = T;
    fn data(&self) -> *mut T {
        self.data
    }
    fn len(&self) -> usize {
        self.len
    }
}

impl<T> Drop for DarrayVec<T> {
    fn drop(&mut self) {
        if !self.data.is_null() {
            unsafe {
                for i in 0..self.len {
                    core::ptr::drop_in_place(self.data.add(i));
                }
                libc::free(self.data as *mut _);
            }
        }
    }
}

pub fn darray_from_c<T>(ptr: *mut T, len: usize, cap: usize) -> DarrayVec<T> {
    DarrayVec {
        data: ptr,
        len,
        capacity: cap,
        _marker: PhantomData,
    }
}
