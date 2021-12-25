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
    skip: usize,
    data: [*mut c_void; TRACE_MAX_SIZE],
}

impl std::ops::Deref for Trace {
    type Target = [*mut c_void];

    fn deref(&self) -> &Self::Target {
        &self.data[..self.size]
    }
}

impl Trace {
    pub fn new() -> Self {
        let mut trace = Self {
            size: 0,
            skip: 0,
            data: [0 as *mut c_void; TRACE_MAX_SIZE],
        };
        trace.fill(2);
        trace
    }

    pub fn fill(&mut self, skip: usize) {
        let mut size = unsafe { libunwind_sys::unw_backtrace(self.data.as_mut_ptr(), TRACE_MAX_SIZE as _) };
        if size < 0 {
            self.size = 0;
            self.skip = 0;
        } else {
            let mut size = size as usize;
            while size > 0 && self.data[size - 1] == 0 as *mut c_void {
                size -= 1;
            }
            self.size = size.saturating_sub(skip);
            self.skip = skip;
        }
    }
}
