use serde::{Deserialize, Serialize};

pub type Timestamp = u64;
pub type TimestampedTraceInstruction = (TraceInstruction, Timestamp);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TraceProtocol {
    Instructions {
        thread_id: usize,
        buffer: Vec<TimestampedTraceInstruction>,
    },
    Timestamp(Timestamp),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TraceInstruction {
    Init(InstrInit),
    Stack(InstrStack),
    Allocate(InstrAllocate),
    Free(InstrFree),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstrInit {
    pub thread_name: String,
    pub thread_id: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstrStack {
    pub name: String,
    pub parent: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstrAllocate {
    pub size: usize,
    pub ptr: u64,
    pub trace_index: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstrFree {
    pub ptr: u64,
}
