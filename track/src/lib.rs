use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::RefCell;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use lazy_static::lazy_static;

use crate::heaptrack::{HeaptrackWriter, TraceInstruction};
use crate::trace::Trace;

mod heaptrack;
mod trace;

pub struct HeaptrackAllocator;

struct HeaptrackInner {
    writer: HeaptrackWriter,
}

enum HeaptrackState {
    /// Not initialized
    None,
    /// Ready to track allocations
    Ready(HeaptrackInner),
    /// Tracking permanently disabled
    Disabled,
}

#[derive(Debug)]
enum HeaptrackGathererProtocol {
    Flush(Vec<TraceInstruction>),
    Register(Arc<Mutex<Vec<TraceInstruction>>>),
}

#[derive(Clone, Debug)]
pub struct HeaptrackGatherHandle {
    sender: Option<crossbeam_channel::Sender<HeaptrackGathererProtocol>>,
    handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl Default for HeaptrackGatherHandle {
    fn default() -> Self {
        let (sender, receiver) = crossbeam_channel::unbounded();

        let handle = std::thread::spawn(|| {
            // Disable tracking for this thread
            HeaptrackInner::WRITER.with(|x| *x.borrow_mut() = HeaptrackState::Disabled);
            let mut gatherer = HeaptrackGatherer::new(receiver);
            gatherer.run();
        });
        let handle = Some(handle);

        Self {
            sender: Some(sender),
            handle: Arc::new(Mutex::new(handle)),
        }
    }
}

impl HeaptrackGatherHandle {
    pub fn register(&self, buffer: Arc<Mutex<Vec<TraceInstruction>>>) {
        self.sender
            .as_ref()
            .expect("Sender exists until drop")
            .send(HeaptrackGathererProtocol::Register(buffer))
            .unwrap();
    }

    pub fn flush(&self, buffer: Vec<TraceInstruction>) {
        self.sender
            .as_ref()
            .expect("Sender exists until drop")
            .send(HeaptrackGathererProtocol::Flush(buffer))
            .unwrap();
    }
}

impl Drop for HeaptrackGatherHandle {
    fn drop(&mut self) {
        self.sender.take();
        // strong count == 2 -> static reference + main thread, so we're the last thread to exit
        if Arc::strong_count(&self.handle) == 2 {
            // Take ownership of the join handle
            let handle = std::mem::take(&mut self.handle);
            // try lock to avoid recusive locks on `GATHER`
            let _ = GATHER.try_lock().map(|mut guard| guard.take());
            // Now, the gatherer thread can exit, and we wait for it
            let _ = handle
                .lock()
                .map(|mut handle| handle.take().map(|handle| handle.join()));
        }
    }
}

struct HeaptrackGatherer {
    buffers: Vec<Arc<Mutex<Vec<TraceInstruction>>>>,
    receiver: crossbeam_channel::Receiver<HeaptrackGathererProtocol>,
}

impl HeaptrackGatherer {
    fn new(receiver: crossbeam_channel::Receiver<HeaptrackGathererProtocol>) -> Self {
        Self {
            buffers: Default::default(),
            receiver,
        }
    }

    fn run(&mut self) {
        let tick = crossbeam_channel::tick(Duration::from_millis(100));
        loop {
            crossbeam_channel::select! {
                recv(self.receiver) -> msg => {
                    match msg {
                       Ok(protocol) => {
                            match protocol {
                                HeaptrackGathererProtocol::Register(shared_buffer) => {
                                    self.buffers.push(shared_buffer);
                                },
                                HeaptrackGathererProtocol::Flush(buffer) => {
                                    self.handle_buffer(buffer);
                                },
                            }
                        }
                        Err(_) => break,
                    }
                },
                recv(tick) -> _tick => {
                    self.handle_buffers();
                },
            }
        }
    }

    fn handle_buffers(&mut self) {
        for index in 0..self.buffers.len() {
            self.handle_buffer(std::mem::take(&mut *self.buffers[index].lock().unwrap()));
        }
    }

    fn handle_buffer(&self, buffer: Vec<TraceInstruction>) {
        for instruction in &buffer {
            println!("instruction: {:?}", instruction);
        }
    }
}

impl Drop for HeaptrackGatherer {
    fn drop(&mut self) {
        for shared_buffer in &self.buffers {
            self.handle_buffer(std::mem::take(&mut *shared_buffer.lock().unwrap()));
        }
    }
}

lazy_static! {
    static ref GATHER: Mutex<Option<HeaptrackGatherHandle>> = Mutex::new(Some(Default::default()));
}

impl HeaptrackInner {
    thread_local! {
        static WRITER: RefCell<HeaptrackState> = RefCell::new(HeaptrackState::None)
    }

    fn writer<F: FnOnce(&mut HeaptrackWriter)>(f: F) {
        let _ = HeaptrackInner::WRITER.try_with(|x| {
            // Try to borrow. Prevents re-entrant allocations and allocations after dropping
            if let Ok(mut borrow) = x.try_borrow_mut() {
                if matches!(*borrow, HeaptrackState::None) {
                    let mut inner = HeaptrackInner {
                        writer: HeaptrackWriter::new(GATHER.lock().unwrap().clone().unwrap()),
                    };
                    inner.writer.init();
                    *borrow = HeaptrackState::Ready(inner);
                }
                if let HeaptrackState::Ready(inner) = &mut *borrow {
                    f(&mut inner.writer);
                }
            }
        });
    }
}

unsafe impl GlobalAlloc for HeaptrackAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        let trace = Trace::new(Self::alloc as _);
        HeaptrackInner::writer(|writer| writer.handle_malloc(ptr, layout.size(), &trace));
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        HeaptrackInner::writer(|writer| writer.handle_free(ptr));
        System.dealloc(ptr, layout)
    }
}
