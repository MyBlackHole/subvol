//! Bucket generation index helpers.

use serde::{Deserialize, Serialize};

/// Number of generation slots stored per bucket_gens key.
///
/// Matches bcachefs `KEY_TYPE_BUCKET_GENS_BITS = 8`.
pub const BUCKET_GENS_PER_KEY: usize = 256;

/// Compact bucket generation payload.
///
/// The on-disk btree stores one value for each chunk of 256 buckets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BchBucketGens {
    pub gens: Vec<u8>,
}

impl BchBucketGens {
    pub fn new() -> Self {
        Self {
            gens: vec![0; BUCKET_GENS_PER_KEY],
        }
    }

    pub fn set(&mut self, idx: usize, gen: u8) {
        if idx < self.gens.len() {
            self.gens[idx] = gen;
        }
    }
}
