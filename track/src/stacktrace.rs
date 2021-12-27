//! This module is loosely based on what Heaptrack uses to observe memory allocations

use std::ffi::c_void;
use std::ops::Deref;

/// A representative for an instruction pointer in the back trace.
#[derive(Debug)]
struct TraceEdge {
    /// The instruction pointer, pointing to the current instruction
    instruction_pointer: *mut c_void,
    /// Tree-relative index of this edge
    index: usize,
    /// Sorted list of children
    children: Vec<TraceEdge>,
}

impl Default for TraceEdge {
    fn default() -> Self {
        Self {
            instruction_pointer: std::ptr::null_mut(),
            index: 0,
            children: Default::default(),
        }
    }
}

/// A tree of trace edges
pub struct TraceTree {
    /// The root of the tree
    root: TraceEdge,
    /// Next index to assign.
    index: usize,
}

impl Default for TraceTree {
    fn default() -> Self {
        Self {
            root: Default::default(),
            index: 1,
        }
    }
}

impl TraceTree {
    /// Obtain the index for the current instruction outside of the tracing logic
    ///
    /// Calls the callback for any newly encountered instruction pointer, or a new path.
    pub fn index<C: FnMut(*const c_void, usize) -> bool>(
        &mut self,
        trace: &Trace,
        mut callback: C,
    ) -> usize {
        let mut index = 0;
        let mut parent = &mut self.root;
        for ip in trace.deref().iter().rev() {
            if ip.is_null() {
                continue;
            }
            let position = match parent
                .children
                .binary_search_by(|edge| edge.instruction_pointer.cmp(ip))
            {
                Ok(position) => position,
                Err(position) => {
                    index = self.index;
                    self.index += 1;
                    let edge = TraceEdge {
                        instruction_pointer: *ip,
                        index,
                        ..Default::default()
                    };
                    parent.children.insert(position, edge);
                    if !callback(*ip, parent.index) {
                        return 0;
                    }
                    position
                }
            };
            parent = &mut parent.children[position];
            index = parent.index;
        }
        index
    }
}

const TRACE_MAX_SIZE: usize = 64;

/// A size-limited stack trace composed of instruction pointers
pub struct Trace {
    size: usize,
    data: [*mut c_void; TRACE_MAX_SIZE],
}

impl std::ops::Deref for Trace {
    type Target = [*mut c_void];

    fn deref(&self) -> &Self::Target {
        &self.data[..self.size]
    }
}

impl Trace {
    /// Construct and fill a new trace, stopping at `stop`.
    #[inline(never)]
    pub fn new(stop: *mut c_void) -> Self {
        let mut trace = Self {
            size: 0,
            data: [std::ptr::null_mut(); TRACE_MAX_SIZE],
        };
        trace.fill(stop);
        trace
    }

    /// Fill a trace from the current call stack.
    pub fn fill(&mut self, stop: *mut c_void) {
        let mut index = 0;
        let data = &mut self.data;
        data.fill(0 as _);

        let mut record = false;
        backtrace::trace(|frame| {
            if !record {
                let symbol = frame.symbol_address();
                record = symbol == stop;
            } else {
                data[index] = frame.ip();
                index += 1;
            }
            index < data.len()
        });
        let mut size = index;
        while size > 0 && self.data[size - 1].is_null() {
            size -= 1;
        }
        self.size = size;
    }
}
