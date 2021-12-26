use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::RefCell;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use lazy_static::lazy_static;
use track_types::{InstrAllocate, InstrFree, InstrStack};
use track_types::{InstrInit, TraceInstruction};

use crate::stacktrace::Trace;
use crate::stacktrace::TraceTree;

mod stacktrace;

pub struct TrackingAllocator;

impl TrackingAllocator {
    pub fn start() {
        START_TRACKING.store(true, Ordering::SeqCst);
    }
}

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        let trace = Trace::new(Self::alloc as _);
        AllocatorInner::writer(|writer| writer.handle_malloc(ptr, layout.size(), &trace));
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        AllocatorInner::writer(|writer| writer.handle_free(ptr));
        System.dealloc(ptr, layout)
    }
}

pub(crate) struct AllocatorInner {
    writer: AllocationWriter,
}

enum TrackingState {
    /// Not initialized
    None,
    /// Ready to track allocations
    Ready(AllocatorInner),
    /// Tracking permanently disabled
    Disabled,
}

#[derive(Debug)]
enum GathererProtocol {
    Flush(usize, Vec<TraceInstruction>),
    Register(Arc<Mutex<ThreadState>>),
}

#[derive(Clone, Debug)]
pub struct GatherHandle {
    sender: Option<crossbeam_channel::Sender<GathererProtocol>>,
    handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl Default for GatherHandle {
    fn default() -> Self {
        let (sender, receiver) = crossbeam_channel::unbounded();

        let handle = std::thread::spawn(|| {
            // Disable tracking for this thread
            AllocatorInner::WRITER.with(|x| *x.borrow_mut() = TrackingState::Disabled);
            let mut gatherer = Gatherer::new(receiver);
            gatherer.run();
        });

        Self {
            sender: Some(sender),
            handle: Arc::new(Mutex::new(Some(handle))),
        }
    }
}

impl GatherHandle {
    fn register(&self, buffer: Arc<Mutex<ThreadState>>) {
        self.sender
            .as_ref()
            .expect("Sender exists until drop")
            .send(GathererProtocol::Register(buffer))
            .unwrap();
    }

    fn flush(&self, thread_id: usize, buffer: Vec<TraceInstruction>) {
        self.sender
            .as_ref()
            .expect("Sender exists until drop")
            .send(GathererProtocol::Flush(thread_id, buffer))
            .unwrap();
    }
}

impl Drop for GatherHandle {
    fn drop(&mut self) {
        self.sender.take();
        // strong count == 2 -> static reference + main thread, so we're the last thread to exit
        if Arc::strong_count(&self.handle) == 2 {
            // Take ownership of the join handle
            let handle = std::mem::take(&mut self.handle);
            // try lock to avoid recusive locks on `GATHER`
            std::mem::drop(GATHER.try_lock().map(|mut guard| guard.take()));
            // Now, the gatherer thread can exit, and we wait for it
            std::mem::drop(
                handle
                    .lock()
                    .map(|mut handle| handle.take().map(|handle| handle.join())),
            );
        }
    }
}

struct Gatherer {
    buffers: Vec<Arc<Mutex<ThreadState>>>,
    receiver: crossbeam_channel::Receiver<GathererProtocol>,
    connection: TcpStream,
}

impl Gatherer {
    fn new(receiver: crossbeam_channel::Receiver<GathererProtocol>) -> Self {
        let connection = TcpStream::connect("127.0.0.1:64123").unwrap();
        Self {
            buffers: Default::default(),
            receiver,
            connection,
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
                                GathererProtocol::Register(shared_buffer) => {
                                    self.buffers.push(shared_buffer);
                                },
                                GathererProtocol::Flush(thread_id, buffer) => {
                                    self.handle_buffer(thread_id, &buffer);
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
            let mut guard = self.buffers[index].lock().unwrap();
            self.handle_buffer(guard.thread_id, &std::mem::take(&mut guard.buffer));
        }
    }

    fn handle_buffer(&self, thread_id: usize, buffer: &[TraceInstruction]) {
        if buffer.is_empty() {
            return;
        }
        bincode::serialize_into(&self.connection, &(thread_id, buffer)).unwrap();
    }
}

impl Drop for Gatherer {
    fn drop(&mut self) {
        self.handle_buffers();
    }
}

lazy_static! {
    static ref GATHER: Mutex<Option<GatherHandle>> = Mutex::new(Some(Default::default()));
}

static START_TRACKING: AtomicBool = AtomicBool::new(false);

impl AllocatorInner {
    thread_local! {
        static WRITER: RefCell<TrackingState> = RefCell::new(TrackingState::None)
    }

    fn writer<F: FnOnce(&mut AllocationWriter)>(f: F) {
        let _ = AllocatorInner::WRITER.try_with(|x| {
            if !START_TRACKING.load(Ordering::SeqCst) {
                return;
            }
            // Try to borrow. Prevents re-entrant allocations and allocations after dropping
            if let Ok(mut borrow) = x.try_borrow_mut() {
                if matches!(*borrow, TrackingState::None) {
                    let mut inner = AllocatorInner {
                        writer: AllocationWriter::new(GATHER.lock().unwrap().clone().unwrap()),
                    };
                    inner.writer.init();
                    *borrow = TrackingState::Ready(inner);
                }
                if let TrackingState::Ready(inner) = &mut *borrow {
                    f(&mut inner.writer);
                }
            }
        });
    }
}

pub struct AllocationWriter {
    trace_tree: TraceTree,
    trace_buffer: TraceBuffer,
}

#[derive(Debug)]
struct ThreadState {
    buffer: Vec<TraceInstruction>,
    thread_id: usize,
}

impl Default for ThreadState {
    fn default() -> Self {
        Self {
            buffer: Default::default(),
            thread_id: thread_id::get(),
        }
    }
}

struct TraceBuffer {
    handle: GatherHandle,
    buffer: Arc<Mutex<ThreadState>>,
}

impl TraceBuffer {
    fn new(handle: GatherHandle) -> Self {
        Self {
            handle,
            buffer: Default::default(),
        }
    }

    fn init(&mut self) {
        self.handle.register(self.buffer.clone());
        self.trace(TraceInstruction::Init(InstrInit {
            thread_name: format!("{:?}", std::thread::current().id().to_owned()),
            thread_id: thread_id::get(),
        }))
    }

    fn trace(&mut self, instruction: TraceInstruction) {
        let mut guard = self.buffer.lock().unwrap();
        let buffer = &mut guard.buffer;
        if buffer.capacity() < 1024 {
            let to_reserve = 1024 - buffer.capacity();
            buffer.reserve(to_reserve);
        }
        buffer.push(instruction);
        if buffer.len() == buffer.capacity() {
            let buffer = std::mem::take(&mut *buffer);
            self.handle.flush(guard.thread_id, buffer);
        }
    }
}

impl AllocationWriter {
    pub fn new(handle: GatherHandle) -> Self {
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
            trace_buffer.trace(TraceInstruction::Stack(InstrStack {
                name: symbol,
                parent: index as _,
            }));
            true
        });
        self.trace_buffer
            .trace(TraceInstruction::Allocate(InstrAllocate {
                trace_index: index as _,
                ptr: ptr as _,
                size,
            }))
    }

    pub fn handle_free(&mut self, ptr: *mut u8) {
        self.trace_buffer
            .trace(TraceInstruction::Free(InstrFree { ptr: ptr as _ }))
    }
}
