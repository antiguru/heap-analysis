//! Tracking allocator implementation

#![forbid(missing_docs)]

use bincode::Options;
use std::alloc::{GlobalAlloc, Layout};
use std::cell::{Cell, RefCell};
use std::io::{BufWriter, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{
    bounded, select, tick, unbounded, Receiver, RecvError, SendError, Sender, TryRecvError,
};

use lazy_static::lazy_static;
use libc::c_void;
use retain_mut::RetainMut;

use track_types::{
    InstrAllocation, InstrInit, InstrStack, InstrStackDetails, TimestampedTraceInstruction,
    TraceInstruction, TraceProtocol, ENV_HEAP_ANALYSIS_ADDR,
};

use crate::stacktrace::TraceTree;
use crate::stacktrace::{GlobalTraceTree, Trace};

mod stacktrace;

/// Tracking memory allocator
///
/// # Example
/// ```rust
/// #[global_allocator]
/// static ALLOC: heap_analysis_track::TrackingAllocator<std::alloc::System> = heap_analysis_track::TrackingAllocator(std::alloc::System);
///
/// fn main() {
///     // Start the analysis service, then enable tracing:
///     // ALLOC.start();
/// }
/// ```
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

/// Monotonically-increasing thread counter to assign thread IDs
static THREAD_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// TODO: Provide realloc, alloc_zeroed
unsafe impl<A: GlobalAlloc> GlobalAlloc for TrackingAllocator<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = self.0.alloc(layout);
        AllocationWriter::writer(|writer| writer.handle_alloc(ptr as _, layout.size(), 2));
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        AllocationWriter::writer(|writer| writer.handle_dealloc(ptr as _, layout.size(), 2));
        self.0.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = self.0.realloc(ptr, layout, new_size);
        if layout.size() != new_size || ptr != new_ptr {
            AllocationWriter::writer(|writer| {
                writer.handle_realloc(ptr as _, new_ptr as _, layout.size(), new_size, 2)
            });
        }
        new_ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = self.0.alloc_zeroed(layout);
        AllocationWriter::writer(|writer| writer.handle_alloc(ptr as _, layout.size(), 2));
        ptr
    }
}

/// State of the allocation writer
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
    Register(
        Arc<Mutex<ThreadState>>,
        Arc<(Mutex<bool>, Condvar)>,
        InstrInit,
    ),
}

/// Handle to the gatherer thread
///
/// Stores a thread-local sender and a shared handle to the gatherer thread for termination.
#[derive(Clone, Debug)]
struct GatherHandle {
    /// Send endpoint to provide data
    sender: Option<Sender<GathererProtocol>>,
    /// shared join handle to wait for termination of the gatherer
    handle: Arc<Mutex<Option<[JoinHandle<()>; 2]>>>,
}

impl Default for GatherHandle {
    fn default() -> Self {
        let (sender, receiver) = bounded(64);

        let (to_resolv_sender, to_resolv_receiver) = unbounded();
        let (resolved_sender, resolved_receiver) = bounded(64);

        let handle = std::thread::Builder::new()
            .name("HA-gather".to_owned())
            .spawn(|| {
                // Disable tracking for this thread
                AllocationWriter::WRITER.with(|x| *x.borrow_mut() = TrackingState::Disabled);
                let addr = std::env::var(ENV_HEAP_ANALYSIS_ADDR);
                let addr = addr.as_deref().unwrap_or("localhost:64123");
                let mut gatherer =
                    Gatherer::new(receiver, addr, to_resolv_sender, resolved_receiver);
                gatherer.run();
            })
            .unwrap();

        let resolv_handle = std::thread::Builder::new()
            .name("HA-resolv".to_owned())
            .spawn(|| {
                // Disable tracking for this thread
                AllocationWriter::WRITER.with(|x| *x.borrow_mut() = TrackingState::Disabled);
                let resolver = Resolver::new(to_resolv_receiver, resolved_sender);
                resolver.run();
            })
            .unwrap();

        Self {
            sender: Some(sender),
            handle: Arc::new(Mutex::new(Some([handle, resolv_handle]))),
        }
    }
}

