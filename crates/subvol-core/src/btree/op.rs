use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BtreeOp {
    Insert = 0,
    Delete = 1,
    Whiteout = 2,
}
