use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct BchError(pub i32);

impl BchError {
    pub const SUCCESS: BchError = BchError(0);
    pub const EINVAL: BchError = BchError(22);
    pub const EIO: BchError = BchError(5);
    pub const ENOMEM: BchError = BchError(12);
    pub const ENOSPC: BchError = BchError(28);
    pub const ENOENT: BchError = BchError(2);
    pub const EEXIST: BchError = BchError(17);
    pub const EFBIG: BchError = BchError(27);
    pub const EAGAIN: BchError = BchError(11);
    pub const EBUSY: BchError = BchError(16);
    pub const EROFS: BchError = BchError(30);
    pub const EACCES: BchError = BchError(13);
    pub const ENOEXEC: BchError = BchError(8);
    pub const EOVERFLOW: BchError = BchError(75);
    pub const EMSGSIZE: BchError = BchError(90);
    pub const BUG: BchError = BchError(256);

    pub fn from_raw(code: i32) -> Self {
        BchError(code)
    }

    pub fn raw(&self) -> i32 {
        self.0
    }

    pub fn msg(&self) -> &'static str {
        match self.0 {
            0 => "success",
            2 => "no entry",
            5 => "IO error",
            11 => "try again",
            12 => "out of memory",
            16 => "busy",
            17 => "exists",
            22 => "invalid argument",
            27 => "file too large",
            28 => "no space",
            30 => "readonly fs",
            75 => "overflow",
            90 => "message too long",
            256 => "internal bug",
            _ => "unknown error",
        }
    }

    pub fn errno(&self) -> i32 {
        self.0
    }
}

impl fmt::Display for BchError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.msg())
    }
}

impl fmt::Debug for BchError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "BchError({}: {})", self.0, self.msg())
    }
}

pub type BchResult<T> = Result<T, BchError>;

pub fn ret_to_result(ret: i32) -> BchResult<i32> {
    if ret < 0 && ret > -4096 {
        Err(BchError(-ret))
    } else {
        Ok(ret)
    }
}

pub fn ret_to_result_void(ret: i32) -> BchResult<()> {
    if ret < 0 && ret > -4096 {
        Err(BchError(-ret))
    } else {
        Ok(())
    }
}

pub fn errptr_to_result<T>(p: *mut T) -> BchResult<*mut T> {
    let addr = p as usize;
    if addr > 0xffff_f000_0000_0000 {
        Err(BchError(-(addr as i32)))
    } else {
        Ok(p)
    }
}

pub fn bch_err_class(code: i32) -> i32 {
    if code < 0 { -code } else { code }
}
