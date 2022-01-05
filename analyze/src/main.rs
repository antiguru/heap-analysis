use bincode::Options;
use std::collections::HashMap;
use std::io::BufReader;
use std::net::TcpListener;
use std::time::Duration;

use crossbeam_channel::{bounded, TryRecvError};
use timely::dataflow::channels::pact::Pipeline;

use timely::dataflow::operators::generic::source;
use timely::dataflow::operators::{Inspect, Map, Operator};
use timely::scheduling::Scheduler;

use differential_dataflow::difference::DiffPair;
use differential_dataflow::operators::arrange::Arrange;
use differential_dataflow::trace::implementations::ord::OrdValSpine;
use differential_dataflow::trace::{BatchReader, Cursor};
use differential_dataflow::AsCollection;
use differential_dataflow::operators::Count;
use timely::dataflow::scopes::Child;

use track_types::{TraceIndex, TraceInstruction, TraceProtocol, ENV_HEAP_ANALYSIS_ADDR};

fn main() {
    const ROUND_TO: u64 = Duration::from_millis(500).as_nanos() as u64;

    timely::execute_from_args(std::env::args(), |worker| {
        worker.dataflow::<u64, _, _>(|scope| {
            let index = scope.index();
            let trace = source(scope, "Trace reader", |cap, info| {
                let mut state = if index == 0 {
                    let activator = scope.sync_activator_for(&info.address[..]);
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
                        })
                        .unwrap();
                    Some((cap, receiver))
                } else {
                    None
                };

                let activator = scope.activator_for(&info.address[..]);
                move |output| {
                    let mut exit = false;
                    if let Some((cap, receiver)) = state.as_mut() {
                        let mut fuel = 16;
                        while fuel > 0 && !exit {
                            fuel -= 1;
                            match receiver.try_recv() {
                                Ok(TraceProtocol::Instructions {
                                       timestamp,
                                       thread_id,
                                       mut buffer,
                                   }) => {
                                    if *cap.time() != timestamp {
                                        cap.downgrade(&timestamp);
                                    }
                                    let mut data = buffer
                                        .drain(..)
                                        .map(|(data, time)| (time, (thread_id, data)))
                                        .collect();
                                    output.session(&cap).give_vec(&mut data);
                                }
                                Ok(TraceProtocol::Init(_info)) => {}
                                Ok(TraceProtocol::Stack {
                                       index: _,
                                       details: _,
                                   }) => {}
                                Ok(TraceProtocol::Timestamp(timestamp)) => {
                                    cap.downgrade(&timestamp);
                                }
                                Err(TryRecvError::Disconnected) => {
                                    exit = true;
                                    break;
                                }
                                Err(TryRecvError::Empty) => break,
                            }
                        }
                        if fuel == 0 {
                            activator.activate();
                        }
                    }
                    if exit {
                        state.take();
                    }
                }
            });
            let collection = trace
                .flat_map(|(time, (thread_id, instr))| {
                    let diff: isize = match &instr {
                        TraceInstruction::Stack(_) => 0,
                        TraceInstruction::Allocate(_) => 1,
                        TraceInstruction::Deallocate(_) => -1,
                    };
                    instr.ptr().map(|ptr| {
                        (
                            (ptr, (time, thread_id, instr.trace_index().unwrap())),
                            time,
                            DiffPair::new(diff, instr.size().unwrap() as isize * diff),
                        )
                    })
                })
                .as_collection();
            let arranged = Arrange::<Child<_, u64>, _, (_, _, _), _>::arrange::<
                OrdValSpine<_, _, _, _>,
            >(&collection);
            let matched = arranged
                .stream
                .unary_frontier(Pipeline, "accumulatable", |_cap, _info| {
                    let mut stash: HashMap<u64, (u64, usize, TraceIndex)> = Default::default();
                    move |input, output| {
                        input.for_each(|time, data| {
                            let mut session = output.session(&time);
                            for wrapper in data.iter() {
                                let batch = &wrapper;
                                let mut cursor = batch.cursor();
                                while let Some(ptr) = cursor.get_key(batch) {
                                    while let Some(current) = cursor.get_val(batch) {
                                        cursor.map_times(batch, |_time, diff| {
                                            if diff.element1 > 0 {
                                                let old = stash.insert(*ptr, *current);
                                                if let Some(old) = old {
                                                    println!("Duplicate allocation for 0x{:x}: old: {:?}, current: {:?}", ptr, old, current);
                                                }
                                            } else {
                                                match stash.remove(ptr) {
                                                    Some(alloc) =>
                                                        session.give((*ptr, (alloc, *current, -diff.element2))),
                                                    None =>
                                                        println!("Deallocation with no allocation for 0x{:x}: {:?}", ptr, current),
                                                }
                                            }
                                        });
                                        cursor.step_val(batch);
                                    }
                                    cursor.step_key(batch);
                                }
                            }
                        });
                        if input.frontier.is_empty() {
                            for (ptr, data) in stash.drain() {
                                eprintln!("Leaked 0x{:x} {:?}", ptr, data);
                            }
                        }
                    }
                });
            matched
                // .inspect(|(ptr, (alloc, dealloc, size))| {
                //     println!(
                //         "ptr: {:x}, {:?} -> {:?}, size: {}",
                //         ptr, alloc, dealloc, size,
                //     );
                // });
                .map(|(_ptr, (alloc, dealloc, size))| (((alloc.1, dealloc.1), ()), (dealloc.0 + ROUND_TO - 1)/ROUND_TO*ROUND_TO, size))
                .as_collection()
                .count()
                .inspect(|((k, v), t, d)| println!("k: {:?} v: {} t: {}, d: {}", k, v, t, d));
        })
    })
        .unwrap(); // asserts error-free execution
}
