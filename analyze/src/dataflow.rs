use crate::{AllocError, AllocPerThreadPair, OutputData};
use bincode::Options;
use core::time::Duration;
use crossbeam_channel::{bounded, TryRecvError};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::BufReader;
use std::net::TcpListener;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use differential_dataflow::collection::AsCollection;

use timely::communication::WorkerGuards;
use timely::dataflow::channels::pact::{Exchange, Pipeline};
use timely::dataflow::operators::capture::{Capture, Event};
use timely::dataflow::operators::generic::{source, Operator};
use timely::dataflow::operators::vec::map::Map;
use timely::dataflow::operators::{Concatenate, Exchange as ExchangeOp};
use timely::worker::Worker;
use track_types::{TraceIndex, TraceInstruction, TraceProtocol, ENV_HEAP_ANALYSIS_ADDR};

/// Width of the aggregation time window, in nanoseconds.
const ROUND_TO: u64 = Duration::from_millis(10_000).as_nanos() as u64;

/// Round a timestamp up to the next multiple of [`ROUND_TO`].
fn round_up(time: u64) -> u64 {
    time.div_ceil(ROUND_TO).saturating_mul(ROUND_TO)
}

fn fnv_hash<H: Hash>(value: &H) -> u64 {
    let mut hasher = fnv::FnvHasher::default();
    value.hash(&mut hasher);
    hasher.finish()
}

/// Allocation metadata: the time, thread, and stack trace of an allocation event.
type AllocInfo = (u64, usize, TraceIndex);

/// Internal event produced by the pointer-matching operator.
#[derive(Clone, Debug)]
enum MatchEvent {
    /// A deallocation matched to its allocation.
    Pair {
        alloc_thread: usize,
        dealloc_thread: usize,
        /// Time window (rounded) the deallocation falls into.
        bucket: u64,
        size: isize,
    },
    /// An error observed while matching.
    Error(AllocError),
}

