use heaptrack_rust_track::TrackingAllocator;

#[global_allocator]
static ALLOC: heaptrack_rust_track::TrackingAllocator = TrackingAllocator;

fn test() -> String {
    let mut s = " ".to_owned();
    for _ in 0..22 {
        s = format!("{}{}", s, s);
    }
    s
}

fn main() {
    TrackingAllocator::start();
    let v = vec![1, 2, 3];
    println!("Hello, world! {:?}", v);
    let v = vec![1, 2, 3];
    println!("Hello, world! {:?}", v);
    println!("test: {}", test().len());
}
