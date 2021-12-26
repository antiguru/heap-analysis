use serde::{Deserialize, Serialize};

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
