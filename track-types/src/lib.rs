//! Types for representing heap allocations

#![forbid(missing_docs)]

use serde::{Deserialize, Serialize};

/// Environment symbol for the heap analysis address.
pub static ENV_HEAP_ANALYSIS_ADDR: &str = "HEAP_ANALSIS_ADDR";

/// Timestamp type, roughly nanoseconds since start of tracking.
pub type Timestamp = u64;

/// A timestamped trace instruction.
pub type TimestampedTraceInstruction = (TraceInstruction, Timestamp);

/// Tracing protocol. Core wire format
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TraceProtocol {
    /// Provide a buffer of thread-local trace instructions.
    Instructions {
        /// The thread's ID
        thread_id: usize,
        /// Vector of timestamped instructions
        buffer: Vec<TimestampedTraceInstruction>,
    },
    /// Update the time. All data up to this point has been shipped.
    Timestamp(Timestamp),
}

/// A choice of trace instructions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TraceInstruction {
    /// Initialize instruction, only send once
    Init(InstrInit),
    /// Stack frame update
    Stack(InstrStack),
    /// Allocate memory
    Allocate(InstrAllocate),
    /// Free memory
    Free(InstrFree),
}

/// Initialization instruction
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstrInit {
    /// Human-readable name of the thread
    pub thread_name: String,
    /// Opaque thread id, unique per thread
    pub thread_id: usize,
}

/// Announce a new stack frame
///
/// Stack frames are a thread-local tree that reference a parent stack frame. The number references
/// the n-th chronological frame on the same thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstrStack {
    /// Resolved symbol
    pub name: String,
    /// Number of the parent stack frame
    pub parent: usize,
}

/// Announce a memory allocation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstrAllocate {
    /// Size of the allocation
    pub size: usize,
    /// Pointer
    pub ptr: u64,
    /// Stack frame
    pub trace_index: usize,
}

/// Announce freeing of mememory
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstrFree {
    /// Pointer
    pub ptr: u64,
}
