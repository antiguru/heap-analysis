use std::net::TcpListener;

use crossbeam_channel::{bounded, TryRecvError};

use timely::dataflow::operators::generic::source;
use timely::dataflow::operators::Inspect;
use timely::scheduling::Scheduler;

use track_types::{TraceProtocol, ENV_HEAP_ANALYSIS_ADDR};

fn main() {
    timely::execute_from_args(std::env::args(), |worker| {
        worker.dataflow::<u64, _, _>(|scope| {
            let index = scope.index();
            source(scope, "Trace reader", |cap, info| {
                let activator = scope.sync_activator_for(&info.address[..]);
                let mut conn = if index == 0 {
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
                                },
                                Err(err) => {
                                    eprintln!("Exiting reader thread: {:?}", err);
                                    break;
                                }
                            }
                        }
                    });
                    Some(receiver)
                } else {
                    None
                };
                let mut cap = Some(cap);
                move |output| {
                    let exit = if let Some(cap) = cap.as_mut() {
                        if let Some(conn) = &conn {
                            let mut fuel = 16;
                            let mut res = false;
                            while fuel > 0 && !res {
                                fuel -= 1;
                                res = match conn.try_recv() {
                                    Ok(TraceProtocol::Instructions {
                                        thread_id,
                                        mut buffer,
                                    }) => {
                                        let mut data = buffer
                                            .drain(..)
                                            .map(|(data, time)| ((thread_id, data), time))
                                            .collect();
                                        output.session(&cap).give_vec(&mut data);
                                        false
                                    }
                                    Ok(TraceProtocol::Timestamp(timestamp)) => {
                                        cap.downgrade(&timestamp);
                                        false
                                    }
                                    Err(TryRecvError::Disconnected) => true,
                                    Err(TryRecvError::Empty) => {
                                        fuel = 0;
                                        false
                                    },
                                }
                            }
                            res
                        } else {
                            true
                        }
                    } else {
                        true
                    };
                    if exit {
                        cap.take();
                        conn.take();
                    }
                }
            })
            .inspect(|x| println!("replayed: {:?}", x));
        })
    })
    .unwrap(); // asserts error-free execution
}
