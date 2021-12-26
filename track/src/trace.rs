use std::ffi::c_void;
use std::ops::Deref;

#[derive(Debug)]
struct TraceEdge {
    instruction_pointer: *mut c_void,
    index: u32,
    children: Vec<TraceEdge>,
}

impl Default for TraceEdge {
    fn default() -> Self {
        Self {
            instruction_pointer: 0 as *mut c_void,
            index: 0,
            children: Default::default(),
        }
    }
}

pub struct TraceTree {
    root: TraceEdge,
    index: u32,
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
    pub fn index<C: FnMut(*const c_void, u32) -> bool>(&mut self, trace: &Trace, mut callback: C) -> u32 {
        let mut index = 0;
        let mut parent = &mut self.root;
        for ip in trace.deref().iter().rev() {
            if ip.is_null() {
                continue;
            }
            let position = match parent.children.binary_search_by(|edge| edge.instruction_pointer.cmp(ip)) {
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
    #[inline(never)]
    pub fn new(stop: *mut c_void) -> Self {
        let mut trace = Self {
            size: 0,
            data: [0 as *mut c_void; TRACE_MAX_SIZE],
        };
        trace.fill(stop);
        trace
    }

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
        while size > 0 && self.data[size - 1] == 0 as *mut c_void {
            size -= 1;
        }
        self.size = size;
    }
}
