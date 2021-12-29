use std::collections::HashMap;
use std::net::TcpListener;

use crossbeam_channel::{bounded, TryRecvError};
use timely::dataflow::channels::pact::Pipeline;

use timely::dataflow::operators::generic::source;
use timely::dataflow::operators::Operator;
use timely::scheduling::Scheduler;

use crate::merge::Merger;
use track_types::{InstrAllocation, TraceInstruction, TraceProtocol, ENV_HEAP_ANALYSIS_ADDR};

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
                                        .map(|(data, time)| (time, (thread_id, data)))
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
                    // time -> thread_id -> (alloc'ed, dealloc'ed)
                    // time -> (alloc_thread_id, dealloc_thread_id, alloc_trace_index, dealloc_trace_index) -> (alloc'ed, dealloc'ed, alloc count, dealloc count)
                    let mut aggregation: HashMap<
                        u64,
                        HashMap<(usize, usize, usize, usize), (usize, usize)>,
                    > = HashMap::new();
                    // ptr -> (thread_id, size, trace_index)
                    let mut allocated: HashMap<u64, (usize, usize, usize)> = Default::default();
                    let mut merger: Merger<Vec<(u64, (usize, TraceInstruction))>> =
                        Default::default();
                    let mut last_ts = 0;
                    move |input, output, not| {
                        while let Some((time, data)) = input.next() {
                            merger.push(data.replace(Default::default()));
                            not.notify_at(time.retain());
                        }
                        for (time, _) in not.by_ref() {
                            let timed = aggregation.entry(*time.time()).or_default();
                            while merger
                                .peek()
                                .map(|head| head.0 < *time.time())
                                .unwrap_or(false)
                            {
                                let (_timestamp_ns, (thread_id, data)) = merger.next().unwrap();
                                assert!(last_ts <= _timestamp_ns);
                                last_ts = _timestamp_ns;
                                match data {
                                    TraceInstruction::Init(_) => {}
                                    TraceInstruction::Stack(_) => {}
                                    TraceInstruction::Allocate(InstrAllocation {
                                        size,
                                        ptr,
                                        trace_index,
                                    }) => {
                                        allocated.insert(ptr, (thread_id, size, trace_index));
                                    }
                                    TraceInstruction::Deallocate(InstrAllocation {
                                        ptr,
                                        size: _,
                                        trace_index,
                                    }) => {
                                        if let Some((alloc_thread_id, size, alloc_trace_index)) =
                                            allocated.remove(&ptr)
                                        {
                                            let entry = timed
                                                .entry((
                                                    alloc_thread_id,
                                                    thread_id,
                                                    alloc_trace_index,
                                                    trace_index,
                                                ))
                                                .or_default();
                                            entry.0 += size;
                                            entry.1 += 1;
                                        } else {
                                            eprintln!("Free without allocation: {:0x}", ptr);
                                        }
                                    }
                                }
                            }

                            for (thread_id, (alloced, freeed)) in aggregation
                                .remove(time.time())
                                .unwrap_or_default()
                                .into_iter()
                                .filter(|(key, _)| key.0 != key.1)
                            {
                                output.session(&time).give((thread_id, alloced, freeed));
                            }
                        }
                    }
                })
                .sink(Pipeline, "sink", {
                    let mut buffer = Default::default();
                    move |input| {
                        while let Some((time, data)) = input.next() {
                            data.swap(&mut buffer);
                            buffer.sort_by_key(|(_thread_id, _alloced, freeed)| *freeed);
                            println!("[{}]", time.time());
                            for (thread_id, alloced, freeed) in buffer.drain(..) {
                                println!(
                                    "\tthread: {:?}, alloced: {}, freeed: {}",
                                    thread_id, alloced, freeed
                                );
                            }
                        }
                    }
                });
        })
    })
    .unwrap(); // asserts error-free execution
}

mod merge {
    use std::cmp::{Ordering, Reverse};
    use std::collections::BinaryHeap;