/// Construct the analysis dataflow.
///
/// Returns a receiver of captured output events and the join handle of the worker thread.
pub fn construct_dataflow() -> (
    Receiver<Event<u64, Vec<OutputData>>>,
    JoinHandle<Result<WorkerGuards<()>, String>>,
) {
    let output = Arc::new(Mutex::new(None));
    let output2 = Arc::clone(&output);
    let thread = std::thread::spawn(|| {
        timely::execute_from_args(std::env::args(), move |worker| {
            let receiver = construct_dataflow_inner(worker);
            if worker.index() == 0 {
                *output2.lock().unwrap() = Some(receiver);
            }

            while worker.step_or_park(None) {
                // nop
            }
        })
    });
    loop {
        if let Some(receiver) = output.lock().unwrap().take() {
            return (receiver, thread);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn construct_dataflow_inner(worker: &mut Worker) -> Receiver<Event<u64, Vec<OutputData>>> {
    worker.dataflow::<u64, _, _>(|scope| {
        let index = scope.index();

        // ---- Source: read the trace protocol from a worker-0-hosted TCP socket. ----
        // Emits `(event_time, thread_id, instruction)` triples at a timely time equal to the
        // protocol's message timestamp.
        let trace = source(scope, "Trace reader", |cap, info| {
            let mut state = if index == 0 {
                let sync_activator = scope.worker().sync_activator_for(info.address.to_vec());
                let (sender, receiver) = bounded(64);

                std::thread::Builder::new()
                    .name("network-reader".to_owned())
                    .spawn(move || {
                        let addr = std::env::var(ENV_HEAP_ANALYSIS_ADDR);
                        let addr = addr.as_deref().unwrap_or("localhost:64123");
                        let listener = TcpListener::bind(addr).unwrap();
                        let stream = listener.incoming().next().unwrap().unwrap();
                        let mut stream = BufReader::new(stream);
                        loop {
                            match bincode::options()
                                .with_fixint_encoding()
                                .deserialize_from::<_, TraceProtocol>(&mut stream)
                            {
                                Ok(data) => {
                                    if sender.send(data).is_err() {
                                        break;
                                    }
                                    let _ = sync_activator.activate();
                                }
                                Err(err) => {
                                    eprintln!("Exiting reader thread: {:?}", err);
                                    break;
                                }
                            }
                        }
                        // Close the channel and wake the worker so the source observes the
                        // disconnect and closes out its capability.
                        drop(sender);
                        let _ = sync_activator.activate();
                    })
                    .unwrap();
                Some((cap, receiver))
            } else {
                None
            };

            let activator = scope.activator_for(info.address.clone());
            move |output| {
                let mut exit = false;
                if let Some((cap, receiver)) = state.as_mut() {
                    let mut fuel = 256;
                    while fuel > 0 && !exit {
                        fuel -= 1;
                        match receiver.try_recv() {
                            Ok(TraceProtocol::Instructions {
                                timestamp,
                                thread_id,
                                mut buffer,
                            }) => {
                                if *cap.time() < timestamp {
                                    cap.downgrade(&timestamp);
                                }
                                let mut session = output.session(&*cap);
                                for (instr, time) in buffer.drain(..) {
                                    session.give((time, thread_id, instr));
                                }
                            }
                            Ok(TraceProtocol::Timestamp(timestamp)) => {
                                if *cap.time() < timestamp {
                                    cap.downgrade(&timestamp);
                                }
                            }
                            Ok(TraceProtocol::CreateThread(_))
                            | Ok(TraceProtocol::DestroyThread(_))
                            | Ok(TraceProtocol::Stack(_, _)) => {}
                            Err(TryRecvError::Disconnected) => {
                                exit = true;
                            }
                            Err(TryRecvError::Empty) => break,
                        }
                    }
                    if fuel == 0 {
                        // Drained a full batch; more data is likely pending, so reschedule now.
                        activator.activate();
                    } else if !exit {
                        // The channel ran dry. Keep polling it ourselves rather than rely solely
                        // on the reader thread: the reader can block on a full bounded channel
                        // before reaching its `activate()` call, and a worker that parks at that
                        // moment would never be woken again. A short timed self-activation
                        // guarantees we revisit the channel and observe new data or a disconnect.
                        activator.activate_after(Duration::from_millis(10));
                    }
                }
                if exit {
                    state.take();
                }
            }
        });

        // ---- Match allocations to deallocations by pointer. ----
        // A plain stateful operator, partitioned by pointer, keeps one stash entry per live
        // allocation. Memory is therefore bounded by the number of outstanding allocations.
        let matched = trace.unary_frontier(
            Exchange::new(|(_time, _thread, instr): &(u64, usize, TraceInstruction)| {
                instr.ptr().map(|ptr| fnv_hash(&ptr)).unwrap_or(0)
            }),
            "Match ptr",
            |default_cap, _info| {
                // Capability used to emit leaks once the input is exhausted. Advanced to the input
                // frontier so it does not hold back downstream progress.
                let mut leak_cap = Some(default_cap);
                let mut stash: HashMap<u64, AllocInfo> = HashMap::new();
                move |(input, frontier), output| {
                    input.for_each(|time, data| {
                        let bucket = round_up(*time.time());
                        let mut session = output.session(&time);
                        for (event_time, thread, instr) in data.drain(..) {
                            match instr {
                                TraceInstruction::Allocate(alloc) => {
                                    let info = (event_time, thread, alloc.trace_index);
                                    if let Some(old) = stash.insert(alloc.ptr, info) {
                                        session.give(MatchEvent::Error(AllocError::DoubleAlloc {
                                            ptr: alloc.ptr,
                                            old,
                                            new: info,
                                        }));
                                    }
                                }
                                TraceInstruction::Deallocate(dealloc) => {
                                    match stash.remove(&dealloc.ptr) {
                                        Some(alloc) => session.give(MatchEvent::Pair {
                                            alloc_thread: alloc.1,
                                            dealloc_thread: thread,
                                            bucket,
                                            size: dealloc.size as isize,
                                        }),
                                        None => {
                                            session.give(MatchEvent::Error(AllocError::DoubleFree {
                                                ptr: dealloc.ptr,
                                                info: (event_time, thread, dealloc.trace_index),
                                            }))
                                        }
                                    }
                                }
                                TraceInstruction::Stack(_) => {}
                            }
                        }
                    });

                    let frontier = frontier.frontier();
                    if frontier.is_empty() {
                        // Input exhausted: everything still outstanding is a leak.
                        if let Some(cap) = leak_cap.take() {
                            let mut session = output.session(&cap);
                            for (ptr, info) in stash.drain() {
                                session.give(MatchEvent::Error(AllocError::Leak { ptr, info }));
                            }
                        }
                    } else if let (Some(cap), Some(time)) = (leak_cap.as_mut(), frontier.first()) {
                        if cap.time() < time {
                            cap.downgrade(time);
                        }
                    }
                }
            },
        );

        // ---- Errors are emitted directly. ----
        let errors = matched.clone().flat_map(|event| match event {
            MatchEvent::Error(error) => Some(OutputData::AllocError(error)),
            MatchEvent::Pair { .. } => None,
        });

        // ---- Matched pairs are aggregated per (alloc_thread, dealloc_thread) and time window. ----
        // The differential timestamp is the (low-cardinality) rounded window, so the arrangement
        // backing `consolidate` stays small.
        let pairs = matched
            .flat_map(|event| match event {
                MatchEvent::Pair {
                    alloc_thread,
                    dealloc_thread,
                    bucket,
                    size,
                } => Some(((alloc_thread, dealloc_thread), bucket, (1isize, size))),
                MatchEvent::Error(_) => None,
            })
            .as_collection()
            .consolidate()
            .inner
            .unary_notify(Pipeline, "group_by_window", None, {
                let mut stash: HashMap<u64, Vec<AllocPerThreadPair>> = HashMap::new();
                move |input, output, notificator| {
                    input.for_each(|time, data| {
                        for ((alloc_thread, dealloc_thread), _window, (count, size)) in data.drain(..)
                        {
                            stash.entry(*time.time()).or_default().push(AllocPerThreadPair {
                                alloc_thread,
                                dealloc_thread,
                                count,
                                size,
                            });
                        }
                        notificator.notify_at(time.retain(output.output_index()));
                    });
                    notificator.for_each(|time, _cnt, _not| {
                        if let Some(pairs) = stash.remove(time.time()) {
                            output
                                .session(&time)
                                .give(OutputData::AllocPerThreadPairs(pairs));
                        }
                    });
                }
            });

        // ---- Merge both output streams onto worker 0 and capture them. ----
        scope
            .concatenate(vec![errors, pairs])
            .exchange(|_| 0)
            .capture()
    })
}
