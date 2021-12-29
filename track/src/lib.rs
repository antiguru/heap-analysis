//! Tracking allocator implementation

#![forbid(missing_docs)]

use std::alloc::{GlobalAlloc, Layout};
use std::cell::{Cell, RefCell};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{select, tick, unbounded, Receiver, Sender, TryRecvError};

use lazy_static::lazy_static;
use libc::c_void;

use track_types::{
    InstrAllocation, InstrInit, InstrStack, InstrStackDetails, TimestampedTraceInstruction,
    TraceInstruction, TraceProtocol, ENV_HEAP_ANALYSIS_ADDR,
};

use crate::stacktrace::Trace;
use crate::stacktrace::TraceTree;

mod stacktrace;

/// Tracking memory allocator
pub struct TrackingAllocator<A>(pub A);

impl<A> TrackingAllocator<A> {
    /// Enable tracking memory allocations. Should be the first call in `main`.
    pub fn start(&self) {
        START_TRACKING.store(true, Ordering::SeqCst);
    }
}

lazy_static! {
    /// Handle to the gatherer thread
    static ref GATHER: Mutex<Option<GatherHandle>> = Mutex::new(Some(Default::default()));
    /// Start time
    static ref START_TIME: std::time::Instant = std::time::Instant::now();
    /// Coordination lock
    static ref REGISTRATION_LOCK: RwLock<()> = Default::default();
}

/// Is tracking enabled?
static START_TRACKING: AtomicBool = AtomicBool::new(false);

unsafe impl<A: GlobalAlloc> GlobalAlloc for TrackingAllocator<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = self.0.alloc(layout);
        AllocationWriter::writer(|writer| {
            writer.handle_malloc(ptr as _, layout.size(), Self::alloc as _)
        });
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        AllocationWriter::writer(|writer| {
            writer.handle_dealloc(ptr as _, layout.size(), Self::dealloc as _)
        });
        self.0.dealloc(ptr, layout)
    }
}

enum TrackingState {
    /// Not initialized
    None,
    /// Ready to track allocations
    Ready(AllocationWriter),
    /// Tracking permanently disabled
    Disabled,
}

/// Protocol definition for worker threads to send commands to the gatherer thread
#[derive(Debug)]
enum GathererProtocol {
    /// Flush a complete trace buffer
    Flush(usize, Vec<TimestampedTraceInstruction>),
    /// Register a shared handle to this thread's state
    Register(Arc<Mutex<ThreadState>>, Arc<(Mutex<bool>, Condvar)>),
}

/// Handle to the gatherer thread
#[derive(Clone, Debug)]
struct GatherHandle {
    /// Send endpoint to provide data
    sender: Option<Sender<GathererProtocol>>,
    /// shared join handle to wait for termination of the gatherer
    handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl Default for GatherHandle {
    fn default() -> Self {
        let (sender, receiver) = unbounded();

        let handle = std::thread::spawn(|| {
            // Disable tracking for this thread
            AllocationWriter::WRITER.with(|x| *x.borrow_mut() = TrackingState::Disabled);
            let addr = std::env::var(ENV_HEAP_ANALYSIS_ADDR);
            let addr = addr.as_deref().unwrap_or("localhost:64123");
            let mut gatherer = Gatherer::new(receiver, addr);
            gatherer.run();
        });

        Self {
            sender: Some(sender),
            handle: Arc::new(Mutex::new(Some(handle))),
        }
    }
}

impl GatherHandle {
    /// Register a thread state with the gatherer
    fn register(&self, thread_state: Arc<Mutex<ThreadState>>) {
        let condvar = Arc::new((Mutex::new(false), Condvar::new()));
        self.sender
            .as_ref()
            .expect("Sender exists until drop")
            .send(GathererProtocol::Register(
                thread_state,
                Arc::clone(&condvar),
            ))
            .unwrap();

        // Wait for the gather thread to accept our registration, only then we're allowed to allocate memory
        let (lock, signal) = &*condvar;
        let mut started = lock.lock().unwrap();
        while !*started {
            started = signal.wait(started).unwrap();
        }
    }

