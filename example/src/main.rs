use heap_analysis_track::TrackingAllocator;

#[global_allocator]
static ALLOC: TrackingAllocator<std::alloc::System> = TrackingAllocator(std::alloc::System);

fn test() -> String {
    let mut s = " ".to_owned();
    for _ in 0..22 {
        s = format!("{}{}", s, s);
    }
    s
}

fn main() {
    ALLOC.start();
    let v = vec![1, 2, 3];
    println!("Hello, world! {:?}", v);
    let v = vec![1, 2, 3];
    println!("Hello, world! {:?}", v);
    println!("test: {}", test().len());
    std::thread::sleep(std::time::Duration::from_millis(100));

    for i in 0..50 {
        std::thread::spawn(|| {
            println!("thread id: {:?}", std::thread::current().id());
            test()
        });
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}
