use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Op {
    pub kind: u8,
    pub a: u8,
    pub b: u8,
    pub n: u8,
    pub data: Vec<u8>,
}
#[derive(Serialize, Deserialize)]
pub struct Batch {
    pub processes: Vec<Vec<Op>>,
}
