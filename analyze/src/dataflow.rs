use crate::{AllocError, OutputData};
use bincode::Options;
use core::default::Default;
use core::hash::Hash;
use core::option::Option::{None, Some};
use core::result::Result;
use core::result::Result::{Err, Ok};
use core::time::Duration;
use crossbeam_channel::{bounded, TryRecvError};
use differential_dataflow::difference::DiffPair;
use differential_dataflow::operators::arrange::arrangement::Arrange;
use differential_dataflow::operators::arrange::ArrangeBySelf;
use differential_dataflow::trace::implementations::ord::OrdValSpine;
use differential_dataflow::trace::{BatchReader, Cursor};
use differential_dataflow::AsCollection;
use std::collections::HashMap;
use std::hash::Hasher;
use std::io::BufReader;
use std::net::TcpListener;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use timely::communication::allocator::Generic;
use timely::communication::WorkerGuards;
use timely::dataflow::channels::pact::{Exchange, Pipeline};
use timely::dataflow::operators::capture::event::Event;
use timely::dataflow::operators::generic::builder_rc::OperatorBuilder;
use timely::dataflow::operators::generic::operator::source;
use timely::dataflow::operators::{Capture, Concatenate, Exchange as ExchangeOp, Inspect, Map, Operator};
use timely::dataflow::scopes::child::Child;
use timely::scheduling::Scheduler;
use timely::worker::Worker;
use track_types::{TraceIndex, TraceInstruction, TraceProtocol, ENV_HEAP_ANALYSIS_ADDR, Timestamp};

fn fnv_hash<H: Hash>(value: &H) -> u64 {
    let mut hasher = fnv::FnvHasher::default();
    value.hash(&mut hasher);
    hasher.finish()
}

const ROUND_TO: u64 = Duration::from_millis(10000).as_nanos() as u64;

pub fn construct_dataflow() -> (
    Receiver<Event<u64, OutputData>>,
    JoinHandle<Result<WorkerGuards<()>, String>>,
) {
    let output = Arc::new(Mutex::new(None));
    let output2 = Arc::clone(&output);
    let thread = std::thread::spawn(|| {
        timely::execute_from_args(std::env::args(), move |worker| {
            let output = construct_dataflow_inner(worker);
            if worker.index() == 0 {
                *output2.lock().unwrap() = Some(output);
            }

            while worker.step_or_park(None) {
                // nop
            }
        })
    });
    loop {
        // println!("taking receiver");
        if let Some(receiver) = output.lock().unwrap().take() {
            return (receiver, thread);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn construct_dataflow_inner(worker: &mut Worker<Generic>) -> Receiver<Event<u64, OutputData>> {
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
        let arranged =
            Arrange::<Child<_, u64>, _, (_, _, _), _>::arrange_core::<_, OrdValSpine<_, _, _, _>>(
                &collection,
                Exchange::new(|((ptr, _), _, _)| fnv_hash(ptr)),
                "ptr arrange",
            );

        let (matched, err_stream) = {
            let mut builder = OperatorBuilder::new("Match ptr".to_owned(), arranged.stream.scope());

            let mut input = builder.new_input(&arranged.stream, Pipeline);
            let (mut output, stream) = builder.new_output();
            let (mut err_output, err_stream) = builder.new_output();

            builder.build(move |mut capabilities| {
                let mut cap = capabilities.pop().unwrap();
                cap.downgrade(&Timestamp::MAX);
                let mut cap = Some(cap);
                let mut stash: HashMap<u64, (u64, usize, TraceIndex)> = Default::default();
                move |frontiers| {
                    let mut output_handle = output.activate();
                    let mut err_output_handle = err_output.activate();
                    input.for_each(|time, data| {
                        let mut session = output_handle.session(&time);
                        let mut err_session = err_output_handle.session(&time);
                        for wrapper in data.iter() {
                            let batch = &wrapper;
                            let mut cursor = batch.cursor();
                            while let Some(ptr) = cursor.get_key(batch) {
                                while let Some(current) = cursor.get_val(batch) {
                                    cursor.map_times(batch, |_time, diff| {
                                        if diff.element1 > 0 {
                                            let old = stash.insert(*ptr, *current);
                                            if let Some(old) = old {
                                                err_session.give(AllocError::DoubleAlloc {
                                                    ptr: *ptr,
                                                    old,
                                                    new: *current,
                                                });
                                            }
                                        } else {
                                            match stash.remove(ptr) {
                                                Some(alloc) => session.give((
                                                    *ptr,
                                                    (alloc, *current, -diff.element2),
                                                )),
                                                None => err_session.give(AllocError::DoubleFree {
                                                    ptr: *ptr,
                                                    info: *current,
                                                }),
                                            }
                                        }
                                    });
                                    cursor.step_val(batch);
                                }
                                cursor.step_key(batch);
                            }
                        }
                    });
                    if frontiers[0].is_empty() {
                        if let Some(cap) = cap.take() {
                            let mut err_session = err_output_handle.session(&cap);
                            for (ptr, data) in stash.drain() {
                                err_session.give(AllocError::DoubleFree {
                                    ptr,
                                    info: data,
                                })
                            }
                        }
                    }
                }
            });
            (stream, err_stream)
        };
        let alloc_per_thread_pair = matched
            // .inspect(|(ptr, (alloc, dealloc, size))| {
            //     println!(
            //         "ptr: {:x}, {:?} -> {:?}, size: {}",
            //         ptr, alloc, dealloc, size,
            //     );
            // });
            .map(|(_ptr, (alloc, dealloc, size))| {
                (
                    ((alloc.1, dealloc.1), ()),
                    (dealloc.0 + ROUND_TO - 1) / ROUND_TO * ROUND_TO,
                    size,
                )
            })
            .as_collection()
            .arrange_by_self()
            .as_collection(|k, _| k.0);
        alloc_per_thread_pair.inspect(|(k, t, d)| println!("k: {:?}, t: {}, d: {}", k, t, d));
        let alloc_per_thread_pair = alloc_per_thread_pair.inner.unary_notify(Exchange::new(|_| 0), "group_by_time", None, {
            let mut stash: HashMap<Timestamp, Vec<(usize, usize, isize)>> = Default::default();
            let mut buffer = Default::default();
            move |input, output, not| {
                while let Some((time, data)) = input.next() {
                    data.swap(&mut buffer);
                    stash.entry(*time.time()).or_default().extend(buffer.drain(..).map(|((t1, t2), _, size)| (t1, t2, size)));
                    not.notify_at(time.retain());
                }
                not.for_each(|time, _cnt, _not| {
                    if let Some(data) = stash.remove(time.time()) {
                        output.session(&time).give(OutputData::AllocPerThreadPair(data));
                    }
                })
            }
        });
        err_stream.inspect(|err| println!("Err: {:?}", err));
        scope
            .concatenate([
                err_stream.map(OutputData::AllocError),
                alloc_per_thread_pair,
            ])
            .exchange(|_| 0)
            .capture()
    })
}