    struct Head<I: Iterator>(I, I::Item);

    impl<I: Iterator> Eq for Head<I> where I::Item: Eq {}

    impl<I: Iterator> PartialEq for Head<I>
    where
        I::Item: Eq,
    {
        fn eq(&self, other: &Self) -> bool {
            self.1.eq(&other.1)
        }
    }

    impl<I: Iterator> PartialOrd for Head<I>
    where
        I::Item: Eq + PartialOrd,
    {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            self.1.partial_cmp(&other.1)
        }
    }

    impl<I: Iterator> Ord for Head<I>
    where
        I::Item: Ord,
    {
        fn cmp(&self, other: &Self) -> Ordering {
            self.1.cmp(&other.1)
        }
    }

    pub struct Merger<I: IntoIterator> {
        iters: BinaryHeap<Reverse<Head<I::IntoIter>>>,
    }

    impl<I: IntoIterator> Merger<I>
    where
        I::Item: Ord,
    {
        fn maybe_push(&mut self, mut iter: I::IntoIter) {
            if let Some(next) = iter.next() {
                self.iters.push(Reverse(Head(iter, next)));
            }
        }
    }

    impl<I: IntoIterator> Default for Merger<I>
    where
        I::Item: Ord,
    {
        fn default() -> Self {
            Self {
                iters: Default::default(),
            }
        }
    }

    impl<I: IntoIterator> Extend<I> for Merger<I>
    where
        I::Item: Ord,
    {
        fn extend<Is: IntoIterator<Item = I>>(&mut self, things: Is) {
            self.iters.extend(things.into_iter().flat_map(|into_iter| {
                let mut iter = into_iter.into_iter();
                iter.next().map(|next| Reverse(Head(iter, next)))
            }))
        }
    }

    impl<I: IntoIterator> Merger<I>
    where
        I::Item: Ord,
    {
        pub fn push(&mut self, into_iter: I) {
            self.maybe_push(into_iter.into_iter());
        }

        pub fn peek(&mut self) -> Option<&I::Item> {
            self.iters.peek().map(|head| &(head.0).1)
        }
    }

    impl<I: IntoIterator> Iterator for Merger<I>
    where
        I::Item: Ord,
    {
        type Item = I::Item;

        fn next(&mut self) -> Option<Self::Item> {
            if let Some(Reverse(Head(iter, item))) = self.iters.pop() {
                self.maybe_push(iter);
                Some(item)
            } else {
                None
            }
        }
    }

    impl<I: IntoIterator> From<Vec<I>> for Merger<I>
    where
        I::Item: Ord,
    {
        fn from(elements: Vec<I>) -> Self {
            let mut merger = Merger::default();
            merger.extend(elements.into_iter());
            merger
        }
    }

    impl<I: IntoIterator> FromIterator<I> for Merger<I>
    where
        I::Item: Ord,
    {
        #[inline]
        fn from_iter<II: IntoIterator<Item = I>>(iter: II) -> Self {
            let mut merger = Merger::default();
            merger.extend(iter.into_iter());
            merger
        }
    }

    #[cfg(test)]
    mod test {
        use crate::Merger;

        #[test]
        fn one_element() {
            let mut merger: Merger<_> = Some(vec![1]).into_iter().collect();
            assert_eq!(merger.next(), Some(1));
            assert_eq!(merger.next(), None);
        }
        #[test]
        fn two_elements() {
            let mut merger: Merger<_> = [vec![0], vec![1]].into_iter().collect();
            assert_eq!(merger.next(), Some(0));
            assert_eq!(merger.next(), Some(1));
            assert_eq!(merger.next(), None);
            let mut merger: Merger<_> = [vec![1], vec![0]].into_iter().collect();
            assert_eq!(merger.next(), Some(0));
            assert_eq!(merger.next(), Some(1));
            assert_eq!(merger.next(), None);
        }
    }
}