    /// Send a complete buffer to the gatherer
    fn flush(&self, thread_id: usize, buffer: Vec<TimestampedTraceInstruction>) {
        self.sender
            .as_ref()
            .expect("Sender exists until drop")
            .send(GathererProtocol::Flush(thread_id, buffer))
            .unwrap();
    }
}

impl Drop for GatherHandle {
    fn drop(&mut self) {
        // The drop implementation is responsible for terminating the gatherer once it's the last
        // handle to it.
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

/// State of the gatherer thread
struct Gatherer {
    /// Shared handle to the thread states.
    buffers: Cell<Vec<Arc<Mutex<ThreadState>>>>,
    /// Receive endpoint to get push updates from threads.
    receiver: Receiver<GathererProtocol>,
    /// Sink to write data
    connection: TcpStream,
}

impl Gatherer {
    /// Construct a new gatherer from a receiver of thread updates
    fn new<A: ToSocketAddrs>(receiver: Receiver<GathererProtocol>, addr: A) -> Self {
        let connection = TcpStream::connect(addr).unwrap();
        Self {
            buffers: Default::default(),
            receiver,
            connection,
        }
    }

    /// Handle data from worker threads. Returns once all work threads disappear.
    fn run(&mut self) {
        let tick = tick(Duration::from_millis(1000));
        loop {
            select! {
                recv(self.receiver) -> msg => {
                    match msg {
                       Ok(protocol) => self.handle_protocol(protocol),
                        Err(_) => break,
                    }
                },
                recv(tick) -> _tick => {
                    self.handle_buffers();
                },
            }
        }
    }

    fn handle_protocol(&mut self, protocol: GathererProtocol) {
        match protocol {
            GathererProtocol::Register(shared_buffer, condvar) => {
                self.buffers.get_mut().push(shared_buffer);
                let (lock, signal) = &*condvar;
                let mut started = lock.lock().unwrap();
                *started = true;
                signal.notify_one();
            }
            GathererProtocol::Flush(thread_id, mut buffer) => {
                self.send_buffer(thread_id, &mut buffer);
            }
        }
    }

    fn try_recv(&mut self) -> Result<(), crossbeam_channel::TryRecvError> {
        loop {
            match self.receiver.try_recv() {
                Ok(protocol) => self.handle_protocol(protocol),
                Err(TryRecvError::Disconnected) => return Err(TryRecvError::Disconnected),
                Err(TryRecvError::Empty) => break,
            }
        }
        Ok(())
    }

    /// Forcibly flush buffers from worker threads
    fn handle_buffers(&mut self) {
        let _coordination_lock = REGISTRATION_LOCK.write();
        let _ = self.try_recv();
        let time = START_TIME.elapsed().as_nanos() as u64;
        let mut buffer = Vec::with_capacity(TraceBuffer::capacity());
        let states = self.buffers.take();
        for state in &states {
            // Limit scope of lock to not include sending data over the network
            let thread_id = {
                let mut guard = state.lock().unwrap();
                let _ = self.try_recv();
                std::mem::swap(&mut buffer, &mut guard.buffer);
                guard.thread_id
            };
            self.send_buffer(thread_id, &mut buffer);
        }
        self.buffers.set(states);
        bincode::serialize_into::<_, TraceProtocol>(
            &self.connection,
            &TraceProtocol::Timestamp(time),
        )
        .unwrap();
    }

    /// Send a buffer.
    fn send_buffer(&self, thread_id: usize, buffer: &mut Vec<TimestampedTraceInstruction>) {
        if buffer.is_empty() {
            return;
        }
        let protocol = TraceProtocol::Instructions {
            thread_id,
            buffer: std::mem::take(buffer),
        };
        bincode::serialize_into::<_, TraceProtocol>(&self.connection, &protocol).unwrap();
        if let TraceProtocol::Instructions { buffer: b, .. } = protocol {
            *buffer = b;
        }
        buffer.clear();
    }
}

impl Drop for Gatherer {
    fn drop(&mut self) {
        self.handle_buffers();
    }
}

struct AllocationWriter {
    trace_tree: TraceTree,
    trace_buffer: TraceBuffer,
}

impl AllocationWriter {
    thread_local! {
        static WRITER: RefCell<TrackingState> = RefCell::new(TrackingState::None)
    }

    /// Apply `f` on the thread-local [AllocationWriter].
    ///
    /// The function `f` will only be called if tracking is enabled. Also, this method ignores
    /// reentrant calls.
    #[inline(always)]
    fn writer<F: FnOnce(&mut AllocationWriter)>(f: F) {
        if !START_TRACKING.load(Ordering::SeqCst) {
            return;
        }
        let _ = AllocationWriter::WRITER.try_with(|x| {
            // Try to borrow. Prevents re-entrant allocations and allocations after dropping
            if let Ok(mut borrow) = x.try_borrow_mut() {
                if matches!(*borrow, TrackingState::None) {
                    let mut inner = AllocationWriter::new(GATHER.lock().unwrap().clone().unwrap());
                    inner.init();
                    *borrow = TrackingState::Ready(inner);
                }
                if let TrackingState::Ready(inner) = &mut *borrow {
                    f(inner);
                }
            }
        });
    }

    fn new(handle: GatherHandle) -> Self {
        Self {
            trace_tree: Default::default(),
            trace_buffer: TraceBuffer::new(handle),
        }
    }

    fn init(&mut self) {
        self.trace_buffer.init();
    }

    fn alloc_index(&mut self, trace: &Trace) -> usize {
        let trace_buffer = &mut self.trace_buffer;
        self.trace_tree.index(trace, |ip, parent| {
            let mut details = InstrStackDetails::default();
            {
                backtrace::resolve(ip as _, |sym| {
                    details.name = sym.name().map(|name| format!("{:#}", name));
                    details.filename = sym.filename().map(Into::into);
                    details.lineno = sym.lineno();
                    details.colno = sym.colno();
                });
            }
            let details = Box::new(details);
            trace_buffer.push(TraceInstruction::Stack(InstrStack { details, parent }));
            true
        })
    }

    fn handle_malloc(&mut self, ptr: u64, size: usize, stop: *mut c_void) {
        let trace = Trace::new(stop);
        let trace_index = self.alloc_index(&trace);
        let allocate = InstrAllocation {
            trace_index,
            ptr,
            size,
        };
        self.trace_buffer.push(TraceInstruction::Allocate(allocate))
    }

    fn handle_dealloc(&mut self, ptr: u64, size: usize, stop: *mut c_void) {
        let trace = Trace::new(stop);
        let trace_index = self.alloc_index(&trace);
        let free = InstrAllocation {
            trace_index,
            ptr,
            size,
        };
        self.trace_buffer.push(TraceInstruction::Deallocate(free))
    }
}

#[derive(Debug)]
struct ThreadState {
    buffer: Vec<(TraceInstruction, u64)>,
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
    const fn capacity() -> usize {
        1024
    }

    fn new(handle: GatherHandle) -> Self {
        Self {
            handle,
            buffer: Default::default(),
        }
    }

    fn init(&mut self) {
        let _coordination_lock = REGISTRATION_LOCK.read();
        self.handle.register(self.buffer.clone());
        self.push(TraceInstruction::Init(InstrInit {
            thread_name: format!("{:?}", std::thread::current().id().to_owned()),
            thread_id: thread_id::get(),
        }))
    }

    fn push(&mut self, instruction: TraceInstruction) {
        let mut guard = self.buffer.lock().unwrap();
        let buffer = &mut guard.buffer;
        if buffer.capacity() < Self::capacity() {
            let to_reserve = Self::capacity() - buffer.capacity();
            buffer.reserve(to_reserve);
        }
        buffer.push((instruction, START_TIME.elapsed().as_nanos() as _));
        if buffer.len() == buffer.capacity() {
            let buffer = std::mem::take(&mut *buffer);
            self.handle.flush(guard.thread_id, buffer);
        }
    }
}
