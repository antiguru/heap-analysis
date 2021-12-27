# Heap analysis tool for Rust

Heap analysis is a pure-Rust implementation to track memory allocations on the heap.

## Usage

_Heap analysis_ provides a custom allocator that wraps the application's own allocator. To use it, add a dependency to
this crate and declare it as the global allocator like this:

```rust
/// 
#[global_allocator]
static ALLOC: heaptrack_rust_track::TrackingAllocator<std::alloc::System> = TrackingAllocator(std::alloc::System);

fn main() {
    ALLOC.start();
    // Rest of application
}
```

By default, the allocator streams its data to `localhost:64123`. This address can be configured with the environment
symbol `HEAP_ANALYSIS_ADDR`.

### Example usage

1. Start the analysis program in one terminal:
   ```shell
   cargo run --release --bin analyze
   ```
2. Start the program to analyze in another terminal:
   ```shell
   cargo run --example flow_controlled --release -- -w 8
   ```
3. Observe the output of the `analyze` program.
