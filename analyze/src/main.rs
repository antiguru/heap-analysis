use std::collections::HashMap;
use std::net::TcpListener;

use crossbeam_channel::{bounded, TryRecvError};
use timely::dataflow::channels::pact::Pipeline;

use timely::dataflow::operators::generic::source;
use timely::dataflow::operators::{Inspect, Operator};
use timely::scheduling::Scheduler;

use track_types::{
    InstrAllocate, InstrFree, TraceInstruction, TraceProtocol, ENV_HEAP_ANALYSIS_ADDR,
};

fn main() {
    timely::execute_from_args(std::env::args(), |worker| {
        worker.dataflow::<u64, _, _>(|scope| {
            let index = scope.index();
            let trace = source(scope, "Trace reader", |cap, info| {
                let activator = scope.sync_activator_for(&info.address[..]);
                let mut state = if index == 0 {
                    let (sender, receiver) = bounded(64);

                    std::thread::spawn(move || {
                        let addr = std::env::var(ENV_HEAP_ANALYSIS_ADDR);
                        let addr = addr.as_deref().unwrap_or("localhost:64123");
                        let listener = TcpListener::bind(addr).unwrap();
                        let stream = listener.incoming().next().unwrap().unwrap();
                        loop {
                            match bincode::deserialize_from::<_, TraceProtocol>(&stream) {
                                Ok(data) => {
                                    sender.send(data).unwrap();
                                    activator.activate().unwrap();
                                }
                                Err(err) => {
                                    eprintln!("Exiting reader thread: {:?}", err);
                                    break;
                                }
                            }
                        }
                        std::mem::drop(sender);
                        activator.activate().unwrap();
                    });
                    Some((cap, receiver))
                } else {
                    None
                };

                move |output| {
                    let mut exit = false;
                    if let Some((cap, receiver)) = state.as_mut() {
                        let mut fuel = 16;
                        while fuel > 0 && !exit {
                            fuel -= 1;
                            match receiver.try_recv() {
                                Ok(TraceProtocol::Instructions {
                                    thread_id,
                                    mut buffer,
                                }) => {
                                    let mut data = buffer
                                        .drain(..)
                                        .map(|(data, time)| ((thread_id, data), time))
                                        .collect();
                                    output.session(&cap).give_vec(&mut data);
                                }
                                Ok(TraceProtocol::Timestamp(timestamp)) => {
                                    cap.downgrade(&timestamp);
                                }
                                Err(TryRecvError::Disconnected) => exit = true,
                                Err(TryRecvError::Empty) => {
                                    fuel = 0;
                                }
                            }
                        }
                    }
                    if exit {
                        state.take();
                    }
                }
            });
            trace
                .unary_notify(Pipeline, "alloc per thread", None, {
                    let mut stash: HashMap<u64, HashMap<usize, (usize, usize)>> = HashMap::new();
                    let mut allocated: HashMap<u64, (usize, usize)> = Default::default();
                    move |input, output, not| {
                        while let Some((time, data)) = input.next() {
                            let timed = stash.entry(*&*time).or_default();
                            not.notify_at(time.retain());
                            for ((thread_id, data), _timestamp_ns) in &*data {
                                match data {
                                    TraceInstruction::Init(_) => {}
                                    TraceInstruction::Stack(_) => {}
                                    TraceInstruction::Allocate(InstrAllocate {
                                        size, ptr, ..
                                    }) => {
                                        timed.entry(*thread_id).or_default().0 += size;
                                        allocated.insert(*ptr, (*thread_id, *size));
                                    }
                                    TraceInstruction::Free(InstrFree { ptr, .. }) => {
                                        if let Some((thread_id, size)) = allocated.remove(ptr) {
                                            timed.entry(thread_id).or_default().1 += size;
                                        } else {
                                            eprintln!("Free without allocation: {:0x}", ptr);
                                        }
                                    }
                                }
                            }
                        }
                        while let Some((time, _)) = not.next() {
                            for (thread_id, (alloced, freeed)) in
                                stash.remove(time.time()).unwrap_or_default().into_iter()
                            {
                                output.session(&time).give((thread_id, alloced, freeed));
                            }
                        }
                    }
                })
                .inspect_time(|time, (thread_id, alloced, freeed)| {
                    println!(
                        "[{}] thread: {}, alloced: {}, freeed: {}",
                        time, thread_id, alloced, freeed
                    );
                });
        })
    })
    .unwrap(); // asserts error-free execution
}