impl GatherHandle {
    /// Register a thread state with the gatherer
    fn register<T>(
        &self,
        thread_state: Arc<Mutex<ThreadState>>,
        info: InstrInit,
        registration_lock: T,
    ) {
        let condvar = Arc::new((Mutex::new(false), Condvar::new()));
        std::mem::drop(registration_lock);

        self.sender
            .as_ref()
            .expect("Sender exists until drop")
            .send(GathererProtocol::Register(
                thread_state,
                Arc::clone(&condvar),
                info,
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
            std::mem::drop(handle.lock().map(|mut handle| {
                handle
                    .take()
                    .map(|handle| handle.into_iter().for_each(|x| x.join().unwrap()))
            }));
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
    connection: BufWriter<TcpStream>,
    to_resolv_sender: Sender<ResolverProtocol>,
    resolved_receiver: Receiver<(usize, InstrStackDetails)>,
    /// Translation table of thread-local stack frames to global stack frames
    trace_tree: GlobalTraceTree,
    /// Last time of timestamp announcement
    last_tick: Instant,
    /// Current timestamp
    timestamp: Duration,
}

impl Gatherer {
    /// Common bincode configuration
    fn bincode() -> impl bincode::Options {
        bincode::options().with_fixint_encoding()
    }

    /// Construct a new gatherer from a receiver of thread updates
    fn new<A: ToSocketAddrs>(
        receiver: Receiver<GathererProtocol>,
        addr: A,
        to_resolv_sender: Sender<ResolverProtocol>,
        resolved_receiver: Receiver<(usize, InstrStackDetails)>,
    ) -> Self {
        let stream = TcpStream::connect(addr).unwrap();
        stream.set_nodelay(true).unwrap();
        let connection = BufWriter::new(stream);
        Self {
            buffers: Default::default(),
            receiver,
            connection,
            to_resolv_sender,
            resolved_receiver,
            trace_tree: Default::default(),
            last_tick: Instant::now(),
            timestamp: Duration::from_secs(0),
        }
    }

    /// Handle data from worker threads. Returns once all work threads disappear.
    fn run(&mut self) {
        let tick = tick(Duration::from_millis(500));
        loop {
            select! {
                recv(tick) -> _tick => {
                    self.maybe_tick();
                },
                recv(self.receiver) -> msg => {
                    match msg {
                        Ok(protocol) => self.handle_protocol(protocol),
                        Err(_) => break,
                    }
                    // self.maybe_tick();
                },
                recv(self.resolved_receiver) -> msg => {
                    match msg {
                        Ok((index, details)) => self.handle_details(index, details),
                        Err(_) => break,
                    }
                }
            }
        }
    }

    fn shutdown(&mut self) {
        self.handle_buffers();
        self.to_resolv_sender
            .send(ResolverProtocol::Shutdown)
            .unwrap();
        while let Ok((index, details)) = self.resolved_receiver.recv() {
            self.handle_details(index, details)
        }
        self.connection.flush().unwrap();
    }

    fn maybe_tick(&mut self) {
        if self.last_tick.elapsed() > Duration::from_millis(500) {
            self.last_tick = Instant::now();
            self.handle_buffers();
        }
    }

    fn handle_details(&mut self, index: usize, details: InstrStackDetails) {
        let details = details.into();
        Self::bincode()
            .serialize_into::<_, TraceProtocol>(
                &mut self.connection,
                &TraceProtocol::Stack { index, details },
            )
            .unwrap();
    }

    fn handle_protocol(&mut self, protocol: GathererProtocol) {
        match protocol {
            GathererProtocol::Register(shared_buffer, condvar, info) => {
                self.buffers.get_mut().push(shared_buffer);
                let (lock, signal) = &*condvar;
                let mut started = lock.lock().unwrap();
                *started = true;
                signal.notify_one();
                Self::bincode()
                    .serialize_into::<_, TraceProtocol>(
                        &mut self.connection,
                        &TraceProtocol::Init(info),
                    )
                    .unwrap();
            }
            GathererProtocol::Flush(thread_id, mut buffer) => {
                self.send_buffer(thread_id, &mut buffer);
            }
        }
        self.connection.flush().unwrap();
    }

    fn drain_receiver(&mut self) -> Result<(), crossbeam_channel::TryRecvError> {
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
        // Block new registrations from appearing.
        let _coordination_lock = REGISTRATION_LOCK.write();
        // Drain all pending data
        let _ = self.drain_receiver();
        // Capture current time as timestamp.
        let next_timestamp = START_TIME.elapsed();
        let mut buffer = Vec::with_capacity(TraceBuffer::capacity());
        // Take the local buffers to allow calling &mut self functions.
        let mut states = self.buffers.take();
        let mut index = 0;
        while index < states.len() {
            // Limit scope of lock to not include sending data over the network
            let (thread_id, dead) = {
                let mut guard = states[index].lock().unwrap();
                let _ = self.drain_receiver();
                std::mem::swap(&mut buffer, &mut guard.buffer);
                (guard.thread_id, guard.dead)
            };
            self.send_buffer(thread_id, &mut buffer);
            // Remove dead threads
            if dead {
                // TODO: Announce thread termination
                states.remove(index);
                self.trace_tree.remove_thread(thread_id);
            } else {
                index += 1;
            }
        }
        let empty = self.buffers.replace(states);
        assert_eq!(empty.len(), 0);
        // Announce new timestamp
        self.timestamp = next_timestamp;
        Self::bincode()
            .serialize_into::<_, TraceProtocol>(
                &mut self.connection,
                &TraceProtocol::Timestamp(self.timestamp.as_nanos() as u64),
            )
            .unwrap();

        self.connection.flush().unwrap();
    }

    /// Send a buffer. Drains the contents from the buffer but leave allocation in place.
    fn send_buffer(&mut self, thread_id: usize, buffer: &mut Vec<TimestampedTraceInstruction>) {
        if buffer.is_empty() {
            return;
        }
        let mut thread_tree = self.trace_tree.for_thread(thread_id);
        buffer.retain_mut(|(instr, _time)| match instr {
            TraceInstruction::Stack(stack) => {
                let (updated, global_parent, global_index) =
                    thread_tree.push(stack.parent, stack.ip as _);
                if updated {
                    self.to_resolv_sender
                        .send(ResolverProtocol::Resolv(global_index, stack.ip))
                        .unwrap();
                    stack.index = global_index;
                    stack.parent = global_parent;
                }
                updated
            }
            TraceInstruction::Allocate(alloc) | TraceInstruction::Deallocate(alloc) => {
                let global_id = thread_tree.lookup(alloc.trace_index.0 as usize);
                alloc.trace_index = global_id.into();
                true
            }
        });
        let protocol = TraceProtocol::Instructions {
            timestamp: self.timestamp.as_nanos() as u64,
            thread_id,
            buffer: std::mem::take(buffer),
        };
        Self::bincode()
            .serialize_into::<_, TraceProtocol>(&mut self.connection, &protocol)
            .unwrap();
        if let TraceProtocol::Instructions { buffer: b, .. } = protocol {
            *buffer = b;
        }
        buffer.clear();
    }
}

impl Drop for Gatherer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

enum ResolverProtocol {
    Resolv(usize, u64),
    Shutdown,
}

struct Resolver {
    receiver: Receiver<ResolverProtocol>,
    sender: Sender<(usize, InstrStackDetails)>,
}

impl Resolver {
    fn new(
        receiver: Receiver<ResolverProtocol>,
        sender: Sender<(usize, InstrStackDetails)>,
    ) -> Self {
        Self { receiver, sender }
    }

    /// Resolve an instruction pointer to a symbol.
    fn resolve(&self, ip: *mut c_void) -> InstrStackDetails {
        let mut details = InstrStackDetails::default();
        unsafe {
            backtrace::resolve_unsynchronized(ip as _, |sym| {
                details.name = sym.name().map(|name| format!("{:#}", name));
                details.filename = sym.filename().map(Into::into);
                details.lineno = sym.lineno();
                details.colno = sym.colno();
            });
        }
        details
    }

    fn run(&self) {
        loop {
            match self.receiver.recv() {
                Ok(ResolverProtocol::Resolv(index, ip)) => {
                    let details = self.resolve(ip as *mut c_void);
                    match self.sender.send((index, details)) {
                        Ok(_) => {}
                        Err(SendError(msg)) => {
                            eprintln!("Failed to send: {:?}", msg);
                            break;
                        }
                    }
                }
                Ok(ResolverProtocol::Shutdown) => break,
                Err(RecvError) => break,
            }
        }
    }
}

/// A thread-local tool to write down allocation details.
struct AllocationWriter {
    /// Map stack frames to identifiers
    trace_tree: TraceTree,
    /// Buffer for outgoing trace messages
    trace_buffer: TraceBuffer,
}

impl AllocationWriter {
    thread_local! {
        /// Thread-local tracking state
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
        // `try_with` to prevent tracking allocations once the TLS is in a destructed state.
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

    /// Construct a new [AllocationWriter]
    fn new(handle: GatherHandle) -> Self {
        Self {
            trace_tree: Default::default(),
            trace_buffer: TraceBuffer::new(handle),
        }
    }

    /// Initialize this [AllocationWriter]. Must only be called once.
    fn init(&mut self) {
        self.trace_buffer.init();
    }

    /// Determine the trace index for the `trace`. Returns the index of the last element of the
    /// trace, i.e, current stack frame.
    fn alloc_index(&mut self, trace: &Trace) -> usize {
        let trace_buffer = &mut self.trace_buffer;
        self.trace_tree.index(trace, |ip, parent, index| {
            trace_buffer.push(TraceInstruction::Stack(InstrStack {
                ip: ip as _,
                parent,
                index,
            }));
            true
        })
    }

    /// Handle a memory allocation
    fn handle_alloc(&mut self, ptr: u64, size: usize, skip: usize) {
        let trace = Trace::new(skip);
        let trace_index = self.alloc_index(&trace).into();
        let allocate = InstrAllocation {
            trace_index,
            ptr,
            size,
        };
        self.trace_buffer.push(TraceInstruction::Allocate(allocate))
    }

    fn handle_dealloc(&mut self, ptr: u64, size: usize, skip: usize) {
        let trace = Trace::new(skip);
        let trace_index = self.alloc_index(&trace).into();
        let free = InstrAllocation {
            trace_index,
            ptr,
            size,
        };
        self.trace_buffer.push(TraceInstruction::Deallocate(free))
    }

    fn handle_realloc(
        &mut self,
        ptr: u64,
        new_ptr: u64,
        size: usize,
        new_size: usize,
        skip: usize,
    ) {
        let trace = Trace::new(skip);
        let trace_index = self.alloc_index(&trace).into();
        let free = InstrAllocation {
            trace_index,
            ptr,
            size,
        };
        self.trace_buffer.push(TraceInstruction::Deallocate(free));
        let allocate = InstrAllocation {
            trace_index,
            ptr: new_ptr,
            size: new_size,
        };
        self.trace_buffer.push(TraceInstruction::Allocate(allocate));
    }
}

impl Drop for AllocationWriter {
    fn drop(&mut self) {
        self.trace_buffer.buffer.lock().unwrap().dead = true;
    }
}

#[derive(Debug)]
struct ThreadState {
    buffer: Vec<(TraceInstruction, u64)>,
    thread_id: usize,
    dead: bool,
}

impl Default for ThreadState {
    fn default() -> Self {
        Self {
            buffer: Default::default(),
            thread_id: THREAD_COUNTER.fetch_add(1, Ordering::SeqCst),
            dead: false,
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
        let registration_lock = REGISTRATION_LOCK.read().unwrap();
        let info = InstrInit {
            thread_name: format!("{:?}", std::thread::current().id().to_owned()),
            thread_id: self.buffer.lock().unwrap().thread_id,
        };
        self.handle
            .register(self.buffer.clone(), info, registration_lock);
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
            let thread_id = guard.thread_id;
            std::mem::drop(guard);
            self.handle.flush(thread_id, buffer);
        }
    }
}
