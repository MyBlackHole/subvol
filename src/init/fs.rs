use crate::c;
use crate::errcode::BchResult;
use std::sync::Mutex;

pub struct Fs {
    pub inner: *mut c::bch_fs,
}

unsafe impl Send for Fs {}
unsafe impl Sync for Fs {}

impl Fs {
    pub fn new() -> BchResult<Self> {
        let inner = unsafe { c::bch2_fs_alloc() };
        if inner.is_null() {
            return Err(crate::errcode::BchError(libc::ENOMEM));
        }
        Ok(Fs { inner })
    }

    pub fn as_ptr(&self) -> *mut c::bch_fs {
        self.inner
    }

    pub fn as_ref(&self) -> &c::bch_fs {
        unsafe { &*self.inner }
    }

    pub fn sb(&self) -> &c::bch_sb_handle {
        unsafe { &(*self.inner).disk_sb }
    }

    pub fn sb_mut(&mut self) -> &mut c::bch_sb_handle {
        unsafe { &mut (*self.inner).disk_sb }
    }

    pub fn init(&self) -> BchResult<i32> {
        crate::errcode::ret_to_result_void(unsafe {
            c::bch2_fs_init(self.inner)
        })
    }

    pub fn start(&self) -> BchResult<i32> {
        crate::errcode::ret_to_result_void(unsafe {
            c::bch2_fs_start(self.inner)
        })
    }

    pub fn read_sb(&self, dev: u32) -> BchResult<i32> {
        crate::errcode::ret_to_result_void(unsafe {
            c::bch2_read_super(self.inner, dev)
        })
    }

    pub fn online(&self) -> bool {
        unsafe { c::bch2_fs_online(self.inner) }
    }

    pub fn gc(&self) -> BchResult<i32> {
        crate::errcode::ret_to_result_void(unsafe {
            c::bch2_gc(self.inner)
        })
    }
}

impl Drop for Fs {
    fn drop(&mut self) {
        if !self.inner.is_null() {
            unsafe { c::bch2_fs_stop(self.inner) };
            unsafe { c::bch2_fs_free(self.inner) };
        }
    }
}

pub struct DevRef<'a> {
    pub fs: &'a Fs,
    pub dev: u32,
}

impl<'a> DevRef<'a> {
    pub fn new(fs: &'a Fs, dev: u32) -> Self {
        DevRef { fs, dev }
    }

    pub fn ca(&self) -> *mut c::bch_dev {
        unsafe { (*self.fs.inner).devs[self.dev as usize] }
    }
}

pub struct SbLockGuard<'a> {
    fs: &'a Fs,
}

impl<'a> SbLockGuard<'a> {
    pub fn lock(fs: &'a Fs) -> Self {
        unsafe { c::bch2_sb_lock(fs.inner) };
        SbLockGuard { fs }
    }
}

impl<'a> Drop for SbLockGuard<'a> {
    fn drop(&mut self) {
        unsafe { c::bch2_sb_unlock(self.fs.inner) }
    }
}
