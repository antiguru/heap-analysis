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

/// Trace index type.
#[derive(Debug, Default, Copy, Clone, Serialize, Deserialize, Ord, PartialOrd, Eq, PartialEq)]
pub struct TraceIndex(pub u32);

impl From<u32> for TraceIndex {
    fn from(trace_index: u32) -> Self {
        Self(trace_index)
    }
}

impl From<usize> for TraceIndex {
    fn from(trace_index: usize) -> Self {
        Self(trace_index as u32)
    }
}

/// Tracing protocol. Core wire format
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TraceProtocol {
    /// Provide a buffer of thread-local trace instructions.
    Instructions {
        /// The current timestamp up to which all data has been shipped.
        timestamp: Timestamp,
        /// The thread's ID
        thread_id: usize,
        /// Vector of timestamped instructions
        buffer: Vec<TimestampedTraceInstruction>,
    },
    /// Update the time. All data up to this point has been shipped.
    Timestamp(Timestamp),
    /// Initialize instruction, only send once
    Init(InstrInit),
    /// Resolved stack frames
    Stack {
        /// The index of the stack frame
        index: usize,
        /// Details of the stack frame, heap allocated to reduce struct size.
        details: Box<InstrStackDetails>,
    },
}

/// A choice of trace instructions
#[derive(Debug, Clone, Serialize, Deserialize, Ord, PartialOrd, Eq, PartialEq)]
pub enum TraceInstruction {
    /// Stack frame update
    Stack(InstrStack),
    /// Allocate memory
    Allocate(InstrAllocation),
    /// Free memory
    Deallocate(InstrAllocation),
}

impl TraceInstruction {
    /// Obtain the pointer this instruction references, if any.
    pub fn ptr(&self) -> Option<u64> {
        match self {
            TraceInstruction::Stack(_) => None,
            TraceInstruction::Allocate(instr) | TraceInstruction::Deallocate(instr) => {
                Some(instr.ptr)
            }
        }
    }

    /// Obtain the trace index, if any.
    pub fn trace_index(&self) -> Option<TraceIndex> {
        match self {
            TraceInstruction::Stack(_) => None,
            TraceInstruction::Allocate(instr) | TraceInstruction::Deallocate(instr) => {
                Some(instr.trace_index)
            }
        }
    }

    /// Obtain the allocation size, if any.
    pub fn size(&self) -> Option<usize> {
        match self {
            TraceInstruction::Stack(_) => None,
            TraceInstruction::Allocate(instr) | TraceInstruction::Deallocate(instr) => {
                Some(instr.size)
            }
        }
    }
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
    /// Instruction pointer on the stack.
    pub ip: u64,
    /// Index of this stack frame.
    pub index: usize,
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
    pub trace_index: TraceIndex,
}
