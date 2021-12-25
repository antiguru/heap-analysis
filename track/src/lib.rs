use std::alloc::{GlobalAlloc, Layout, System};
use std::borrow::BorrowMut;
use std::cell::RefCell;
use std::io::Write;
use std::mem::ManuallyDrop;
use std::ops::DerefMut;
use std::path::Path;
use std::time::Instant;

use crate::heaptrack::HeaptrackWriter;
use crate::trace::Trace;

mod heaptrack;
mod trace;

pub struct HeaptrackAllocator;

struct HeaptrackInner {
    writer: HeaptrackWriter<std::fs::File>,
    in_alloc: bool,
}

enum HeaptrackState {
    None,
    Ready(HeaptrackInner),
}

impl HeaptrackInner {
    thread_local! {
        static WRITER: RefCell<HeaptrackState> = RefCell::new(HeaptrackState::None)
    }

    fn writer<F: FnOnce(&mut HeaptrackWriter<std::fs::File>)>(f: F) {
        let _ = HeaptrackInner::WRITER.try_with(|x| {
            if let Ok(mut borrow) = x.try_borrow_mut() {
                if matches!(*borrow, HeaptrackState::None) {
                    let mut inner = HeaptrackInner {
                        writer: HeaptrackWriter::new(std::fs::File::create(Path::new("heaptrack.XX")).unwrap(), Instant::now()),
                        in_alloc: false,
                    };
                    inner.writer.init();
                    *borrow = HeaptrackState::Ready(inner);
                }
                if let HeaptrackState::Ready(inner) = &mut *borrow {
                    if !inner.in_alloc {
                        inner.in_alloc = true;
                        f(&mut inner.writer);
                        inner.in_alloc = false;
                    }
                }
            }
        });
    }
}

unsafe impl GlobalAlloc for HeaptrackAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        let trace = Trace::new();
        HeaptrackInner::writer(|writer| {
            dbg!(&ptr);
            writer.handle_malloc(ptr, layout.size(), &trace)
        });
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        HeaptrackInner::writer(|writer| {
            writer.handle_free(ptr)
        });
        System.dealloc(ptr, layout)
    }
}
