use serde::{Deserialize, Serialize};
use track_types::{Timestamp, TraceIndex};

pub mod dataflow;
pub mod web;

type AllocInfo = (Timestamp, /* thread_id */ usize, TraceIndex);

#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub enum AllocError {
    DoubleAlloc {
        ptr: u64,
        old: AllocInfo,
        new: AllocInfo,
    },
    DoubleFree {
        ptr: u64,
        info: AllocInfo,
    },
    Leak {
        ptr: u64,
        info: AllocInfo,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum OutputData {
    AllocError(AllocError),
    AllocPerThreadPairs(Vec<AllocPerThreadPair>),
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct AllocPerThreadPair {
    pub alloc_thread: usize,
    pub dealloc_thread: usize,
    pub size: isize,
}
