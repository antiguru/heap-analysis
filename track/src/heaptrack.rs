use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::trace::{Trace, TraceTree};
use crate::HeaptrackGatherHandle;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TraceInstruction {
    Init(TraceInstructionInit),
    Stack(TraceInstructionStack),
    Allocate(TraceInstructionAllocate),
    Free(TraceInstructionFree),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceInstructionInit {
    pub thread_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceInstructionStack {
    pub name: String,
    pub parent: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceInstructionAllocate {
    pub size: usize,
    pub ptr: u64,
    pub trace_index: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceInstructionFree {
    pub ptr: u64,
}

pub struct HeaptrackWriter {
    trace_tree: TraceTree,
    trace_buffer: TraceBuffer,
}

struct TraceBuffer {
    handle: HeaptrackGatherHandle,
    buffer: Arc<Mutex<Vec<TraceInstruction>>>,
}

impl TraceBuffer {
    fn new(handle: HeaptrackGatherHandle) -> Self {
        Self {
            handle,
            buffer: Default::default(),
        }
    }

    fn init(&mut self) {
        self.handle.register(self.buffer.clone());
        // self.trace(TraceInstruction::Init(TraceInstructionInit {
        //     thread_name: std::thread::current()
        //         .name()
        //         .map_or_else(|| "unknown".to_owned(), |name| name.to_owned()),
        // }))
    }

    fn trace(&mut self, instruction: TraceInstruction) {
        let mut buffer = self.buffer.lock().unwrap();
        if buffer.capacity() < 1024 {
            let to_reserve = 1024 - buffer.capacity();
            buffer.reserve(to_reserve);
        }
        buffer.push(instruction);
        if buffer.len() == buffer.capacity() {
            let buffer = std::mem::take(&mut *buffer);
            self.handle.flush(buffer);
        }
    }
}

impl HeaptrackWriter {
    pub fn new(handle: HeaptrackGatherHandle) -> Self {
        Self {
            trace_tree: Default::default(),
            trace_buffer: TraceBuffer::new(handle),
        }
    }

    pub fn init(&mut self) {
        self.trace_buffer.init();
    }

    fn _write_timestamp(&mut self) {
        // TODO
    }

    pub fn handle_malloc(&mut self, ptr: *mut u8, size: usize, trace: &Trace) {
        let trace_buffer = &mut self.trace_buffer;
        let index = self.trace_tree.index(trace, |ip, index| {
            let mut symbol = None;
            {
                let symbol = &mut symbol;
                backtrace::resolve(ip as _, |sym| {
                    *symbol = sym.name().map(|name| format!("{:#}", name));
                });
            }
            let symbol = symbol.unwrap_or_else(|| "<unresolved>".to_owned());
            trace_buffer.trace(TraceInstruction::Stack(TraceInstructionStack {
                name: symbol,
                parent: index as _,
            }));
            true
        });
        self.trace_buffer
            .trace(TraceInstruction::Allocate(TraceInstructionAllocate {
                trace_index: index as _,
                ptr: ptr as _,
                size,
            }))
    }

    pub fn handle_free(&mut self, ptr: *mut u8) {
        self.trace_buffer
            .trace(TraceInstruction::Free(TraceInstructionFree {
                ptr: ptr as _,
            }))
    }
}
