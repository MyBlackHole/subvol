use crate::bcachefs::*;
use crate::errcode::BchResult;
use crate::opts::Printbuf;

pub fn bch2_copygc_can_make_progress(_ca: *mut BchDev) -> bool {
    todo!()
}

pub fn bch2_copygc_dev_wait_amount(_ca: *mut BchDev) -> u64 {
    todo!()
}

pub fn bch2_copygc_wait_amount(_c: &BchFs) -> u64 {
    todo!()
}

pub fn bch2_copygc_wait_to_text(_out: &mut Printbuf, _c: &BchFs) {
    todo!()
}

pub fn bch2_copygc_wakeup(_c: &BchFs) {
    todo!()
}

pub fn bch2_copygc_start(_c: &BchFs) -> BchResult<i32> {
    todo!()
}

pub fn bch2_copygc_stop(_c: &BchFs) {
    todo!()
}

pub fn bch2_fs_copygc_init(_c: &BchFs) {
    todo!()
}

pub fn bch2_fs_copygc_exit(_c: &BchFs) {
    todo!()
}
