//! Types for representing heap allocations

#![forbid(missing_docs)]

use serde::{Deserialize, Serialize};

/// Environment symbol for the heap analysis address.
pub static ENV_HEAP_ANALYSIS_ADDR: &str = "HEAP_ANALYSIS_ADDR";

/// Nanosecond timestamp type, not relative to a specific moment in time. Can only be used to
/// compare to each other.
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
#[derive(Debug, Clone, Serialize, Deserialize, Ord, PartialOrd, Eq, PartialEq)]
pub enum TraceInstruction {
    /// Initialize instruction, only send once
    Init(InstrInit),
    /// Stack frame update
    Stack(InstrStack),
    /// Allocate memory
    Allocate(InstrAllocation),
    /// Free memory
    Deallocate(InstrAllocation),
}

/// Initialization instruction
#[derive(Debug, Clone, Serialize, Deserialize, Ord, PartialOrd, Eq, PartialEq)]
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
#[derive(Debug, Clone, Serialize, Deserialize, Ord, PartialOrd, Eq, PartialEq)]
pub struct InstrStack {
    /// Resolved symbol, heap-allocated to reduce the size of [TraceInstruction]
    pub details: Box<InstrStackDetails>,
    /// Number of the parent stack frame
    pub parent: usize,
}

/// Symbol details
///
/// The availability of some data depends on debug information, especially filename/lineno/colno.
#[derive(Debug, Default, Clone, Serialize, Deserialize, Ord, PartialOrd, Eq, PartialEq)]
pub struct InstrStackDetails {
    /// Name of the symbol.
    pub name: Option<String>,
    /// Filename.
    pub filename: Option<std::path::PathBuf>,
    /// Line of the instruction.
    pub lineno: Option<u32>,
    /// Column of the instruction.
    pub colno: Option<u32>,
}

/// Announce a memory allocation
#[derive(Debug, Clone, Serialize, Deserialize, Ord, PartialOrd, Eq, PartialEq)]
pub struct InstrAllocation {
    /// Size of the allocation
    pub size: usize,
    /// Pointer
    pub ptr: u64,
    /// Stack frame
    pub trace_index: usize,
}
