use std::net::TcpListener;
use std::time::{Duration, Instant};

use crossbeam_channel::TryRecvError;

use timely::dataflow::operators::generic::source;
use timely::dataflow::operators::Inspect;
use timely::scheduling::Scheduler;

use track_types::TraceInstruction;

fn main() {
    timely::execute_from_args(std::env::args(), |worker| {
        let mut conn = if worker.index() == 0 {
            let (sender, receiver) = crossbeam_channel::bounded(64);

            std::thread::spawn(move || {
                let listener = TcpListener::bind(format!("127.0.0.1:{}", 64123)).unwrap();
                let stream = listener.incoming().next().unwrap().unwrap();
                loop {
                    match bincode::deserialize_from::<_, Vec<TraceInstruction>>(&stream) {
                        Ok(data) => sender.send(data).unwrap(),
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

        worker.dataflow::<u64, _, _>(move |scope| {
            let time = Instant::now();
            source(scope, "Trace reader", |cap, info| {
                let activator = scope.activator_for(&info.address[..]);
                let mut conn = conn.take();
                let mut cap = Some(cap);
                move |output| {
                    let (exit, activate) = if let Some(cap) = cap.as_mut() {
                        cap.downgrade(&(time.elapsed().as_nanos() as u64));
                        if let Some(conn) = &conn {
                            match conn.try_recv() {
                                Ok(mut data) => {
                                    output.session(&cap).give_vec(&mut data);
                                    (false, true)
                                }
                                Err(TryRecvError::Disconnected) => (true, false),
                                Err(TryRecvError::Empty) => (false, false),
                            }
                        } else {
                            (true, false)
                        }
                    } else {
                        (true, false)
                    };
                    if exit {
                        cap.take();
                        conn.take();
                    } else if activate {
                        activator.activate();
                    } else {
                        activator.activate_after(Duration::from_millis(100));
                    }
                }
            })
            .inspect(|x| println!("replayed: {:?}", x));
        })
    })
    .unwrap(); // asserts error-free execution
}
