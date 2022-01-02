//! This module is loosely based on what Heaptrack uses to observe memory allocations

use std::collections::HashMap;
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
    skip: usize,
    // TODO: Check if a thread-local heap-allocated buffer is better. This is a 4KiB buffer...
    data: [*mut c_void; TRACE_MAX_SIZE],
}

impl std::ops::Deref for Trace {
    type Target = [*mut c_void];

    fn deref(&self) -> &Self::Target {
        &self.data[self.skip..self.size]
    }
}

impl Trace {
    /// Construct and fill a new trace, stopping at `stop`.
    #[inline(never)]
    pub fn new(skip: usize) -> Self {
        let mut trace = Self {
            size: 0,
            skip: 0,
            data: [std::ptr::null_mut(); TRACE_MAX_SIZE],
        };
        trace.fill(skip);
        trace
    }

    /// Fill a trace from the current call stack.
    pub fn fill(&mut self, skip: usize) {
        let data = &mut self.data;
        data.fill(0 as _);
        let size =
            unsafe { libunwind_sys::unw_backtrace(self.data.as_mut_ptr(), TRACE_MAX_SIZE as _) };
        if size < 0 {
            self.size = 0;
            self.skip = 0;
        } else {
            let mut size = size as usize;
            while size > 0 && self.data[size - 1].is_null() {
                size -= 1;
            }
            self.size = size.saturating_sub(skip);
            self.skip = skip;
        }
    }
}

/// A node in a tree of stack traces, with a list of children and their instruction pointer
#[derive(Debug, Default)]
struct GlobalTraceEdge {
    /// Children, sorted by instruction pointer
    children: Vec<(*const c_void, usize)>,
}

/// Tool to translate thread-local trace trees to a global representation.
///
/// TODO: Provide API to indicate thread termination
#[derive(Debug)]
pub struct GlobalTraceTree {
    // (thread_id, local_trace_index) -> global_trace_index
    local_to_global: HashMap<(usize, usize), usize>,
    // global_trace_index -> child of global_trace_index
    global_parent: Vec<GlobalTraceEdge>,
    //
    local_count: HashMap<usize, usize>,
}

impl Default for GlobalTraceTree {
    fn default() -> Self {
        Self {
            local_to_global: Default::default(),
            global_parent: vec![Default::default()],
            local_count: Default::default(),
        }
    }
}

impl GlobalTraceTree {
    /// Push a new thread-local stack frame to the global tree
    ///
    /// The arguments are:
    /// * `thread_id`: The ID of the thread adding the stack frame
    /// * `local_parent`: The thread-local parent stack frame
    /// * `ip`: The instruction pointer for the stack frame we're adding.
    ///
    /// Returns `true` if the stack frame was not encountered before.
    pub fn push(&mut self, thread_id: usize, local_parent: usize, ip: *const c_void) -> bool {
        // Determine the current local ID for the stack frame.
        let local_id = {
            let entry = self.local_count.entry(thread_id).or_default();
            *entry += 1;
            *entry
        };
        // Determine the global ID of the local parent
        let global_parent = if local_parent == 0 {
            // 0 for the root
            0
        } else {
            // By induction, `local_to_global` already contains the desired information, anything
            // else is a bug.
            *self
                .local_to_global
                .get(&(thread_id, local_parent))
                .unwrap()
        };
        // Determine the position of the child...
        let (updated, index) = match self.global_parent[global_parent]
            .children
            .binary_search_by(|(this_ip, _)| this_ip.cmp(&ip))
        {
            // Found -> not updated, global index
            Ok(position) => (
                false,
                self.global_parent[global_parent].children[position].1,
            ),
            // Not found -> updates, insert new global index
            Err(position) => {
                let index = self.global_parent.len();
                self.global_parent.push(Default::default());
                self.global_parent[0].children.insert(position, (ip, index));
                (true, index)
            }
        };
        // Remember the mapping for future lookups
        self.local_to_global.insert((thread_id, local_id), index);
        updated
    }

    /// Lookup a thread-local parent index and return its global index
    ///
    /// The lookup must be preceeded with a number of `push` operations to populate the tree.
    pub fn lookup(&mut self, thread_id: usize, local_parent: usize) -> usize {
        let global_id = *self
            .local_to_global
            .get(&(thread_id, local_parent))
            .unwrap_or(&0);
        global_id
    }
}
