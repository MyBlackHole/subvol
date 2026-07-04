//! subvol-core: 核心库，包含后端抽象、btree、journal、缓存

#![allow(dead_code)]
#![allow(non_upper_case_globals)]

pub mod alloc;
pub mod bch_vol;
pub mod block_device;
pub mod btree;
pub mod config;
pub mod io;
pub mod journal;
pub mod lock;
pub mod recovery;
pub mod replicas;
pub mod snap;
pub mod storage;
pub mod subvol;
pub mod types;
pub use bch_vol::BchVol;
pub use types::*;

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
